//! Main-thread Blender operation dispatcher.
//!
//! This is the second half of the sandbox. The Steel engine in the server is the first;
//! here private names are rejected and functions are checked against Blender's public
//! RNA metadata or a small, type-checked list of inherited Blender helpers.

use pyo3::{
    prelude::*,
    types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};

use crate::{
    artifacts::ArtifactStore,
    catalog::{build_catalog, describe_operator, property_descriptor},
    errors::{operation_error, operation_error_with_data},
    marshal::{json_to_py, py_to_json},
    refs::ReferenceStore,
    scene,
};

const MAX_SERIALIZED_DEPTH: usize = 8;
const MAX_SERIALIZED_ITEMS: usize = 4_096;
const MAX_SERIALIZED_BYTES: usize = 8 * 1024 * 1024;

/// Collection helpers Blender does not publish in `bl_rna.functions` but that are
/// required to work with data at all.
const COLLECTION_CALLS: [&str; 10] = [
    "clear",
    "find",
    "foreach_get",
    "foreach_set",
    "get",
    "link",
    "move",
    "new",
    "remove",
    "unlink",
];

/// These inherited `bpy_struct` methods are absent from `bl_rna.functions`.
const STRUCT_CALLS: [&str; 2] = ["keyframe_insert", "keyframe_delete"];

#[pyclass]
pub(crate) struct BlenderOperations {
    references: ReferenceStore,
    catalog: Option<Value>,
    artifacts: ArtifactStore,
    closed: bool,
}

#[pymethods]
impl BlenderOperations {
    #[new]
    #[pyo3(signature = (*, reference_capacity=4096, artifact_ttl_secs=3600))]
    fn new(
        python: Python<'_>,
        reference_capacity: usize,
        artifact_ttl_secs: u64,
    ) -> PyResult<Self> {
        if reference_capacity == 0 || reference_capacity > 65_536 {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "reference_capacity must be 1..65536",
            ));
        }
        if artifact_ttl_secs == 0 || artifact_ttl_secs > 86400 {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "artifact_ttl_secs must be 1..86400",
            ));
        }
        let root: String = python
            .import("tempfile")?
            .call_method1("mkdtemp", ("blender-mcp-artifacts-",))?
            .extract()?;
        Ok(Self {
            references: ReferenceStore::new(python, reference_capacity)?,
            catalog: None,
            artifacts: ArtifactStore::new(
                PathBuf::from(root),
                Duration::from_secs(artifact_ttl_secs),
            ),
            closed: false,
        })
    }

    /// The operator catalog, built on first use.
    #[getter]
    fn catalog<'py>(&mut self, python: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let catalog = self.ensure_catalog(python)?.clone();
        json_to_py(python, &catalog)
    }

    #[getter]
    fn catalog_revision(&mut self, python: Python<'_>) -> PyResult<String> {
        Ok(self.ensure_catalog(python)?["revision"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }

    #[getter]
    fn generation(&self) -> u64 {
        self.references.generation()
    }

    fn invalidate_references(&mut self) -> u64 {
        self.references.invalidate()
    }

    fn invalidate_subdata(&mut self) -> usize {
        self.references.invalidate_subdata()
    }

    fn close(&mut self) -> PyResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.references.invalidate();
        self.artifacts.close()
    }

    #[staticmethod]
    fn validate_batch(python: Python<'_>, requests: &Bound<'_, PyAny>) -> PyResult<()> {
        validate_batch_requests(python, &py_to_json(requests)?)?;
        Ok(())
    }

    fn refresh_catalog<'py>(&mut self, python: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let catalog = build_catalog(python)?;
        self.catalog = Some(catalog.clone());
        json_to_py(python, &catalog)
    }

    /// Run one bridge request, returning `(result, reports)`.
    fn execute<'py>(
        &mut self,
        python: Python<'py>,
        request: &Bound<'py, PyAny>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyList>)> {
        let request = py_to_json(request)?;
        let result = self.dispatch(python, &request)?;
        Ok((json_to_py(python, &result)?, PyList::empty(python)))
    }
}

impl BlenderOperations {
    fn ensure_catalog(&mut self, python: Python<'_>) -> PyResult<&Value> {
        if self.catalog.is_none() {
            self.catalog = Some(build_catalog(python)?);
        }
        Ok(self.catalog.as_ref().expect("just populated"))
    }

    fn dispatch(&mut self, python: Python<'_>, request: &Value) -> PyResult<Value> {
        let mark = self.references.mark();
        let result = self.dispatch_inner(python, request);
        if result.is_err() {
            self.references.rollback(mark);
        }
        result
    }

