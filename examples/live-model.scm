; Add a cube and keep a usable handle to it.
;
; An operator call returns its status, not the object it created:
;   (bpy/ops/mesh/primitive_cube_add ...)  =>  {"operator": ..., "status": ("FINISHED")}
;
; The object it just made is the active one, so take the handle from the context.

(bpy/ops/mesh/primitive_cube_add
  (hash "size" 2.0 "location" (list 0.0 0.0 1.0)))

(define cube (rna-get (context-ref) "object"))

(rna-get cube "name")
(rna-get cube "location")

; Move it, then look again. Edits made by hand in Blender between calls are
; visible the same way; there is no copied scene.
(rna-set! cube "location" (list 2.0 0.0 1.0))
(rna-get cube "location")

(scene-summary)
