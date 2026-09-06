# Scheme binding reference

`scheme_eval` is the only MCP tool. It accepts `code`, optional
`session`, `timeout_secs`, `reset`, `include_events`, and `background`.
Omitting `session` selects `default`; clients selecting the same session share
its Scheme definitions and Blender scene. See [Sessions](sessions.md).
Set `background: true` only from a client declaring the MCP tasks
extension; see [Background MCP tasks](tasks.md) for polling and cancellation.

This page lists the functions this server adds. For the Steel language itself see
`blender-mcp://reference/steel`; for how these map onto Blender's Python API,
including the behaviours worth knowing before you start, see
`blender-mcp://reference/blender-api`.

## Results

`structuredContent.session` identifies the selected session. Returned artifact
metadata includes a `uri` that routes resource reads to its originating session.

Every top-level expression contributes a value. One expression returns that
value; several return a list of them, in order:

```scheme
(+ 1 1) (+ 2 2)   ; => [2, 4]
```

Values are bounded at 16 levels deep, 10,000 items, and 256 KiB each for display
and structured conversion. Byte accounting includes container and escaping costs.
A real Blender catalogs around 2,500 operators with some 6,200 properties between
them, which is far past those limits, so the discovery functions below return
names and summaries rather than whole descriptors.

The item budget applies **in both directions**. It is spent converting arguments
on the way to Blender as well as results on the way back, so a single call cannot
carry a large buffer either — a few thousand vertex coordinates already exhausts
it. Split the work with `(chunk items size)`.

Inspect `structuredContent.result_complete`, `display_truncated`, and
`serialization_error` before treating a large reply as complete. A failed result
conversion does not mean Blender rolled back earlier mutations. Per-evaluation
`metrics.bridge_calls` and `metrics.bridge_elapsed_ms` expose transport cost.
When events are requested, `events_truncated` reports whether the bounded event
history omitted earlier entries; at most the last 128 events are retained.

## Discovery

- `(operators)` returns every operator ID as a string.
- `(operator-search "cube")` matches IDs, labels, and descriptions, returning
  `{"total": n, "truncated": bool, "matches": [...]}`. Each match carries
  `idname`, `steel_name`, `label`, and `description`; at most 200 are returned,
  so narrow the query when `truncated` is true.
- `(operator-info "mesh.primitive_cube_add")` returns the one full descriptor,
  including properties, enum values, bounds, and options such as `REGISTER`
  and `UNDO`.
- `(operator-poll "mesh.primitive_cube_add")` checks the current context.
- `(catalog-refresh!)` rediscovers operators and returns
  `{"revision", "operator_count", "blender_version"}`. The regenerated
  `bpy/ops/...` aliases are installed once the current evaluation returns, so
  call it, then use the new names in the next `scheme_eval`.

## Operators

```scheme
(op-call "mesh.primitive_cube_add")
(op-call "mesh.primitive_cube_add" (hash "size" 2.0))
(bpy/ops/mesh/primitive_cube_add (hash "size" 2.0))
```

The keyword hash is optional for both forms. Extra positional arguments are rejected.
Keyword arguments are a `hash`; vectors and enum sets are lists:

```scheme
(bpy/ops/transform/translate (hash "value" (list 0 0 2)))
```

A call returns its status, not the data it created:

```scheme
(bpy/ops/mesh/primitive_cube_add)
; => {"operator": "mesh.primitive_cube_add", "status": ("FINISHED")}
```

To work with what an operator just made, take the active object from the
context:

```scheme
(define cube (rna-get (context-ref) "object"))
(rna-get cube "name")
```

An operator whose `poll()` fails stays in the catalog and returns a structured
`operator_unavailable` error rather than disappearing.

For call-level options, wrap keyword arguments:

```scheme
(op-call "wm.save_as_mainfile"
  (hash "kwargs" (hash "filepath" "/tmp/scene.blend")
        "execution_context" "EXEC_DEFAULT"
        "undo" #f))
```

## RNA

- `(context-ref)` and `(data-ref)` return versioned opaque references.
- `(rna-get ref "attribute")`
- `(rna-set! ref "attribute" value)`
- `(rna-call ref "function" [args [kwargs]])` — defaults to an empty list and hash.
- `(rna-describe ref)`
- `(rna-property-info ref "attribute")` — one property's runtime metadata.
- `(rna-function-info ref "function")` — one allowed function's runtime signature.
- `(rna-items ref [offset [limit [expected-revision]]])` — defaults to offset 0 and
  limit 100. Pass the previous page's `collection_revision` on subsequent pages
  to detect membership or ordering changes during inspection.
- `(reference-stats)` and `(reference-release! (list ref ...))` — inspect and release
  retained handles. Reusing a released reference returns `stale_reference`.

The older `/default` forms remain supported. Square brackets above indicate optional
arguments, not literal Scheme syntax.