    fn dispatch_inner(&mut self, python: Python<'_>, request: &Value) -> PyResult<Value> {
        if self.closed {
            return Err(operation_error(
                python,
                "bridge_stopped",
                "native dispatcher is closed",
            ));
        }
        let operation = request
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("");
        match operation {
            "catalog" => Ok(self.ensure_catalog(python)?.clone()),
            "catalog_refresh" => {
                let catalog = build_catalog(python)?;
                self.catalog = Some(catalog.clone());
                Ok(catalog)
            }
            "operator_info" => describe_operator(python, string_field(request, "idname")),
            "operator_poll" => {
                let operator = Self::operator(python, string_field(request, "idname"))?;
                let available = self.poll(python, &operator, request.get("context_override"))?;
                Ok(json!({ "available": available }))
            }
            "operator_call" => self.operator_call(python, request),
            "context_ref" => {
                let context = python.import("bpy")?.getattr("context")?;
                self.references.insert(&context)
            }
            "data_ref" => {
                let data = python.import("bpy")?.getattr("data")?;
                self.references.insert(&data)
            }
            "rna_get" | "rna_set" | "rna_call" | "rna_describe" | "rna_items"
            | "rna_property_info" | "rna_function_info" => self.rna(python, operation, request),
            "status" => {
                let version = python
                    .import("bpy")?
                    .getattr("app")?
                    .getattr("version_string")?
                    .extract::<String>()?;
                let background = python
                    .import("bpy")?
                    .getattr("app")?
                    .getattr("background")?
                    .extract::<bool>()?;
                let catalog = self.ensure_catalog(python)?;
                Ok(json!({
                    "blender_version": version,
                    "operator_count": catalog["operators"].as_array().map_or(0, Vec::len),
                    "catalog_revision": catalog["revision"],
                    "background": background,
                }))
            }
            "reference_stats" => Ok(self.references.stats()),
            "reference_release" => self
                .references
                .release(python, array_field(python, request, "references")?),
            "scene_summary" => scene::summary(python),
            "scene_snapshot" => scene::snapshot(
                python,
                self.generation(),
                request
                    .get("limit")
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(1000),
            ),
            "scene_diff" => {
                let before = request.get("before").ok_or_else(|| {
                    operation_error(python, "invalid_arguments", "before snapshot is required")
                })?;
                let after = match request.get("after").filter(|v| !v.is_null()) {
                    Some(after) => after.clone(),
                    None => scene::snapshot(python, self.generation(), 1000)?,
                };
                scene::diff(python, before, &after)
            }
            "batch" => self.batch(python, request),
            "checkpoint" => self.checkpoint(python, request),
            "render" => self.render(python, request),
            "thumbnail" => self.thumbnail(python, request),
            "artifact" => self.artifacts.fetch(
                python,
                string_field(request, "artifact_id"),
                request
                    .get("include_data")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            ),
            "artifact_release" => self
                .artifacts
                .release(python, array_field(python, request, "artifact_ids")?),
            "shutdown" => Ok(json!({ "shutting_down": true })),
            other => Err(operation_error(
                python,
                "unknown_operation",
                format!("unknown bridge operation: {other}"),
            )),
        }
    }

    // ---- names and values -------------------------------------------------

    /// Reject private and malformed RNA names before they reach `getattr`.
    fn public_name<'a>(python: Python<'_>, name: &'a str, kind: &str) -> PyResult<&'a str> {
        if name.is_empty() || name.starts_with('_') {
            return Err(operation_error(
                python,
                "access_denied",
                format!("private or invalid RNA {kind}: '{name}'"),
            ));
        }
        Ok(name)
    }

