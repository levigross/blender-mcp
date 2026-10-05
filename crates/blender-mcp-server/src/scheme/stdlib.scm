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

;; Custom properties (obj["key"]) for batch!, e.g. tagging many objects for export.
(define (prop-set-command reference key value)
  (hash "operation" "id_property_set" "reference" reference "key" key "value" value))

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

;; -------------------------------------------------------------------- matrices
;;
;; Blender hands a matrix over as four rows of four. Every matrix here is affine, so
;; inverting one is a 3x3 inverse plus a moved origin -- no general 4x4 solver needed.

(define (matrix-ref matrix row column) (list-ref (list-ref matrix row) column))

;; Transform a point (not a direction) by an affine matrix.
(define (matrix-apply matrix point)
  (map (lambda (row)
         (+ (* (list-ref row 0) (list-ref point 0))
            (* (list-ref row 1) (list-ref point 1))
            (* (list-ref row 2) (list-ref point 2))
            (list-ref row 3)))
       (take matrix 3)))

(define (matrix-invert-affine matrix)
  (let* ([a (matrix-ref matrix 0 0)] [b (matrix-ref matrix 0 1)] [c (matrix-ref matrix 0 2)]
         [d (matrix-ref matrix 1 0)] [e (matrix-ref matrix 1 1)] [f (matrix-ref matrix 1 2)]
         [g (matrix-ref matrix 2 0)] [h (matrix-ref matrix 2 1)] [i (matrix-ref matrix 2 2)]
         [det (exact->inexact
                (+ (* a (- (* e i) (* f h)))
                   (- (* b (- (* d i) (* f g))))
                   (* c (- (* d h) (* e g)))))])
    (when (< (abs det) 1e-12)
      (error "matrix is singular; an object with zero scale cannot be a parent"))
    (let* ([inverse (list (list (/ (- (* e i) (* f h)) det)
                                (/ (- (* c h) (* b i)) det)
                                (/ (- (* b f) (* c e)) det))
                          (list (/ (- (* f g) (* d i)) det)
                                (/ (- (* a i) (* c g)) det)
                                (/ (- (* c d) (* a f)) det))
                          (list (/ (- (* d h) (* e g)) det)
                                (/ (- (* b g) (* a h)) det)
                                (/ (- (* a e) (* b d)) det)))]
           [origin (list (matrix-ref matrix 0 3) (matrix-ref matrix 1 3) (matrix-ref matrix 2 3))]
           [moved (map (lambda (row) (- (apply + (map * row origin)))) inverse)])
      (list (append (list-ref inverse 0) (list (list-ref moved 0)))
            (append (list-ref inverse 1) (list (list-ref moved 1)))
            (append (list-ref inverse 2) (list (list-ref moved 2)))
            (list 0.0 0.0 0.0 1.0)))))

(define (normalize vector)
  (let ([size (sqrt (apply + (map * vector vector)))])
    (if (< size 1e-12)
        (error "cannot normalize a zero-length vector")
        (map (lambda (component) (/ component size)) vector))))

;; ------------------------------------------------------------------- selection
;;
;; Many operators act on "the selection and the active object" rather than on an
;; argument. These make that implicit state explicit before each such call.

(define (view-layer) (rna-get (context-ref) "view_layer"))

