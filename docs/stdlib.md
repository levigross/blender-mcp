# Standard library

Helpers that ship in every Steel environment, so a scene can be built without first
writing a toolkit. They are ordinary Scheme and Rust bindings — nothing here needs the
bridge to know about it — and they exist because each one had to be hand-written before
anything could be built.

**Angles are degrees.** Blender's RNA is radians. `set-rotation!`, `object-rotation`,
and `look-at!` convert for you; `degrees->radians` is there for the rest.
Math helpers accept integers, exact fractions, and floating-point values.

## Maths

Steel's prelude already carries `sin` `cos` `tan` `asin` `acos` `atan` `exp` `log`
`sqrt` `expt` `square` `abs` `floor` `ceiling` `round` `truncate` `min` `max` `modulo`
`remainder` `quotient` `gcd` `lcm` and the numeric predicates. Added here:

| | |
|---|---|
| `pi`, `tau` | constants (`tau` is 2π) |
| `atan2 y x` | quadrant-correct angle; Steel's `atan` takes one argument |
| `hypot x y` | √(x²+y²) without overflow |
| `sinh` `cosh` `tanh` `asinh` `acosh` `atanh` | |
| `log2` `log10` `cbrt` | |
| `degrees->radians`, `radians->degrees` | |
| `clamp x lo hi` | tolerates a reversed range instead of failing |
| `lerp a b t` | `(lerp a b 1.0)` is exactly `b` |
| `smoothstep e0 e1 x` | flat outside the edges |
| `remap x from-lo from-hi to-lo to-hi` | |
| `sign x` | `0.0` for zero, unlike `signum` |
| `random`, `random-range lo hi`, `random-seed! n` | seeded, so a scene is reproducible |

`e` is deliberately absent: `(lambda (e) ...)` is the usual exception-handler idiom, and
every name here becomes un-`set!`-able. Use `(exp 1)`.

## Finding things

```scheme
(scene)                      ; the active scene
(active-object)              ; bpy.context.object
(bpy-data "objects")         ; a bpy.data collection
(find-object "Cube")         ; #<void> when absent
(find-material "Lake") (find-mesh "Grid") (find-node-group "Terrain")
(exists? value)              ; the readable form of (not (void? value))
(collection-get coll "name") ; collections index with `get`, not attributes
(collection-list coll)       ; the items, without the paging envelope
(names-of coll) (object-names)
```

`collection-list` fetches complete collections in pages, with a default cap of 2,000
items; pass a second argument for an explicit cap. Exceeding the cap is an error,
never a silently truncated list. Pagination verifies a membership/order revision.
Use `rna-items` directly when partial pages are intended.

## Creating

Operators return their status, never the object they made. These return the object:

```scheme
(add-cube (hash "size" 2.0))
(add-sphere) (add-plane) (add-cylinder)
(add-grid subdivisions size location)
(add-camera (list 0 -10 4))
(add-light "AREA" (list 0 0 6))
(delete-object! object)
```

## Placing

```scheme
(object-location object)          (set-location! object x y z)
(object-rotation object)          (set-rotation! object rx ry rz)   ; degrees
(set-scale! object sx sy sz)      (move! object dx dy dz)
(rename! object "Name")
(look-at! object (list 0 0 0))    ; aim a camera or light
(facing object)                   ; the unit vector it faces
(ring count radius height)        ; points on a circle
```

## Selecting and organising

Many operators act on "the selection and the active object" rather than on an
argument; these make that state explicit. Helpers that call such operators select
their target themselves.

```scheme
(select-only! object)             ; deselect everything else, then make it active
(select! object) (deselect! object) (deselect-all!) (set-active! object)
(selected-objects)
(add-empty "PLAIN_AXES" (list 0 0 0))
(parent! child parent)            ; keeps the child where it is, like Ctrl+P
(unparent! child)                 ; also keeps its world placement
(define props (make-collection! "Props"))   ; optional second argument: parent
(move-to-collection! object props)          ; leaves every other collection
(object-collections object)
(hide! object #true)              ; viewport and render
(duplicate! object)               ; own mesh, like Shift+D
(duplicate! object #true)         ; shared mesh, like Alt+D
(instance-collection! props (list 5 0 0))
(array-along! object 5 (list 2 0 0))   ; 4 shared-data copies in a line
(scatter-ring! object 8 6.0 0.0)       ; 8 copies on a circle of radius 6
(refresh!)                        ; recompute matrix_world after RNA edits
```

