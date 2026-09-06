# Tasks for the Scheme-first Blender MCP server.
# `just` with no arguments lists everything.

set shell := ["bash", "-uc"]

# From the environment so it survives into the nested `just _rpc` calls below,
# and matches the knob scripts/live.sh already takes.
bind := env_var_or_default("MCP_BIND", "127.0.0.1:8000")

default:
    @just --list

# Launch a graphical Blender with the bridge started, then serve MCP against it.
# Pass a .blend to open it: `just live scene.blend`
live *ARGS:
    ./scripts/live.sh {{ ARGS }}

# Launch Blender only; start the server yourself.
live-only *ARGS:
    ./scripts/live.sh --no-serve {{ ARGS }}

# Serve MCP against a server-managed background Blender.
serve *ARGS:
    ./scripts/serve.sh {{ ARGS }}

# Stop a running session: the MCP server and the Blender it launched.
stop:
    ./scripts/stop.sh

# The whole cycle after editing anything: stop, rebuild, launch.
# `just live` resolves the current build; relaunch also stops the previous session.
relaunch *ARGS:
    @just stop
    @just build
    ./scripts/live.sh {{ ARGS }}

# Build the server and the packaged extension.
build:
    nix build .#blender-mcp

# The full gate: fmt, clippy, tests, docs, deny, audit, and the extension.
check:
    nix flake check -L

# Compile and test only inside Nix derivations.
test:
    nix build --no-link .#checks.{{arch()}}-linux.cargo-test .#checks.{{arch()}}-linux.cargo-test-native .#checks.{{arch()}}-linux.blender-api

fmt:
    cargo fmt --all

lint:
    nix build --no-link .#checks.{{arch()}}-linux.cargo-clippy .#checks.{{arch()}}-linux.python-lint

# Supply-chain policy from deny.toml.
deny:
    nix build --no-link .#checks.{{arch()}}-linux.cargo-deny .#checks.{{arch()}}-linux.cargo-audit

# Validate and build the Blender extension with Blender itself.
extension:
    nix build .#blender-extension

# Subsystem health of a running server.
health:
    @curl -s -H "Host: {{ bind }}" "http://{{ bind }}/healthz" | jq .

# List the MCP tool surface of a running server.
tools:
    @just _rpc tools/list '{}' | jq '.result.tools | map({name, description})'

# List MCP resources of a running server.
resources:
    @just _rpc resources/list '{}' | jq -r '.result.resources[] | "\(.uri)\n    \(.description)"'

# Read one MCP resource:
#   just resource blender-mcp://reference/blender-api
resource uri:
    @just _rpc resources/read \
      "$(jq -nc --arg u {{ quote(uri) }} '{uri: $u}')" {{ quote(uri) }} \
      | jq -r '.result.contents[0].text'

# Evaluate Scheme against a running server:
#   just eval '(scene-summary)'
#   just eval '(bpy/ops/mesh/primitive_cube_add)'
eval code:
    #!/usr/bin/env bash
    set -euo pipefail
    request=$(jq -nc --arg code {{ quote(code) }} '{
      jsonrpc: "2.0", id: 1, method: "tools/call",
      params: {
        name: "scheme_eval",
        arguments: { code: $code, timeout_secs: 300 },
        _meta: {
          "io.modelcontextprotocol/protocolVersion": "2026-07-28",
          "io.modelcontextprotocol/clientCapabilities": {}
        }
      }
    }')
    curl -s --max-time 300 -X POST "http://{{ bind }}/mcp" \
      -H "Host: {{ bind }}" -H "Content-Type: application/json" \
      -H "Accept: application/json, text/event-stream" \
      -H "MCP-Protocol-Version: 2026-07-28" \
      -H "Mcp-Method: tools/call" -H "Mcp-Name: scheme_eval" \
      -d "$request" \
    | sed -n 's/^data: //p' \
    | jq -r 'if .error then "protocol error: \(.error.message)"
             elif .result.isError then "error: \(.result.structuredContent.message)"
             else .result.content[0].text end'

# Internal: one JSON-RPC round trip against a running server. `name` fills the
# SEP-2243 `Mcp-Name` header, which every method except the bare list calls requires.
_rpc method params name='':
    #!/usr/bin/env bash
    set -euo pipefail
    request=$(jq -nc --arg m {{ quote(method) }} --argjson p {{ quote(params) }} '{
      jsonrpc: "2.0", id: 1, method: $m,
      params: ($p + { _meta: {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
      }})
    }')
    name_header=()
    [[ -n {{ quote(name) }} ]] && name_header=(-H "Mcp-Name: {{ name }}")
    curl -s --max-time 60 -X POST "http://{{ bind }}/mcp" \
      -H "Host: {{ bind }}" -H "Content-Type: application/json" \
      -H "Accept: application/json, text/event-stream" \
      -H "MCP-Protocol-Version: 2026-07-28" -H "Mcp-Method: {{ method }}" \
      "${name_header[@]}" \
      -d "$request" \
    | sed -n 's/^data: //p'
