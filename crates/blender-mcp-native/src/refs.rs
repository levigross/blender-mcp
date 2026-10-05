//! Epoch-scoped handles. RNA owners and traversal paths are resolved afresh so
//! removing an object never leaves a cached Rust handle dereferencing freed RNA.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use pyo3::prelude::*;
use serde_json::{Value, json};

use crate::errors::operation_error;

const MAX_RESOLVE_SCAN: usize = 100_000;

enum Locator {
    Context,
    Data,
    Id {
        collection: String,
        uid: u64,
        name: String,
    },
    Modifier {
        owner: Arc<Locator>,
        uid: i64,
        pointer: u64,
    },
    Attribute {
        parent: Arc<Locator>,
        name: String,
        pointer: Option<u64>,
        uid: Option<u64>,
        durable: bool,
    },
    Member {
        parent: Arc<Locator>,
        pointer: u64,
        durable: bool,
    },
    Path {
        owner: Arc<Locator>,
        path: String,
        pointer: u64,
        durable: bool,
    },
    // Ordinary Python objects own their memory; unlike RNA these are safe to retain.
    Python(Py<PyAny>),
}

struct Entry {
    locator: Arc<Locator>,
    identity: String,
    type_name: String,
    serial: u64,
    /// Logical time of the last mint or resolution, for least-recently-used eviction.
    last_used: AtomicU64,
}

pub(crate) struct ReferenceStore {
    generation: u64,
    next_id: u64,
    capacity: usize,
    nonce: String,
    entries: HashMap<String, Entry>,
    identities: HashMap<String, String>,
    /// Logical clock for `Entry::last_used`.
    clock: AtomicU64,
    /// First serial minted by the request in progress; those entries are never evicted,
    /// so a result cannot lose its own handles before it is returned.
    request_floor: u64,
    /// Why recently retired handles were retired, so `stale_reference` can say so.
    retired: HashMap<String, String>,
    retired_order: VecDeque<String>,
}

/// Retirement reasons kept for explaining stale handles.
const RETIRED_REASONS: usize = 4_096;

impl ReferenceStore {
    pub(crate) fn new(python: Python<'_>, capacity: usize) -> PyResult<Self> {
        let secrets = python.import("secrets")?;
        Ok(Self {
            generation: secrets.call_method1("randbits", (52,))?.extract::<u64>()? + 1,
            next_id: 1,
            capacity,
            nonce: secrets.call_method1("token_hex", (12,))?.extract()?,
            entries: HashMap::new(),
            identities: HashMap::new(),
            clock: AtomicU64::new(0),
            request_floor: 1,
            retired: HashMap::new(),
            retired_order: VecDeque::new(),
        })
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    /// Start a request: entries minted from here on are protected from eviction and
    /// are what `rollback` removes if the request fails.
    pub(crate) fn mark(&mut self) -> u64 {
        self.request_floor = self.next_id;
        self.next_id
    }

    fn touch(&self, entry: &Entry) {
        // Only Blender's main thread dispatches; atomics just satisfy `Sync`.
        let now = self.clock.fetch_add(1, Ordering::Relaxed) + 1;
        entry.last_used.store(now, Ordering::Relaxed);
    }

    /// Remove every entry for which `reason` returns one, remembering why.
    fn retire_where(&mut self, mut reason: impl FnMut(&str, &Entry) -> Option<String>) -> usize {
        let doomed: Vec<(String, String)> = self
            .entries
            .iter()
            .filter_map(|(id, entry)| reason(id, entry).map(|why| (id.clone(), why)))
            .collect();
        for (id, why) in &doomed {
            if let Some(entry) = self.entries.remove(id) {
                self.identities.remove(&entry.identity);
                self.remember(id, format!("{} handle {why}", entry.type_name));
            }
        }
        doomed.len()
    }

    fn remember(&mut self, id: &str, why: String) {
        if self.retired.insert(id.to_owned(), why).is_none() {
            self.retired_order.push_back(id.to_owned());
            while self.retired_order.len() > RETIRED_REASONS {
                if let Some(oldest) = self.retired_order.pop_front() {
                    self.retired.remove(&oldest);
                }
            }
        }
    }

    /// Make room by retiring the least recently used eighth of the store, never
    /// touching entries minted by the request in progress. Eviction is safe: every
    /// handle is re-found from its live owner on use, so an evicted one can only
    /// report `stale_reference`, never reach freed memory.
    fn evict(&mut self) -> bool {
        let mut candidates: Vec<(u64, String)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.serial < self.request_floor)
            .map(|(id, entry)| (entry.last_used.load(Ordering::Relaxed), id.clone()))
            .collect();
        if candidates.is_empty() {
            return false;
        }
        let count = (self.capacity / 8).clamp(1, candidates.len());
        candidates.select_nth_unstable(count - 1);
        let evicted: HashSet<String> = candidates[..count]
            .iter()
            .map(|(_, id)| id.clone())
            .collect();
        let capacity = self.capacity;
        self.retire_where(|id, _| {
            evicted.contains(id).then(|| {
                format!(
                    "was evicted: the store holds {capacity} handles and this was among the least recently used"
                )
            })
        });
        true
    }
    pub(crate) fn rollback(&mut self, mark: u64) {
        self.retire_where(|_, entry| {
            (entry.serial >= mark).then(|| "was minted by a request that failed".to_owned())
        });
    }