`parent!`, `unparent!`, and the bounds helpers call `refresh!` themselves: Blender
recomputes world matrices only on a depsgraph update, which an RNA write does not
trigger.

## Materials and nodes

```scheme
(define material (make-material "Lake"))     ; nodes already enabled
(assign-material! object material)
(define tree (material-tree material))       ; or (world-tree)
(define bsdf (principled material))
(define noise (node-add tree "ShaderNodeTexNoise"))
(node-at! noise -400 0)
(node-set! noise "Scale" 12.0)               ; an unconnected input
(node-link! tree noise "Fac" bsdf "Metallic")
(node-input node "Scale") (node-output node "Fac") (node-find tree "Noise Texture")
(node-remove! tree node)
```

## Modifiers

```scheme
(add-modifier! object "Subdivision" "SUBSURF")
(add-geometry-nodes! object "Terrain" (find-node-group "TerrainNodes"))
```

Prefer modifiers to edit-mode operators: they are non-destructive, need no editor
context, and one property write changes the result. Each returns its modifier:

```scheme
(bevel! object 0.05 3)                  ; width, segments
(subdivide! object 2)                   ; viewport and render levels
(solidify! object 0.1)
(array! object 6 (list 1.1 0 0))        ; count, offset relative to object size
(mirror! object "X" "Y")                ; X alone when no axis is given
(boolean! object cutter "DIFFERENCE")   ; the cutter becomes a hidden wireframe
(weld! object 0.001)
```

Destructive steps select their target first:

```scheme
(apply-modifier! object modifier)       ; the modifier or its name
(apply-transforms! object)
(shade-smooth! object) (shade-flat! object)
(set-origin! object "ORIGIN_GEOMETRY")
(join! (list base part-a part-b))       ; merged into the first, which is returned
(with-edit-mode object (lambda () (op-call "mesh.subdivide" (hash "number_cuts" 2))))
(inset-faces! object 0.05 0.1)          ; thickness, depth
```

`with-edit-mode` returns to object mode even when the body fails. Mode changes
invalidate mesh element handles, so fetch those again afterwards. Transform-based
edit operators (`extrude_region_move`, `translate`) need a 3D viewport and fail in a
headless session; `mesh.inset` and `mesh.subdivide` do not.

## Building meshes from data

`mesh-from-data!` makes a mesh object from vertex positions and faces in a single
bridge call, linked into the scene or the given collection:

```scheme
(mesh-from-data! "Tetra"
                 (list (list 0 0 0) (list 1 0 0) (list 0 1 0) (list 0 0 1))
                 (list (list 0 2 1) (list 0 1 3) (list 1 2 3) (list 0 3 2))
                 props)                  ; optional collection
```

Faces list vertex indices counter-clockwise as seen from outside. Every index is
checked before Blender reads it. The generators below produce that input as plain
lists, so it can be inspected or transformed before anything reaches Blender:

```scheme
(make-extrusion "Plan" (list (list 0 0) (list 4 0) (list 4 3) (list 0 3)) 2.5)
(make-prism "Hex" 6 1.0 0.3)                       ; sides, radius, height
(make-heightfield "Hills" 30 30 20.0
                  (lambda (x y) (* 0.5 (sin x) (cos y))))
(extrusion-data outline height) (prism-data sides radius height)
(heightfield-data columns rows size height-at)
```

A call carries at most 10,000 items. Vertices cost four each (three numbers and
their list), and quads five, so about 30 × 30 heightfield cells fit in one object;
tile larger surfaces.

## Animation

