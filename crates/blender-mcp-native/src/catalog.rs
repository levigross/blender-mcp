//! Runtime operator catalog derived from Blender RNA.

use blender_mcp_protocol::PROTOCOL_VERSION;
use pyo3::{prelude::*, types::PyString};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::errors::operation_error;

/// `mesh.primitive_cube_add` becomes `bpy/ops/mesh/primitive_cube_add`.
pub(crate) fn steel_name(idname: &str) -> String {
    match idname.split_once('.') {
        Some((module, function)) => format!("bpy/ops/{module}/{function}"),
        None => format!("bpy/ops/{idname}"),
    }
}

/// The inverse of [`steel_name`].
pub(crate) fn idname_from_steel(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split('/').collect();
    match parts.as_slice() {
        ["bpy", "ops", module, function] => Some(format!("{module}.{function}")),
        _ => None,
    }
}

/// Coerce an arbitrary RNA default into something JSON can carry.
fn json_value(value: &Bound<'_, PyAny>) -> Value {
    if value.is_none() {
        return Value::Null;
    }
    if let Ok(flag) = value.cast::<pyo3::types::PyBool>() {
        return Value::Bool(flag.is_true());
    }
    if let Ok(integer) = value.extract::<i64>() {
        return Value::from(integer);
    }
    if let Ok(number) = value.extract::<f64>() {
        return serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number);
    }
    if let Ok(text) = value.cast::<PyString>() {
        return text.extract::<String>().map_or(Value::Null, Value::String);
    }
    // Vectors and enum sets iterate; anything else falls back to its repr, matching the
    // Python implementation rather than dropping information.
    if let Ok(iterator) = value.try_iter() {
        return Value::Array(iterator.flatten().map(|item| json_value(&item)).collect());
    }
    value
        .str()
        .and_then(|text| text.extract::<String>())
        .map_or(Value::Null, Value::String)
}

fn enum_items(property: &Bound<'_, PyAny>) -> Value {
    let is_enum = property
        .getattr("type")
        .and_then(|kind| kind.extract::<String>())
        .is_ok_and(|kind| kind == "ENUM");
    if !is_enum {
        return Value::Array(Vec::new());
    }
    // Context-dependent enum callbacks are not inspectable in every mode.
    let Ok(items) = property.getattr("enum_items") else {
        return Value::Array(Vec::new());
    };
    let Ok(iterator) = items.try_iter() else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        iterator
            .flatten()
            .map(|item| {
                json!({
                    "identifier": attribute_string(&item, "identifier"),
                    "name": attribute_string(&item, "name"),
                    "description": attribute_string(&item, "description"),
                })
            })
            .collect(),
    )
}

fn attribute_string(value: &Bound<'_, PyAny>, name: &str) -> String {
    value
        .getattr(name)
        .and_then(|attribute| attribute.extract::<String>())
        .unwrap_or_default()
}

pub(crate) fn property_descriptor(property: &Bound<'_, PyAny>) -> Value {
    let kind = attribute_string(property, "type");
    let mut descriptor = Map::new();
    descriptor.insert(
        "identifier".to_owned(),
        Value::String(attribute_string(property, "identifier")),
    );
    descriptor.insert("kind".to_owned(), Value::String(kind.clone()));
    descriptor.insert(
        "description".to_owned(),
        Value::String(attribute_string(property, "description")),
    );
    descriptor.insert(
        "required".to_owned(),
        Value::Bool(
            property
                .getattr("is_required")
                .and_then(|flag| flag.extract::<bool>())
                .unwrap_or(false),
        ),
    );
    descriptor.insert("enum_items".to_owned(), enum_items(property));
    for (key, source) in [
        ("read_only", "is_readonly"),
        ("animatable", "is_animatable"),
        ("output", "is_output"),
        ("enum_flag", "is_enum_flag"),
    ] {
        if let Ok(flag) = property.getattr(source).and_then(|v| v.extract::<bool>()) {
            descriptor.insert(key.to_owned(), Value::Bool(flag));
        }
    }
    for key in ["name", "subtype", "unit"] {
        if let Ok(text) = property.getattr(key).and_then(|v| v.extract::<String>()) {
            descriptor.insert(key.to_owned(), Value::String(text));
        }
    }
    if let Ok(fixed_type) = property
        .getattr("fixed_type")
        .and_then(|v| v.getattr("identifier"))
        .and_then(|v| v.extract::<String>())
    {
        descriptor.insert("fixed_type".to_owned(), Value::String(fixed_type));
    }
    if let Ok(default) = property.getattr("default") {
        descriptor.insert("default".to_owned(), json_value(&default));
    }
    if kind == "INT" || kind == "FLOAT" {
        for (key, source) in [("minimum", "hard_min"), ("maximum", "hard_max")] {
            if let Some(bound) = property
                .getattr(source)
                .ok()
                .and_then(|value| value.extract::<f64>().ok())
                .and_then(serde_json::Number::from_f64)
            {
                descriptor.insert(key.to_owned(), Value::Number(bound));
            }
        }
    }
    let array_length = property
        .getattr("array_length")
        .ok()
        .and_then(|value| value.extract::<u64>().ok())
        .unwrap_or(0);
    if array_length > 0 {
        descriptor.insert("array_length".to_owned(), Value::from(array_length));
        if let Ok(default) = property.getattr("default_array") {
            descriptor.insert("default".to_owned(), json_value(&default));
        }
    }
    Value::Object(descriptor)
}