    pub(crate) fn invalidate(&mut self) -> u64 {
        self.entries.clear();
        self.identities.clear();
        self.retired.clear();
        self.retired_order.clear();
        self.generation += 1;
        self.generation
    }

    /// Retire volatile (array-backed) sub-data handles; `cause` says what may have
    /// reallocated them.
    pub(crate) fn invalidate_subdata(&mut self, cause: &str) -> usize {
        self.retire_where(|_, entry| {
            entry.locator.has_physical_subdata().then(|| {
                format!("was retired after {cause}: array-backed data may have been reallocated")
            })
        })
    }

    /// Retire handles reached through a collection that `function` is about to change.
    /// Adding or reordering members moves only array-backed members, so durable
    /// members (individually allocated) survive those; clearing or popping retires all.
    pub(crate) fn invalidate_collection(
        &mut self,
        python: Python<'_>,
        reference: &Value,
        function: &str,
    ) -> PyResult<()> {
        let target = self.entry(python, reference)?.locator.clone();
        let volatile_only = matches!(function, "add" | "insert" | "move");
        self.retire_where(|_, entry| {
            let shares_owner_path = matches!(
                &*entry.locator,
                Locator::Path { .. } | Locator::Modifier { .. }
            ) && entry.locator.owner_uid().is_some()
                && entry.locator.owner_uid() == target.owner_uid();
            let affected = !Arc::ptr_eq(&entry.locator, &target)
                && (entry.locator.descends_from(&target) || shares_owner_path)
                && (!volatile_only || entry.locator.has_physical_subdata());
            affected.then(|| format!("was retired by `{function}` on its collection"))
        });
        Ok(())
    }

    /// Pointers of the members a `remove` call is about to free, when every one of them
    /// is durable node-tree data; `None` means fall back to `invalidate_collection`.
    /// Siblings of node-tree data are allocated separately, so removing one moves no
    /// other; only the removed member's handles need retiring (see `retire_pointers`).
    pub(crate) fn durable_member_pointers(
        &self,
        python: Python<'_>,
        members: &[&Value],
    ) -> PyResult<Option<Vec<u64>>> {
        let mut pointers = Vec::with_capacity(members.len());
        for reference in members {
            let entry = self.entry(python, reference)?;
            if !entry.locator.is_durable() {
                return Ok(None);
            }
            match pointer(&self.resolve(python, reference)?) {
                Some(address) => pointers.push(address),
                None => return Ok(None),
            }
        }
        Ok((!pointers.is_empty()).then_some(pointers))
    }

