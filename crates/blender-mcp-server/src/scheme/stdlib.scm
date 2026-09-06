;; A small standard library for driving Blender.
;;
;; Everything here is built from the primitives in `bindings.rs`; nothing needs the
;; bridge to know about it. The selection is not speculative -- these are the helpers
;; that had to be hand-written before a scene could be built at all, so they are the
;; ones worth shipping.
;;
;; Angles are in DEGREES throughout. Blender's RNA is radians, and forgetting that is
;; the single easiest way to produce a scene that looks wrong for no visible reason,
;; so the conversion happens here rather than in every caller.

;; ---------------------------------------------------------------- roots and lookup

(define (scene) (rna-get (context-ref) "scene"))
(define (active-object) (rna-get (context-ref) "object"))
(define (bpy-data name) (rna-get (data-ref) name))

;; Collections are indexed with `get`, not attribute access; a miss is `#<void>`.
(define (collection-get collection name)
  (rna-call collection "get" (list name) (hash)))

(define (data-find kind name) (collection-get (bpy-data kind) name))
(define (find-object name) (data-find "objects" name))
(define (find-material name) (data-find "materials" name))
(define (find-mesh name) (data-find "meshes" name))
(define (find-node-group name) (data-find "node_groups" name))

(define (exists? value) (not (void? value)))

