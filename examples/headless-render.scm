; Build a small scene and render it. The render comes back as an MCP image block.

(bpy/ops/mesh/primitive_cube_add (hash "size" 2.0))
(bpy/ops/mesh/primitive_uv_sphere_add (hash "location" (list 3.0 0.0 0.0)))

; Smooth shading applies to the active object, which is the sphere just added.
(bpy/ops/object/shade_smooth)

(scene-summary)

; (render!) writes to a server-managed temporary file; (render-to! path) picks one.
(render!)