```scheme
(frame-range! 1 120) (set-fps! 24) (set-frame! 1)
(key-location! object 1 0 0 0)          ; set, then key, at frame 1
(key-rotation! object 60 0 0 90)        ; degrees
(key-scale! object 120 2 2 2)
(keyframe! object "hide_render" 30)     ; any animatable property of the object
(set-interpolation! object "LINEAR")    ; optionally one path: "rotation_euler"
(turntable! object 1 120)               ; one constant-speed turn about Z
(orbit-camera! camera (list 0 0 1) 8.0 3.0 1 120)   ; returns the pivot empty
(object-fcurves object)
(clear-animation! object)
```

Blender 4.4 and later keep F-curves in layered actions; `object-fcurves` walks
action → layers → strips → the object's channelbag, since `action.fcurves` no longer
exists.

## Lighting and camera

```scheme
(sun! 4.0 35 120)                       ; strength, elevation, azimuth (degrees)
(three-point-lights! (list 0 0 1) 6.0)  ; key, fill, rim area lights, aimed
(set-light! light 500 (list 1.0 0.9 0.8))
(world-color! (list 0.05 0.06 0.08) 1.0)
(hdri! "/path/to/sky.exr" 1.0)
(object-bounds object)                  ; ((min x y z) (max x y z)), world space
(scene-bounds (list a b c))
(frame-camera! camera (list a b c))     ; optional margin, default 1.1
(set-lens! camera 50) (depth-of-field! camera 6.0 2.8)
```

`frame-camera!` keeps the camera's bearing, moves it back until the objects'
bounding sphere fits the narrower field of view, and aims it. Unlike the viewport
operator it replaces, it works headless.

## Custom properties

Custom properties (`obj["role"]` in Python) travel with exports — the glTF exporter
writes them as `extras` when "Custom Properties" is enabled.

```scheme
(prop-set! object "role" "walkable_ground")
(prop-get object "role")                ; #<void> when absent
(prop-keys object)
(prop-delete! object "role")
(batch! (map (lambda (o) (prop-set-command o "role" "prop")) crates))
```

Values may be numbers, strings, booleans, lists, hashes, or ID handles. Keys are
1–63 bytes and may not start with `_`. Registered add-on properties share this
storage but are refused here; set those with `rna-set!` so their types are enforced.

## Rendering

```scheme
(set-engine! "CYCLES") (set-resolution! 1920 1080) (set-samples! 128)
(set-camera! object)
(render-file! "/tmp/frame.png")  ; writes the file, returns the path, no base64
(preview! "/tmp/look.png" 25)    ; the same at 25% resolution
(thumbnail! (hash "max_size" 512)) ; bounded PNG image attachment
(render-preset! "draft")           ; 16 samples, 50%; "final" is 256, denoised, 100%
(color-management! "AgX" "None" 0.0)
(set-output! "/tmp/out/frame_" "PNG")
```

Both return a resolved file path without fetching image bytes. They use the same
bridge render operation as `render!` and `render-to!`, which additionally attach the
image as MCP content. Render output paths are restored even when Blender reports an
error; `preview!` also restores the resolution percentage on ordinary Scheme errors.
A timeout can prevent further Scheme cleanup, and a running Blender render may continue.

`thumbnail!` uses native cleanup to restore size and image format, and attaches an
immutable PNG artifact. For a durable render receipt, use `render-start` and the
`job-status` / `job-result` operations described in the Scheme reference.

## Guarding

```scheme
(try (lambda () (rna-get object "size")) "not on this subtype")
```

Without a handler the first failure abandons the whole evaluation, discarding every
expression after it.

## Splitting large uploads

```scheme
(chunk items 2000)
```

The chunk size must be a positive integer.

The 10,000-item budget is spent on arguments travelling **to** Blender as well as on
results coming back, so one call cannot carry a large buffer. Vertex coordinates are
three numbers each, so a few thousand vertices exhausts it and a `foreach_set` upload
fails at exactly the point a procedural mesh gets interesting. Build it in pieces.

`rna-set-command` and `rna-get-command` create typed command hashes for `batch!`:

```scheme
(batch! (list (rna-set-command object "location" (list 1 2 3))
              (rna-get-command object "location")))
```