    /// Retire every handle to the freed members -- whichever path minted it -- and
    /// everything resolved through one, so a later allocation at the same address can
    /// never be reached through an old handle.
    pub(crate) fn retire_pointers(&mut self, pointers: &[u64]) {
        let freed: Vec<Arc<Locator>> = self
            .entries
            .values()
            .filter(|entry| {
                entry
                    .locator
                    .pointer()
                    .is_some_and(|address| pointers.contains(&address))
            })
            .map(|entry| entry.locator.clone())
            .collect();
        self.retire_where(|_, entry| {
            freed
                .iter()
                .any(|target| {
                    Arc::ptr_eq(&entry.locator, target) || entry.locator.descends_from(target)
                })
                .then(|| "was retired: its target was removed".to_owned())
        });
    }

    pub(crate) fn prune_removed_modifiers(&mut self, python: Python<'_>) {
        // Blender can reuse both persistent_uid and memory on new(). Retire
        // missing modifiers before allocation can make an old locator resolve
        // to a replacement, including when deletion happened outside MCP.
        self.retire_where(|_, entry| {
            (matches!(&*entry.locator, Locator::Modifier { .. })
                && resolve_locator(python, &entry.locator, 0).is_err())
            .then(|| "was retired: its modifier was removed".to_owned())
        });
    }

    pub(crate) fn stats(&self) -> Value {
        json!({"generation": self.generation, "count": self.entries.len(), "capacity": self.capacity})
    }

    pub(crate) fn release(&mut self, python: Python<'_>, references: &[Value]) -> PyResult<Value> {
        for reference in references {
            self.check_generation(python, reference)?;
        }
        let mut released = 0;
        for reference in references {
            if let Some(id) = reference.get("id").and_then(Value::as_str)
                && let Some(entry) = self.entries.remove(id)
            {
                self.identities.remove(&entry.identity);
                self.remember(id, format!("{} handle was released", entry.type_name));
                released += 1;
            }
        }
        Ok(
            json!({"released": released, "count": self.entries.len(), "generation": self.generation}),
        )
    }

    pub(crate) fn insert(&mut self, value: &Bound<'_, PyAny>) -> PyResult<Value> {
        let locator = infer_locator(value)?;
        self.store(value, locator, None)
    }

