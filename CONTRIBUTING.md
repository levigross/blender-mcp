# Contributing

Use a Linux Git checkout with Nix flakes enabled. The flake pins the Rust
toolchain, Blender, and validation tools. `nix develop` provides editor and
formatting tools; compilation and tests run through Nix derivations.

## Source layout

- `crates/blender-mcp-server`: MCP HTTP service, resources, Steel worker, and
  Scheme bindings. `src/scheme/stdlib.scm` is the embedded helper library.
- `crates/blender-mcp-transport`: live and headless Blender connections.
- `crates/blender-mcp-protocol`: shared bridge messages and framing.
- `crates/blender-mcp-native`: Blender-side RNA operations, reference handles,
  scene inspection, and artifacts, loaded as a Python extension.
- `blender_extension`: Blender registration, main-thread dispatch, and lifecycle.
- `docs` and `examples`: embedded reference material and executable Scheme examples.
- `scripts`: launchers and packaged Blender/MCP integration checks.

Keep Blender API execution on Blender's main thread. Scheme state belongs to
one persistent worker per named session and is shared by clients selecting it.
Changes to either boundary should include regression coverage and update the
relevant reference docs.

## Build and verify

Run the smallest relevant checks first. For example, on `x86_64-linux`:

```sh
nix build --no-link \
  .#checks.x86_64-linux.cargo-test \
  .#checks.x86_64-linux.cargo-test-native
nix build --no-link .#checks.x86_64-linux.blender-api
nix build --no-link .#checks.x86_64-linux.mcp-headless
```

Replace `x86_64-linux` with `aarch64-linux` on an ARM Linux machine. The
`blender-api` and `mcp-headless` checks use CPU Blender. To build that package:

```sh
nix build .#blender-mcp-cpu
```

The complete local gate also builds the default CUDA Blender package:

```sh
nix flake check . -L
```

Use `nix develop -c cargo fmt --all` for Rust formatting. Do not run direct
Cargo builds or tests: the flake supplies the native module and Blender runtime
needed for representative verification.

GitHub CI runs formatting, Clippy, documentation, dependency checks, Rust
tests, Python and shell lint, source layout, extension packaging, and CPU
Blender integration. It does not verify CUDA rendering, the default GPU
package, graphical interaction, or ARM execution. Report those checks
separately when a change needs them.

## Pull requests and bug reports

Describe the concrete problem, resulting behavior, and exact checks run.
For bugs, include a minimal Scheme reproducer, live/headless mode, Blender
version, and expected versus observed output. Keep changes focused and preserve
existing API behavior unless the pull request explicitly changes it.

Keep private scenes, local transcripts, credentials, and generated binaries out
of patches. See [SECURITY.md](SECURITY.md) for the trust boundary and handling
of vulnerability reports. The server/transport/protocol crates use Apache-2.0;
the native module and Blender extension use GPL-3.0-or-later. Preserve the
license declarations of the components you change.