Private attributes, Python expressions, imports, and arbitrary callable access are
denied. Public RNA functions, collection helpers, and `bpy_struct.keyframe_insert` /
`keyframe_delete` are available; `rna-describe` lists the permitted methods on a target.

`rna-get` is `getattr`, so indexing a collection by name goes through `get`:

```scheme
(rna-call (rna-get (data-ref) "objects") "get" (list "Cube") (hash))
```

Blender's `None` — returned by a `get` that misses, and by `remove`, `link`, and
`clear` — arrives as `#<void>`; test it with `(void? x)`.

## Runtime

- `(blender-status)`
- `(control-status)` — instance identity, versions, capabilities, document generation,
  active work, queue depth, and receipt retention.
- `(scene-summary)` — object counts, selection, camera, render settings, material
  names, and evaluated geometry bounds in the current view layer. Reports scan
  limits and omissions; bounds exclude instances and objects without bounds.
- `(scene-snapshot [limit])` — bounded value snapshot; default and maximum 1,000 objects.
- `(scene-diff before [after])` — compare complete snapshots from the same document epoch;
  omitted `after` takes a fresh snapshot.
- `(checkpoint! "/tmp/scene.blend")` — save a copy without changing the active filepath
  or invalidating references; the destination must not already exist.
- `(render!)`
- `(render-to! "/tmp/render.png")`
- `(render-file! "/tmp/render.png")` — writes the file and returns its resolved path.
- `(thumbnail! [options])` — attach a PNG with longest edge at most `max_size`
  (default 512, range 1–2,048); options may also specify `filepath`. Render size and
  image format are restored afterward.
- `(artifact-get artifact-id)`
- `(artifact-info artifact-id)` — metadata without fetching or attaching image bytes.
- `(artifact-release! artifact-id-or-list)` — release retained snapshots explicitly.

`render!`, `render-to!`, and `artifact-get` attach PNG/JPEG/WebP/GIF images within
the inline limit as MCP image content; other artifacts use resource content.
Structured results contain metadata only, with no
duplicated base64. `artifact-get` returns the descriptor as its Scheme value.
Use `render-file!` or `(preview! path percent)` for a file without an attachment.

Artifact IDs identify immutable snapshots of generated files. Rendering again to the
same output path does not replace the bytes behind an earlier ID. Descriptors include
the digest, image dimensions, and render provenance. Snapshots expire after one hour;
the store holds at most 64 artifacts, 64 MiB per file, and 256 MiB total. Inline
retrieval is limited to 8 MiB. A larger completed render reports
`inline_available: false` and keeps its metadata/path without automatically attaching
bytes. Unknown, expired, or released IDs return explicit errors.
`resources/read` on `blender-mcp://artifact/ID` returns the artifact's actual blob.

Snapshots compare object base and delta transforms, parent identity, render
visibility, frame, and basic render metadata. They exclude constraints, parent
inverse matrices, mesh topology, materials, and animation curves. Diffs reject
truncated snapshots. The request journal covers MCP operations only. Checkpoints
are caller-owned `.blend` files; the server does not rotate or delete them.

## Batches

`(batch! commands)` executes at most 100 typed bridge operations in order. For example:

```scheme
(batch! (list
  (rna-set-command cube "location" (list 1 2 3))
  (rna-get-command cube "location")))
```

References may use the normal `$rna_ref` wrapper or its inner reference body. The
bridge yields between commands when its main-thread time slice expires. A batch stops
at the first error and returns `completed`, indexed `results`, `complete`, and, on
failure, `failed_index` and `error`. Successful earlier commands remain applied. A
batch is neither a transaction nor a way to bypass per-operation access checks.

## Long-running work and recovery

```scheme
(define job (render-start))
(define job-id (hash-try-get job "job_id"))
(job-status job-id)
(job-result job-id #f)  ; completed render metadata, no attachment
(job-result job-id)     ; attach the completed image
```

`render-start` also accepts a settings hash with `filepath` and `timeout_secs`.
`job-status`, `job-result`, and `job-cancel` work from another client selecting the
same Blender session. Job states are `queued`, `running`, `succeeded`, `failed`, `cancelled`,
and `expired`. Request receipts can be inspected with `(request-status request-id
[session-id])` and `(request-result request-id [session-id])` within the advertised
retention window.

`GET /healthz` uses the independent bridge control channel while Blender is busy.
Scheme calls enter the selected session's FIFO worker, so a status expression waits behind
an active synchronous `render!` evaluation. `render-start` releases that worker after
submitting the job, allowing subsequent status evaluations during the render.
While a render job is queued or running, ordinary Blender calls and additional
jobs return a retryable `bridge_busy` error. Inspect the job and retry after its
terminal state; status/result/cancellation controls remain available.

Queued work can be cancelled or expire before execution. Cancellation of a running
Blender call is a request, not proof that it stopped; inspect `potentially_continuing`
and the eventual terminal state. Do not automatically replay an uncertain mutation.