    pub(crate) fn insert_attribute(
        &mut self,
        reference: &Value,
        name: &str,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<Value> {
        if let Some(locator) = infer_owned_locator(value)? {
            return self.store(value, locator, None);
        }
        let parent = self.entry(value.py(), reference)?.locator.clone();
        let uid = value
            .getattr("session_uid")
            .and_then(|v| v.extract::<u64>())
            .ok();
        let identity = format!(
            "attr:{}:{name}:{:?}:{uid:?}",
            reference["id"],
            pointer(value)
        );
        let locator = Arc::new(Locator::Attribute {
            parent,
            name: name.to_owned(),
            pointer: pointer(value),
            uid,
            durable: !is_volatile_data(value),
        });
        self.store(value, locator, Some(identity))
    }

    pub(crate) fn insert_member(
        &mut self,
        reference: &Value,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<Value> {
        if let Some(locator) = infer_owned_locator(value)? {
            return self.store(value, locator, None);
        }
        let Some(pointer) = pointer(value) else {
            return self.insert(value);
        };
        let parent = self.entry(value.py(), reference)?.locator.clone();
        let identity = format!("member:{}:{pointer}", reference["id"]);
        self.store(
            value,
            Arc::new(Locator::Member {
                parent,
                pointer,
                durable: !is_volatile_data(value),
            }),
            Some(identity),
        )
    }

    fn store(
        &mut self,
        value: &Bound<'_, PyAny>,
        locator: Arc<Locator>,
        identity: Option<String>,
    ) -> PyResult<Value> {
        let type_name = type_name(value);
        let identity = identity.unwrap_or_else(|| {
            if let Ok(uid) = value
                .getattr("session_uid")
                .and_then(|v| v.extract::<u64>())
            {
                format!("id:{type_name}:{uid}")
            } else if let Locator::Modifier {
                owner,
                uid,
                pointer,
            } = &*locator
            {
                format!(
                    "modifier:{:?}:{uid}:{pointer}:{type_name}",
                    owner.owner_uid()
                )
            } else if let Locator::Path {
                owner,
                path,
                pointer,
                ..
            } = &*locator
            {
                format!("path:{:?}:{path}:{type_name}:{pointer}", owner.owner_uid())
            } else {
                format!("python:{type_name}:{:p}", value.as_ptr())
            }
        });
        if let Some(id) = self.identities.get(&identity).cloned() {
            if let Some(entry) = self.entries.get(&id)
                && resolve_locator(value.py(), &entry.locator, 0).is_ok()
            {
                self.touch(entry);
                return Ok(self.envelope(&id, &type_name));
            }
            if let Some(entry) = self.entries.remove(&id) {
                self.remember(
                    &id,
                    format!(
                        "{} handle was retired: its target no longer resolves",
                        entry.type_name
                    ),
                );
            }
            self.identities.remove(&identity);
        }
        if self.entries.len() >= self.capacity && !self.evict() {
            return Err(operation_error(
                value.py(),
                "reference_limit",
                "this single result needs more handles than the reference capacity; request a smaller page",
            ));
        }
        let id = format!("{}-{}", self.nonce, self.next_id);
        let serial = self.next_id;
        self.next_id += 1;
        self.identities.insert(identity.clone(), id.clone());
        self.entries.insert(
            id.clone(),
            Entry {
                locator,
                identity,
                type_name: type_name.clone(),
                serial,
                last_used: AtomicU64::new(0),
            },
        );
        if let Some(entry) = self.entries.get(&id) {
            self.touch(entry);
        }
        Ok(self.envelope(&id, &type_name))
    }

    fn envelope(&self, id: &str, type_name: &str) -> Value {
        json!({"$rna_ref": {"generation": self.generation, "id": id, "type_name": type_name}})
    }

    fn check_generation(&self, python: Python<'_>, reference: &Value) -> PyResult<()> {
        if reference.get("generation").and_then(Value::as_u64) != Some(self.generation) {
            return Err(operation_error(
                python,
                "stale_reference",
                "RNA handle belongs to another document epoch or bridge instance",
            ));
        }
        Ok(())
    }

    fn entry(&self, python: Python<'_>, reference: &Value) -> PyResult<&Entry> {
        self.check_generation(python, reference)?;
        self.entries
            .get(reference.get("id").and_then(Value::as_str).unwrap_or(""))
            .inspect(|entry| self.touch(entry))
            .ok_or_else(|| {
                let id = reference.get("id").and_then(Value::as_str).unwrap_or("");
                let message = self.retired.get(id).map_or_else(
                    || "unknown or released RNA handle; fetch it again".to_owned(),
                    |why| format!("{why}; fetch it again"),
                );
                operation_error(python, "stale_reference", message)
            })
    }

    pub(crate) fn resolve<'py>(
        &self,
        python: Python<'py>,
        reference: &Value,
    ) -> PyResult<Bound<'py, PyAny>> {
        let entry = self.entry(python, reference)?;
        let value = resolve_locator(python, &entry.locator, 0).map_err(|error| {
            operation_error(
                python,
                "stale_reference",
                format!("RNA target no longer resolves: {error}"),
            )
        })?;
        if type_name(&value) != entry.type_name {
            return Err(operation_error(
                python,
                "stale_reference",
                "RNA target type changed",
            ));
        }
        Ok(value)
    }
}

