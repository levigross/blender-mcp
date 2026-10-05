//! Versioned, length-prefixed JSON protocol for the Blender bridge.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BridgeRequest {
    pub version: u32,
    pub id: u64,
    #[serde(default)]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
    #[serde(flatten)]
    pub operation: BridgeOperation,
}

impl BridgeRequest {
    pub const fn new(id: u64, operation: BridgeOperation) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            session_id: String::new(),
            expected_instance: None,
            expected_generation: None,
            deadline_unix_ms: None,
            operation,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.version != PROTOCOL_VERSION {
            return Err(ProtocolError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                actual: self.version,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum BridgeOperation {
    Catalog,
    CatalogRefresh,
    OperatorInfo {
        idname: String,
    },
    OperatorPoll {
        idname: String,
        #[serde(default)]
        context_override: Option<Value>,
    },
    OperatorCall {
        idname: String,
        #[serde(default)]
        kwargs: BTreeMap<String, Value>,
        #[serde(default)]
        execution_context: Option<String>,
        #[serde(default)]
        undo: Option<bool>,
        #[serde(default)]
        context_override: Option<Value>,
    },
    ContextRef,
    DataRef,
    RnaGet {
        reference: RnaReference,
        attribute: String,
    },
    RnaSet {
        reference: RnaReference,
        attribute: String,
        value: Value,
    },
    RnaCall {
        reference: RnaReference,
        function: String,
        #[serde(default)]
        args: Vec<Value>,
        #[serde(default)]
        kwargs: BTreeMap<String, Value>,
    },
    RnaDescribe {
        reference: RnaReference,
    },
    RnaPropertyInfo {
        reference: RnaReference,
        attribute: String,
    },
    RnaFunctionInfo {
        reference: RnaReference,
        function: String,
    },
    /// Custom (ID) properties are item access -- `obj["role"]` -- which the
    /// attribute-only RNA operations cannot express.
    IdPropertyKeys {
        reference: RnaReference,
    },
    IdPropertyGet {
        reference: RnaReference,
        key: String,
    },
    IdPropertySet {
        reference: RnaReference,
        key: String,
        value: Value,
    },
    IdPropertyDelete {
        reference: RnaReference,
        key: String,
    },
    ReferenceRelease {
        references: Vec<RnaReference>,
    },
    ReferenceStats,
    /// Retire every handle and start a new reference epoch: a Scheme reset leaves no
    /// value that could still hold one.
    ReferenceReset,
    Batch {
        requests: Vec<BridgeOperation>,
        #[serde(default = "default_batch_budget")]
        max_elapsed_ms: u64,
    },
    RnaItems {
        reference: RnaReference,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_item_limit")]
        limit: usize,
        #[serde(default)]
        expected_revision: Option<String>,
    },
    Status,
    ControlStatus,
    SceneSummary,
    SceneSnapshot {
        #[serde(default = "default_snapshot_limit")]
        limit: usize,
    },
    SceneDiff {
        before: Value,
        #[serde(default)]
        after: Option<Value>,
    },
    Checkpoint {
        filepath: String,
    },
    /// Build or update a node tree in one request: interface sockets, nodes upserted
    /// by name (`{name, type, location?, label?, properties{}, inputs}`), then links
    /// (`[from_node, from_socket, to_node, to_socket]`). Nodes and links stay loosely
    /// typed here; the Blender side validates them against the live node types.
    NodeTreeBuild {
        tree: RnaReference,
        #[serde(default)]
        clear: bool,
        #[serde(default)]
        interface: Vec<Value>,
        nodes: Vec<Value>,
        #[serde(default)]
        links: Vec<Value>,
    },
    /// A page of a collection attribute through `foreach_get`: elements
    /// `[offset, offset + count)`, flattened (`stride` values per element).
    CollectionRead {
        reference: RnaReference,
        attribute: String,
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        count: Option<usize>,
    },
    /// Overwrite elements starting at `offset` with flattened `values` through
    /// `foreach_set`, leaving the rest of the collection unchanged.
    CollectionWrite {
        reference: RnaReference,
        attribute: String,
        offset: usize,
        values: Vec<Value>,
    },
    /// Build a mesh object from vertex positions and index lists. `Mesh.from_pydata`
    /// is Python-defined rather than RNA-published, so the RNA sandbox cannot reach it.
    MeshFromData {
        name: String,
        vertices: Vec<[f64; 3]>,
        #[serde(default)]
        edges: Vec<[u32; 2]>,
        #[serde(default)]
        faces: Vec<Vec<u32>>,
        #[serde(default)]
        collection: Option<RnaReference>,
    },
    Render {
        #[serde(default)]
        filepath: Option<String>,
        #[serde(default)]
        write_still: bool,
    },
    Artifact {
        artifact_id: String,
        #[serde(default = "default_true")]
        include_data: bool,
    },
    ArtifactRelease {
        artifact_ids: Vec<String>,
    },
    RenderStart {
        #[serde(default)]
        filepath: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    Thumbnail {
        #[serde(default)]
        filepath: Option<String>,
        #[serde(default = "default_thumbnail_size")]
        max_size: usize,
    },
    JobStatus {
        job_id: String,
    },
    JobResult {
        job_id: String,
    },
    JobCancel {
        job_id: String,
    },
    RequestStatus {
        request_id: u64,
        #[serde(default)]
        target_session: Option<String>,
    },
    RequestResult {
        request_id: u64,
        #[serde(default)]
        target_session: Option<String>,
    },
    Cancel {
        request_id: u64,
        #[serde(default)]
        target_session: Option<String>,
    },
    Shutdown,
}

const fn default_true() -> bool {
    true
}

const fn default_thumbnail_size() -> usize {
    512
}

impl BridgeOperation {
    pub fn is_mutating(&self) -> bool {
        match self {
            Self::OperatorCall { .. }
            | Self::RnaSet { .. }
            | Self::RnaCall { .. }
            | Self::Render { .. }
            | Self::RenderStart { .. }
            | Self::Thumbnail { .. }
            | Self::Checkpoint { .. }
            | Self::MeshFromData { .. }
            | Self::CollectionWrite { .. }
            | Self::NodeTreeBuild { .. }
            | Self::IdPropertySet { .. }
            | Self::IdPropertyDelete { .. }
            | Self::Shutdown => true,
            Self::Batch { requests, .. } => requests.iter().any(Self::is_mutating),
            _ => false,
        }
    }
}

const fn default_batch_budget() -> u64 {
    20
}

const fn default_snapshot_limit() -> usize {
    1000
}

const fn default_item_limit() -> usize {
    100
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RnaReference {
    pub generation: u64,
    pub id: String,
    pub type_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BridgeResponse {
    pub version: u32,
    pub id: u64,
    // Plain `Option<Value>` would decode a present `"result": null` to `None`, making it
    // indistinguishable from an absent field -- and `validate` would then reject the
    // envelope as carrying neither result nor error. Blender returns `None` constantly
    // (`collection.remove`, `.link`, and a `.get` that misses), so `null` must survive as
    // `Some(Value::Null)`. `default` still covers genuine absence.
    // `skip_serializing_if` still has to stay: an error response must omit the field
    // entirely rather than emit `"result": null`, which now reads back as a real result.
    #[serde(
        default,
        deserialize_with = "null_is_a_result",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<BridgeError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<BlenderReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_revision: Option<String>,
}

/// Wrap whatever is there, including `null`. Serde only calls this when the field is
/// present, so absence still falls through to `Default` and yields `None`.
fn null_is_a_result<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

impl BridgeResponse {
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            result: Some(result),
            error: None,
            reports: Vec::new(),
            events: Vec::new(),
            catalog_revision: None,
        }
    }

    pub fn failure(id: u64, error: BridgeError) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            result: None,
            error: Some(error),
            reports: Vec::new(),
            events: Vec::new(),
            catalog_revision: None,
        }
    }

    pub fn validate(&self, request_id: u64) -> Result<(), ProtocolError> {
        if self.version != PROTOCOL_VERSION {
            return Err(ProtocolError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                actual: self.version,
            });
        }
        if self.id != request_id {
            return Err(ProtocolError::CorrelationMismatch {
                expected: request_id,
                actual: self.id,
            });
        }
        if self.result.is_some() == self.error.is_some() {
            return Err(ProtocolError::InvalidEnvelope(
                "response must contain exactly one of result or error".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BridgeError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub potentially_continuing: bool,
}

impl BridgeError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: None,
            retryable: false,
            potentially_continuing: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlenderReport {
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorCatalog {
    pub protocol_version: u32,
    pub revision: String,
    pub blender_version: String,
    pub operators: Vec<OperatorDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorDescriptor {
    pub idname: String,
    pub steel_name: String,
    pub label: String,
    pub description: String,
    pub module: String,
    pub function: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub properties: Vec<PropertyDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropertyDescriptor {
    pub identifier: String,
    pub kind: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub array_length: Option<usize>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub enum_items: Vec<EnumItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumItem {
    pub identifier: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDescriptor {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub path: String,
    pub size: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Value>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("frame declares {actual} bytes, exceeding the {maximum}-byte limit")]
    FrameTooLarge { actual: usize, maximum: usize },
    #[error("malformed JSON frame: {0}")]
    MalformedJson(String),
    #[error("protocol version mismatch: expected {expected}, received {actual}")]
    VersionMismatch { expected: u32, actual: u32 },
    #[error("response correlation mismatch: expected request {expected}, received {actual}")]
    CorrelationMismatch { expected: u64, actual: u64 },
    #[error("invalid protocol envelope: {0}")]
    InvalidEnvelope(String),
}

#[derive(Debug, Clone)]
pub struct FrameDecoder {
    bytes: Vec<u8>,
    maximum: usize,
}

impl FrameDecoder {
    pub fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), ProtocolError> {
        let buffered = self.bytes.len().saturating_add(chunk.len());
        if buffered > self.maximum.saturating_add(4) {
            return Err(ProtocolError::FrameTooLarge {
                actual: buffered.saturating_sub(4),
                maximum: self.maximum,
            });
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub fn next_json<T: for<'de> Deserialize<'de>>(&mut self) -> Result<Option<T>, ProtocolError> {
        if self.bytes.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_be_bytes(
            self.bytes[..4]
                .try_into()
                .expect("invariant: four-byte prefix is present"),
        ) as usize;
        if length > self.maximum {
            return Err(ProtocolError::FrameTooLarge {
                actual: length,
                maximum: self.maximum,
            });
        }
        if self.bytes.len() < length.saturating_add(4) {
            return Ok(None);
        }
        let payload = &self.bytes[4..4 + length];
        let value = serde_json::from_slice(payload)
            .map_err(|error| ProtocolError::MalformedJson(error.to_string()))?;
        self.bytes.drain(..4 + length);
        Ok(Some(value))
    }

    pub fn buffered_len(&self) -> usize {
        self.bytes.len()
    }
}

pub fn encode_json<T: Serialize>(value: &T, maximum: usize) -> Result<Vec<u8>, ProtocolError> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| ProtocolError::MalformedJson(error.to_string()))?;
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|_| payload.len() <= maximum)
        .ok_or(ProtocolError::FrameTooLarge {
            actual: payload.len(),
            maximum,
        })?;
    let mut framed = Vec::with_capacity(payload.len() + 4);
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(&payload);
    Ok(framed)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    #[test]
    fn rejects_declared_frame_over_limit_before_payload_arrives() {
        let mut decoder = FrameDecoder::new(8);
        decoder
            .push(&9_u32.to_be_bytes())
            .expect("prefix fits buffer");
        assert_eq!(
            decoder.next_json::<Value>(),
            Err(ProtocolError::FrameTooLarge {
                actual: 9,
                maximum: 8
            })
        );
    }

    #[test]
    fn rejects_malformed_json() {
        let payload = b"{";
        let mut bytes = u32::try_from(payload.len())
            .expect("test payload fits in a frame prefix")
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(payload);
        let mut decoder = FrameDecoder::new(128);
        decoder.push(&bytes).expect("frame fits");
        assert!(matches!(
            decoder.next_json::<Value>(),
            Err(ProtocolError::MalformedJson(_))
        ));
    }

    #[test]
    fn validates_versions_and_correlation() {
        let request = BridgeRequest {
            version: 99,
            ..BridgeRequest::new(7, BridgeOperation::Status)
        };
        assert!(matches!(
            request.validate(),
            Err(ProtocolError::VersionMismatch { actual: 99, .. })
        ));

        let response = BridgeResponse::success(8, json!({"ok": true}));
        assert!(matches!(
            response.validate(7),
            Err(ProtocolError::CorrelationMismatch { actual: 8, .. })
        ));
    }

    #[test]
    fn a_null_result_is_a_result() {
        // Blender returns `None` from `collection.remove`, `.link`, and a `.get` that
        // misses. Python encodes that as `"result": null`, which must not read back as an
        // absent field -- doing so made every such call fail as a malformed envelope.
        let response: BridgeResponse =
            serde_json::from_str(r#"{"version":1,"id":1,"result":null}"#).expect("valid frame");
        assert_eq!(response.result, Some(Value::Null));
        assert!(response.validate(1).is_ok());
    }

    #[test]
    fn an_absent_result_is_still_absent() {
        let response: BridgeResponse = serde_json::from_str(
            r#"{"version":1,"id":1,"error":{"code":"boom","message":"boom"}}"#,
        )
        .expect("valid frame");
        assert_eq!(response.result, None);
        assert!(response.validate(1).is_ok());
    }

    #[test]
    fn an_error_response_round_trips_without_growing_a_null_result() {
        // Were `result` serialized as `null` here, the peer would now decode it as a real
        // result sitting alongside the error, and reject the envelope.
        let response = BridgeResponse::failure(1, BridgeError::new("boom", "boom"));
        let encoded = serde_json::to_string(&response).expect("serializable");
        assert!(!encoded.contains("result"), "{encoded}");
        let decoded: BridgeResponse = serde_json::from_str(&encoded).expect("valid frame");
        assert_eq!(decoded, response);
        assert!(decoded.validate(1).is_ok());
    }

    proptest! {
        #[test]
        fn decodes_after_arbitrary_tcp_chunking(
            value in any::<i64>(),
            split_points in proptest::collection::vec(0_usize..32, 0..20),
        ) {
            let expected = json!({"value": value, "text": "frame"});
            let encoded = encode_json(&expected, 1024).expect("test JSON fits");
            let mut decoder = FrameDecoder::new(1024);
            let mut cursor = 0;
            for width in split_points {
                if cursor == encoded.len() {
                    break;
                }
                let end = (cursor + width.max(1)).min(encoded.len());
                decoder.push(&encoded[cursor..end]).expect("chunks fit");
                cursor = end;
            }
            if cursor < encoded.len() {
                decoder.push(&encoded[cursor..]).expect("tail fits");
            }
            let actual: Value = decoder.next_json().expect("valid JSON").expect("complete frame");
            prop_assert_eq!(actual, expected);
            prop_assert_eq!(decoder.buffered_len(), 0);
        }
    }
}