/// Operator flags such as `REGISTER` and `UNDO`.
///
/// These live on the `bpy.ops` callable, not on the RNA type it returns, and not on a
/// `bpy.types` class -- built-in C operators are absent from `bpy.types`.
fn options(operator: &Bound<'_, PyAny>) -> Value {
    let Ok(flags) = operator.getattr("bl_options") else {
        return Value::Array(Vec::new());
    };
    let Ok(iterator) = flags.try_iter() else {
        return Value::Array(Vec::new());
    };
    let mut names: Vec<String> = iterator
        .flatten()
        .filter_map(|flag| flag.extract::<String>().ok())
        .collect();
    names.sort_unstable();
    Value::Array(names.into_iter().map(Value::String).collect())
}

/// Every `module.function` under `bpy.ops` that is a real operator.
pub(crate) fn operator_idnames(python: Python<'_>) -> PyResult<Vec<String>> {
    let ops = python.import("bpy")?.getattr("ops")?;
    let mut modules: Vec<String> = ops
        .dir()?
        .iter()
        .filter_map(|name| name.extract::<String>().ok())
        .filter(|name| !name.starts_with('_'))
        .collect();
    modules.sort_unstable();

    let mut idnames = Vec::new();
    for module_name in modules {
        let Ok(module) = ops.getattr(module_name.as_str()) else {
            continue;
        };
        let mut functions: Vec<String> = module
            .dir()?
            .iter()
            .filter_map(|name| name.extract::<String>().ok())
            .filter(|name| !name.starts_with('_'))
            .collect();
        functions.sort_unstable();
        for function_name in functions {
            let Ok(operator) = module.getattr(function_name.as_str()) else {
                continue;
            };
            if operator.is_callable() && operator.hasattr("get_rna_type").unwrap_or(false) {
                idnames.push(format!("{module_name}.{function_name}"));
            }
        }
    }
    Ok(idnames)
}

/// The full descriptor for one operator.
pub(crate) fn describe_operator(python: Python<'_>, idname: &str) -> PyResult<Value> {
    let unknown = || {
        operation_error(
            python,
            "unknown_operator",
            format!("unknown Blender operator: {idname}"),
        )
    };
    let (module_name, function_name) = idname.split_once('.').ok_or_else(unknown)?;
    let operator = python
        .import("bpy")?
        .getattr("ops")
        .and_then(|ops| ops.getattr(module_name))
        .and_then(|module| module.getattr(function_name))
        .map_err(|_| unknown())?;
    let rna = operator
        .call_method0("get_rna_type")
        .map_err(|_| unknown())?;

    let properties = rna
        .getattr("properties")?
        .try_iter()?
        .flatten()
        .filter(|property| attribute_string(property, "identifier") != "rna_type")
        .map(|property| property_descriptor(&property))
        .collect::<Vec<_>>();

    let label = attribute_string(&rna, "name");
    Ok(json!({
        "idname": idname,
        "steel_name": steel_name(idname),
        "label": if label.is_empty() { idname.to_owned() } else { label },
        "description": attribute_string(&rna, "description"),
        "module": module_name,
        "function": function_name,
        "options": options(&operator),
        "properties": properties,
    }))
}

/// Discover every operator and hash the result into a revision.
pub(crate) fn build_catalog(python: Python<'_>) -> PyResult<Value> {
    let operators = operator_idnames(python)?
        .iter()
        .map(|idname| describe_operator(python, idname))
        .collect::<PyResult<Vec<_>>>()?;

    // The revision must be stable across runs, so hash a key-sorted encoding.
    // `serde_json::Value::Object` is a BTreeMap by default, which sorts for us.
    let encoded = serde_json::to_vec(&Value::Array(operators.clone()))
        .map_err(|error| operation_error(python, "internal_error", error.to_string()))?;
    let revision = format!("{:x}", Sha256::digest(&encoded));

    let version = python
        .import("bpy")?
        .getattr("app")?
        .getattr("version_string")?
        .extract::<String>()?;

    Ok(json!({
        "protocol_version": PROTOCOL_VERSION,
        "revision": revision,
        "blender_version": version,
        "operators": operators,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steel_names_round_trip() {
        let idname = "mesh.primitive_cube_add";
        let generated = steel_name(idname);
        assert_eq!(generated, "bpy/ops/mesh/primitive_cube_add");
        assert_eq!(idname_from_steel(&generated).as_deref(), Some(idname));
    }

    #[test]
    fn malformed_generated_names_are_rejected() {
        for name in [
            "bpy/ops/mesh",
            "mesh/primitive_cube_add",
            "bpy/types/a/b",
            "",
        ] {
            assert!(idname_from_steel(name).is_none(), "{name}");
        }
    }
}