pub(crate) fn pointer(value: &Bound<'_, PyAny>) -> Option<u64> {
    value
        .call_method0("as_pointer")
        .and_then(|v| v.extract::<u64>())
        .ok()
        .filter(|v| *v != 0)
}

fn infer_owned_locator(value: &Bound<'_, PyAny>) -> PyResult<Option<Arc<Locator>>> {
    let python = value.py();
    let bpy = python.import("bpy")?;
    if value.is(&bpy.getattr("context")?) {
        return Ok(Some(Arc::new(Locator::Context)));
    }
    let data = bpy.getattr("data")?;
    if value.is(&data) {
        return Ok(Some(Arc::new(Locator::Data)));
    }
    // Modifiers have a stable identity within their owner's stack. Geometry
    // evaluation may run between bridge requests, so a pointer-scoped member
    // would expire immediately after modifiers.new() in a live session.
    if value.is_instance(&bpy.getattr("types")?.getattr("Modifier")?)?
        && let Some(owner) = infer_owned_locator(&value.getattr("id_data")?)?
        && let Some(pointer) = pointer(value)
    {
        return Ok(Some(Arc::new(Locator::Modifier {
            owner,
            uid: value.getattr("persistent_uid")?.extract()?,
            pointer,
        })));
    }
    if !value.is_instance(&bpy.getattr("types")?.getattr("ID")?)? {
        return Ok(None);
    }
    let uid = value.getattr("session_uid")?.extract::<u64>()?;
    let name = value.getattr("name")?.extract::<String>()?;
    for property in data.getattr("bl_rna")?.getattr("properties")?.try_iter()? {
        let property = property?;
        let Ok(fixed_type) = property.getattr("fixed_type") else {
            continue;
        };
        let Ok(identifier) = fixed_type
            .getattr("identifier")
            .and_then(|v| v.extract::<String>())
        else {
            continue;
        };
        let Ok(class) = bpy.getattr("types")?.getattr(identifier.as_str()) else {
            continue;
        };
        if !value.is_instance(&class)? {
            continue;
        }
        let collection = property.getattr("identifier")?.extract::<String>()?;
        // Linked libraries can contain identically named IDs. Use the same
        // bounded UID lookup here as resolution, rather than retaining a member
        // pointer when the first name match belongs to a different library.
        if resolve_id(python, &collection, uid, &name).is_ok() {
            return Ok(Some(Arc::new(Locator::Id {
                collection,
                uid,
                name,
            })));
        }
    }
    Ok(None)
}

fn infer_locator(value: &Bound<'_, PyAny>) -> PyResult<Arc<Locator>> {
    if let Some(locator) = infer_owned_locator(value)? {
        return Ok(locator);
    }
    if value.hasattr("bl_rna")? {
        if let (Ok(owner), Ok(path), Some(pointer)) = (
            value.getattr("id_data"),
            value
                .call_method0("path_from_id")
                .and_then(|v| v.extract::<String>()),
            pointer(value),
        ) && !path.is_empty()
            && let Some(owner) = infer_owned_locator(&owner)?
        {
            return Ok(Arc::new(Locator::Path {
                owner,
                path,
                pointer,
                durable: !is_volatile_data(value),
            }));
        }
        return Err(operation_error(
            value.py(),
            "reference_unavailable",
            "RNA target has no durable owner path; reacquire it through a property or collection",
        ));
    }
    Ok(Arc::new(Locator::Python(value.clone().unbind())))
}