;; `matrix_world` and bounds are only recomputed when the depsgraph updates, which an
;; RNA write alone does not trigger. Anything that reads them after moving an object
;; needs this first.
(define (refresh!) (rna-call (view-layer) "update" '() (hash)))

(define (selected-objects) (rna-get (context-ref) "selected_objects"))
(define (select! object) (rna-call object "select_set" (list #true) (hash)) object)
(define (deselect! object) (rna-call object "select_set" (list #false) (hash)) object)
(define (deselect-all!) (for-each deselect! (selected-objects)))

(define (set-active! object)
  (rna-set! (rna-get (view-layer) "objects") "active" object)
  object)

(define (select-only! object)
  (deselect-all!)
  (select! object)
  (set-active! object))

;; ---------------------------------------------------------- scene organisation

(define (add-empty kind location)
  (op-call "object.empty_add" (hash "type" kind "location" location))
  (created))

;; Parent without moving the child, the way Ctrl+P "Object" does: the parent inverse
;; cancels whatever transform the parent has right now.
(define (parent! child parent)
  (refresh!)
  (rna-set! child "parent" parent)
  (rna-set! child "matrix_parent_inverse"
            (matrix-invert-affine (rna-get parent "matrix_world")))
  child)

;; Clear the parent and keep the world-space placement.
(define (unparent! child)
  (refresh!)
  (let ([world (rna-get child "matrix_world")])
    (rna-set! child "parent" (void))
    (rna-set! child "matrix_world" world))
  child)

;; A new collection, linked under `parent` or under the scene's root collection.
(define (make-collection! name . parent)
  (let ([collection (rna-call (bpy-data "collections") "new" (list name) (hash))]
        [owner (if (null? parent) (rna-get (scene) "collection") (car parent))])
    (rna-call (rna-get owner "children") "link" (list collection) (hash))
    collection))

;; Blender computes `users_collection` on demand and hands back a plain list, not a
;; collection handle; accept either so a future change cannot break this.
(define (object-collections object)
  (let ([owners (rna-get object "users_collection")])
    (if (list? owners) owners (collection-list owners))))

;; Move rather than add: the object leaves every collection it was in.
(define (move-to-collection! object collection)
  (for-each (lambda (owner)
              (rna-call (rna-get owner "objects") "unlink" (list object) (hash)))
            (object-collections object))
  (rna-call (rna-get collection "objects") "link" (list object) (hash))
  object)

(define (hide! object hidden)
  (rna-set! object "hide_viewport" hidden)
  (rna-set! object "hide_render" hidden)
  object)

;; A copy placed in the same collections. By default the copy gets its own mesh (or
;; other data), like Shift+D; pass #true to share it, like Alt+D -- far cheaper when
;; scattering many copies of the same thing.
(define (duplicate! object . linked)
  (let ([copy (rna-call object "copy" '() (hash))]
        [data (rna-get object "data")])
    (when (and (or (null? linked) (not (car linked))) (exists? data))
      (rna-set! copy "data" (rna-call data "copy" '() (hash))))
    (for-each (lambda (owner)
                (rna-call (rna-get owner "objects") "link" (list copy) (hash)))
              (object-collections object))
    copy))

;; Place a whole collection again as a single instance.
(define (instance-collection! collection location)
  (let ([empty (add-empty "PLAIN_AXES" location)])
    (rna-set! empty "instance_type" "COLLECTION")
    (rna-set! empty "instance_collection" collection)
    empty))

;; `count` objects in a line, the original first, each `offset` (x y z) from the last.
;; The copies share the original's data. Returns only the copies.
(define (array-along! object count offset)
  (let ([origin (object-location object)])
    (map (lambda (index)
           (let ([copy (duplicate! object #true)])
             (apply set-location! copy
                    (map (lambda (start step) (+ start (* index step))) origin offset))
             copy))
         (range 1 count))))

;; Shared-data copies on a ring around the origin. The original stays where it is.
(define (scatter-ring! object count radius height)
  (map (lambda (point)
         (let ([copy (duplicate! object #true)])
           (apply set-location! copy point)
           copy))
       (ring count radius height)))

;; ------------------------------------------------------------ mesh modifiers
;;
;; Modifiers first: they are non-destructive, they need no edit-mode context, and one
;; property write changes the result. Each helper returns its modifier for tuning.

(define (bevel! object width segments)
  (let ([modifier (add-modifier! object "Bevel" "BEVEL")])
    (rna-set! modifier "width" width)
    (rna-set! modifier "segments" segments)
    modifier))

(define (subdivide! object levels)
  (let ([modifier (add-modifier! object "Subdivision" "SUBSURF")])
    (rna-set! modifier "levels" levels)
    (rna-set! modifier "render_levels" levels)
    modifier))

(define (solidify! object thickness)
  (let ([modifier (add-modifier! object "Solidify" "SOLIDIFY")])
    (rna-set! modifier "thickness" thickness)
    modifier))

;; `offset` is relative to the object's size: (1 0 0) places copies edge to edge in X.
(define (array! object count offset)
  (let ([modifier (add-modifier! object "Array" "ARRAY")])
    (rna-set! modifier "count" count)
    (rna-set! modifier "relative_offset_displace" offset)
    modifier))

;; Mirror across the object's own axes: (mirror! obj "X" "Y"). X alone by default.
(define (mirror! object . axes)
  (let ([modifier (add-modifier! object "Mirror" "MIRROR")]
        [chosen (if (null? axes) (list "X") axes)])
    (rna-set! modifier "use_axis"
              (map (lambda (axis)
                     (not (null? (filter (lambda (wanted) (equal? wanted axis)) chosen))))
                   (list "X" "Y" "Z")))
    modifier))

;; Cut, join, or intersect with `cutter`, which is then shown as a wire and left out
;; of renders so only the result is visible.
(define (boolean! object cutter operation)
  (let ([modifier (add-modifier! object "Boolean" "BOOLEAN")])
    (rna-set! modifier "operation" operation)
    (rna-set! modifier "object" cutter)
    (rna-set! cutter "display_type" "WIRE")
    (rna-set! cutter "hide_render" #true)
    modifier))

(define (weld! object distance)
  (let ([modifier (add-modifier! object "Weld" "WELD")])
    (rna-set! modifier "merge_threshold" distance)
    modifier))

;; ------------------------------------------------------- destructive mesh edits
;;
;; These are operators, so each one selects its target first.

(define (modifier-name modifier)
  (if (string? modifier) modifier (rna-get modifier "name")))

(define (apply-modifier! object modifier)
  (let ([name (modifier-name modifier)])
    (select-only! object)
    (op-call "object.modifier_apply" (hash "modifier" name))
    object))

(define (apply-transforms! object)
  (select-only! object)
  (op-call "object.transform_apply" (hash "location" #true "rotation" #true "scale" #true))
  object)

(define (shade-smooth! object)
  (select-only! object)
  (op-call "object.shade_smooth" (hash))
  object)

(define (shade-flat! object)
  (select-only! object)
  (op-call "object.shade_flat" (hash))
  object)

;; kind: "ORIGIN_GEOMETRY", "ORIGIN_CURSOR", "ORIGIN_CENTER_OF_MASS", or
;; "GEOMETRY_ORIGIN" (moves the mesh to the origin instead).
(define (set-origin! object kind)
  (select-only! object)
  (op-call "object.origin_set" (hash "type" kind))
  object)

;; Merge `objects` into the first one, which is returned; the rest cease to exist.
(define (join! objects)
  (deselect-all!)
  (for-each select! objects)
  (set-active! (car objects))
  (op-call "object.join" (hash))
  (car objects))

;; Run `thunk` in edit mode on `object`, returning to object mode afterwards even if
;; it fails. Mode changes invalidate mesh sub-data handles: fetch them again after.
(define (with-edit-mode object thunk)
  (select-only! object)
  (dynamic-wind
    (lambda () (op-call "object.mode_set" (hash "mode" "EDIT")))
    thunk
    (lambda () (op-call "object.mode_set" (hash "mode" "OBJECT")))))

;; Inset every face by `thickness`, raising the inset by `depth` -- a panelled or
;; extruded look. A worked edit-mode example; mesh.inset needs no 3D viewport.
(define (inset-faces! object thickness depth)
  (with-edit-mode object
    (lambda ()
      (op-call "mesh.select_all" (hash "action" "SELECT"))
      (op-call "mesh.inset" (hash "thickness" thickness "depth" depth "use_individual" #true))))
  object)

;; ------------------------------------------------------------ mesh from data
;;
;; `mesh-from-data!` (native) builds an object from vertices and faces in one call.
;; The generators below produce its input, so they are plain lists you can inspect,
;; transform, or combine before anything reaches Blender.

;; A closed solid from a counter-clockwise 2D outline ((x y) ...), rising `height`
;; from z = 0. Floor plans, letters, gears: any simple polygon.
(define (extrusion-data outline height)
  (let* ([count (length outline)]
         [at (lambda (z) (map (lambda (point) (list (list-ref point 0) (list-ref point 1) z))
                              outline))]
         [walls (map (lambda (index)
                       (let ([next (if (= (+ index 1) count) 0 (+ index 1))])
                         (list index next (+ next count) (+ index count))))
                     (range 0 count))])
    (when (< count 3) (error "an outline needs at least three points"))
    (list (append (at 0.0) (at (exact->inexact height)))
          (cons (reverse (range 0 count))
                (cons (range count (* 2 count)) walls)))))

(define (make-extrusion name outline height . collection)
  (let ([data (extrusion-data outline height)])
    (apply mesh-from-data! name (list-ref data 0) (list-ref data 1) collection)))

(define (prism-data sides radius height)
  (extrusion-data (map (lambda (point) (take point 2)) (ring sides radius 0.0)) height))

(define (make-prism name sides radius height . collection)
  (let ([data (prism-data sides radius height)])
    (apply mesh-from-data! name (list-ref data 0) (list-ref data 1) collection)))

;; A `size`-square grid centred on the origin whose z is (height-at x y). About 30 x 30
;; cells fits in one call; tile larger terrain into several objects.
(define (heightfield-data columns rows size height-at)
  (let* ([extent (exact->inexact size)]
         [half (/ extent 2.0)]
         [width (+ columns 1)]
         [vertices (apply append
                          (map (lambda (row)
                                 (map (lambda (column)
                                        (let ([x (- (* column (/ extent columns)) half)]
                                              [y (- (* row (/ extent rows)) half)])
                                          (list x y (exact->inexact (height-at x y)))))
                                      (range 0 width)))
                               (range 0 (+ rows 1))))]
         [faces (apply append
                       (map (lambda (row)
                              (map (lambda (column)
                                     (let ([corner (+ (* row width) column)])
                                       (list corner (+ corner 1)
                                             (+ corner 1 width) (+ corner width))))
                                   (range 0 columns)))
                            (range 0 rows)))])
    (list vertices faces)))

(define (make-heightfield name columns rows size height-at . collection)
  (let ([data (heightfield-data columns rows size height-at)])
    (apply mesh-from-data! name (list-ref data 0) (list-ref data 1) collection)))

;; ------------------------------------------------------------------ bulk data
;;
;; `collection-read` / `collection-write!` (native) move one attribute of a whole
;; collection in pages. These page automatically, at most 3,000 values per bridge
;; call, so a mesh of any size can be read or rewritten.

;; Every value of `attribute` across `collection`, flattened (vertex co: x y z x y z ...).
(define (collection-values collection attribute)
  (let* ([probe (collection-read collection attribute 0 1)]
         [total (hash-ref probe "total")]
         [page (max 1 (quotient 3000 (max 1 (hash-ref probe "stride"))))])
    (let loop ([offset 0] [pages '()])
      (if (>= offset total)
          (apply append (reverse pages))
          (loop (+ offset page)
                (cons (hash-ref (collection-read collection attribute offset page) "values")
                      pages))))))

;; Vertex positions of a mesh object, ((x y z) ...), in the object's local space.
(define (mesh-positions object)
  (chunk (collection-values (rna-get (rna-get object "data") "vertices") "co") 3))

;; Replace vertex positions from ((x y z) ...); the count must match the mesh.
(define (mesh-set-positions! object points)
  (let ([vertices (rna-get (rna-get object "data") "vertices")])
    (let loop ([offset 0] [pieces (chunk points 1000)])
      (unless (null? pieces)
        (collection-write! vertices "co" offset (apply append (car pieces)))
        (loop (+ offset (length (car pieces))) (cdr pieces))))
    (length points)))

;; Faces as lists of vertex indices, in the order mesh-from-data! takes them.
(define (mesh-faces object)
  (let* ([mesh (rna-get object "data")]
         [starts (collection-values (rna-get mesh "polygons") "loop_start")]
         [sizes (collection-values (rna-get mesh "polygons") "loop_total")]
         [corners (list->vector (collection-values (rna-get mesh "loops") "vertex_index"))])
    (map (lambda (start size)
           (map (lambda (corner) (vector-ref corners (+ start corner))) (range 0 size)))
         starts sizes)))

;; ------------------------------------------------------------------ animation

(define (set-frame! frame) (rna-call (scene) "frame_set" (list frame) (hash)))

(define (frame-range! start end)
  (let ([current (scene)])
    (rna-set! current "frame_start" start)
    (rna-set! current "frame_end" end)))

(define (set-fps! fps) (rna-set! (render-settings) "fps" fps))

(define (keyframe! object path frame)
  (rna-call object "keyframe_insert" (list path) (hash "frame" frame)))

(define (key-location! object frame x y z)
  (set-location! object x y z)
  (keyframe! object "location" frame))

;; Degrees, like set-rotation!.
(define (key-rotation! object frame rx ry rz)
  (set-rotation! object rx ry rz)
  (keyframe! object "rotation_euler" frame))

(define (key-scale! object frame sx sy sz)
  (set-scale! object sx sy sz)
  (keyframe! object "scale" frame))

(define (clear-animation! object) (rna-call object "animation_data_clear" '() (hash)))

;; The F-curves animating `object`. Blender 4.4+ keeps them in layered actions --
;; action -> layers -> strips -> the channelbag for this object's slot -- and
;; `action.fcurves` no longer exists, so this is the walk to use.
(define (object-fcurves object)
  (let ([animation (rna-get object "animation_data")])
    (if (not (exists? animation))
        '()
        (let ([action (rna-get animation "action")]
              [slot (rna-get animation "action_slot")])
          (if (or (not (exists? action)) (not (exists? slot)))
              '()
              (apply append
                     (map (lambda (layer)
                            (apply append
                                   (map (lambda (strip)
                                          (let ([bag (rna-call strip "channelbag" (list slot) (hash))])
                                            (if (exists? bag)
                                                (collection-list (rna-get bag "fcurves"))
                                                '())))
                                        (collection-list (rna-get layer "strips")))))
                          (collection-list (rna-get action "layers")))))))))

;; Set every key's interpolation ("LINEAR", "CONSTANT", "BEZIER"), optionally only on
;; curves for one data path such as "rotation_euler". Writes go out 100 per batch.
(define (set-interpolation! object mode . path)
  (for-each
    (lambda (curve)
      (when (or (null? path) (equal? (rna-get curve "data_path") (car path)))
        (for-each (lambda (piece)
                    (batch! (map (lambda (point) (rna-set-command point "interpolation" mode))
                                 piece)))
                  (chunk (collection-list (rna-get curve "keyframe_points")) 100))))
    (object-fcurves object)))

;; One full turn about Z between `start` and `end` at constant speed.
(define (turntable! object start end)
  (let* ([angles (object-rotation object)]
         [x (list-ref angles 0)] [y (list-ref angles 1)] [z (list-ref angles 2)])
    (key-rotation! object start x y z)
    (key-rotation! object end x y (+ z 360.0))
    (set-interpolation! object "LINEAR" "rotation_euler")
    object))

;; Circle `camera` around `target` (x y z) once between `start` and `end`, `radius`
;; out and `height` up. Returns the pivot empty that carries the motion.
(define (orbit-camera! camera target radius height start end)
  (let ([pivot (add-empty "PLAIN_AXES" target)])
    (rename! pivot "Camera Orbit")
    (set-location! camera
                   (list-ref target 0)
                   (- (list-ref target 1) radius)
                   (+ (list-ref target 2) height))
    (look-at! camera target)
    (parent! camera pivot)
    (turntable! pivot start end)
    pivot))

;; ----------------------------------------------------------------- lighting

(define (set-light! light power color)
  (let ([data (rna-get light "data")])
    (rna-set! data "energy" power)
    (rna-set! data "color" color)
    light))

;; A sun `elevation` degrees above the horizon, standing at `azimuth` degrees counter-
;; clockwise from +X, shining at the origin. `strength` is in W/m^2 (3-5 is daylight).
(define (sun! strength elevation azimuth)
  (let* ([up (degrees->radians elevation)]
         [around (degrees->radians azimuth)]
         [sun (add-light "SUN" (list (* 10.0 (cos around) (cos up))
                                     (* 10.0 (sin around) (cos up))
                                     (* 10.0 (sin up))))])
    (rna-set! (rna-get sun "data") "energy" strength)
    (look-at! sun (list 0.0 0.0 0.0))
    sun))

;; Where a classic three-point rig stands around `target`, seen from a camera on the
;; -Y side: key front-right and high, fill front-left and low, rim behind.
;; Each entry is (name bearing-degrees elevation-degrees power-fraction).
(define three-point-rig
  (list (list "Key" 45.0 35.0 1.0)
        (list "Fill" -50.0 15.0 0.35)
        (list "Rim" 160.0 50.0 0.6)))

(define (rig-position target distance bearing elevation)
  (let ([around (degrees->radians bearing)]
        [up (degrees->radians elevation)])
    (list (+ (list-ref target 0) (* distance (sin around) (cos up)))
          (- (list-ref target 1) (* distance (cos around) (cos up)))
          (+ (list-ref target 2) (* distance (sin up))))))

;; Three area lights aimed at `target` from `distance` away. Power scales with the
;; square of the distance so the subject is lit the same however far out they stand.
;; Returns (key fill rim).
(define (three-point-lights! target distance)
  (map (lambda (entry)
         (let ([light (add-light "AREA" (rig-position target distance
                                                      (list-ref entry 1) (list-ref entry 2)))])
           (rename! light (list-ref entry 0))
           (rna-set! (rna-get light "data") "energy"
                     (* 40.0 distance distance (list-ref entry 3)))
           (rna-set! (rna-get light "data") "size" (/ distance 3.0))
           (look-at! light target)
           light))
       three-point-rig))

;; The world, created and assigned first if the scene has none.
(define (scene-world)
  (let ([world (rna-get (scene) "world")])
    (if (exists? world)
        world
        (let ([created-world (rna-call (bpy-data "worlds") "new" (list "World") (hash))])
          (rna-set! (scene) "world" created-world)
          created-world))))

(define (world-background)
  (let ([world (scene-world)])
    (try (lambda () (rna-set! world "use_nodes" #true)) #false)
    (node-find (rna-get world "node_tree") "Background")))

;; A flat-colour sky. `rgb` is (r g b) in 0..1.
(define (world-color! rgb strength)
  (let ([background (world-background)])
    (node-set! background "Color" (append rgb (list 1.0)))
    (node-set! background "Strength" strength)
    background))

;; Light the scene from an equirectangular image on disk (.hdr or .exr).
(define (hdri! path strength)
  (let* ([background (world-background)]
         [tree (rna-get (scene-world) "node_tree")]
         [image (rna-call (bpy-data "images") "load" (list path) (hash "check_existing" #true))]
         [texture (node-add tree "ShaderNodeTexEnvironment")])
    (rna-set! texture "image" image)
    (node-at! texture -300 300)
    (node-link! tree texture "Color" background "Color")
    (node-set! background "Strength" strength)
    texture))

;; ----------------------------------------------------------- bounds and camera

;; ((min-x min-y min-z) (max-x max-y max-z)) around some points.
(define (bounds-of points)
  (let ([axis (lambda (pick index) (apply pick (map (lambda (point) (list-ref point index)) points)))])
    (list (map (lambda (index) (axis min index)) (list 0 1 2))
          (map (lambda (index) (axis max index)) (list 0 1 2)))))

;; World-space bounding box, including modifiers, rotation, and parenting.
(define (object-bounds object)
  (refresh!)
  (let ([matrix (rna-get object "matrix_world")])
    (bounds-of (map (lambda (corner) (matrix-apply matrix corner))
                    (rna-get object "bound_box")))))

(define (scene-bounds objects)
  (bounds-of (apply append (map object-bounds objects))))

;; Move `camera` back along its current bearing until every object fits, then aim it.
;; `margin` (default 1.1) leaves room around the edges. Works headless, unlike the
;; viewport operator it replaces.
(define (frame-camera! camera objects . margin)
  (let* ([bounds (scene-bounds objects)]
         [low (list-ref bounds 0)]
         [high (list-ref bounds 1)]
         [center (map (lambda (a b) (/ (+ a b) 2.0)) low high)]
         [radius (* 0.5 (sqrt (apply + (map (lambda (a b) (* (- b a) (- b a))) low high))))]
         [scale (if (null? margin) 1.1 (car margin))]
         [data (rna-get camera "data")]
         [settings (render-settings)]
         [width (rna-get settings "resolution_x")]
         [height (rna-get settings "resolution_y")]
         ;; `angle` covers the longer side; the shorter side is what limits framing.
         [half-wide (/ (rna-get data "angle") 2.0)]
         [half-narrow (atan2 (* (tan half-wide) (min width height)) (max width height))]
         [distance (/ (* (max radius 1e-3) scale) (sin half-narrow))]
         [offset (map - (object-location camera) center)]
         [bearing (if (< (sqrt (apply + (map * offset offset))) 1e-6)
                      (normalize (list 0.0 -1.0 0.5))
                      (normalize offset))])
    (apply set-location! camera (map (lambda (c d) (+ c (* distance d))) center bearing))
    (look-at! camera center)
    (when (> (* 2.0 distance) (rna-get data "clip_end"))
      (rna-set! data "clip_end" (* 4.0 distance)))
    camera))

(define (set-lens! camera millimetres)
  (rna-set! (rna-get camera "data") "lens" millimetres)
  camera)

(define (depth-of-field! camera focus-distance fstop)
  (let ([dof (rna-get (rna-get camera "data") "dof")])
    (rna-set! dof "use_dof" #true)
    (rna-set! dof "focus_distance" focus-distance)
    (rna-set! dof "aperture_fstop" fstop)
    camera))

;; ------------------------------------------------------------- render output

;; view: "AgX", "Filmic", "Standard"; look: "None" or one the view offers.
(define (color-management! view look exposure)
  (let ([settings (rna-get (scene) "view_settings")])
    (rna-set! settings "view_transform" view)
    (rna-set! settings "look" look)
    (rna-set! settings "exposure" exposure)))

;; format: "PNG", "JPEG", "OPEN_EXR", ...
(define (set-output! path format)
  (let ([settings (render-settings)])
    (rna-set! settings "filepath" path)
    (rna-set! (rna-get settings "image_settings") "file_format" format)))

;; "draft": fast and noisy, for checking composition. "final": clean.
(define (render-preset! name)
  (let ([settings (render-settings)])
    (cond [(equal? name "draft")
           (set-engine! "CYCLES")
           (set-samples! 16)
           (rna-set! (cycles-settings) "use_denoising" #false)
           (rna-set! settings "resolution_percentage" 50)]
          [(equal? name "final")
           (set-engine! "CYCLES")
           (set-samples! 256)
           (rna-set! (cycles-settings) "use_denoising" #true)
           (rna-set! settings "resolution_percentage" 100)]
          [else (error "render-preset! expects \"draft\" or \"final\"")])))
