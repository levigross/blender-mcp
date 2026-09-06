//! Exception types mirroring the ones the Python implementation raised.

use pyo3::{create_exception, exceptions::PyRuntimeError, prelude::*};

create_exception!(
    scheme_blender_mcp_native,
    ProtocolError,
    PyRuntimeError,
    "A malformed or incompatible bridge frame."
);

create_exception!(
    scheme_blender_mcp_native,
    OperationError,
    PyRuntimeError,
    "A failure carrying a machine-readable code."
);

/// Raise an `OperationError` with no structured data.
pub(crate) fn operation_error(python: Python<'_>, code: &str, message: impl Into<String>) -> PyErr {
    operation_error_with_data(python, code, message, python.None())
}

/// Raise an `OperationError` carrying structured `data`.
///
/// `code` and `data` are set as instance attributes rather than modelled as a
/// `#[pyclass]`, so `bridge.py` keeps reading `error.code` and `error.data` exactly as
/// it did against the Python original.
pub(crate) fn operation_error_with_data(
    python: Python<'_>,
    code: &str,
    message: impl Into<String>,
    data: Py<PyAny>,
) -> PyErr {
    let error = OperationError::new_err(message.into());
    let value = error.value(python);
    if let Err(failure) = value.setattr("code", code) {
        return failure;
    }
    if let Err(failure) = value.setattr("data", data) {
        return failure;
    }
    error
}