fn resolve_id<'py>(
    python: Python<'py>,
    collection: &str,
    uid: u64,
    name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let items = python.import("bpy")?.getattr("data")?.getattr(collection)?;
    if let Ok(candidate) = items.call_method1("get", (name,))
        && candidate
            .getattr("session_uid")
            .and_then(|v| v.extract::<u64>())
            .ok()
            == Some(uid)
    {
        return Ok(candidate);
    }
    for item in items.try_iter()?.take(MAX_RESOLVE_SCAN) {
        let item = item?;
        if item
            .getattr("session_uid")
            .and_then(|v| v.extract::<u64>())
            .ok()
            == Some(uid)
        {
            return Ok(item);
        }
    }
    Err(operation_error(
        python,
        "stale_reference",
        "ID was removed or is outside the bounded owner search",
    ))
}

/// Scan the live collection for the exact member; never dereference a cached pointer.
fn resolve_member<'py>(
    python: Python<'py>,
    parent: &Locator,
    expected: u64,
    depth: usize,
) -> PyResult<Bound<'py, PyAny>> {
    for item in resolve_locator(python, parent, depth + 1)?
        .try_iter()?
        .take(MAX_RESOLVE_SCAN)
    {
        let item = item?;
        if pointer(&item) == Some(expected) {
            return Ok(item);
        }
    }
    Err(operation_error(
        python,
        "stale_reference",
        "RNA collection member was removed",
    ))
}

fn resolve_locator<'py>(
    python: Python<'py>,
    locator: &Locator,
    depth: usize,
) -> PyResult<Bound<'py, PyAny>> {
    if depth > 64 {
        return Err(operation_error(
            python,
            "stale_reference",
            "RNA traversal exceeds 64 steps",
        ));
    }
    match locator {
        Locator::Context => python.import("bpy")?.getattr("context"),
        Locator::Data => python.import("bpy")?.getattr("data"),
        Locator::Python(value) => Ok(value.bind(python).clone()),
        Locator::Id {
            collection,
            uid,
            name,
        } => resolve_id(python, collection, *uid, name),
        Locator::Modifier {
            owner,
            uid,
            pointer: expected,
        } => {
            for item in resolve_locator(python, owner, depth + 1)?
                .getattr("modifiers")?
                .try_iter()?
                .take(MAX_RESOLVE_SCAN)
            {
                let item = item?;
                if item.getattr("persistent_uid")?.extract::<i64>()? == *uid
                    && pointer(&item) == Some(*expected)
                {
                    return Ok(item);
                }
            }
            Err(operation_error(
                python,
                "stale_reference",
                "modifier was removed or is outside the bounded stack search",
            ))
        }
        Locator::Attribute {
            parent,
            name,
            pointer: expected,
            uid,
            ..
        } => {
            let value = resolve_locator(python, parent, depth + 1)?.getattr(name.as_str())?;
            let matches = if let Some(uid) = uid {
                value
                    .getattr("session_uid")
                    .and_then(|v| v.extract::<u64>())
                    .ok()
                    == Some(*uid)
            } else {
                expected.is_none() || pointer(&value) == *expected
            };
            if !matches {
                return Err(operation_error(
                    python,
                    "stale_reference",
                    "RNA property target was replaced",
                ));
            }
            Ok(value)
        }
        Locator::Member {
            parent,
            pointer: expected,
            ..
        } => resolve_member(python, parent, *expected, depth),
        Locator::Path {
            owner,
            path,
            pointer: expected,
            ..
        } => {
            let value =
                resolve_locator(python, owner, depth + 1)?.call_method1("path_resolve", (path,))?;
            if pointer(&value) != Some(*expected) {
                return Err(operation_error(
                    python,
                    "stale_reference",
                    "RNA path target was replaced",
                ));
            }
            Ok(value)
        }
    }
}

fn type_name(value: &Bound<'_, PyAny>) -> String {
    value
        .getattr("bl_rna")
        .and_then(|rna| rna.getattr("identifier"))
        .and_then(|v| v.extract::<String>())
        .or_else(|_| value.get_type().name().and_then(|v| v.extract::<String>()))
        .unwrap_or_else(|_| "object".to_owned())
}

