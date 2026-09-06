#!/usr/bin/env bash
# Start the server that .mcp.json points at, on the default 127.0.0.1:8000.
#
#   scripts/serve.sh                      # server-managed background Blender
#   scripts/serve.sh --backend live       # attach to a running Blender session
#   scripts/serve.sh --blend-file a.blend # open a file first (headless only)
#
# The Nix wrapper supplies Blender, the extension directory, and the headless
# bootstrap, so no paths need to be passed.
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repository_root"

package_store="$(nix build .#blender-mcp --no-link --print-out-paths)"
echo "using package ${package_store}" >&2

# Keep the packaged extension from picking up the invoking user's Blender
# configuration, matching how the automated checks run.
isolated="$(mktemp -d)"
server_pid=""
cleanup() {
  if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
    kill -TERM "${server_pid}" 2>/dev/null || true
    wait "${server_pid}" 2>/dev/null || true
  fi
  rm -rf "${isolated}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export BLENDER_USER_CONFIG="${isolated}/config"
export BLENDER_USER_SCRIPTS="${isolated}/scripts"
export BLENDER_USER_EXTENSIONS="${isolated}/extensions"
export BLENDER_MCP_LOG="${BLENDER_MCP_LOG:-info}"

backend_given=false
for argument in "$@"; do
  if [[ "${argument}" == "--backend" || "${argument}" == --backend=* ]]; then
    backend_given=true
  fi
done

arguments=("$@")
if [[ "${backend_given}" != true ]]; then
  arguments=(--backend headless "${arguments[@]}")
fi
"${package_store}/bin/blender-mcp" "${arguments[@]}" &
server_pid=$!
wait "${server_pid}"
