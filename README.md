# blender-mcp

`blender-mcp` is a Scheme-first MCP server for Blender. It exposes exactly one
MCP tool, `scheme_eval`, and provides Blender operators and RNA access as Steel
Scheme functions. Each named Blender session has a persistent, FIFO-serialized
Steel environment shared by clients selecting that session until reset or restart.

The project supports a live Blender extension and a server-managed headless
Blender process. The default HTTP endpoint is Streamable HTTP at
`http://127.0.0.1:8000/mcp`; health is available at `/healthz`.

## Requirements and first run

The Nix flake provides packages for `x86_64-linux` and `aarch64-linux`.
Use Linux with Nix's `nix-command` and `flakes` features enabled, and run the
commands below from a Git checkout of this repository. macOS and Windows
packages are not provided. Graphical mode also needs a working desktop session.

Start a CPU-backed headless server with the packaged Blender and extension:

```sh
nix run .#blender-mcp-cpu -- --backend headless
```

For the launcher and development commands below, enter `nix develop` to make
`just` and the pinned tools available. The default package and development shell
include CUDA-enabled Blender; its first build can be substantial. The CPU
package above does not need that build.

## Using it from another project

Run the packaged MCP server directly from GitHub:

```sh
nix run github:levigross/blender-mcp#blender-mcp-cpu -- --backend headless
```

Or add it to your game or AI project's flake. This minimal example provides a
development shell with the server and its pinned Blender runtime:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    blender-mcp.url = "github:levigross/blender-mcp";
  };

  outputs = { nixpkgs, blender-mcp, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = [ blender-mcp.packages.${system}.blender-mcp-cpu ];
      };
    };
}
```

Use `aarch64-linux` for ARM Linux. In an existing flake, add the `blender-mcp`
input and the package to your existing shell. Commit the resulting `flake.lock`
to pin the MCP revision. The package keeps its own tested dependency pins.

Run `nix develop`, then `blender-mcp --backend headless`, and connect your AI
client to `http://127.0.0.1:8000/mcp`. No project checkout or separately installed
Blender is needed at runtime. For live editing, see [live mode](docs/live-mode.md).

Available `packages.${system}` outputs include:

| Output | Use |
| --- | --- |
| `blender-mcp-cpu` | MCP server with CPU Blender and the bridge; suitable for headless automation and CI. |
| `blender-mcp` / `default` | MCP server with CUDA-enabled Blender and the bridge. |
| `blender-extension` | Blender extension directory and installable ZIP. |
| `blender` / `blender-cpu` | The corresponding pinned Blender runtime. |
| `distribution` | CUDA server, extension, documentation, and examples. |

## Multiple agents and Blender instances

Agents selecting the same session share its scene and Scheme definitions, with
serialized evaluations. Independent sessions have separate Blender instances and
workers and can progress concurrently behind the same `/mcp` endpoint.

Existing commands configure the `default` session. Add `--sessions-file sessions.json`
to configure additional headless or live sessions, then pass `session: "assets"`
alongside `code` to `scheme_eval`. Read `resources://blender/sessions` to discover
names, and use the returned artifact URIs to read from the correct session.
See [Shared and independent sessions](docs/sessions.md) for configuration and
coordination rules.

## Reusable Scheme toolkits

Start the server with `--scheme-library DIR` (or `BLENDER_MCP_SCHEME_LIBRARY`) to let
sessions load `DIR/name.scm` with `(use "name")`. The directory is read-only to
clients, names are restricted to letters, digits, `-` and `_`, and loaded code passes
the same sandbox checks as anything sent to `scheme_eval`. Using a file again after
editing it updates functions already defined against it.

## Connecting an MCP client

The server speaks Streamable HTTP, so it must be running before a client
connects. Start it, then point the client at `/mcp`:

```sh
just live      # graphical Blender, extension installed, bridge started
just serve     # server-managed background Blender
just --list    # everything else
```

