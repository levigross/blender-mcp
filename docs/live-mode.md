# Live Blender mode

## Launching

One command starts a graphical Blender with the extension installed and the
bridge running, then serves MCP against it:

```sh
just live                 # throwaway Blender profile
just live scene.blend     # open a file
just live --profile user  # install into your real Blender profile
```

`just live-only` launches Blender without starting the server, so you can run
the server yourself. `BLENDER=/path/to/blender just live` picks a specific
Blender; otherwise the flake's is used.

Each launch resolves the current Git-flake package and prints its Nix store path.
An existing `result` symlink does not select the package. Blender resolution fails
if the pinned build is unavailable; it does not silently choose an ambient
installation. `BLENDER_MCP_BIN` remains an explicit server override.
With `--profile user`, an already loaded extension must match the selected package.
If it differs, update that installation and restart Blender, or use the default
throwaway profile; native code is not hot-reloaded into an existing process.

Once it is up:

```sh
just health                        # server, bridge, Blender, and Steel state
just tools                         # the single scheme_eval tool
just eval '(scene-summary)'
just eval '(bpy/ops/mesh/primitive_cube_add)'
```

### Doing it by hand

`just live` automates what you would otherwise do through the UI:

1. `nix build .#blender-extension`.
2. In Blender, **Edit ▸ Preferences ▸ Add-ons ▸ Install from Disk**, and pick
   `result/share/blender-mcp/scheme_blender_mcp.zip`.
3. Enable **Scheme Blender MCP Bridge**.
4. In the 3D viewport press <kbd>N</kbd>, open the **Scheme MCP** tab, and
   press **Start**.
5. `just serve --backend live` (or `nix run .#blender-mcp -- --backend live`).

The N-panel is also where you stop and restart the bridge, and where the
address, connection count, queue depth, catalog revision, last request, and
last error are reported.

## How it works

The extension's socket threads perform framing and queueing only. All `bpy`
work runs from a persistent `bpy.app.timers` callback on Blender's main thread.
This keeps the viewport user-steerable between MCP calls.

Use the N-panel to start or stop the loopback bridge and inspect its address,
connection count, queue depth, catalog revision, last operation, and last
error. Auto-start is an explicit user preference.

After a Scheme call, edit the scene normally in Blender. The next call observes
the current scene; there is no copied or shadow scene. Loading a file, undoing,
or destructive collection edits can invalidate an RNA reference. Obtain a new
root/reference and retry after a `stale_reference` error.
