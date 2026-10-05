# Blender's API through Scheme

How `bpy` maps onto `rna-get`, `rna-set!`, `rna-call`, and `rna-items`. Every behaviour
here was checked against Blender 5.2.1 LTS; the surprises are the point.

## Collections

`rna-get` is `getattr`, so it cannot do what `bpy.data.objects["Cube"]` does. Square
brackets are `__getitem__`, and the way in is `get`:

```scheme
(rna-call (rna-get (data-ref) "objects") "get" (list "Cube") (hash))
```

A miss returns Blender's `None`, which arrives as `#<void>`. That makes `get` the
existence check:

```scheme
(define (find-object name)
  (rna-call (rna-get (data-ref) "objects") "get" (list name) (hash)))

(void? (find-object "Cube"))   ; #true when absent
```

Only these collection functions are allowed: `clear`, `find`, `foreach_get`,
`foreach_set`, `get`, `link`, `move`, `new`, `remove`, `unlink`. Everything else must be
a function Blender publishes in `bl_rna.functions` — `materials.append`, for instance,
is fine, but it lives on the collection and not on the mesh. The inherited
`bpy_struct.keyframe_insert` and `keyframe_delete` methods are also available on RNA
structs despite being absent from that metadata:

```scheme
(rna-call (rna-get (rna-get object "data") "materials") "append" (list material) (hash))
```

`get` takes a **string** key. Passing an integer fails with
`<built-in method get ...> returned a result with an exception set`, which names nothing
useful. Index by position with `rna-items` instead — this is how you reach things like
colour ramp elements:

```scheme
(define (nth-item ref i)
  (list-ref (hash-try-get (rna-items ref i 1) "items") 0))
```

### One shape for Blender collections

Blender collections always arrive as opaque references, including anonymous collections
such as `Screen.areas` and `Object.material_slots`. Iterate with `rna-items` or the
`collection-list` helper:

```scheme
(collection-list (rna-get (active-object) "material_slots"))
```

Vectors, numeric arrays, and ordinary Python lists still become Scheme lists. Older
servers expanded anonymous collections automatically; migrate direct `map` calls over
those values to `collection-list`.

## `None` and the calls that return it

Plenty of the API returns `None`: `collection.remove`, `.link`, `.unlink`, `.clear`,
`.foreach_set`, and `.get` on a miss. All of these arrive as `#<void>` and are normal
successful results.

> Servers built before this was fixed rejected those replies as a malformed envelope.
> The failure was misleading in the worst way: Blender had already *performed* the
> action, and only the reply was refused. If you see
> `invalid protocol envelope: response must contain exactly one of result or error`,
> the server is older than the fix and the mutation probably went through.

Remove what you actually need to remove. Emptying `bpy.data` wholesale — every object,
mesh, material, node group, light and camera in one pass — crashed a graphical Blender
5.2.1 on the next `setattr` after the purge. Single removals are routine; a bulk purge
against a live session with a populated UI is not, and starting a fresh session is both
faster and safer than clearing one out.

## References go stale, and worse, go subtly wrong

A reference carries a generation. File load, undo/redo, and deletion invalidate it, and
you get a clear `stale_reference` error — reacquire from `(context-ref)` or `(data-ref)`.

RNA references resolve their owner and traversal path afresh on each use. ID
references check Blender's session UID, so renaming an object preserves its
identity and recreating a deleted object under the same name does not reuse it.
The resolved type must also match the reference's original type:

```scheme
(define L (rna-call (rna-get (data-ref) "lights") "get" (list "Key") (hash)))
(rna-set! L "type" "AREA")
(rna-get L "size")
; => stale_reference: RNA target type changed
```

Reacquire after changes that switch a subtype, such as light `type`, modifier or
node kind, or object data:

```scheme
(rna-get (rna-call (rna-get (data-ref) "lights") "get" (list "Key") (hash)) "size")
; => 0.25
```

How long a handle below an ID lasts depends on how Blender stores the data. Handles
are **durable by default**: render and view settings, a camera's depth of field, nodes,
sockets, links, modifiers and the like are allocated once and never move, so they
survive operators, mode changes and the depsgraph updates that edits trigger.

Only **array-backed or rebuilt data** is volatile: mesh vertices, edges, polygons,
loops and triangles; attribute layers and their values; UV and colour layers;
vertex-group weights; shape-key and spline points; keyframes; and bones (rebuilt on
leaving edit mode). Blender can reallocate these with a *different* element at the same
address, so their handles are retired after any operator, mode change, keyframe edit,
or geometry update. Fetch them again, or move their values in bulk with
`collection-read` / `collection-write!`.