    /// JSON to Python, turning `$rna_ref` envelopes back into live objects.
    fn deserialize<'py>(&self, python: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
        match value {
            Value::Array(items) => {
                let list = PyList::empty(python);
                for item in items {
                    list.append(self.deserialize(python, item)?)?;
                }
                Ok(list.into_any())
            }
            Value::Object(entries) => {
                // A lone `$rna_ref` key is a handle; anything else is a plain mapping.
                if let (1, Some(reference)) = (entries.len(), entries.get("$rna_ref")) {
                    return self.references.resolve(python, reference);
                }
                let mapping = PyDict::new(python);
                for (key, item) in entries {
                    mapping.set_item(key, self.deserialize(python, item)?)?;
                }
                Ok(mapping.into_any())
            }
            scalar => json_to_py(python, scalar),
        }
    }

    /// Charge every scalar and container before descending; never inspect an
    /// arbitrarily large/cyclic container just to discover whether it is scalar.
    fn serialize(
        &mut self,
        value: &Bound<'_, PyAny>,
        depth: usize,
        budget: &mut SerializationBudget,
    ) -> PyResult<Value> {
        if depth > MAX_SERIALIZED_DEPTH {
            return Err(serialization_limit(value.py()));
        }
        budget.consume(value.py(), 1, 2)?;
        if value.is_none()
            || value.is_instance_of::<PyBool>()
            || value.is_instance_of::<PyInt>()
            || value.is_instance_of::<PyFloat>()
            || value.is_instance_of::<PyString>()
        {
            if let Ok(text) = value.cast::<PyString>()
                && text.to_str()?.len() > budget.bytes
            {
                return Err(serialization_limit(value.py()));
            }
            let scalar = py_to_json(value)?;
            budget.consume(
                value.py(),
                0,
                serde_json::to_vec(&scalar).map_or(MAX_SERIALIZED_BYTES + 1, |v| v.len()),
            )?;
            return Ok(scalar);
        }
        if is_rna_collection(value) || value.hasattr("bl_rna")? {
            return self.references.insert(value);
        }
        if let Ok(mapping) = value.cast::<PyDict>() {
            if mapping.len() > budget.items {
                return Err(serialization_limit(value.py()));
            }
            let mut result = Map::new();
            for (key, item) in mapping.iter() {
                let key = key
                    .cast::<PyString>()
                    .map_err(|_| {
                        operation_error(
                            value.py(),
                            "serialization_error",
                            "mapping keys must be strings",
                        )
                    })?
                    .to_str()?;
                budget.consume(value.py(), 0, key.len().saturating_mul(6).saturating_add(3))?;
                result.insert(key.to_owned(), self.serialize(&item, depth + 1, budget)?);
            }
            return Ok(Value::Object(result));
        }
        if let Ok(iterator) = value.try_iter() {
            if value.len().is_ok_and(|len| len > budget.items) {
                return Err(serialization_limit(value.py()));
            }
            let mut result = Vec::new();
            for item in iterator {
                if budget.items == 0 {
                    return Err(serialization_limit(value.py()));
                }
                result.push(self.serialize(&item?, depth + 1, budget)?);
            }
            return Ok(Value::Array(result));
        }
        self.references.insert(value)
    }

    fn bounded(&mut self, python: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Value> {
        let result = self.serialize(value, 0, &mut SerializationBudget::new())?;
        if serde_json::to_vec(&result).map_or(usize::MAX, |v| v.len()) > MAX_SERIALIZED_BYTES {
            return Err(serialization_limit(python));
        }
        Ok(result)
    }

    // ---- operators --------------------------------------------------------

    fn operator<'py>(python: Python<'py>, idname: &str) -> PyResult<Bound<'py, PyAny>> {
        let unknown = || {
            operation_error(
                python,
                "unknown_operator",
                format!("unknown Blender operator: {idname}"),
            )
        };
        let (module, function) = idname.split_once('.').ok_or_else(unknown)?;
        python
            .import("bpy")?
            .getattr("ops")
            .and_then(|ops| ops.getattr(module))
            .and_then(|module| module.getattr(function))
            .map_err(|_| unknown())
    }

    fn context_override<'py>(
        &self,
        python: Python<'py>,
        raw: Option<&Value>,
    ) -> PyResult<Option<Bound<'py, PyDict>>> {
        let Some(raw) = raw.filter(|value| !value.is_null()) else {
            return Ok(None);
        };
        let Value::Object(entries) = raw else {
            return Err(operation_error(
                python,
                "invalid_context",
                "context_override must be an object",
            ));
        };
        if entries.is_empty() {
            return Ok(None);
        }
        let mapping = PyDict::new(python);
        for (key, item) in entries {
            let key = Self::public_name(python, key, "context key")?;
            mapping.set_item(key, self.deserialize(python, item)?)?;
        }
        Ok(Some(mapping))
    }

    fn poll(
        &self,
        python: Python<'_>,
        operator: &Bound<'_, PyAny>,
        context_override: Option<&Value>,
    ) -> PyResult<bool> {
        let override_map = self.context_override(python, context_override)?;
        with_override(python, override_map.as_ref(), || {
            operator.call_method0("poll")?.extract::<bool>()
        })
    }

    fn operator_call(&mut self, python: Python<'_>, request: &Value) -> PyResult<Value> {
        let idname = string_field(request, "idname");
        let operator = Self::operator(python, idname)?;
        let override_map = self.context_override(python, request.get("context_override"))?;
        if !self.poll(python, &operator, request.get("context_override"))? {
            return Err(operation_error_with_data(
                python,
                "operator_unavailable",
                format!("{idname} failed poll() in the current Blender context"),
                json_to_py(python, &json!({ "idname": idname }))?.unbind(),
            ));
        }

        let kwargs = match request.get("kwargs") {
            Some(value) if !value.is_null() => self.deserialize(python, value)?,
            _ => PyDict::new(python).into_any(),
        };
        let kwargs = kwargs.cast::<PyDict>().map_err(|_| {
            operation_error(python, "invalid_arguments", "kwargs must be an object")
        })?;

        let execution_context = request.get("execution_context").and_then(Value::as_str);
        let undo = request.get("undo").and_then(Value::as_bool);
        let mut positional: Vec<Bound<'_, PyAny>> = Vec::new();
        if execution_context.is_some() || undo.is_some() {
            let context = execution_context.unwrap_or("EXEC_DEFAULT");
            positional.push(context.into_pyobject(python)?.into_any());
        }
        if let Some(undo) = undo {
            positional.push(undo.into_pyobject(python)?.to_owned().into_any());
        }
        let positional = PyTuple::new(python, positional)?;

        self.references.invalidate_subdata();
        let result = with_override(python, override_map.as_ref(), || {
            operator.call(&positional, Some(kwargs))
        })?;

        // Blender returns a set such as `{'FINISHED'}`; sort it for a stable wire value.
        let mut status: Vec<String> = result
            .try_iter()?
            .flatten()
            .filter_map(|flag| flag.extract::<String>().ok())
            .collect();
        status.sort_unstable();
        Ok(json!({ "operator": idname, "status": status }))
    }

    // ---- RNA --------------------------------------------------------------

    fn rna(&mut self, python: Python<'_>, operation: &str, request: &Value) -> PyResult<Value> {
        let reference = request.get("reference").cloned().unwrap_or(Value::Null);
        let target = self.references.resolve(python, &reference)?;
        match operation {
            "rna_get" => {
                let attribute = string_field(request, "attribute");
                let attribute = Self::public_name(python, attribute, "attribute")?;
                let value = target.getattr(attribute)?;
                if is_rna_collection(&value) || value.hasattr("bl_rna")? {
                    self.references
                        .insert_attribute(&reference, attribute, &value)
                } else {
                    self.bounded(python, &value)
                }
            }
            "rna_set" => {
                let attribute = string_field(request, "attribute");
                let attribute = Self::public_name(python, attribute, "attribute")?;
                let raw = request.get("value").unwrap_or(&Value::Null);
                let value = self.deserialize(python, raw)?;
                let value = if is_row_matrix(raw) && is_matrix_property(&target, attribute) {
                    // Reads return rows, but Blender assigns a nested list to a matrix
                    // column by column. Build a Matrix so writes take rows too.
                    python
                        .import("mathutils")?
                        .getattr("Matrix")?
                        .call1((value,))?
                } else {
                    value
                };
                target.setattr(attribute, value)?;
                Ok(json!({ "updated": attribute }))
            }
            "rna_call" => self.rna_call(python, &reference, &target, request),
            "rna_describe" => Ok(describe_rna(&target)),
            "rna_property_info" => {
                let attribute =
                    Self::public_name(python, string_field(request, "attribute"), "property")?;
                let property = target
                    .getattr("bl_rna")?
                    .getattr("properties")?
                    .call_method1("get", (attribute,))?;
                if property.is_none() {
                    return Err(operation_error(
                        python,
                        "unknown_property",
                        format!("no RNA property named {attribute}"),
                    ));
                }
                Ok(property_descriptor(&property))
            }
            "rna_function_info" => {
                let function =
                    Self::public_name(python, string_field(request, "function"), "function")?;
                if !rna_function_allowed(&target, function) {
                    return Err(operation_error(
                        python,
                        "access_denied",
                        "function is not callable through this bridge",
                    ));
                }
                let metadata = target
                    .getattr("bl_rna")
                    .and_then(|rna| rna.getattr("functions"))
                    .and_then(|items| items.call_method1("get", (function,)))
                    .ok()
                    .filter(|v| !v.is_none());
                if let Some(metadata) = metadata {
                    let parameters = metadata
                        .getattr("parameters")?
                        .try_iter()?
                        .take(256)
                        .map(|v| v.map(|p| property_descriptor(&p)))
                        .collect::<PyResult<Vec<_>>>()?;
                    Ok(
                        json!({"identifier": function, "callable": true, "inherited": false, "parameters": parameters, "description": metadata.getattr("description")?.extract::<String>()?}),
                    )
                } else {
                    let callable = target.getattr(function)?;
                    Ok(
                        json!({"identifier": function, "callable": true, "inherited": true, "parameters": [], "parameters_known": false, "description": callable.getattr("__doc__").ok().and_then(|v| v.extract::<String>().ok()).unwrap_or_default()}),
                    )
                }
            }
            _ => self.rna_items(python, &reference, &target, request),
        }
    }

    fn rna_call(
        &mut self,
        python: Python<'_>,
        reference: &Value,
        target: &Bound<'_, PyAny>,
        request: &Value,
    ) -> PyResult<Value> {
        let function = string_field(request, "function");
        let function = Self::public_name(python, function, "function")?;
        if !rna_function_allowed(target, function) {
            return Err(operation_error(
                python,
                "access_denied",
                format!("{function} is not a public RNA function"),
            ));
        }
        let callable = target.getattr(function)?;
        let args = match request.get("args") {
            Some(Value::Array(items)) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.deserialize(python, item)?);
                }
                values
            }
            _ => Vec::new(),
        };
        let kwargs = match request.get("kwargs") {
            Some(value @ Value::Object(_)) => {
                let mapping = self.deserialize(python, value)?;
                Some(mapping.cast::<PyDict>()?.clone())
            }
            _ => None,
        };
        if matches!(
            function,
            "remove"
                | "clear"
                | "move"
                | "add"
                | "insert"
                | "pop"
                | "clear_geometry"
                | "from_pydata"
        ) {
            self.references.invalidate_collection(python, reference)?;
        } else if matches!(
            function,
            "update_from_editmode" | "mode_set" | "keyframe_insert" | "keyframe_delete"
        ) {
            self.references.invalidate_subdata();
        }
        if function == "new" {
            self.references.prune_removed_modifiers(python);
        }
        let value = callable.call(PyTuple::new(python, args)?, kwargs.as_ref())?;
        if is_rna_collection(target) && value.hasattr("bl_rna")? {
            self.references.insert_member(reference, &value)
        } else {
            self.bounded(python, &value)
        }
    }

    fn rna_items(
        &mut self,
        python: Python<'_>,
        reference: &Value,
        target: &Bound<'_, PyAny>,
        request: &Value,
    ) -> PyResult<Value> {
        let offset = request
            .get("offset")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let limit = request
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(100);
        if limit == 0 || limit > 1_000 {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "page limit must be 1..1000",
            ));
        }
        let total = target.len()?;
        let revision = collection_revision(python, target, self.generation())?;
        if let Some(expected) = request.get("expected_revision").and_then(Value::as_str)
            && expected != revision
        {
            return Err(operation_error(
                python,
                "collection_changed",
                "collection membership or order changed between pages; restart pagination",
            ));
        }
        let end = offset.saturating_add(limit).min(total);
        let mut items = Vec::new();
        let mut budget = SerializationBudget::new();
        for index in offset..end {
            let item = target.get_item(index)?;
            items.push(if item.hasattr("bl_rna")? {
                self.references.insert_member(reference, &item)?
            } else {
                self.serialize(&item, 0, &mut budget)?
            });
        }
        Ok(json!({
            "offset": offset,
            "items": items,
            "has_more": end < total,
            "total": total,
            "generation": self.generation(),
            "collection_revision": revision,
        }))
    }

    // ---- runtime ----------------------------------------------------------

    fn batch(&mut self, python: Python<'_>, request: &Value) -> PyResult<Value> {
        let requests = request.get("requests").ok_or_else(|| {
            operation_error(python, "invalid_arguments", "requests array is required")
        })?;
        validate_batch_requests(python, requests)?;
        let requests = requests.as_array().expect("validated array");
        let mut results = Vec::new();
        let mut completed = 0;
        let mut bytes = 0;
        for (index, request) in requests.iter().enumerate() {
            match self.dispatch(python, request) {
                Ok(result) => {
                    bytes += serde_json::to_vec(&result).map_or(MAX_SERIALIZED_BYTES, |v| v.len());
                    completed += 1;
                    if bytes > MAX_SERIALIZED_BYTES / 2 {
                        let error = json!({"code": "serialization_limit", "message": "batch output exceeded 4 MiB; last command executed but its result was omitted", "executed": true});
                        results.push(json!({"index": index, "error": error}));
                        return Ok(
                            json!({"completed": completed, "results": results, "complete": false, "failed_index": index, "error": error}),
                        );
                    }
                    results.push(json!({"index": index, "result": result}));
                }
                Err(error) => {
                    let value = error.value(python);
                    let code = value
                        .getattr("code")
                        .and_then(|v| v.extract::<String>())
                        .unwrap_or_else(|_| "blender_error".to_owned());
                    let error = json!({"code": code, "message": error.to_string(), "data": value.getattr("data").ok().and_then(|v| py_to_json(&v).ok())});
                    results.push(json!({"index": index, "error": error}));
                    return Ok(
                        json!({"completed": completed, "results": results, "complete": false, "failed_index": index, "error": error}),
                    );
                }
            }
        }
        Ok(json!({"completed": completed, "results": results, "complete": true}))
    }

    fn checkpoint(&mut self, python: Python<'_>, request: &Value) -> PyResult<Value> {
        let filepath = string_field(request, "filepath");
        if filepath.is_empty() {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "checkpoint filepath is required",
            ));
        }
        let path = python
            .import("pathlib")?
            .getattr("Path")?
            .call1((filepath,))?
            .call_method0("expanduser")?
            .call_method0("resolve")?;
        let path = if path.getattr("suffix")?.extract::<String>()? == ".blend" {
            path
        } else {
            path.call_method1("with_suffix", (".blend",))?
        };
        if path.call_method0("exists")?.extract::<bool>()? {
            return Err(operation_error(
                python,
                "checkpoint_exists",
                "checkpoint path already exists; choose a new path",
            ));
        }
        let kwargs = PyDict::new(python);
        kwargs.set_item("parents", true)?;
        kwargs.set_item("exist_ok", true)?;
        path.getattr("parent")?
            .call_method("mkdir", (), Some(&kwargs))?;
        let filepath = path.str()?.extract::<String>()?;
        let bpy = python.import("bpy")?;
        let before = bpy
            .getattr("data")?
            .getattr("filepath")?
            .extract::<String>()?;
        let kwargs = PyDict::new(python);
        kwargs.set_item("filepath", &filepath)?;
        kwargs.set_item("copy", true)?;
        kwargs.set_item("check_existing", false)?;
        let result = bpy
            .getattr("ops")?
            .getattr("wm")?
            .getattr("save_as_mainfile")?
            .call((), Some(&kwargs))?;
        if !result.contains("FINISHED")? {
            return Err(operation_error(
                python,
                "checkpoint_failed",
                "Blender did not finish saving the checkpoint",
            ));
        }
        let after = bpy
            .getattr("data")?
            .getattr("filepath")?
            .extract::<String>()?;
        if after != before {
            return Err(operation_error(
                python,
                "checkpoint_failed",
                "checkpoint unexpectedly changed the active file",
            ));
        }
        Ok(
            json!({"path": filepath, "size": std::fs::metadata(&filepath)?.len(), "generation": self.generation(), "active_filepath": before, "copy": true}),
        )
    }

    fn thumbnail(&mut self, python: Python<'_>, request: &Value) -> PyResult<Value> {
        let max_size = request
            .get("max_size")
            .and_then(Value::as_u64)
            .unwrap_or(512);
        if max_size == 0 || max_size > 2048 {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "thumbnail max_size must be 1..2048",
            ));
        }
        let render = python
            .import("bpy")?
            .getattr("context")?
            .getattr("scene")?
            .getattr("render")?;
        let format = render.getattr("image_settings")?;
        let previous_format: Vec<_> = ["file_format", "color_mode", "color_depth", "views_format"]
            .iter()
            .map(|name| format.getattr(*name).map(|value| (*name, value)))
            .collect::<PyResult<_>>()?;
        let previous: Vec<_> = [
            "resolution_x",
            "resolution_y",
            "resolution_percentage",
            "use_file_extension",
        ]
        .iter()
        .map(|name| render.getattr(*name).map(|value| (*name, value)))
        .collect::<PyResult<_>>()?;
        let width = render.getattr("resolution_x")?.extract::<u64>()?;
        let height = render.getattr("resolution_y")?.extract::<u64>()?;
        let largest = width.max(height).max(1);
        let bound = max_size.min(largest);
        let scaled_width = ((width * bound + largest / 2) / largest).max(1);
        let scaled_height = ((height * bound + largest / 2) / largest).max(1);
        // Blender's base resolution is at least four pixels. Percentage scaling
        // permits genuine 1--3 pixel outputs without exceeding the requested edge.
        let (factor, percentage) = if scaled_width.min(scaled_height) < 4 {
            (4, 25)
        } else {
            (1, 100)
        };
        let outcome = (|| {
            render.setattr("resolution_x", scaled_width * factor)?;
            render.setattr("resolution_y", scaled_height * factor)?;
            render.setattr("resolution_percentage", percentage)?;
            render.setattr("use_file_extension", true)?;
            format.setattr("file_format", "PNG")?;
            let mut request = request.clone();
            request["write_still"] = Value::Bool(true);
            self.render(python, &request)
        })();
        let mut restore_error = None;
        for (name, value) in previous {
            if let Err(error) = render.setattr(name, value) {
                restore_error = Some(error);
            }
        }
        for (name, value) in previous_format {
            if let Err(error) = format.setattr(name, value) {
                restore_error = Some(error);
            }
        }
        if let Some(error) = restore_error {
            return Err(error);
        }
        outcome
    }

    fn render(&mut self, python: Python<'_>, request: &Value) -> PyResult<Value> {
        if request.get("write_still").and_then(Value::as_bool) == Some(false) {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "artifact rendering requires write_still=true; use the render operator for a preview-only render",
            ));
        }
        let pathlib = python.import("pathlib")?.getattr("Path")?;
        let requested = request.get("filepath").and_then(Value::as_str);
        let path = match requested {
            Some(filepath) if !filepath.is_empty() => pathlib.call1((filepath,))?,
            _ => pathlib.call1((self
                .artifacts
                .root()
                .join("render.png")
                .to_string_lossy()
                .as_ref(),))?,
        };
        // `expanduser` and `resolve` are Python's, so `~` behaves as callers expect.
        let path = path.call_method0("expanduser")?.call_method0("resolve")?;
        let parent = path.getattr("parent")?;
        let mkdir_kwargs = PyDict::new(python);
        mkdir_kwargs.set_item("parents", true)?;
        mkdir_kwargs.set_item("exist_ok", true)?;
        parent.call_method("mkdir", (), Some(&mkdir_kwargs))?;

        let bpy = python.import("bpy")?;
        let render_settings = bpy
            .getattr("context")?
            .getattr("scene")?
            .getattr("render")?;
        let previous = render_settings.getattr("filepath")?;
        let path_text = path.str()?.extract::<String>()?;
        // Match Blender's `do_ensure_image_extension`: retain format aliases,
        // replace known image suffixes, and append to other names. `frame_path()`
        // is unsuitable here because it adds animation frame numbers.
        let output_path = if render_settings
            .getattr("use_file_extension")?
            .extract::<bool>()?
        {
            let extension = render_settings
                .getattr("file_extension")?
                .extract::<String>()?;
            let suffix = path
                .getattr("suffix")?
                .extract::<String>()?
                .to_ascii_lowercase();
            let matches_format = suffix == extension
                || matches!(
                    (extension.as_str(), suffix.as_str()),
                    (".jpg", ".jpeg") | (".tif", ".tiff")
                );
            if matches_format {
                path_text.clone()
            } else if bpy
                .getattr("path")?
                .getattr("extensions_image")?
                .contains(&suffix)?
            {
                path.call_method1("with_suffix", (&extension,))?
                    .str()?
                    .extract::<String>()?
            } else {
                format!("{path_text}{extension}")
            }
        } else {
            path_text.clone()
        };
        render_settings.setattr("filepath", &path_text)?;

        let write_still = request
            .get("write_still")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let render_kwargs = PyDict::new(python);
        render_kwargs.set_item("write_still", write_still)?;
        let outcome = bpy
            .getattr("ops")?
            .getattr("render")?
            .getattr("render")?
            .call((), Some(&render_kwargs));
        // Restore the scene's own output path whether or not the render succeeded.
        render_settings.setattr("filepath", previous)?;
        validate_render_status(&outcome?)?;

        if !pathlib
            .call1((&output_path,))?
            .call_method0("is_file")?
            .extract::<bool>()?
        {
            return Err(operation_error(
                python,
                "artifact_missing",
                format!("render did not create {output_path}"),
            ));
        }
        Ok(
            json!({ "artifact": self.artifacts.capture(python, &output_path, &scene::render_provenance(python, self.generation())?)? }),
        )
    }
}

