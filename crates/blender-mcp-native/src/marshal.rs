//! Conversions between plain Python values and JSON.
//!
//! Only JSON-shaped values cross here. Anything else is a caller error rather than
//! something to coerce, which is what keeps non-finite floats and arbitrary objects off
//! the wire -- the equivalent of the `allow_nan=False` the Python implementation passed
//! to `json.dumps`.

use pyo3::{
    exceptions::PyValueError,
    prelude::*,
    types::{PyBool, PyDict, PyFloat, PyInt, PyList, PySet, PyString, PyTuple},
};
use serde_json::{Map, Number, Value};

/// Convert a plain Python value to JSON.
pub(crate) fn py_to_json(value: &Bound<'_, PyAny>) -> PyResult<Value> {
    if value.is_none() {
        return Ok(Value::Null);
    }
    // `bool` is checked before `int`, because Python's bool is a subclass of int and
    // would otherwise serialize as 0 or 1.
    if value.is_instance_of::<PyBool>() {
        return Ok(Value::Bool(value.extract::<bool>()?));
    }
    if value.is_instance_of::<PyInt>() {
        return Ok(Value::Number(Number::from(value.extract::<i64>()?)));
    }
    if value.is_instance_of::<PyFloat>() {
        let number = value.extract::<f64>()?;
        return Number::from_f64(number)
            .map(Value::Number)
            .ok_or_else(|| PyValueError::new_err(format!("{number} has no JSON representation")));
    }
    if value.is_instance_of::<PyString>() {
        return Ok(Value::String(value.extract::<String>()?));
    }
    if let Ok(mapping) = value.cast::<PyDict>() {
        let mut object = Map::with_capacity(mapping.len());
        for (key, item) in mapping.iter() {
            object.insert(key.str()?.extract::<String>()?, py_to_json(&item)?);
        }
        return Ok(Value::Object(object));
    }
    if value.is_instance_of::<PyList>()
        || value.is_instance_of::<PyTuple>()
        || value.is_instance_of::<PySet>()
    {
        let mut items = Vec::new();
        for item in value.try_iter()? {
            items.push(py_to_json(&item?)?);
        }
        return Ok(Value::Array(items));
    }
    Err(PyValueError::new_err(format!(
        "value of type {} has no JSON representation",
        value.get_type().name()?
    )))
}

/// Convert JSON back to a plain Python value.
pub(crate) fn json_to_py<'py>(python: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        Value::Null => python.None().into_bound(python),
        Value::Bool(flag) => flag.into_pyobject(python)?.to_owned().into_any(),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer.into_pyobject(python)?.into_any()
            } else {
                number
                    .as_f64()
                    .unwrap_or_default()
                    .into_pyobject(python)?
                    .into_any()
            }
        }
        Value::String(text) => text.into_pyobject(python)?.into_any(),
        Value::Array(items) => {
            let list = PyList::empty(python);
            for item in items {
                list.append(json_to_py(python, item)?)?;
            }
            list.into_any()
        }
        Value::Object(entries) => {
            let mapping = PyDict::new(python);
            for (key, item) in entries {
                mapping.set_item(key, json_to_py(python, item)?)?;
            }
            mapping.into_any()
        }
    })
}
