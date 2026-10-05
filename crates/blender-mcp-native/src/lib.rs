//! The Blender-side bridge, implemented in Rust and loaded by Blender's own Python.
//!
//! This replaces `protocol.py`, `operations.py`, and `catalog.py`. The socket loop and
//! main-thread dispatch stay in `bridge.py`, because the timer callback has to be a
//! Python object registered with `bpy.app.timers`, and add-on registration has to be
//! Python because that is the only interface Blender offers.
//!
//! Python owns bridge scheduling and lifecycle; this module owns RNA access,
//! reference validity, bounded serialization, and artifact storage.

use pyo3::prelude::*;

mod artifacts;
mod catalog;
mod errors;
mod marshal;
mod operations;
mod protocol;
mod refs;
mod scene;

/// `mesh.primitive_cube_add` -> `bpy/ops/mesh/primitive_cube_add`.
#[pyfunction]
fn steel_name(idname: &str) -> String {
    catalog::steel_name(idname)
}

/// The inverse of [`steel_name`], raising on a malformed name.
#[pyfunction]
fn idname_from_steel(python: Python<'_>, name: &str) -> PyResult<String> {
    catalog::idname_from_steel(name).ok_or_else(|| {
        pyo3::exceptions::PyValueError::new_err(format!("invalid generated operator name: {name}"))
            .restore(python);
        PyErr::fetch(python)
    })
}

/// Describe one operator.
#[pyfunction]
fn describe_operator<'py>(python: Python<'py>, idname: &str) -> PyResult<Bound<'py, PyAny>> {
    marshal::json_to_py(python, &catalog::describe_operator(python, idname)?)
}

/// Discover every operator and hash the result into a revision.
#[pyfunction]
fn build_catalog(python: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    marshal::json_to_py(python, &catalog::build_catalog(python)?)
}

#[pymodule]
fn scheme_blender_mcp_native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add(
        "CAPABILITIES",
        pyo3::types::PyTuple::new(
            module.py(),
            [
                "reference_epochs",
                "reference_release",
                "bounded_serialization",
                "immutable_artifacts",
                "rna_metadata",
                "batch",
                "scene_snapshot",
                "checkpoint",
                "mesh_from_data",
                "collection_values",
                "node_tree_build",
                "id_properties",
                "thumbnail",
                "artifact_release",
            ],
        )?,
    )?;
    module.add("PROTOCOL_VERSION", protocol::protocol_version())?;
    module.add(
        "MAX_FRAME_BYTES",
        blender_mcp_protocol::DEFAULT_MAX_FRAME_BYTES,
    )?;
    module.add(
        "ProtocolError",
        module.py().get_type::<errors::ProtocolError>(),
    )?;
    module.add(
        "OperationError",
        module.py().get_type::<errors::OperationError>(),
    )?;
    module.add_class::<operations::BlenderOperations>()?;
    module.add_function(wrap_pyfunction!(protocol::read_frame, module)?)?;
    module.add_function(wrap_pyfunction!(protocol::write_frame, module)?)?;
    module.add_function(wrap_pyfunction!(protocol::success, module)?)?;
    module.add_function(wrap_pyfunction!(protocol::failure, module)?)?;
    module.add_function(wrap_pyfunction!(steel_name, module)?)?;
    module.add_function(wrap_pyfunction!(idname_from_steel, module)?)?;
    module.add_function(wrap_pyfunction!(describe_operator, module)?)?;
    module.add_function(wrap_pyfunction!(build_catalog, module)?)?;
    Ok(())
}
