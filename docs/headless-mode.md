# Headless Blender mode

`--backend headless` starts the flake-provided Blender with `--background` and
the packaged bootstrap. The optional `.blend` is loaded before the bootstrap.
The bootstrap dispatches on Blender's main thread and uses the same operation
implementation as the live extension.

Rust captures bounded stdout/stderr, waits for bridge readiness, detects early
exit, and owns graceful/forced shutdown. Use `GET /healthz` to distinguish the
HTTP server, Steel worker, bridge connection, and child process states.

Background tests use `--factory-startup` and isolated Blender user directories
so installed user extensions and preferences cannot affect catalog results.

`just serve` resolves the current Git-flake package, prints its store path, and
removes its temporary Blender profile after the owned server exits. The
`mcp-headless` Nix check runs the same package wrapper with pinned CPU Blender;
it verifies persistence through HTTP, RNA calls, and actual PNG artifacts.
