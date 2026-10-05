"""Run with Blender --background --factory-startup --python-exit-code 1 --python.

BLENDER_MCP_EXTENSION_DIR must contain the built native module. These tests use
real Blender RNA objects and call the same dispatcher as the bridge.
"""

import importlib
import base64
import hashlib
import itertools
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import bpy

sys.path.insert(0, os.environ["BLENDER_MCP_EXTENSION_DIR"])
native = importlib.import_module("scheme_blender_mcp_native")


class NativeRnaTests(unittest.TestCase):
    def setUp(self):
        self.operations = native.BlenderOperations()
        self.data = self.execute("data_ref")
        self.context = self.execute("context_ref")

    def tearDown(self):
        self.operations.close()
        self.operations.close()

    def execute(self, operation, **kwargs):
        result, reports = self.operations.execute({"operation": operation, **kwargs})
        self.assertEqual(reports, [])
        return result

    def get(self, target, attribute):
        return self.execute(
            "rna_get", reference=target["$rna_ref"], attribute=attribute
        )

    def call(self, target, function, *args, **kwargs):
        return self.execute(
            "rna_call",
            reference=target["$rna_ref"],
            function=function,
            args=list(args),
            kwargs=kwargs,
        )

    def describe(self, target):
        return self.execute("rna_describe", reference=target["$rna_ref"])

    def items(self, target, **kwargs):
        self.assertIsInstance(target, dict, "RNA collections must remain handles")
        return self.execute("rna_items", reference=target["$rna_ref"], **kwargs)[
            "items"
        ]

    def set_value(self, target, attribute, value):
        return self.execute(
            "rna_set", reference=target["$rna_ref"], attribute=attribute, value=value
        )

    def assert_denied(self, target, function):
        with self.assertRaises(native.OperationError) as raised:
            self.call(target, function)
        self.assertEqual(raised.exception.code, "access_denied")

    def assert_code(self, code, operation, **kwargs):
        with self.assertRaises(native.OperationError) as raised:
            self.execute(operation, **kwargs)
        self.assertEqual(raised.exception.code, code)

    def test_reference_identity_release_and_epoch(self):
        self.assertEqual(self.data, self.execute("data_ref"))
        objects = self.get(self.data, "objects")
        self.assertEqual(objects, self.get(self.data, "objects"))
        cube = self.call(objects, "get", "Cube")
        self.assertEqual(cube, self.call(objects, "get", "Cube"))
        self.execute("reference_release", references=[self.data["$rna_ref"]])
        self.assertEqual(self.get(cube, "name"), "Cube")
        self.assert_code("stale_reference", "rna_get", reference=self.data["$rna_ref"], attribute="objects")
        previous = self.operations.generation
        self.operations.invalidate_references()
        self.assertNotEqual(previous, self.operations.generation)
        self.assertEqual(self.execute("reference_stats")["count"], 0)
        self.assert_code("stale_reference", "rna_get", reference=cube["$rna_ref"], attribute="name")
        another = native.BlenderOperations()
        try:
            fresh = self.execute("context_ref")
            with self.assertRaises(native.OperationError) as raised:
                another.execute({"operation": "rna_get", "reference": fresh["$rna_ref"], "attribute": "scene"})
            self.assertEqual(raised.exception.code, "stale_reference")
        finally:
            another.close()

    def test_reference_capacity_and_removed_or_renamed_owners(self):
        # A full store evicts its least recently used handle rather than refusing new
        # work; the evicted handle then reports stale_reference, like any other.
        limited = native.BlenderOperations(reference_capacity=2)
        try:
            data = limited.execute({"operation": "data_ref"})[0]
            context = limited.execute({"operation": "context_ref"})[0]
            limited.execute({"operation": "rna_get", "reference": context["$rna_ref"], "attribute": "scene"})
            with self.assertRaises(native.OperationError) as raised:
                limited.execute({"operation": "rna_get", "reference": data["$rna_ref"], "attribute": "objects"})
            self.assertEqual(raised.exception.code, "stale_reference")
            self.assertIn("evicted", str(raised.exception))
            # One result that alone needs more handles than the store holds still fails,
            # rather than evicting its own handles before they are returned.
            objects = limited.execute({"operation": "rna_get", "reference": limited.execute({"operation": "data_ref"})[0]["$rna_ref"], "attribute": "objects"})[0]
            bpy.data.objects.new("MCP capacity a", None)
            bpy.data.objects.new("MCP capacity b", None)
            with self.assertRaises(native.OperationError) as raised:
                limited.execute({"operation": "rna_items", "reference": objects["$rna_ref"], "limit": 3})
            self.assertEqual(raised.exception.code, "reference_limit")
        finally:
            for name in ("MCP capacity a", "MCP capacity b"):
                if name in bpy.data.objects:
                    bpy.data.objects.remove(bpy.data.objects[name])
            limited.close()
        objects = self.get(self.data, "objects")
        obj = self.call(objects, "new", "MCP reference test", None)
        actual = bpy.data.objects[self.get(obj, "name")]
        actual.name = "MCP renamed owner"
        self.assertEqual(self.get(obj, "name"), actual.name)
        bpy.data.objects.remove(actual)
        replacement = bpy.data.objects.new("MCP renamed owner", None)
        try:
            self.assert_code("stale_reference", "rna_get", reference=obj["$rna_ref"], attribute="name")
        finally:
            bpy.data.objects.remove(replacement)

    def test_subtype_changes_require_a_fresh_reference(self):
        light = bpy.data.lights.new("MCP subtype test", "POINT")
        try:
            lights = self.get(self.data, "lights")
            previous = self.call(lights, "get", light.name)
            self.set_value(previous, "type", "AREA")
            self.assert_code("stale_reference", "rna_get", reference=previous["$rna_ref"], attribute="size")
            fresh = self.call(lights, "get", light.name)
            self.assertNotEqual(previous, fresh)
            self.assertGreater(self.get(fresh, "size"), 0)
        finally:
            bpy.data.lights.remove(light)

    def test_same_named_linked_ids_use_durable_owner_identity(self):
        original = bpy.data.objects.new("MCP same-name linked object", None)
        linked = []
        libraries = []
        try:
            with tempfile.TemporaryDirectory(prefix="blender-mcp-linked-ids-test-") as directory:
                for number in range(2):
                    path = str(Path(directory) / f"library-{number}.blend")
                    bpy.data.libraries.write(path, {original})
                    with bpy.data.libraries.load(path, link=True) as (_available, loaded):
                        loaded.objects = [original.name]
                    linked.append(loaded.objects[0])
                    libraries.append(loaded.objects[0].library)
                self.assertEqual([obj.name for obj in linked], [original.name, original.name])
                wanted = {obj.session_uid for obj in [original, *linked]}
                objects = self.get(self.data, "objects")
                references = {
                    self.get(reference, "session_uid"): reference
                    for reference in self.items(objects)
                }
                self.assertTrue(wanted.issubset(references))
                self.operations.invalidate_subdata()
                for uid in wanted:
                    self.assertEqual(self.get(references[uid], "session_uid"), uid)
                removed = linked.pop()
                removed_uid = removed.session_uid
                bpy.data.objects.remove(removed, do_unlink=True)
                self.assert_code("stale_reference", "rna_get", reference=references[removed_uid]["$rna_ref"], attribute="name")
                self.assertEqual(self.get(references[linked[0].session_uid], "name"), original.name)
        finally:
            for obj in linked:
                bpy.data.objects.remove(obj, do_unlink=True)
            for library in libraries:
                bpy.data.libraries.remove(library)
            bpy.data.objects.remove(original)

    def test_nested_rna_is_resolved_without_retaining_removed_members(self):
        material = bpy.data.materials.new("MCP node reference test")
        material.use_nodes = True
        try:
            materials = self.get(self.data, "materials")
            reference = self.call(materials, "get", material.name)
            nodes = self.get(self.get(reference, "node_tree"), "nodes")
            node = self.call(nodes, "new", "ShaderNodeValue")
            actual = material.node_tree.nodes[self.get(node, "name")]
            actual.name = "MCP renamed node"
            self.assertEqual(self.get(node, "name"), actual.name)
            material.node_tree.nodes.remove(actual)
            self.assert_code("stale_reference", "rna_get", reference=node["$rna_ref"], attribute="name")
            self.operations.invalidate_subdata()
            for _ in range(20):
                node = self.call(nodes, "new", "ShaderNodeValue")
                name = self.get(node, "name")
                self.call(nodes, "remove", node)
                replacement = self.call(nodes, "new", "ShaderNodeValue")
                self.set_value(replacement, "name", name)
                self.assertNotEqual(node, replacement)
                self.assert_code("stale_reference", "rna_get", reference=node["$rna_ref"], attribute="name")
                self.call(nodes, "remove", replacement)
        finally:
            bpy.data.materials.remove(material)

    def test_modifier_survives_deferred_geometry_update(self):
        cube = self.call(self.get(self.data, "objects"), "get", "Cube")
        modifiers = self.get(cube, "modifiers")
        modifier = self.call(modifiers, "new", "Tailored surface", "SUBSURF")
        try:
            # A live depsgraph callback can arrive between new() and rna_set.
            bpy.context.view_layer.update()
            self.operations.invalidate_subdata()
            self.set_value(modifier, "levels", 1)
            self.assertEqual(self.get(modifier, "levels"), 1)
            self.assertEqual(modifier, self.call(modifiers, "get", "Tailored surface"))
            self.assertIn(modifier, self.items(modifiers))
            self.set_value(modifier, "name", "Renamed tailored surface")
            self.operations.invalidate_subdata()
            self.assertEqual(self.get(modifier, "name"), "Renamed tailored surface")
            actual = bpy.data.objects["Cube"].modifiers["Renamed tailored surface"]
            bpy.data.objects["Cube"].modifiers.remove(actual)
            replacement = self.call(modifiers, "new", "Renamed tailored surface", "SUBSURF")
            self.assertNotEqual(modifier, replacement)
            self.assert_code("stale_reference", "rna_get", reference=modifier["$rna_ref"], attribute="levels")
            self.call(modifiers, "remove", replacement)
            self.assert_code("stale_reference", "rna_get", reference=replacement["$rna_ref"], attribute="levels")
            for _ in range(20):
                fresh = self.call(modifiers, "new", "Renamed tailored surface", "SUBSURF")
                self.assertNotEqual(replacement, fresh)
                self.assert_code("stale_reference", "rna_get", reference=replacement["$rna_ref"], attribute="levels")
                self.call(modifiers, "remove", fresh)
                replacement = fresh
        finally:
            for name in ("Tailored surface", "Renamed tailored surface"):
                actual = bpy.data.objects["Cube"].modifiers.get(name)
                if actual is not None:
                    bpy.data.objects["Cube"].modifiers.remove(actual)

    def test_edit_mode_expires_mesh_elements_but_preserves_ids(self):
        cube = self.call(self.get(self.data, "objects"), "get", "Cube")
        mesh = self.get(cube, "data")
        vertices = self.get(mesh, "vertices")
        vertex = self.items(vertices, limit=1)[0]
        previous_active = bpy.context.view_layer.objects.active
        previous_selected = list(bpy.context.selected_objects)
        for obj in previous_selected:
            obj.select_set(False)
        bpy.data.objects["Cube"].select_set(True)
        bpy.context.view_layer.objects.active = bpy.data.objects["Cube"]
        try:
            self.execute("operator_call", idname="object.mode_set", kwargs={"mode": "EDIT"})
            self.assert_code("stale_reference", "rna_get", reference=vertex["$rna_ref"], attribute="co")
            self.assertEqual(self.get(mesh, "name"), bpy.data.objects["Cube"].data.name)
            self.execute("operator_call", idname="object.mode_set", kwargs={"mode": "OBJECT"})
            self.assertNotEqual(vertex, self.items(vertices, limit=1)[0])
        finally:
            if bpy.context.object and bpy.context.object.mode != "OBJECT":
                bpy.ops.object.mode_set(mode="OBJECT")
            bpy.data.objects["Cube"].select_set(False)
            for obj in previous_selected:
                obj.select_set(True)
            bpy.context.view_layer.objects.active = previous_active

    def test_serialization_stops_at_item_depth_and_byte_limits(self):
        cycle = []
        cycle.append(cycle)
        examples = [lambda: list(range(5000)), lambda: cycle, lambda: "x" * (9 * 1024 * 1024), lambda: itertools.count()]
        for factory in examples:
            bpy.types.Scene.mcp_test_large = property(lambda self, make=factory: make())
            try:
                scene = self.get(self.context, "scene")
                self.assert_code("serialization_limit", "rna_get", reference=scene["$rna_ref"], attribute="mcp_test_large")
            finally:
                del bpy.types.Scene.mcp_test_large

    def test_paging_is_exact_and_detects_collection_changes(self):
        objects = self.get(self.data, "objects")
        total = len(bpy.data.objects)
        page = self.execute("rna_items", reference=objects["$rna_ref"], offset=0, limit=total)
        self.assertFalse(page["has_more"])
        self.assertEqual(page["total"], total)
        self.assert_code("invalid_arguments", "rna_items", reference=objects["$rna_ref"], limit=0)
        added = bpy.data.objects.new("MCP page mutation", None)
        try:
            self.assert_code("collection_changed", "rna_items", reference=objects["$rna_ref"], offset=1, limit=1, expected_revision=page["collection_revision"])
        finally:
            bpy.data.objects.remove(added)

    def test_targeted_metadata_and_batch_stop(self):
        cube = self.call(self.get(self.data, "objects"), "get", "Cube")
        property_info = self.execute("rna_property_info", reference=cube["$rna_ref"], attribute="location")
        self.assertEqual(property_info["identifier"], "location")
        self.assertEqual(property_info["array_length"], 3)
        self.assertTrue(property_info["animatable"])
        info = self.execute("rna_function_info", reference=cube["$rna_ref"], function="keyframe_insert")
        self.assertTrue(info["callable"] and info["inherited"])
        self.assert_code("access_denied", "rna_function_info", reference=cube["$rna_ref"], function="as_pointer")
        previous = self.get(cube, "location")
        request = {"operation": "rna_set", "reference": cube["$rna_ref"], "attribute": "location", "value": [1, 2, 3]}
        try:
            self.assert_code("invalid_arguments", "batch", requests=[request, {"operation": "shutdown"}])
            self.assertEqual(self.get(cube, "location"), previous)
            result = self.execute("batch", requests=[request, {"operation": "rna_call", "reference": cube["$rna_ref"], "function": "__repr__"}, {**request, "value": [9, 9, 9]}])
            self.assertEqual(result["completed"], 1)
            self.assertEqual(result["failed_index"], 1)
            self.assertFalse(result["complete"])
            self.assertEqual(self.get(cube, "location"), [1.0, 2.0, 3.0])
        finally:
            self.set_value(cube, "location", previous)

    def test_snapshot_diff_summary_and_checkpoint(self):
        before = self.execute("scene_snapshot")
        self.assertFalse(before["truncated"])
        obj = bpy.data.objects.new("MCP snapshot test", None)
        bpy.context.scene.collection.objects.link(obj)
        try:
            obj.location = (1, 2, 3)
            after = self.execute("scene_snapshot")
            difference = self.execute("scene_diff", before=before, after=after)
            self.assertEqual([item["name"] for item in difference["added"]], [obj.name])
            self.assertTrue(self.execute("scene_diff", before=after, after=after)["unchanged"])
            incomplete = self.execute("scene_snapshot", limit=1)
            self.assert_code("invalid_snapshot", "scene_diff", before=incomplete, after=after)
            summary = self.execute("scene_summary")
            self.assertIn("selected_objects", summary)
            self.assertIsNotNone(summary["world_bounds"])
            epoch = self.operations.generation
            active_path = bpy.data.filepath
            with tempfile.TemporaryDirectory(prefix="blender-mcp-checkpoint-test-") as directory:
                result = self.execute("checkpoint", filepath=str(Path(directory) / "checkpoint.blend"))
                self.assertTrue(Path(result["path"]).is_file())
                self.assertEqual(bpy.data.filepath, active_path)
                self.assertEqual(self.operations.generation, epoch)
                self.assertEqual(self.get(self.get(self.context, "scene"), "name"), bpy.context.scene.name)
                self.assert_code("checkpoint_exists", "checkpoint", filepath=result["path"])
        finally:
            bpy.data.objects.remove(obj, do_unlink=True)

    def test_summary_evaluates_geometry_and_bounds_material_discovery(self):
        mesh = bpy.data.objects["Cube"].data.copy()
        obj = bpy.data.objects.new("MCP evaluated bounds test", mesh)
        empty_mesh = bpy.data.meshes.new("MCP empty bounds mesh")
        empty = bpy.data.objects.new("MCP unavailable bounds test", empty_mesh)
        materials = []
        try:
            bpy.context.scene.collection.objects.link(obj)
            bpy.context.scene.collection.objects.link(empty)
            obj.location = (100.0, 0.0, 0.0)
            empty.location = (1000.0, 0.0, 0.0)
            modifier = obj.modifiers.new("MCP bounds array", "ARRAY")
            modifier.count = 3
            modifier.use_relative_offset = False
            modifier.use_constant_offset = True
            modifier.constant_offset_displace = (10.0, 0.0, 0.0)
            materials = [bpy.data.materials.new(f"MCP summary material {index}") for index in range(129)]
            summary = self.execute("scene_summary")
            self.assertAlmostEqual(summary["world_bounds"]["maximum"][0], 121.0)
            self.assertFalse(summary["bounds_truncated"])
            self.assertEqual(summary["bounds_total_objects"], len(bpy.context.view_layer.objects))
            self.assertEqual(summary["materials"]["total"], len(bpy.data.materials))
            self.assertEqual(len(summary["materials"]["names"]), 128)
            self.assertTrue(summary["materials"]["truncated"])
            render = bpy.context.scene.render
            for attribute in ["engine", "resolution_x", "resolution_y", "resolution_percentage"]:
                self.assertEqual(summary["render"][attribute], getattr(render, attribute))
            self.assertEqual(summary["render"]["file_format"], render.image_settings.file_format)
        finally:
            bpy.data.objects.remove(obj, do_unlink=True)
            bpy.data.objects.remove(empty, do_unlink=True)
            bpy.data.meshes.remove(mesh)
            bpy.data.meshes.remove(empty_mesh)
            for material in materials:
                bpy.data.materials.remove(material)

    def test_snapshot_detects_axis_angle_and_delta_transform_changes(self):
        obj = bpy.data.objects.new("MCP transform snapshot test", None)
        bpy.context.scene.collection.objects.link(obj)
        obj.rotation_mode = "AXIS_ANGLE"
        try:
            for attribute, changed in [
                ("rotation_axis_angle", (0.5, 0.0, 0.0, 1.0)),
                ("delta_location", (1.0, 2.0, 3.0)),
                ("delta_scale", (2.0, 3.0, 4.0)),
                ("delta_rotation_euler", (0.25, 0.5, 0.75)),
                ("delta_rotation_quaternion", (0.0, 1.0, 0.0, 0.0)),
            ]:
                with self.subTest(attribute=attribute):
                    before = self.execute("scene_snapshot")
                    setattr(obj, attribute, changed)
                    after = self.execute("scene_snapshot")
                    difference = self.execute("scene_diff", before=before, after=after)
                    self.assertFalse(difference["unchanged"])
                    self.assertEqual([item["uid"] for item in difference["changed"]], [obj.session_uid])
                    self.assertNotEqual(
                        difference["changed"][0]["before"][attribute],
                        difference["changed"][0]["after"][attribute],
                    )
        finally:
            bpy.data.objects.remove(obj, do_unlink=True)

    def test_live_collections_and_value_arrays(self):
        objects = self.get(self.data, "objects")
        self.assertEqual(len(self.items(objects)), len(bpy.data.objects))
        cube = self.call(objects, "get", "Cube")
        self.assertEqual(self.get(cube, "name"), "Cube")
        self.assertEqual(
            self.get(cube, "location"), list(bpy.data.objects["Cube"].location)
        )
        self.assertEqual(
            self.get(cube, "lock_location"),
            list(bpy.data.objects["Cube"].lock_location),
        )
        functions = self.describe(objects)["functions"]
        self.assertIn("get", functions)
        self.assertNotIn("keyframe_insert", functions)
        self.assertTrue(
            all(callable(getattr(bpy.data.objects, name)) for name in functions)
        )
        self.assert_denied(objects, "keyframe_insert")

        screens = self.get(self.data, "screens")
        screen = self.items(screens, limit=1)[0]
        areas = self.get(screen, "areas")
        actual = bpy.data.screens[self.get(screen, "name")].areas
        self.assertGreater(len(actual), 0)
        self.assertEqual(
            [self.get(area, "type") for area in self.items(areas)],
            [area.type for area in actual],
        )
        self.assertEqual(
            len(self.items(areas, offset=1, limit=1)), min(1, len(actual) - 1)
        )
        self.assertNotIn("new", self.describe(areas)["functions"])
        self.assert_denied(areas, "new")

    def test_property_group_collection_subclass(self):
        class NativeTestItem(bpy.types.PropertyGroup):
            pass

        bpy.utils.register_class(NativeTestItem)
        bpy.types.Scene.mcp_test_items = bpy.props.CollectionProperty(
            type=NativeTestItem
        )
        try:
            items = bpy.context.scene.mcp_test_items
            self.assertIsInstance(items, bpy.types.bpy_prop_collection_idprop)
            items.add().name = "first"
            scene = self.get(self.context, "scene")
            reference = self.get(scene, "mcp_test_items")
            self.assertEqual(self.get(self.items(reference)[0], "name"), "first")
            self.assertEqual(
                self.get(self.call(reference, "get", "first"), "name"), "first"
            )
            self.assertIn("clear", self.describe(reference)["functions"])
            self.call(reference, "clear")
            self.assertEqual(self.items(reference), [])
            self.assertEqual(len(items), 0)
        finally:
            del bpy.types.Scene.mcp_test_items
            bpy.utils.unregister_class(NativeTestItem)

    def test_mesh_from_data_builds_a_linked_object(self):
        before = set(bpy.data.meshes)
        prism = self.execute(
            "mesh_from_data",
            name="MCP prism",
            vertices=[[0, 0, 0], [1, 0, 0], [0, 1, 0], [0, 0, 1]],
            faces=[[0, 2, 1], [0, 1, 3], [1, 2, 3], [0, 3, 2]],
        )
        actual = bpy.data.objects[self.get(prism, "name")]
        try:
            self.assertEqual(len(actual.data.vertices), 4)
            self.assertEqual(len(actual.data.polygons), 4)
            self.assertIn(actual.name, bpy.context.scene.collection.objects)
        finally:
            mesh = actual.data
            bpy.data.objects.remove(actual)
            bpy.data.meshes.remove(mesh)

        collection = bpy.data.collections.new("MCP build target")
        bpy.context.scene.collection.children.link(collection)
        try:
            target = self.call(self.get(self.data, "collections"), "get", collection.name)
            placed = self.execute(
                "mesh_from_data",
                name="MCP placed",
                vertices=[[0, 0, 0], [1, 0, 0], [0, 1, 0]],
                faces=[[0, 1, 2]],
                collection=target["$rna_ref"],
            )
            self.assertIn(self.get(placed, "name"), collection.objects)
            self.assertNotIn(self.get(placed, "name"), bpy.context.scene.collection.objects)
            for obj in list(collection.objects):
                mesh = obj.data
                bpy.data.objects.remove(obj)
                bpy.data.meshes.remove(mesh)
        finally:
            bpy.data.collections.remove(collection)

        # Rejected before Blender reads the indices, and no datablock is left behind.
        for faces in ([[0, 1, 9]], [[0, 1]]):
            self.assert_code(
                "invalid_arguments",
                "mesh_from_data",
                name="MCP bad",
                vertices=[[0, 0, 0], [1, 0, 0], [0, 1, 0]],
                faces=faces,
            )
        self.assert_code(
            "invalid_arguments",
            "mesh_from_data",
            name="MCP bad",
            vertices=[[0, 0, 0]],
            collection=self.get(self.data, "objects")["$rna_ref"],
        )
        self.assertEqual(set(bpy.data.meshes), before)

    def test_matrix_writes_take_the_rows_that_reads_return(self):
        # Blender assigns a nested list to a matrix column by column, so writing back
        # what rna_get returned used to transpose it: the translation landed in the
        # bottom row and the object was thrown across the scene.
        actual = bpy.data.objects.new("MCP matrix", None)
        bpy.context.scene.collection.objects.link(actual)
        try:
            obj = self.call(self.get(self.data, "objects"), "get", actual.name)
            rows = [[0.0, -1.0, 0.0, 5.0], [1.0, 0.0, 0.0, -2.0], [0.0, 0.0, 1.0, 3.0], [0.0, 0.0, 0.0, 1.0]]
            self.set_value(obj, "matrix_world", rows)
            self.assertEqual(tuple(actual.location), (5.0, -2.0, 3.0))
            self.assertEqual(self.get(obj, "matrix_world"), rows)
            self.set_value(obj, "matrix_parent_inverse", rows)
            self.assertEqual(self.get(obj, "matrix_parent_inverse"), rows)
            # Plain arrays that are not matrices keep their ordinary meaning.
            self.set_value(obj, "scale", [1.0, 2.0, 3.0])
            self.assertEqual(tuple(actual.scale), (1.0, 2.0, 3.0))
        finally:
            bpy.data.objects.remove(actual)

    def test_handles_are_durable_unless_array_backed_and_say_why_they_retire(self):
        # Settings structs embedded in an ID never move; only array-backed elements
        # (mesh vertices, spline points, bones, ...) can be reallocated in place.
        scene = self.get(self.context, "scene")
        render = self.get(scene, "render")
        camera = bpy.data.cameras.new("MCP durable camera")
        mesh = bpy.data.meshes.new("MCP volatile mesh")
        mesh.from_pydata([(0, 0, 0), (1, 0, 0), (0, 1, 0)], [], [(0, 1, 2)])
        curve = bpy.data.curves.new("MCP volatile curve", "CURVE")
        curve.splines.new("POLY")
        armature = bpy.data.armatures.new("MCP volatile armature")
        try:
            dof = self.get(self.call(self.get(self.data, "cameras"), "get", camera.name), "dof")
            vertex = self.items(self.get(self.call(self.get(self.data, "meshes"), "get", mesh.name), "vertices"))[0]
            points = self.get(self.items(self.get(self.call(self.get(self.data, "curves"), "get", curve.name), "splines"))[0], "points")
            point = self.items(points)[0]

            self.operations.invalidate_subdata("a test geometry update")

            self.set_value(render, "resolution_percentage", 37)
            self.assertEqual(bpy.context.scene.render.resolution_percentage, 37)
            self.set_value(dof, "use_dof", True)
            self.assertTrue(camera.dof.use_dof)
            with self.assertRaises(native.OperationError) as raised:
                self.get(vertex, "co")
            self.assertEqual(raised.exception.code, "stale_reference")
            self.assertIn("MeshVertex handle was retired after a test geometry update", str(raised.exception))

            # Adding to an array-backed collection retires its members, and says so.
            point = self.items(points)[0]
            self.call(points, "add", 2)
            with self.assertRaises(native.OperationError) as raised:
                self.get(point, "co")
            self.assertIn("retired by `add` on its collection", str(raised.exception))

            # Operators name themselves as the cause.
            vertex = self.items(self.get(self.call(self.get(self.data, "meshes"), "get", mesh.name), "vertices"))[0]
            self.execute("operator_call", idname="object.select_all", kwargs={"action": "DESELECT"})
            with self.assertRaises(native.OperationError) as raised:
                self.get(vertex, "co")
            self.assertIn("after operator object.select_all", str(raised.exception))
            self.set_value(render, "resolution_percentage", 100)
        finally:
            bpy.data.cameras.remove(camera)
            bpy.data.meshes.remove(mesh)
            bpy.data.curves.remove(curve)
            bpy.data.armatures.remove(armature)

    def test_node_handles_survive_subdata_invalidation(self):
        # Editing a node fires a shading depsgraph update, which marks sub-data dirty.
        # Node-tree data is individually allocated, so its handles must survive that;
        # mesh elements are array-backed and must still be retired.
        material = bpy.data.materials.new("MCP durable nodes")
        mesh = bpy.data.meshes.new("MCP volatile verts")
        mesh.from_pydata([(0, 0, 0), (1, 0, 0), (0, 1, 0)], [], [(0, 1, 2)])
        try:
            material.use_nodes = True
            handle = self.call(self.get(self.data, "materials"), "get", material.name)
            tree = self.get(handle, "node_tree")
            node = self.call(self.get(tree, "nodes"), "new", "ShaderNodeTexNoise")
            scale = self.call(self.get(node, "inputs"), "get", "Scale")
            link = self.call(
                self.get(tree, "links"),
                "new",
                self.call(self.get(node, "outputs"), "get", "Fac"),
                self.call(
                    self.get(self.call(self.get(tree, "nodes"), "get", "Principled BSDF"), "inputs"),
                    "get",
                    "Roughness",
                ),
            )
            mesh_handle = self.call(self.get(self.data, "meshes"), "get", mesh.name)
            vertex = self.items(self.get(mesh_handle, "vertices"))[0]

            self.operations.invalidate_subdata()

            self.set_value(scale, "default_value", 7.5)
            self.assertEqual(self.get(node, "bl_idname"), "ShaderNodeTexNoise")
            self.assertEqual(self.get(self.get(link, "from_node"), "name"), self.get(node, "name"))
            self.assertAlmostEqual(material.node_tree.nodes[self.get(node, "name")].inputs["Scale"].default_value, 7.5)
            self.assert_code("stale_reference", "rna_get", reference=vertex["$rna_ref"], attribute="co")

            # Removing one node retires every handle to it -- including the alias reached
            # through the link and its sockets -- but leaves its siblings usable, so a
            # loop that removes nodes from a listed collection can keep going.
            sibling = self.call(self.get(tree, "nodes"), "new", "ShaderNodeMath")
            alias = self.get(link, "from_node")
            self.call(self.get(tree, "nodes"), "remove", node)
            for retired in (node, alias, scale):
                self.assert_code("stale_reference", "rna_get", reference=retired["$rna_ref"], attribute="name")
            self.assertEqual(self.get(sibling, "bl_idname"), "ShaderNodeMath")
            # A failed removal retires nothing.
            other = bpy.data.node_groups.new("MCP other tree", "ShaderNodeTree")
            try:
                with self.assertRaises(Exception):
                    self.call(self.get(self.call(self.get(self.data, "node_groups"), "get", other.name), "nodes"), "remove", sibling)
                self.assertEqual(self.get(sibling, "bl_idname"), "ShaderNodeMath")
            finally:
                bpy.data.node_groups.remove(other)

            # Array-backed layers can shift on removal, so their siblings are retired.
            mesh.uv_layers.new(name="first")
            mesh.uv_layers.new(name="second")
            layers = self.get(mesh_handle, "uv_layers")
            first, second = self.items(layers)
            self.call(layers, "remove", first)
            self.assert_code("stale_reference", "rna_get", reference=second["$rna_ref"], attribute="name")
        finally:
            bpy.data.materials.remove(material)
            bpy.data.meshes.remove(mesh)

    def test_custom_properties_round_trip_and_guard_rna(self):
        actual = bpy.data.objects.new("MCP custom props", None)
        try:
            obj = self.call(self.get(self.data, "objects"), "get", actual.name)
            ref = obj["$rna_ref"]
            self.execute("id_property_set", reference=ref, key="role", value="walkable_ground")
            self.execute("id_property_set", reference=ref, key="spawn", value={"weight": 2, "tags": ["a", "b"]})
            self.assertEqual(actual["role"], "walkable_ground")
            self.assertEqual(self.execute("id_property_get", reference=ref, key="role"), "walkable_ground")
            self.assertEqual(
                self.execute("id_property_get", reference=ref, key="spawn"),
                {"weight": 2, "tags": ["a", "b"]},
            )
            self.assertEqual(self.execute("id_property_keys", reference=ref), ["role", "spawn"])
            self.assertIsNone(self.execute("id_property_get", reference=ref, key="missing"))
            self.assertEqual(self.execute("id_property_delete", reference=ref, key="spawn"), {"deleted": True})
            self.assertEqual(self.execute("id_property_delete", reference=ref, key="spawn"), {"deleted": False})
            self.assertNotIn("spawn", actual.keys())

            # Registered RNA properties share this storage; they must go through rna_set.
            bpy.types.Object.mcp_registered = bpy.props.IntProperty()
            try:
                self.assert_code("access_denied", "id_property_set", reference=ref, key="mcp_registered", value=3)
            finally:
                del bpy.types.Object.mcp_registered
            with self.assertRaises(native.OperationError) as raised:
                self.call(obj, "__setitem__", "role", "x")
            self.assertEqual(raised.exception.code, "access_denied")
            self.assertIn("prop-set!", str(raised.exception))
            for key in ("", "_RNA_UI", "x" * 64):
                self.assert_code("invalid_arguments", "id_property_set", reference=ref, key=key, value=1)
            self.assert_code("invalid_arguments", "id_property_set", reference=ref, key="role", value=None)

            # A batch can tag many objects in one bridge request.
            self.operations.validate_batch([
                {"operation": "id_property_set", "reference": ref, "key": "role", "value": "prop"},
            ])
        finally:
            bpy.data.objects.remove(actual)

    def test_animation_without_editor_context(self):
        objects = self.get(self.data, "objects")
        obj = self.call(objects, "new", "MCP animation test", None)
        actual = bpy.data.objects[self.get(obj, "name")]
        bpy.context.scene.collection.objects.link(actual)
        try:
            functions = self.describe(obj)["functions"]
            self.assertIn("keyframe_insert", functions)
            self.assertIn("keyframe_delete", functions)
            self.assertIsNone(actual.bl_rna.functions.get("keyframe_insert"))
            for frame, x in [(1, 0.0), (11, 10.0)]:
                self.set_value(obj, "location", [x, 0.0, 0.0])
                self.assertTrue(
                    self.call(obj, "keyframe_insert", "location", index=0, frame=frame)
                )

            action = self.get(self.get(obj, "animation_data"), "action")
            layer = self.items(self.get(action, "layers"))[0]
            strip = self.items(self.get(layer, "strips"))[0]
            bag = self.items(self.get(strip, "channelbags"))[0]
            curve = self.items(self.get(bag, "fcurves"))[0]
            points = self.get(curve, "keyframe_points")
            self.assertEqual(len(self.items(points)), 2)
            for point in self.items(points):
                self.set_value(point, "interpolation", "LINEAR")
            self.assertAlmostEqual(self.call(curve, "evaluate", 6.0), 5.0)
            scene = self.get(self.context, "scene")
            self.call(scene, "frame_set", 6)
            self.assertAlmostEqual(self.get(obj, "location")[0], 5.0)
            keyframe = self.items(points)[0]
            self.assertTrue(
                self.call(obj, "keyframe_delete", "location", index=0, frame=11)
            )
            # Keyframes are array elements and are retired; the collection handle is
            # re-found through its F-curve and sees the deletion.
            self.assert_code("stale_reference", "rna_get", reference=keyframe["$rna_ref"], attribute="co")
            self.assertEqual(len(self.items(points)), 1)
            action = self.get(self.get(obj, "animation_data"), "action")
            layer = self.items(self.get(action, "layers"))[0]
            strip = self.items(self.get(layer, "strips"))[0]
            bag = self.items(self.get(strip, "channelbags"))[0]
            curve = self.items(self.get(bag, "fcurves"))[0]
            self.assertEqual(len(self.items(self.get(curve, "keyframe_points"))), 1)
        finally:
            action = actual.animation_data.action if actual.animation_data else None
            bpy.data.objects.remove(actual, do_unlink=True)
            if action:
                bpy.data.actions.remove(action)

    def test_helpers_require_actual_blender_types(self):
        # A Python object with the same class name and callable names is not RNA.
        fake_type = type(
            "bpy_prop_collection",
            (),
            {"get": lambda self: None, "keyframe_insert": lambda self: True},
        )
        bpy.types.Scene.mcp_test_fake = property(lambda self: fake_type())
        try:
            scene = self.get(self.context, "scene")
            fake = self.get(scene, "mcp_test_fake")
            self.assertEqual(self.describe(fake)["functions"], [])
            for function in ["get", "keyframe_insert", "__repr__"]:
                self.assert_denied(fake, function)
            for function in ["as_pointer", "__repr__", "not_a_function"]:
                self.assert_denied(scene, function)
        finally:
            del bpy.types.Scene.mcp_test_fake

    def test_catalog_revision_without_full_catalog(self):
        self.assertEqual(
            self.operations.catalog_revision, self.operations.catalog["revision"]
        )
        self.assertEqual(
            self.operations.catalog_revision,
            self.operations.refresh_catalog()["revision"],
        )

    def test_catalog_reads_enum_defaults_without_rna_warnings(self):
        # Reading `default` on a flag enum or a dynamic enum makes Blender log "current
        # value ... matches no enum" and yield '' -- about 130 lines on every launch.
        def prop(idname, name):
            return next(
                p for p in native.describe_operator(idname)["properties"] if p["identifier"] == name
            )

        flag = prop("mesh.dissolve_limited", "delimit")
        self.assertTrue(flag["enum_flag"])
        self.assertEqual(flag["default"], ["NORMAL"])
        dynamic = prop("transform.rotate", "orient_type")
        self.assertTrue(dynamic["enum_items_dynamic"])
        self.assertNotIn("default", dynamic)
        self.assertEqual(prop("object.select_by_type", "type")["default"], "MESH")

        # Count what Blender writes to stderr while the whole catalog is built.
        sys.stderr.flush()
        saved = os.dup(2)
        with tempfile.TemporaryFile() as captured:
            os.dup2(captured.fileno(), 2)
            try:
                native.build_catalog()
            finally:
                sys.stderr.flush()
                os.dup2(saved, 2)
                os.close(saved)
            captured.seek(0)
            warnings = [line for line in captured.read().decode(errors="replace").splitlines()
                        if "matches no enum" in line]
        # What remains are Blender operators whose declared default is not one of
        # their own options, which only the read itself can reveal.
        self.assertLessEqual(len(warnings), 12, "\n".join(warnings))

    def test_cancelled_render_does_not_capture_an_existing_output(self):
        render = bpy.context.scene.render
        previous_path = render.filepath
        previous_extension = render.use_file_extension
        try:
            render.use_file_extension = False
            with tempfile.TemporaryDirectory(prefix="blender-mcp-cancelled-render-test-") as directory:
                path = Path(directory) / "previous.png"
                original = b"existing output must not become a successful new render"
                path.write_bytes(original)
                for status, code in [({"CANCELLED"}, "render_cancelled"), ({"RUNNING_MODAL"}, "render_failed")]:
                    with self.subTest(status=status):
                        fake_render = mock.Mock(return_value=status)
                        fake_ops = SimpleNamespace(render=SimpleNamespace(render=fake_render))
                        with mock.patch.object(bpy, "ops", fake_ops):
                            self.assert_code(code, "render", filepath=str(path))
                        fake_render.assert_called_once_with(write_still=True)
                        self.assertEqual(render.filepath, previous_path)
                        self.assertEqual(path.read_bytes(), original)
                        self.assertEqual(self.execute("artifact_release", artifact_ids=[])["count"], 0)
        finally:
            render.filepath = previous_path
            render.use_file_extension = previous_extension

    def test_render_output_extension_and_restored_settings(self):
        scene = bpy.context.scene
        render = scene.render
        previous = {
            "engine": render.engine,
            "filepath": render.filepath,
            "resolution_x": render.resolution_x,
            "resolution_y": render.resolution_y,
            "resolution_percentage": render.resolution_percentage,
            "use_file_extension": render.use_file_extension,
        }
        previous_format = render.image_settings.file_format
        previous_samples = scene.cycles.samples
        previous_denoising = scene.cycles.use_denoising
        previous_device = scene.cycles.device
        previous_camera = scene.camera
        try:
            render.engine = "CYCLES"
            render.resolution_x = render.resolution_y = 8
            render.resolution_percentage = 100
            scene.cycles.samples = 1
            scene.cycles.use_denoising = False
            scene.cycles.device = "CPU"
            with tempfile.TemporaryDirectory(
                prefix="blender-mcp-render-test-"
            ) as directory:
                first_artifact = None
                first_bytes = None
                for image_format, suffix, output_suffix in [
                    ("PNG", "", ".png"),
                    ("PNG", ".PNG", ".PNG"),
                    ("PNG", ".custom", ".custom.png"),
                    ("JPEG", ".png", ".jpg"),
                    ("JPEG", ".jpeg", ".jpeg"),
                    ("TIFF", ".tiff", ".tiff"),
                ]:
                    render.image_settings.file_format = image_format
                    render.use_file_extension = True
                    requested = Path(directory) / (image_format + suffix)
                    result = self.execute(
                        "render", filepath=str(requested), write_still=True
                    )
                    self.assertEqual(
                        result["artifact"]["path"],
                        str(Path(directory) / (image_format + output_suffix)),
                    )
                    self.assertTrue(Path(result["artifact"]["path"]).is_file())
                    self.assertEqual(render.filepath, previous["filepath"])
                    artifact = result["artifact"]
                    fetched = self.execute("artifact", artifact_id=artifact["id"])
                    content = base64.b64decode(fetched["data_base64"])
                    self.assertEqual(hashlib.sha256(content).hexdigest(), artifact["sha256"])
                    self.assertNotIn("data_base64", self.execute("artifact", artifact_id=artifact["id"], include_data=False))
                    if first_artifact is None:
                        first_artifact, first_bytes = artifact, content
                        Path(artifact["path"]).write_bytes(b"original output overwritten")

                render.use_file_extension = False
                requested = Path(directory) / "without-extension"
                result = self.execute(
                    "render", filepath=str(requested), write_still=True
                )
                self.assertEqual(result["artifact"]["path"], str(requested))
                self.assertTrue(requested.is_file())
                self.assertEqual(render.filepath, previous["filepath"])

                old = self.execute("artifact", artifact_id=first_artifact["id"])
                self.assertEqual(base64.b64decode(old["data_base64"]), first_bytes)
                self.execute("artifact_release", artifact_ids=[first_artifact["id"]])
                self.assert_code("artifact_missing", "artifact", artifact_id=first_artifact["id"])

                render.resolution_x, render.resolution_y = 320, 180
                render.resolution_percentage = 75
                image_format = render.image_settings.file_format
                thumbnail = self.execute("thumbnail", filepath=str(Path(directory) / "preview"), max_size=16)["artifact"]
                preview_bytes = base64.b64decode(self.execute("artifact", artifact_id=thumbnail["id"])["data_base64"])
                self.assertEqual(preview_bytes[:8], b"\x89PNG\r\n\x1a\n")
                self.assertEqual(int.from_bytes(preview_bytes[16:20], "big"), 16)
                self.assertLessEqual(int.from_bytes(preview_bytes[20:24], "big"), 16)
                self.assertEqual((render.resolution_x, render.resolution_y, render.resolution_percentage), (320, 180, 75))
                self.assertEqual(render.image_settings.file_format, image_format)
                smallest = self.execute("thumbnail", filepath=str(Path(directory) / "single-pixel"), max_size=1)["artifact"]
                smallest_bytes = base64.b64decode(self.execute("artifact", artifact_id=smallest["id"])["data_base64"])
                self.assertEqual((int.from_bytes(smallest_bytes[16:20], "big"), int.from_bytes(smallest_bytes[20:24], "big")), (1, 1))

                scene.camera = None
                with self.assertRaises(RuntimeError):
                    self.execute("render", filepath=str(Path(directory) / "failure"))
                self.assertEqual(render.filepath, previous["filepath"])
                with self.assertRaises(RuntimeError):
                    self.execute("thumbnail", max_size=16)
                self.assertEqual((render.resolution_x, render.resolution_y, render.resolution_percentage), (320, 180, 75))
                self.assertEqual(render.image_settings.file_format, image_format)
        finally:
            scene.camera = previous_camera
            scene.cycles.samples = previous_samples
            scene.cycles.use_denoising = previous_denoising
            scene.cycles.device = previous_device
            render.image_settings.file_format = previous_format
            for name, value in previous.items():
                setattr(render, name, value)


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
