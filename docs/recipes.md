# Recipes

## Live model, edit, inspect

```scheme
(bpy/ops/mesh/primitive_cube_add (hash "size" 2.0))
(scene-summary)
```

Move the cube in the Blender UI, then call `(scene-summary)` or reacquire the
object through RNA to observe the edit.

## Discover before calling

```scheme
(operator-search "bevel")
(operator-info "object.modifier_add")
(operator-poll "object.modifier_add")
```

## Render and inspect

```scheme
(render!)
```

The tool response includes the render as an MCP image block.

For a file without an image attachment, use `(render-file! "/tmp/frame.png")`.

## Keyframe an object without selecting it

```scheme
(define ball (add-sphere))
(set-location! ball 0 0 0)
(rna-call ball "keyframe_insert" (list "location") (hash "frame" 1))
(set-location! ball 0 0 3)
(rna-call ball "keyframe_insert" (list "location") (hash "frame" 25))
(rna-call (scene) "frame_set" (list 13))
(object-location ball)
```

`keyframe_insert` and `keyframe_delete` operate on the referenced data and work in
headless mode. They use Blender's normal interpolation defaults. Each `rna-call`
still crosses the bridge; prefer a few key poses and interpolation over densely
keying every frame.

## Show materials in the live viewport

Solid viewport shading can look grey even when a render has colored materials.
This explicitly switches the current screen's 3D views to material preview:

```scheme
(define screen (rna-get (context-ref) "screen"))
(when (exists? screen)
  (for-each
    (lambda (area)
      (when (equal? (rna-get area "type") "VIEW_3D")
        (let ([space (rna-get (rna-get area "spaces") "active")])
          (rna-set! (rna-get space "shading") "type" "MATERIAL"))))
    (collection-list (rna-get screen "areas"))))
```

## Attach Blender after the server is already up

In live mode the server outlives Blender, so it starts whether or not the
bridge is running. `GET /healthz` tells you which part is missing:

```json
{ "server": "ready", "bridge_connected": false, "blender": null }
```

Start the bridge from the **Scheme MCP** N-panel, then rebuild the operator
aliases from the now-attached Blender:

```scheme
(catalog-refresh!)
```

That returns the new revision and operator count. The regenerated
`bpy/ops/...` aliases are installed once the call returns, so use them in the
next `scheme_eval`. Stopping and restarting the bridge does not require
restarting Blender or the server; `catalog-refresh!` again if extensions
changed in between.

Until a catalog has been loaded, `operator_count` may still report the last
Blender the server saw. `bridge_connected` and `blender` are the fields that
say whether anything is attached right now.

## Recover

Use `reset: true` when Scheme definitions are no longer coherent. Refresh the
catalog after enabling/disabling a Blender extension. Reacquire RNA references
after load, undo/redo, or destructive edits.

Names bound by Steel's own prelude, such as `log`, cannot be rebound with
`set!`; choose another name for mutable state.

## Check a scene change before continuing

```scheme
(define before (scene-snapshot))
(set-location! (find-object "Cube") 1 2 3)
(scene-diff before)
(checkpoint! "/tmp/scene-checkpoint.blend")
```

Snapshots cover object transforms, parenting, render visibility, and render settings;
they do not compare mesh topology, materials, or animation curves. Diffs reject
truncated snapshots and snapshots from different document epochs. A checkpoint saves
a copy while preserving the active document path and current references.

## Reconnect to a render

```scheme
(define job (render-start (hash "timeout_secs" 120)))
(define job-id (hash-try-get job "job_id"))
(job-status job-id)
```

Keep the returned job ID. After reconnecting to the same server, poll its status and
fetch the image with `(job-result job-id)` only after success. Use
`(job-result job-id #f)` for metadata. A cancellation request may leave a running
Blender render active; the status says whether work is potentially continuing.