/// Array-backed or rebuilt data: mesh elements, attribute layers and their values, UV
/// and colour layers, shape-key and spline points, keyframes, and bones (rebuilt on
/// leaving edit mode). Blender reallocates these in place, so a *different* element can
/// appear at the same address; their handles are retired on sub-data invalidation.
/// Everything else -- IDs' embedded settings (render, view, depth of field), nodes,
/// sockets, links, modifiers' structs -- is allocated once and never moved, so its
/// handles stay valid. Either way resolution re-finds the target from its live owner
/// and checks pointer and type, so no handle can ever reach freed memory.
fn is_volatile_data(value: &Bound<'_, PyAny>) -> bool {
    const VOLATILE: [&str; 21] = [
        "MeshVertex",
        "MeshEdge",
        "MeshPolygon",
        "MeshLoop",
        "MeshLoopTriangle",
        "Attribute",
        "MeshUVLoopLayer",
        "MeshUVLoop",
        "MeshLoopColorLayer",
        "MeshLoopColor",
        "VertexGroupElement",
        "ShapeKeyPoint",
        "ShapeKeyBezierPoint",
        "ShapeKeyCurvePoint",
        "SplinePoint",
        "BezierSplinePoint",
        "CurvePoint",
        "Keyframe",
        "EditBone",
        "Bone",
        "PoseBone",
    ];
    let name = type_name(value);
    // Per-element attribute values (FloatAttributeValue, ...) share no base class.
    if name.ends_with("AttributeValue") || name.ends_with("NormalValue") {
        return true;
    }
    let Ok(types) = value
        .py()
        .import("bpy")
        .and_then(|bpy| bpy.getattr("types"))
    else {
        return false;
    };
    VOLATILE.iter().any(|name| {
        types
            .getattr(*name)
            .and_then(|class| value.is_instance(&class))
            .unwrap_or(false)
    })
}

impl Locator {
    /// Whether invalidating sub-data must retire this handle: it, or anything it is
    /// resolved through, may be reallocated in place by Blender.
    fn has_physical_subdata(&self) -> bool {
        match self {
            Self::Member {
                parent, durable, ..
            } => !durable || parent.has_physical_subdata(),
            Self::Path { owner, durable, .. } => !durable || owner.has_physical_subdata(),
            Self::Attribute {
                parent,
                pointer,
                uid,
                durable,
                ..
            } => (!durable && pointer.is_some() && uid.is_none()) || parent.has_physical_subdata(),
            _ => false,
        }
    }
    /// Durable node-tree data, resolved only through durable or ID steps.
    fn is_durable(&self) -> bool {
        matches!(
            self,
            Self::Member { durable: true, .. }
                | Self::Path { durable: true, .. }
                | Self::Attribute { durable: true, .. }
        ) && !self.has_physical_subdata()
    }

    /// The address this handle was minted for, when it is pointer-checked.
    fn pointer(&self) -> Option<u64> {
        match self {
            Self::Member { pointer, .. } | Self::Path { pointer, .. } => Some(*pointer),
            Self::Attribute { pointer, .. } => *pointer,
            _ => None,
        }
    }

    fn owner_uid(&self) -> Option<u64> {
        match self {
            Self::Id { uid, .. } => Some(*uid),
            Self::Attribute { parent, uid, .. } => uid.or_else(|| parent.owner_uid()),
            Self::Member { parent, .. } => parent.owner_uid(),
            Self::Path { owner, .. } | Self::Modifier { owner, .. } => owner.owner_uid(),
            _ => None,
        }
    }
    fn descends_from(&self, target: &Arc<Self>) -> bool {
        match self {
            Self::Attribute { parent, .. } | Self::Member { parent, .. } => {
                Arc::ptr_eq(parent, target) || parent.descends_from(target)
            }
            Self::Path { owner, .. } | Self::Modifier { owner, .. } => {
                Arc::ptr_eq(owner, target) || owner.descends_from(target)
            }
            _ => false,
        }
    }
}