Removing a member through MCP retires every handle to it (and to its sockets), but
not its siblings, so a loop can remove nodes from a listed collection. Adding to an
array-backed collection retires its members. Every retired handle reports why in its
`stale_reference` error. Resolution always re-finds the target from its live owner and
checks its pointer and type, so no handle can reach freed memory.

## Operators

An operator returns its *status*, never the thing it made:

```scheme
(bpy/ops/mesh/primitive_cube_add (hash "size" 2.0))
; => {"operator": "mesh.primitive_cube_add", "status": ("FINISHED")}
```

Take the result from the context:

```scheme
(bpy/ops/mesh/primitive_cube_add)
(define cube (rna-get (context-ref) "object"))
```

Operators depend on context. One that fails `poll()` returns a structured
`operator_unavailable` error; `(operator-poll idname)` checks first. Prefer the RNA data
API for anything an operator is not required for — it does not care about the active
area, selection, or mode.

## Node trees

Node graphs are built through ordinary RNA. `nodes.new` takes the type name, and
`links.new` takes two sockets:

```scheme
(define tree (rna-get material "node_tree"))
(define nodes (rna-get tree "nodes"))
(define emission (rna-call nodes "new" (list "ShaderNodeEmission") (hash)))
(rna-call (rna-get tree "links") "new"
          (list (rna-call (rna-get emission "outputs") "get" (list "Emission") (hash))
                (rna-call (rna-get output "inputs") "get" (list "Surface") (hash)))
          (hash))
```

Socket default values live at `default_value`, and the socket must be re-fetched after
the node's type changes — the subtype rule above applies to sockets as well.

### The compositor moved in Blender 5.x

`scene.node_tree` no longer exists:

```scheme
(rna-get (rna-get (context-ref) "scene") "node_tree")
; => 'Scene' object has no attribute 'node_tree'
```

The compositor is now a node *group* hanging off the scene:

```scheme
(rna-get (rna-get (context-ref) "scene") "compositing_node_group")
; => <ref CompositorNodeTree>
```

Because it is a group, its input comes from the group's interface rather than from a
`Render Layers` node, and `use_nodes` is not what enables it. Most compositor snippets
written for Blender 4.x and earlier need reworking.

A half-built compositor group does not fail — it *replaces* your render, usually with
flat white, which reads like a lighting bug and sends you looking in the wrong place.
When a render looks inexplicably wrong, take the compositor out of the picture first:

```scheme
(rna-set! (rna-get (rna-get (context-ref) "scene") "render") "use_compositing" #false)
```

## Names that moved in 5.x

Blender renames enum items and properties between releases, and generated code from an
older version fails in ways that name no version. Two confirmed in 5.2:

- `scene.node_tree` is now `scene.compositing_node_group` (above).
- The Sky texture's `sky_type` default is `MULTIPLE_SCATTERING`; the `NISHITA` item that
  4.x snippets set is gone. Its `dust_density` went with it — the property is now
  `aerosol_density`, alongside `ozone_density` and `air_density`.

`(rna-describe ref)` lists what actually exists on this build. Prefer reading the enum
over trusting a remembered identifier — the error for a bad enum item does not suggest
alternatives.

## Rendering device

Setting `scene.cycles.device` to `"GPU"` does not guarantee a GPU is used. Cycles falls
back to the CPU when no compatible device is enabled, silently. Enumerate first:

```scheme
(let* ([addons (rna-get (rna-get (context-ref) "preferences") "addons")]
       [cycles (rna-call addons "get" (list "cycles") (hash))]
       [devices (rna-get (rna-get cycles "preferences") "devices")])
  (map (lambda (d) (list (rna-get d "name") (rna-get d "type"))) (collection-list devices)))
```

The flake builds Blender with `cudaSupport`, which also enables OptiX, and both
`scripts/live.sh` and the headless bootstrap select the best backend at startup and
switch every scene to `GPU` — the stock nixpkgs `blender` omits CUDA, and empty Cycles
preferences are why a GPU-capable build otherwise renders on the CPU in silence. Startup
reports what it picked:

```
SCHEME_MCP: cycles device {'device_type': 'OPTIX', 'devices': ['NVIDIA GeForce RTX 4090']}
```

