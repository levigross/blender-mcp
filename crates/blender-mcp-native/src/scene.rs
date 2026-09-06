//! Bounded value snapshots and comparisons; these never retain RNA handles.

use std::collections::BTreeMap;

use pyo3::prelude::*;
use serde_json::{Value, json};

use crate::{errors::operation_error, marshal::py_to_json};

pub(crate) const MAX_SNAPSHOT_OBJECTS: usize = 1_000;

fn value(target: &Bound<'_, PyAny>, name: &str) -> PyResult<Value> {
    py_to_json(&target.getattr(name)?)
}
fn vector(target: &Bound<'_, PyAny>, name: &str) -> PyResult<Value> {
    let values = target
        .getattr(name)?
        .try_iter()?
        .take(16)
        .map(|item| py_to_json(&item?))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(Value::Array(values))
}

pub(crate) fn render_provenance(python: Python<'_>, generation: u64) -> PyResult<Value> {
    let scene = python.import("bpy")?.getattr("context")?.getattr("scene")?;
    let render = scene.getattr("render")?;
    let percentage = render.getattr("resolution_percentage")?.extract::<u64>()?;
    let width = render.getattr("resolution_x")?.extract::<u64>()? * percentage / 100;
    let height = render.getattr("resolution_y")?.extract::<u64>()? * percentage / 100;
    Ok(json!({
        "generation": generation, "scene": value(&scene, "name")?, "frame": value(&scene, "frame_current")?,
        "engine": value(&render, "engine")?, "width": width, "height": height,
        "file_format": value(&render.getattr("image_settings")?, "file_format")?,
        "blender_version": value(&python.import("bpy")?.getattr("app")?, "version_string")?,
        "camera": scene.getattr("camera")?.getattr("name").ok().and_then(|v| v.extract::<String>().ok()),
    }))
}

pub(crate) fn summary(python: Python<'_>) -> PyResult<Value> {
    let bpy = python.import("bpy")?;
    let context = bpy.getattr("context")?;
    // Evaluate once before reading modifier geometry and constrained transforms.
    let depsgraph = context.call_method0("evaluated_depsgraph_get")?;
    let scene = context.getattr("scene")?;
    let objects = scene.getattr("objects")?;
    let render = scene.getattr("render")?;
    let materials = bpy.getattr("data")?.getattr("materials")?;
    let material_names = materials
        .try_iter()?
        .take(128)
        .map(|material| value(&material?, "name"))
        .collect::<PyResult<Vec<_>>>()?;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut selected = Vec::new();
    let mut bounds = Bounds::default();
    let mut scanned = 0;
    for object in objects.try_iter()?.take(MAX_SNAPSHOT_OBJECTS) {
        let object = object?;
        scanned += 1;
        *counts
            .entry(object.getattr("type")?.extract::<String>()?)
            .or_default() += 1;
        if object.call_method0("select_get")?.extract::<bool>()? {
            selected.push(value(&object, "name")?);
        }
    }
    let view_objects = context.getattr("view_layer")?.getattr("objects")?;
    let mut bounds_scanned = 0;
    for object in view_objects.try_iter()?.take(MAX_SNAPSHOT_OBJECTS) {
        let evaluated = object?.call_method1("evaluated_get", (&depsgraph,))?;
        bounds.extend(&evaluated)?;
        bounds_scanned += 1;
    }
    Ok(json!({
        "scene": value(&scene, "name")?, "objects": objects.len()?, "scanned_objects": scanned,
        "truncated": objects.len()? > scanned, "object_types": counts,
        "frame": value(&scene, "frame_current")?, "render_engine": value(&render, "engine")?,
        "render": {"engine": value(&render, "engine")?, "resolution_x": value(&render, "resolution_x")?,
            "resolution_y": value(&render, "resolution_y")?, "resolution_percentage": value(&render, "resolution_percentage")?,
            "file_format": value(&render.getattr("image_settings")?, "file_format")?},
        "materials": {"total": materials.len()?, "names": material_names, "truncated": materials.len()? > 128,
            "scope": "document material datablocks"},
        "camera": scene.getattr("camera")?.getattr("name").ok().and_then(|v| v.extract::<String>().ok()),
        "active_object": context.getattr("active_object")?.getattr("name").ok().and_then(|v| v.extract::<String>().ok()),
        "selected_objects": selected, "world_bounds": bounds.json(),
        "world_bounds_scope": "evaluated object geometry in the current view layer; excludes instances and unavailable bounds",
        "bounds_scanned_objects": bounds_scanned, "bounds_total_objects": view_objects.len()?,
        "bounds_truncated": view_objects.len()? > bounds_scanned,
    }))
}

