# Getting started

## Live mode

Build the extension ZIP and server through Nix:

```sh
nix build .#blender-extension
nix build .#blender-mcp
```

Install `scheme_blender_mcp.zip` through Blender's Extensions preferences,
enable it, and start the bridge from the **Scheme MCP** N-panel. Then run:

```sh
nix run . -- --backend live
```

The defaults are Blender bridge `127.0.0.1:9876` and MCP HTTP
`127.0.0.1:8000`.

## Headless mode

```sh
nix run . -- --backend headless
```

Add `--blend-file scene.blend` to load an existing file. The server owns the
background Blender child and shuts it down when the MCP service exits.

## First Scheme calls

```scheme
(blender-status)
(operator-search "cube")
(bpy/ops/mesh/primitive_cube_add)
(scene-summary)
```

Definitions persist:

```scheme
(define cube-count 1)
(set! cube-count (+ cube-count 1))
cube-count
```

Pass `reset: true` to rebuild the environment before evaluating the submitted
code.

## Where to read next

- `blender-mcp://reference/stdlib` — the built-in helpers. Start here: they
  cover creation, placement, materials, nodes and rendering, and save writing a
  toolkit before you can build anything.
- `blender-mcp://reference/scheme` — the `scheme_eval` binding surface.
- `blender-mcp://reference/steel` — the Steel language itself: forms, data
  structures, error handling, and which prelude names you must not shadow.
- `blender-mcp://reference/blender-api` — how `bpy` maps onto the bindings.
  Read this before a first modelling session; it covers the behaviours that
  are hard to guess, such as indexing collections, Blender's `None`, and what
  a bridge round trip costs.
- `blender-mcp://recipes` — worked end-to-end examples.