Set `BLENDER_MCP_GPU=0` to stay on the CPU, and use `nix run .#blender-cpu` when the
CUDA compile is not worth the wait. If the enumeration above lists only a CPU, this
Blender was built without GPU compute and no setting will change it.

Loading a `.blend` brings its own `scene.cycles.device`, so re-assert it after opening
a file you did not author:

```scheme
(rna-set! (rna-get (rna-get (context-ref) "scene") "cycles") "device" "GPU")
```

## Volumes at world scale

A world volume is unbounded, so at kilometre scale any density high enough to read as
haze scatters every camera ray past the bounce limit and the frame renders black. Use a
bounded mesh volume around the region you actually want fogged rather than a world
volume, and treat a black render with volumetrics present as a density problem before
suspecting the lights.

## Rendering

- `(render!)` renders to a temporary file.
- `(render-to! "/tmp/out.png")` renders to a path you choose.

Both attach supported images within the 8 MiB inline limit as MCP image blocks.
Larger outputs return metadata with `inline_available: false`; other formats use
resource content. The path argument does not suppress attachment. Binary data
appears only in content blocks; structured results contain artifact metadata.
For a file without an attachment:

```scheme
(render-file! "/tmp/frame.png")
(preview! "/tmp/preview.png" 25)
```

These use the same native render operation and return its resolved output path.
Blender's image format and `use_file_extension` setting determine the filename;
for example, a JPEG render requested at `/tmp/frame.png` writes `/tmp/frame.jpg`.
The scene's original filepath is restored after success or a Blender error. Preview
also restores resolution after ordinary Scheme errors; timeout cancellation can
prevent further cleanup. Multiview and filename templates are not covered by the
still-render helper; use the Blender operator for those workflows.

## Large data moves in pages

Results are bounded at 10,000 items and 256 KiB. Arguments on the way to Blender have
a separate, larger budget matching the bridge's 2 MiB request limit (100,000 items):
about 14,000 vertex coordinates, or a heightfield of roughly 80 × 80 cells with its
faces, fit in one call. Past that, and for anything read back, move data in pages. Blender's `foreach_get`/`foreach_set` always cover a
whole collection, so they cannot be split into ranges by hand; `collection-read` and
`collection-write!` page one attribute of any collection for you, and the stdlib
builds on them:

```scheme
(mesh-positions obj)                    ; ((x y z) ...), any vertex count
(mesh-set-positions! obj points)        ; written back 1,000 vertices per call
(mesh-faces obj)                        ; ((i j k ...) ...)
(collection-values (rna-get (collection-get (rna-get mesh "attributes") "wet") "data") "value")
(collection-read (rna-get mesh "vertices") "co" 1000 500) ; one page: total, stride, values
```

New geometry is still best built with `mesh-from-data!`, splitting very large meshes
into several objects with `(chunk items size)`.

## Cost, and designing around it

Each scalar RNA operation crosses the socket and Blender's main-thread queue. Putting
many scalar calls in one `scheme_eval` still incurs those bridge round trips. Prefer,
in order:

1. A node graph or modifier that expresses the result declaratively.
2. `mesh-from-data!` to create a mesh from vertices and faces in one call, or
   `foreach_set` for bulk numeric data on existing geometry.
3. `(batch! commands)` for up to 100 independent typed operations per bridge request.
4. Individual RNA calls where each step depends on the previous result.

Batch execution yields between commands to leave time for Blender's event loop. It stops
on the first error and reports completed commands; it does not roll them back. Calls that
arrive back to back are served without waiting for the dispatcher's next poll: about
0.6-0.9 ms each in headless mode, where a fixed 10 ms poll used to set the pace. Live
mode serves such streams in short slices between UI updates. Runtime
latency still depends on the Blender mode and current workload. Each evaluation reports
`metrics.bridge_calls` and `metrics.bridge_elapsed_ms`; the `mcp-benchmark` Nix check
measures equivalent 100- and 1,000-operation scalar/batch read and write workloads
and writes `result.json`. These CPU headless measurements do not measure live UI
responsiveness or render speed.

## Discovering the API

`(rna-describe ref)` lists the properties and functions Blender publishes on a value —
the fastest way to answer "what can I do with this?" without leaving Scheme.
`(operator-search "bevel")` and `(operator-info idname)` cover the operator side.

Use `(rna-property-info ref "property")` or `(rna-function-info ref "function")` when
only one signature or property's constraints are needed. Start a session with
`(control-status)` to identify the running instance and capabilities before choosing
an API from remembered examples.