pub(crate) fn snapshot(python: Python<'_>, generation: u64, limit: usize) -> PyResult<Value> {
    if limit == 0 || limit > MAX_SNAPSHOT_OBJECTS {
        return Err(operation_error(
            python,
            "invalid_arguments",
            "snapshot limit must be 1..1000",
        ));
    }
    let scene = python.import("bpy")?.getattr("context")?.getattr("scene")?;
    let objects = scene.getattr("objects")?;
    let mut entries = Vec::new();
    for object in objects.try_iter()?.take(limit) {
        let object = object?;
        let parent = object.getattr("parent")?;
        entries.push(json!({
            "uid": value(&object, "session_uid")?, "name": value(&object, "name")?, "type": value(&object, "type")?,
            "location": vector(&object, "location")?, "rotation_euler": vector(&object, "rotation_euler")?,
            "rotation_mode": value(&object, "rotation_mode")?, "rotation_quaternion": vector(&object, "rotation_quaternion")?,
            "rotation_axis_angle": vector(&object, "rotation_axis_angle")?,
            "delta_location": vector(&object, "delta_location")?, "delta_scale": vector(&object, "delta_scale")?,
            "delta_rotation_euler": vector(&object, "delta_rotation_euler")?,
            "delta_rotation_quaternion": vector(&object, "delta_rotation_quaternion")?,
            "scale": vector(&object, "scale")?, "hide_render": value(&object, "hide_render")?,
            "parent_uid": if parent.is_none() { Value::Null } else { value(&parent, "session_uid")? },
        }));
    }
    entries.sort_by_key(|entry| entry["uid"].as_u64().unwrap_or(0));
    Ok(json!({
        "schema_version": 1, "generation": generation,
        "scene": value(&scene, "name")?, "frame": value(&scene, "frame_current")?,
        "render": render_provenance(python, generation)?, "objects": entries,
        "total_objects": objects.len()?, "truncated": objects.len()? > limit,
        "scope": "object base and delta transform channels, parent identity, render visibility and basic render metadata; excludes constraints, parent inverse matrices, mesh topology, materials and animation curves",
    }))
}

