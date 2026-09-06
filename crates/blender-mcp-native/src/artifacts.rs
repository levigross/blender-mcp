//! Bounded, immutable snapshots. Capture streams data; inline retrieval has its
//! own smaller bound. Original output paths are provenance and are never reread.

use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use pyo3::prelude::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::errors::operation_error;

pub(crate) const MAX_INLINE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARTIFACTS: usize = 64;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

struct Snapshot {
    path: PathBuf,
    descriptor: Value,
    created: Instant,
}

pub(crate) struct ArtifactStore {
    root: PathBuf,
    entries: HashMap<String, Snapshot>,
    expired: VecDeque<String>,
    ttl: Duration,
    bytes: u64,
    next_capture: u64,
}

impl ArtifactStore {
    pub(crate) fn new(root: PathBuf, ttl: Duration) -> Self {
        Self {
            root,
            entries: HashMap::new(),
            expired: VecDeque::new(),
            ttl,
            bytes: 0,
            next_capture: 0,
        }
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn expire(&mut self) {
        let expired: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.created.elapsed() >= self.ttl)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            if let Some(snapshot) = self.entries.remove(&id) {
                let _ = fs::remove_file(&snapshot.path);
                self.bytes = self
                    .bytes
                    .saturating_sub(snapshot.descriptor["size"].as_u64().unwrap_or(0));
                if self.expired.len() >= MAX_ARTIFACTS {
                    self.expired.pop_front();
                }
                self.expired.push_back(id);
            }
        }
    }

    pub(crate) fn capture(
        &mut self,
        python: Python<'_>,
        path: &str,
        provenance: &Value,
    ) -> PyResult<Value> {
        self.expire();
        let mut source = File::open(path).map_err(|e| artifact_io(python, &e))?;
        let size = source
            .metadata()
            .map_err(|e| artifact_io(python, &e))?
            .len();
        if size > MAX_ARTIFACT_BYTES
            || self.bytes + size > MAX_TOTAL_BYTES
            || self.entries.len() >= MAX_ARTIFACTS
        {
            return Err(operation_error(
                python,
                "artifact_limit",
                "snapshot limit reached (64 MiB/file, 256 MiB total, 64 artifacts); release artifacts or use a thumbnail",
            ));
        }
        self.next_capture += 1;
        let staging = self.root.join(format!("capture-{}.tmp", self.next_capture));
        let outcome = (|| -> PyResult<(u64, String)> {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staging)
                .map_err(|e| artifact_io(python, &e))?;
            let mut digest = Sha256::new();
            let mut buffer = vec![0_u8; 32 * 1024].into_boxed_slice();
            let mut total = 0;
            loop {
                let count = source
                    .read(&mut buffer)
                    .map_err(|e| artifact_io(python, &e))?;
                if count == 0 {
                    break;
                }
                total += count as u64;
                if total > MAX_ARTIFACT_BYTES || self.bytes + total > MAX_TOTAL_BYTES {
                    return Err(operation_error(
                        python,
                        "artifact_limit",
                        "artifact grew beyond snapshot storage limits",
                    ));
                }
                digest.update(&buffer[..count]);
                output
                    .write_all(&buffer[..count])
                    .map_err(|e| artifact_io(python, &e))?;
            }
            Ok((total, format!("{:x}", digest.finalize())))
        })();
        let (size, digest) = match outcome {
            Ok(value) => value,
            Err(error) => {
                let _ = fs::remove_file(staging);
                return Err(error);
            }
        };
        let mut identity = Sha256::new();
        identity.update(digest.as_bytes());
        identity.update(serde_json::to_vec(provenance).unwrap_or_default());
        identity.update(path.as_bytes());
        let id = format!("{:x}", identity.finalize());
        if let Some(snapshot) = self.entries.get(&id) {
            let _ = fs::remove_file(staging);
            return Ok(snapshot.descriptor.clone());
        }
        let snapshot_path = self.root.join(format!("{id}.snapshot"));
        fs::rename(&staging, &snapshot_path).map_err(|e| artifact_io(python, &e))?;
        let name = Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mime = python
            .import("mimetypes")?
            .call_method1("guess_type", (&name,))?
            .get_item(0)?
            .extract::<Option<String>>()?
            .unwrap_or_else(|| "application/octet-stream".to_owned());
        let mut descriptor = json!({
            "id": id, "name": name, "mime_type": mime, "path": path,
            "original_path": path, "snapshot_path": snapshot_path.to_string_lossy(),
            "size": size, "sha256": digest, "provenance": provenance,
            "inline_available": size <= MAX_INLINE_BYTES, "expires_after_secs": self.ttl.as_secs(),
        });
        if let Some(width) = descriptor["provenance"].get("width").cloned() {
            descriptor["width"] = width;
        }
        if let Some(height) = descriptor["provenance"].get("height").cloned() {
            descriptor["height"] = height;
        }
        self.bytes += size;
        self.entries.insert(
            id,
            Snapshot {
                path: snapshot_path,
                descriptor: descriptor.clone(),
                created: Instant::now(),
            },
        );
        Ok(descriptor)
    }

    pub(crate) fn fetch(
        &mut self,
        python: Python<'_>,
        id: &str,
        include_data: bool,
    ) -> PyResult<Value> {
        self.expire();
        let snapshot = self.entries.get(id).ok_or_else(|| {
            operation_error(
                python,
                if self.expired.iter().any(|expired| expired == id) {
                    "artifact_expired"
                } else {
                    "artifact_missing"
                },
                "artifact was released, expired, or does not belong to this session",
            )
        })?;
        let mut result = json!({"artifact": snapshot.descriptor});
        if include_data {
            if snapshot.descriptor["size"].as_u64().unwrap_or(u64::MAX) > MAX_INLINE_BYTES {
                return Err(operation_error(
                    python,
                    "artifact_inline_limit",
                    "artifact exceeds 8 MiB inline limit; request metadata or generate a thumbnail",
                ));
            }
            let bytes = read_inline(python, &snapshot.path)?;
            if Value::String(format!("{:x}", Sha256::digest(&bytes)))
                != snapshot.descriptor["sha256"]
            {
                return Err(operation_error(
                    python,
                    "artifact_corrupt",
                    "artifact snapshot no longer matches its content digest",
                ));
            }
            result["data_base64"] = Value::String(STANDARD.encode(bytes));
        }
        Ok(result)
    }

    pub(crate) fn release(&mut self, python: Python<'_>, ids: &[Value]) -> PyResult<Value> {
        self.expire();
        if ids.len() > MAX_ARTIFACTS || ids.iter().any(|id| !id.is_string()) {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "artifact_ids must contain at most 64 strings",
            ));
        }
        let mut released = 0;
        for id in ids {
            let id = id.as_str().expect("validated string");
            if let Some(snapshot) = self.entries.get(id) {
                match fs::remove_file(&snapshot.path) {
                    Ok(()) => (),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(error) => return Err(artifact_io(python, &error)),
                }
                let size = snapshot.descriptor["size"].as_u64().unwrap_or(0);
                self.entries.remove(id);
                self.bytes = self.bytes.saturating_sub(size);
                released += 1;
            }
        }
        Ok(json!({"released": released, "count": self.entries.len(), "bytes": self.bytes}))
    }

    pub(crate) fn close(&mut self) -> PyResult<()> {
        self.entries.clear();
        self.expired.clear();
        self.bytes = 0;
        match fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(pyo3::exceptions::PyOSError::new_err(e.to_string())),
        }
    }
}
impl Drop for ArtifactStore {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn artifact_io(python: Python<'_>, error: &std::io::Error) -> PyErr {
    operation_error(python, "artifact_missing", error.to_string())
}
fn read_inline(python: Python<'_>, path: &Path) -> PyResult<Vec<u8>> {
    let file = File::open(path).map_err(|e| artifact_io(python, &e))?;
    if file.metadata().map_err(|e| artifact_io(python, &e))?.len() > MAX_INLINE_BYTES {
        return Err(operation_error(
            python,
            "artifact_inline_limit",
            "snapshot exceeds inline limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_INLINE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| artifact_io(python, &e))?;
    if bytes.len() as u64 > MAX_INLINE_BYTES {
        return Err(operation_error(
            python,
            "artifact_inline_limit",
            "snapshot grew beyond inline limit",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_recovers_missing_files_and_retains_failed_deletions() {
        Python::initialize();
        Python::attach(|python| {
            let root: String = python
                .import("tempfile")
                .unwrap()
                .call_method0("mkdtemp")
                .unwrap()
                .extract()
                .unwrap();
            let source = PathBuf::from(&root).join("source.bin");
            fs::write(&source, b"snapshot payload").unwrap();
            let mut store = ArtifactStore::new(PathBuf::from(&root), Duration::from_secs(3600));
            let descriptor = store
                .capture(python, source.to_str().unwrap(), &json!({}))
                .unwrap();
            let id = descriptor["id"].as_str().unwrap();
            let snapshot_path = PathBuf::from(descriptor["snapshot_path"].as_str().unwrap());
            fs::remove_file(&snapshot_path).unwrap();
            // A directory at the snapshot path makes remove_file fail without
            // depending on the effective user's permission privileges.
            fs::create_dir(&snapshot_path).unwrap();
            assert!(store.release(python, &[json!(id)]).is_err());
            assert!(store.entries.contains_key(id));
            assert_eq!(store.bytes, descriptor["size"].as_u64().unwrap());
            fs::remove_dir(&snapshot_path).unwrap();
            let released = store.release(python, &[json!(id)]).unwrap();
            assert_eq!(released["released"], 1);
            assert_eq!(released["count"], 0);
            assert_eq!(released["bytes"], 0);
            assert_eq!(store.release(python, &[json!(id)]).unwrap()["released"], 0);
            assert!(source.is_file());
            store
                .capture(python, source.to_str().unwrap(), &json!({}))
                .unwrap();
            store.close().unwrap();
        });
    }

    #[test]
    fn immutable_streamed_artifacts_have_separate_inline_limits_and_expire() {
        Python::initialize();
        Python::attach(|python| {
            let root: String = python
                .import("tempfile")
                .unwrap()
                .call_method0("mkdtemp")
                .unwrap()
                .extract()
                .unwrap();
            let source = PathBuf::from(&root).join("source.bin");
            let mut file = File::create(&source).unwrap();
            file.write_all(b"initial-content").unwrap();
            let mut store = ArtifactStore::new(PathBuf::from(&root), Duration::from_secs(3600));
            let descriptor = store
                .capture(python, source.to_str().unwrap(), &json!({"frame": 1}))
                .unwrap();
            let id = descriptor["id"].as_str().unwrap();
            fs::write(&source, b"overwritten-source").unwrap();
            let fetched = store.fetch(python, id, true).unwrap();
            assert_eq!(
                STANDARD
                    .decode(fetched["data_base64"].as_str().unwrap())
                    .unwrap(),
                b"initial-content"
            );
            assert!(
                store
                    .fetch(python, id, false)
                    .unwrap()
                    .get("data_base64")
                    .is_none()
            );
            fs::write(
                &source,
                vec![7_u8; usize::try_from(MAX_INLINE_BYTES + 1).unwrap()],
            )
            .unwrap();
            let large = store
                .capture(python, source.to_str().unwrap(), &json!({"frame": 2}))
                .unwrap();
            assert_eq!(large["inline_available"], false);
            let large_id = large["id"].as_str().unwrap();
            assert!(store.fetch(python, large_id, false).is_ok());
            assert!(store.fetch(python, large_id, true).is_err());
            store.entries.get_mut(id).unwrap().created = Instant::now()
                .checked_sub(Duration::from_secs(3601))
                .unwrap();
            let error = store.fetch(python, id, false).unwrap_err();
            assert_eq!(
                error
                    .value(python)
                    .getattr("code")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "artifact_expired"
            );
            let released = store.release(python, &[json!(large_id)]).unwrap();
            assert_eq!(released["count"], 0);
            assert_eq!(released["bytes"], 0);
            store.close().unwrap();
        });
    }
}