fn validate_render_status(status: &Bound<'_, PyAny>) -> PyResult<()> {
    if !status.contains("FINISHED")? {
        return Err(operation_error(
            status.py(),
            if status.contains("CANCELLED")? {
                "render_cancelled"
            } else {
                "render_failed"
            },
            "Blender did not finish rendering; no artifact was captured",
        ));
    }
    Ok(())
}

fn is_blender_instance(target: &Bound<'_, PyAny>, type_name: &str) -> bool {
    target
        .py()
        .import("bpy")
        .and_then(|bpy| bpy.getattr("types"))
        .and_then(|types| types.getattr(type_name))
        .and_then(|class| target.is_instance(&class))
        .unwrap_or(false)
}

fn is_rna_collection(target: &Bound<'_, PyAny>) -> bool {
    is_blender_instance(target, "bpy_prop_collection")
        || is_blender_instance(target, "bpy_prop_collection_idprop")
}

/// Keep discovery and invocation in agreement for helpers outside RNA metadata.
fn inherited_functions(target: &Bound<'_, PyAny>) -> Vec<&'static str> {
    let candidates: &[&str] = if is_rna_collection(target) {
        &COLLECTION_CALLS
    } else if is_blender_instance(target, "bpy_struct") {
        &STRUCT_CALLS
    } else {
        &[]
    };
    candidates
        .iter()
        .copied()
        .filter(|name| target.getattr(*name).is_ok_and(|value| value.is_callable()))
        .collect()
}

/// RNA metadata plus narrowly scoped, type-checked inherited Blender helpers.
fn rna_function_allowed(target: &Bound<'_, PyAny>, function: &str) -> bool {
    if let Ok(functions) = target
        .getattr("bl_rna")
        .and_then(|rna| rna.getattr("functions"))
        && let Ok(found) = functions.call_method1("get", (function,))
        && !found.is_none()
    {
        return true;
    }
    inherited_functions(target).contains(&function)
}

fn describe_rna(target: &Bound<'_, PyAny>) -> Value {
    let Ok(rna) = target.getattr("bl_rna") else {
        let kind = target
            .get_type()
            .name()
            .and_then(|name| name.extract::<String>())
            .unwrap_or_else(|_| "object".to_owned());
        return json!({ "type": kind, "functions": inherited_functions(target) });
    };
    let names = |attribute: &str| -> Vec<String> {
        rna.getattr(attribute)
            .and_then(|collection| collection.try_iter().map(Iterator::collect::<Vec<_>>))
            .map(|entries| {
                entries
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        entry
                            .getattr("identifier")
                            .and_then(|value| value.extract::<String>())
                            .ok()
                    })
                    .filter(|identifier| !identifier.starts_with('_'))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut functions = names("functions");
    functions.extend(inherited_functions(target).into_iter().map(str::to_owned));
    functions.sort_unstable();
    functions.dedup();
    json!({
        "identifier": rna.getattr("identifier").and_then(|v| v.extract::<String>()).unwrap_or_default(),
        "name": rna.getattr("name").and_then(|v| v.extract::<String>()).unwrap_or_default(),
        "description": rna.getattr("description").and_then(|v| v.extract::<String>()).unwrap_or_default(),
        "properties": names("properties"),
        "functions": functions,
    })
}

/// Run `body` inside `bpy.context.temp_override(**override)` when one is supplied.
fn with_override<R>(
    python: Python<'_>,
    override_map: Option<&Bound<'_, PyDict>>,
    body: impl FnOnce() -> PyResult<R>,
) -> PyResult<R> {
    let Some(override_map) = override_map else {
        return body();
    };
    let guard = python.import("bpy")?.getattr("context")?.call_method(
        "temp_override",
        (),
        Some(override_map),
    )?;
    guard.call_method0("__enter__")?;
    let outcome = body();
    let none = python.None();
    guard.call_method1("__exit__", (&none, &none, &none))?;
    outcome
}

fn string_field<'a>(request: &'a Value, key: &str) -> &'a str {
    request.get(key).and_then(Value::as_str).unwrap_or("")
}