pub(crate) fn diff(python: Python<'_>, before: &Value, after: &Value) -> PyResult<Value> {
    for snapshot in [before, after] {
        if snapshot.get("schema_version").and_then(Value::as_u64) != Some(1)
            || snapshot.get("truncated").and_then(Value::as_bool) != Some(false)
            || !snapshot.get("objects").is_some_and(Value::is_array)
            || !snapshot.get("generation").is_some_and(Value::is_u64)
        {
            return Err(operation_error(
                python,
                "invalid_snapshot",
                "diff requires complete schema-version 1 scene snapshots",
            ));
        }
        let objects = snapshot["objects"].as_array().expect("validated array");
        let mut identifiers = std::collections::BTreeSet::new();
        if objects.len() > MAX_SNAPSHOT_OBJECTS
            || objects.iter().any(|object| {
                object
                    .get("uid")
                    .and_then(Value::as_u64)
                    .is_none_or(|uid| !identifiers.insert(uid))
            })
        {
            return Err(operation_error(
                python,
                "invalid_snapshot",
                "snapshot requires at most 1000 unique object IDs",
            ));
        }
    }
    if before.get("generation") != after.get("generation") {
        return Err(operation_error(
            python,
            "snapshot_epoch_mismatch",
            "snapshots belong to different document epochs",
        ));
    }
    let index = |snapshot: &Value| -> BTreeMap<u64, Value> {
        snapshot["objects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|object| object["uid"].as_u64().map(|uid| (uid, object.clone())))
            .collect()
    };
    let previous = index(before);
    let current = index(after);
    let added: Vec<_> = current
        .iter()
        .filter(|(uid, _)| !previous.contains_key(uid))
        .map(|(_, value)| value.clone())
        .collect();
    let removed: Vec<_> = previous
        .iter()
        .filter(|(uid, _)| !current.contains_key(uid))
        .map(|(_, value)| value.clone())
        .collect();
    let changed: Vec<_> = current
        .iter()
        .filter_map(|(uid, value)| {
            previous
                .get(uid)
                .filter(|old| *old != value)
                .map(|old| json!({"uid": uid, "before": old, "after": value}))
        })
        .collect();
    let settings: Vec<_> = ["scene", "frame", "render"]
        .into_iter()
        .filter(|key| before[*key] != after[*key])
        .map(|key| json!({"field": key, "before": before[key], "after": after[key]}))
        .collect();
    Ok(
        json!({"added": added, "removed": removed, "changed": changed, "settings": settings,
        "unchanged": added.is_empty() && removed.is_empty() && changed.is_empty() && settings.is_empty()}),
    )
}

#[derive(Default)]
struct Bounds {
    minimum: Option<[f64; 3]>,
    maximum: Option<[f64; 3]>,
}
impl Bounds {
    fn extend(&mut self, object: &Bound<'_, PyAny>) -> PyResult<()> {
        // Empty objects/cameras/lights do not have meaningful geometric bounds.
        let object_type = object.getattr("type")?.extract::<String>()?;
        if !matches!(
            object_type.as_str(),
            "MESH" | "CURVE" | "SURFACE" | "META" | "FONT" | "VOLUME"
        ) {
            return Ok(());
        }
        // Evaluated empty meshes can report a zero box rather than the RNA
        // sentinel. Inspect evaluated vertices so modifier-created geometry
        // remains eligible even when the original mesh was empty.
        if object_type == "MESH" && object.getattr("data")?.getattr("vertices")?.len()? == 0 {
            return Ok(());
        }
        let corners = object
            .getattr("bound_box")?
            .try_iter()?
            .take(8)
            .map(|corner| corner?.extract::<[f64; 3]>())
            .collect::<PyResult<Vec<_>>>()?;
        // Blender encodes unavailable bounds with an exact all-minus-one sentinel.
        if corners.is_empty()
            || corners
                .iter()
                .flatten()
                .all(|coordinate| coordinate.to_bits() == (-1.0_f64).to_bits())
        {
            return Ok(());
        }
        let vector = object.py().import("mathutils")?.getattr("Vector")?;
        let matrix = object.getattr("matrix_world")?;
        for corner in corners {
            let world = matrix.call_method1("__matmul__", (vector.call1((corner,))?,))?;
            let point = [
                world.get_item(0)?.extract::<f64>()?,
                world.get_item(1)?.extract::<f64>()?,
                world.get_item(2)?.extract::<f64>()?,
            ];
            let minimum = self.minimum.get_or_insert(point);
            let maximum = self.maximum.get_or_insert(point);
            for axis in 0..3 {
                minimum[axis] = minimum[axis].min(point[axis]);
                maximum[axis] = maximum[axis].max(point[axis]);
            }
        }
        Ok(())
    }
    fn json(&self) -> Value {
        self.minimum.map_or(
            Value::Null,
            |minimum| json!({"minimum": minimum, "maximum": self.maximum}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_diff_preserves_float_identity_across_json_round_trips() {
        // Blender's default Light has this f32 rotation. Its promoted f64 is
        // 1.8663908243179321; best-effort JSON parsing can lose one ULP.
        let rotation = f64::from(1.866_390_8_f32);
        let snapshot = json!({
            "schema_version": 1, "generation": 1, "truncated": false,
            "objects": [{"uid": 117, "rotation_euler": [rotation]}],
        });
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        let round_tripped: Value = serde_json::from_slice(&encoded).unwrap();
        Python::initialize();
        Python::attach(|python| {
            assert_eq!(
                diff(python, &round_tripped, &snapshot).unwrap()["unchanged"],
                true,
            );
            let mut changed = snapshot.clone();
            changed["objects"][0]["rotation_euler"][0] = json!(rotation.next_up());
            let difference = diff(python, &snapshot, &changed).unwrap();
            assert_eq!(difference["changed"].as_array().unwrap().len(), 1);
            assert_eq!(difference["unchanged"], false);
        });
    }
}
