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

## Rendering

```scheme
(set-engine! "CYCLES") (set-resolution! 1920 1080) (set-samples! 128)
(set-camera! object)
(render-file! "/tmp/frame.png")  ; writes the file, returns the path, no base64
(preview! "/tmp/look.png" 25)    ; the same at 25% resolution
(thumbnail! (hash "max_size" 512)) ; bounded PNG image attachment
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