`just live` is the whole live setup in one command; see
[docs/live-mode.md](docs/live-mode.md) for the equivalent manual steps. Once a
server is up, `just health`, `just tools`, and
`just eval '(scene-summary)'` exercise it from the shell.

`.mcp.json` in this repository registers that endpoint for project-scoped
clients:

```json
{
  "mcpServers": {
    "blender-scheme": {
      "type": "http",
      "url": "http://127.0.0.1:8000/mcp"
    }
  }
}
```

`tools/list` returns exactly one tool, `scheme_eval`. Start from the
`blender-mcp://guide/getting-started` and `blender-mcp://reference/scheme`
resources, and read `blender-mcp://runtime/catalog` for the operator count and
catalog revision of the Blender actually attached.

`resources://blender` provides a discovery index with guides, reference pages,
runtime status, and artifact URI templates. Existing `blender-mcp://` links and
their `resources://blender/` aliases resolve to the same content; see
[Resources](docs/resources.md).

Clients declaring the MCP tasks extension can pass `background: true` to
`scheme_eval`, disconnect, and retrieve the result through `tasks/get`.
`tasks/cancel` requests cooperative cancellation. Ordinary evaluations remain
synchronous. See [Background tasks](docs/tasks.md) for capability negotiation,
retention limits, and the distinction from render jobs.

Use `rna-property-info` and `rna-function-info` for targeted discovery, and
`batch!` for up to 100 ordered operations with explicit partial failures.
`scene-snapshot`, `scene-diff`, `thumbnail!`, and `checkpoint!` support the
inspect/edit/review loop. Render artifacts retain immutable bytes and provenance.

For longer renders, `render-start` returns a job ID that can be inspected with
`job-status`, `job-result`, and `job-cancel` after reconnecting. Queued work honors
deadlines and cancellation; a running Blender operator may continue. Receipts
identify that uncertainty so clients can inspect the outcome before retrying.
See the [Scheme reference](docs/scheme-reference.md) for limits and recovery.

## Development

```sh
nix build .#blender-mcp
nix flake check . -L
```

The flake supplies Rust 1.98, Blender, nextest, cargo-deny, Ruff, and the other
validation tools. The ambient host does not need Blender on `PATH`.
Build and test only through Nix derivations. `just build`, `just test`, `just lint`,
`just deny`, and `scripts/validate.sh` use the flake; they do not create a local Cargo
`target/`. The full gate includes real background Blender checks for collection
traversal, animation, rendering, and extension validation.

`checks.mcp-headless` runs the packaged HTTP server against pinned CPU Blender,
including real MCP calls and image output. `checks.blender-api` exercises native
RNA operations, `checks.bridge` covers queue and lifecycle behavior, and
`checks.source-layout` verifies that Cargo receives its Scheme and embedded docs
without local build products or Python scripts. Python-only bridge edits reuse
the Rust compilation artifacts. `nix build .#blender-mcp-cpu` builds the same
packaged server with CPU Blender for machines and checks that do not need CUDA.

`nix develop` is for editor tools and formatting, not direct Cargo builds or tests.

Setup and API documentation lives in [docs](docs).
See [CONTRIBUTING.md](CONTRIBUTING.md) for the source layout, focused checks,
and pull request guidance. GitHub CI runs the Rust checks and CPU Blender
integration checks; GPU and graphical validation remain separate checks.

## Trust boundary

The Steel runtime does not expose direct filesystem, process, dynamic-library,
or network primitives. Blender itself is intentionally powerful: operators can
open, save, import, export, render, and execute functionality installed by other
enabled extensions. Only run this server for trusted callers and trusted Scheme
code. Non-loopback operation requires an explicit opt-in and bearer token.

## License

The server, transport, and protocol crates are [Apache-2.0](LICENSE).
The Blender extension and its Rust native module (`blender-mcp-native`) are
[GPL-3.0-or-later](LICENSES/GPL-3.0-or-later.txt). The extension ZIP includes
both license texts.