fn array_field<'a>(python: Python<'_>, request: &'a Value, key: &str) -> PyResult<&'a [Value]> {
    request
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| {
            operation_error(
                python,
                "invalid_arguments",
                format!("{key} must be an array"),
            )
        })
}

/// A list of equal-length numeric rows -- the shape `rna_get` returns for a matrix.
fn is_row_matrix(value: &Value) -> bool {
    let Some(rows) = value.as_array() else {
        return false;
    };
    let width = rows.first().and_then(Value::as_array).map_or(0, Vec::len);
    (2..=4).contains(&rows.len())
        && (2..=4).contains(&width)
        && rows.iter().all(|row| {
            row.as_array()
                .is_some_and(|row| row.len() == width && row.iter().all(Value::is_number))
        })
}

fn is_matrix_property(target: &Bound<'_, PyAny>, attribute: &str) -> bool {
    target
        .getattr("bl_rna")
        .and_then(|rna| rna.getattr("properties"))
        .and_then(|properties| properties.call_method1("get", (attribute,)))
        .and_then(|property| property.getattr("subtype"))
        .and_then(|subtype| subtype.extract::<String>())
        .is_ok_and(|subtype| subtype == "MATRIX")
}

fn collection_revision(
    python: Python<'_>,
    target: &Bound<'_, PyAny>,
    generation: u64,
) -> PyResult<String> {
    let total = target.len()?;
    if total > 100_000 {
        return Err(operation_error(
            python,
            "collection_limit",
            "stable pagination supports at most 100000 members; use Blender bulk array functions for larger data",
        ));
    }
    let mut digest = Sha256::new();
    digest.update(generation.to_be_bytes());
    digest.update(total.to_be_bytes());
    let mut scanned = 0;
    for item in target.try_iter()?.take(total + 1) {
        let item = item?;
        scanned += 1;
        if scanned > total {
            return Err(operation_error(
                python,
                "collection_changed",
                "collection changed during pagination",
            ));
        }
        let identity = item
            .getattr("session_uid")
            .and_then(|v| v.extract::<u64>())
            .or_else(|_| {
                item.call_method0("as_pointer")
                    .and_then(|v| v.extract::<u64>())
            });
        match identity {
            Ok(identity) => digest.update(identity.to_be_bytes()),
            Err(_) => digest.update(serde_json::to_vec(&py_to_json(&item)?).map_err(|error| {
                operation_error(python, "serialization_error", error.to_string())
            })?),
        }
    }
    if scanned != total {
        return Err(operation_error(
            python,
            "collection_changed",
            "collection changed during pagination",
        ));
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn validate_batch_requests(python: Python<'_>, requests: &Value) -> PyResult<()> {
    let requests = requests
        .as_array()
        .ok_or_else(|| operation_error(python, "invalid_arguments", "requests must be an array"))?;
    if requests.is_empty() || requests.len() > 100 {
        return Err(operation_error(
            python,
            "invalid_arguments",
            "a batch must contain 1..100 commands",
        ));
    }
    for request in requests {
        if !matches!(
            string_field(request, "operation"),
            "rna_get"
                | "rna_set"
                | "rna_call"
                | "rna_describe"
                | "rna_items"
                | "rna_property_info"
                | "rna_function_info"
                | "operator_call"
                | "operator_poll"
                | "operator_info"
                | "context_ref"
                | "data_ref"
                | "scene_summary"
        ) {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "batch contains a nested, control, or unsupported operation",
            ));
        }
        if matches!(
            string_field(request, "idname"),
            "wm.open_mainfile"
                | "wm.read_homefile"
                | "wm.read_factory_settings"
                | "wm.quit_blender"
                | "ed.undo"
                | "ed.redo"
                | "ed.undo_history"
        ) {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "document lifecycle operations cannot run inside a batch",
            ));
        }
        let operation = string_field(request, "operation");
        if operation.starts_with("rna_")
            && !request.get("reference").is_some_and(|reference| {
                reference.get("generation").is_some_and(Value::is_u64)
                    && reference.get("id").is_some_and(Value::is_string)
                    && reference.get("type_name").is_some_and(Value::is_string)
            })
        {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "batch RNA commands require a reference body",
            ));
        }
        let field = match operation {
            "rna_get" | "rna_set" | "rna_property_info" => Some("attribute"),
            "rna_call" | "rna_function_info" => Some("function"),
            "operator_call" | "operator_poll" | "operator_info" => Some("idname"),
            _ => None,
        };
        if field.is_some_and(|field| !request.get(field).is_some_and(Value::is_string))
            || (operation == "rna_set" && request.get("value").is_none())
            || request.get("args").is_some_and(|value| !value.is_array())
            || request
                .get("kwargs")
                .is_some_and(|value| !value.is_object() && !value.is_null())
        {
            return Err(operation_error(
                python,
                "invalid_arguments",
                "batch command fields have invalid types",
            ));
        }
    }
    if serde_json::to_vec(requests).map_or(usize::MAX, |v| v.len()) > 1024 * 1024 {
        return Err(operation_error(
            python,
            "invalid_arguments",
            "batch input exceeds 1 MiB",
        ));
    }
    Ok(())
}

struct SerializationBudget {
    items: usize,
    bytes: usize,
}
impl SerializationBudget {
    const fn new() -> Self {
        Self {
            items: MAX_SERIALIZED_ITEMS,
            bytes: MAX_SERIALIZED_BYTES,
        }
    }
    fn consume(&mut self, python: Python<'_>, items: usize, bytes: usize) -> PyResult<()> {
        if items > self.items || bytes > self.bytes {
            return Err(serialization_limit(python));
        }
        self.items -= items;
        self.bytes -= bytes;
        Ok(())
    }
}
fn serialization_limit(python: Python<'_>) -> PyErr {
    operation_error(
        python,
        "serialization_limit",
        "result exceeds depth 8, 4096 values, or 8 MiB; use bounded collection pages",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_collection_allowlist_is_exactly_what_python_permitted() {
        // Widening this set widens the sandbox, so pin it.
        let mut allowed = COLLECTION_CALLS;
        allowed.sort_unstable();
        assert_eq!(
            allowed,
            [
                "clear",
                "find",
                "foreach_get",
                "foreach_set",
                "get",
                "link",
                "move",
                "new",
                "remove",
                "unlink",
            ]
        );
    }
}