;; Fetch complete collections in bounded pages. Reaching the caller's cap is an
;; explicit error; use rna-items when a partial page is the intended result.
(define (collection-list collection . limit)
  (when (> (length limit) 1) (error "collection-list expects at most one limit"))
  (let ([maximum (if (null? limit) 2000 (car limit))])
    (unless (and (integer? maximum) (> maximum 0))
      (error "collection-list limit must be a positive integer"))
    (let loop ([offset 0] [reversed '()] [expected-total #false] [revision #false])
      (let* ([page (rna-items collection offset (min 100 (- maximum offset)) revision)]
             [items (hash-try-get page "items")]
             [total (hash-try-get page "total")]
             [more (hash-try-get page "has_more")]
             [next (+ offset (length items))]
             [all (append (reverse items) reversed)])
        (when (and expected-total total (not (= expected-total total)))
          (error "collection changed during pagination; restart inspection"))
        (cond [(not more) (reverse all)]
              [(>= next maximum) (error "collection exceeds limit; use rna-items for explicit pages")]
              [(= next offset) (error "collection pagination made no progress")]
              [else (loop next all total (hash-try-get page "collection_revision"))])))))

(define (rna-set-command reference attribute value)
  (hash "operation" "rna_set" "reference" reference "attribute" attribute "value" value))

(define (rna-get-command reference attribute)
  (hash "operation" "rna_get" "reference" reference "attribute" attribute))

(define (names-of collection)
  (map (lambda (item) (rna-get item "name")) (collection-list collection)))

(define (object-names) (names-of (bpy-data "objects")))

;; ------------------------------------------------------------------- creation
;;
;; Operators return their status, never the thing they made, so each of these takes
;; the freshly created object from the context. That is the single most common
;; stumbling block when driving Blender, and it belongs here rather than in a note.

(define (created) (active-object))

(define (add-cube . args)
  (op-call "mesh.primitive_cube_add" (if (null? args) (hash) (car args)))
  (created))

(define (add-sphere . args)
  (op-call "mesh.primitive_uv_sphere_add" (if (null? args) (hash) (car args)))
  (created))

(define (add-plane . args)
  (op-call "mesh.primitive_plane_add" (if (null? args) (hash) (car args)))
  (created))

(define (add-cylinder . args)
  (op-call "mesh.primitive_cylinder_add" (if (null? args) (hash) (car args)))
  (created))

(define (add-grid subdivisions size location)
  (op-call "mesh.primitive_grid_add"
           (hash "x_subdivisions" subdivisions
                 "y_subdivisions" subdivisions
                 "size" size
                 "location" location))
  (created))

(define (add-camera location)
  (op-call "object.camera_add" (hash "location" location))
  (created))

(define (add-light kind location)
  (op-call "object.light_add" (hash "type" kind "location" location))
  (created))

(define (delete-object! object)
  (rna-call (bpy-data "objects") "remove" (list object) (hash)))

;; ------------------------------------------------------------------ transforms

(define (object-location object) (rna-get object "location"))

(define (set-location! object x y z)
  (rna-set! object "location" (list x y z)))

(define (move! object dx dy dz)
  (let ([here (object-location object)])
    (set-location! object
                   (+ (list-ref here 0) dx)
                   (+ (list-ref here 1) dy)
                   (+ (list-ref here 2) dz))))

;; Degrees in, radians out.
(define (set-rotation! object rx ry rz)
  (rna-set! object "rotation_euler"
            (list (degrees->radians rx) (degrees->radians ry) (degrees->radians rz))))

(define (object-rotation object)
  (map radians->degrees (rna-get object "rotation_euler")))

(define (set-scale! object sx sy sz)
  (rna-set! object "scale" (list sx sy sz)))

(define (rename! object name) (rna-set! object "name" name))

;; Point `object` at a point, the way a camera or a light wants to be aimed.
;;
;; A Blender camera looks down its own -Z. Rotating by X then Z sends that to
;; (-sin z * sin x, cos z * sin x, -cos x), so matching it to the direction vector
;; gives x = atan2(flat, -dz) and z = atan2(-dx, dy). The z term is emphatically not
;; atan2(dy, dx), which is a quarter turn out and aims at nothing.
(define (look-at! object target)
  (let* ([here (object-location object)]
         [dx (- (list-ref target 0) (list-ref here 0))]
         [dy (- (list-ref target 1) (list-ref here 1))]
         [dz (- (list-ref target 2) (list-ref here 2))]
         [flat (hypot dx dy)])
    (rna-set! object "rotation_euler"
              (list (atan2 flat (- dz)) 0.0 (atan2 (- dx) dy)))))

;; The unit vector an object is actually facing, derived from its euler angles. Mostly
;; useful for confirming that an aim did what you meant.
(define (facing object)
  (let* ([angles (rna-get object "rotation_euler")]
         [x (list-ref angles 0)]
         [z (list-ref angles 2)])
    (list (* (- (sin z)) (sin x))
          (* (cos z) (sin x))
          (- (cos x)))))

;; ------------------------------------------------------------------- modifiers

(define (add-modifier! object name kind)
  (rna-call (rna-get object "modifiers") "new" (list name kind) (hash)))

(define (add-geometry-nodes! object name group)
  (let ([modifier (add-modifier! object name "NODES")])
    (rna-set! modifier "node_group" group)
    modifier))

;; ------------------------------------------------------- materials and node trees

(define (make-material name)
  (let ([material (rna-call (bpy-data "materials") "new" (list name) (hash))])
    (rna-set! material "use_nodes" #true)
    material))

(define (assign-material! object material)
  (rna-call (rna-get (rna-get object "data") "materials") "append" (list material) (hash)))

(define (material-tree material) (rna-get material "node_tree"))
(define (world-tree) (rna-get (rna-get (scene) "world") "node_tree"))

(define (node-find tree name) (collection-get (rna-get tree "nodes") name))
(define (principled material) (node-find (material-tree material) "Principled BSDF"))

(define (node-add tree kind)
  (rna-call (rna-get tree "nodes") "new" (list kind) (hash)))

(define (node-remove! tree node)
  (rna-call (rna-get tree "nodes") "remove" (list node) (hash)))

(define (node-input node name) (collection-get (rna-get node "inputs") name))
(define (node-output node name) (collection-get (rna-get node "outputs") name))

;; Set an unconnected input socket's value.
(define (node-set! node socket value)
  (rna-set! (node-input node socket) "default_value" value))

(define (node-at! node x y) (rna-set! node "location" (list x y)))

(define (node-link! tree from-node from-socket to-node to-socket)
  (rna-call (rna-get tree "links") "new"
            (list (node-output from-node from-socket)
                  (node-input to-node to-socket))
            (hash)))

;; ------------------------------------------------------------- render settings

(define (render-settings) (rna-get (scene) "render"))
(define (cycles-settings) (rna-get (scene) "cycles"))

(define (set-engine! name) (rna-set! (render-settings) "engine" name))

(define (set-resolution! width height)
  (let ([settings (render-settings)])
    (rna-set! settings "resolution_x" width)
    (rna-set! settings "resolution_y" height)))

(define (set-samples! count) (rna-set! (cycles-settings) "samples" count))

(define (set-camera! object) (rna-set! (scene) "camera" object))

;; A cheap look before committing to a full frame: renders at `percent` of the
;; configured resolution. Restore it on success and ordinary evaluation errors.
(define (preview! path percent)
  (let* ([settings (render-settings)]
         [before (rna-get settings "resolution_percentage")])
    (dynamic-wind
      (lambda () (rna-set! settings "resolution_percentage" percent))
      (lambda () (render-file! path))
      (lambda () (rna-set! settings "resolution_percentage" before)))))

;; ------------------------------------------------------------------- convenience

;; Guard a speculative call: returns `fallback` instead of aborting the evaluation.
(define (try thunk fallback)
  (call-with-exception-handler (lambda (error) fallback) thunk))

;; Split a list into pieces of at most `size`.
;;
;; Arguments travel to Blender through the same 10,000-item budget as results come
;; back through, so one call cannot carry a large buffer: a mesh of a few thousand
;; vertices is already three numbers each. Build it in pieces.
(define (chunk items size)
  (unless (and (integer? size) (> size 0))
    (error "chunk size must be a positive integer"))
  (if (null? items)
      '()
      (let ([take-count (min size (length items))])
        (cons (take items take-count)
              (chunk (drop items take-count) size)))))

;; Points on a circle, for arranging things without writing the trigonometry again.
(define (ring count radius height)
  (map (lambda (index)
         (let ([angle (* tau (/ (exact->inexact index) (exact->inexact count)))])
           (list (* radius (cos angle)) (* radius (sin angle)) height)))
       (range 0 count)))
