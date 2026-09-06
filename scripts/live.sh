#!/usr/bin/env bash
# Launch a graphical Blender with the extension installed and the bridge started,
# then serve MCP against it. This is the scripted equivalent of the manual flow:
# install the extension, enable it, and press Start in the 3D viewport's
# "Scheme MCP" N-panel.
#
#   scripts/live.sh                    # throwaway Blender profile (default)
#   scripts/live.sh scene.blend        # open a file
#   scripts/live.sh --profile user     # install into your real Blender profile
#   scripts/live.sh --no-serve         # launch Blender only, start the server yourself
#
# Environment:
#   BLENDER          Blender executable (default: this flake's pinned Blender)
#   BLENDER_MCP_BIN  explicit server binary override (default: current Git-flake package)
#   BRIDGE_PORT      loopback port the extension listens on (default 9876)
#   MCP_BIND         address the MCP HTTP service binds to (default 127.0.0.1:8000)
#   BLENDER_MCP_GPU  set to 0 to leave Cycles on the CPU (default: pick the best GPU)
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repository_root}"

bridge_port="${BRIDGE_PORT:-9876}"
mcp_bind="${MCP_BIND:-127.0.0.1:8000}"
use_gpu="${BLENDER_MCP_GPU:-1}"
profile="throwaway"
serve=true
blend_file=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile) profile="$2"; shift 2 ;;
    --no-serve) serve=false; shift ;;
    -h|--help) sed -n '2,17p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) blend_file="$1"; shift ;;
  esac
done

# Resolve on every launch: an existing ./result may belong to older source or a
# different flake output. Nix reuses a current package without rebuilding it.
package_store="$(nix build .#blender-mcp --no-link --print-out-paths)"
extension_zip="${package_store}/share/blender-mcp/scheme_blender_mcp.zip"
server_binary="${BLENDER_MCP_BIN:-${package_store}/bin/blender-mcp}"
echo "using package ${package_store}" >&2
echo "using server ${server_binary}" >&2

blender="${BLENDER:-}"
if [[ -z "${blender}" ]]; then
  # Resolve through this flake, not `nixpkgs#blender`: the latter goes through the
  # caller's flake registry and silently picks up whatever their channel provides,
  # which is how a non-LTS Blender ends up running.
  blender_store="$(nix build .#blender --no-link --print-out-paths)"
  blender="${blender_store}/bin/blender"
fi
echo "using $("${blender}" --version 2>/dev/null | head -n 1)" >&2

if [[ "${profile}" == "throwaway" ]]; then
  # Keep the caller's real Blender configuration untouched.
  workspace="$(mktemp -d -t blender-mcp-live-XXXXXX)"
  export BLENDER_USER_CONFIG="${workspace}/config"
  export BLENDER_USER_SCRIPTS="${workspace}/scripts"
  export BLENDER_USER_EXTENSIONS="${workspace}/extensions"
  mkdir -p "${BLENDER_USER_CONFIG}" "${BLENDER_USER_SCRIPTS}" "${BLENDER_USER_EXTENSIONS}"
  echo "using a throwaway Blender profile at ${workspace}" >&2
  # Write preferences once so the graphical session skips the first-run Quick Setup
  # dialog, which otherwise covers the viewport.
  "${blender}" --background --factory-startup \
    --python-expr "import bpy; bpy.ops.wm.save_userpref()" >/dev/null 2>&1 || true
else
  echo "using your real Blender profile" >&2
fi

# Check both ports before launching Blender: binding failures used to surface only
# after startup, leaving an orphaned Blender behind.
port_holder() {
  local port="$1"
  if (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null; then
    exec 3>&- 3<&-
    return 0
  fi
  return 1
}

mcp_port="${mcp_bind##*:}"
ports=("${bridge_port}:the Blender bridge:BRIDGE_PORT")
[[ "${serve}" == true ]] && ports+=("${mcp_port}:the MCP service:MCP_BIND")
for busy in "${ports[@]}"; do
  port="${busy%%:*}"
  rest="${busy#*:}"
  what="${rest%%:*}"
  knob="${rest##*:}"
  if port_holder "${port}"; then
    echo "port ${port} is already in use, so ${what} cannot start." >&2
    if command -v ss >/dev/null; then
      ss -lntp 2>/dev/null | grep ":${port} " >&2 || true
    fi
    echo >&2
    echo "If that is an earlier session of this project:  just stop" >&2
    echo "Otherwise pick another port with:               ${knob}=... just live" >&2
    exit 1
  fi
done

startup="$(mktemp -t scheme-mcp-startup-XXXXXX.py)"
trap 'rm -f "${startup}"' EXIT
export BLENDER_MCP_INSTALL_ZIP="${extension_zip}"
export BLENDER_MCP_LIVE_PORT="${bridge_port}"
export BLENDER_MCP_GPU="${use_gpu}"
cat > "${startup}" <<'PYTHON'
"""Install, enable, and start the bridge -- the N-panel Start button in script form."""
import importlib, os, pathlib, sys, traceback, zipfile
import bpy

MODULE = "bl_ext.user_default.scheme_blender_mcp"
try:
    if MODULE in bpy.context.preferences.addons:
        installed = pathlib.Path(importlib.import_module(MODULE).__file__).parent
        with zipfile.ZipFile(os.environ["BLENDER_MCP_INSTALL_ZIP"]) as archive:
            for entry in archive.infolist():
                if entry.is_dir():
                    continue
                relative = pathlib.PurePosixPath(entry.filename).relative_to("scheme_blender_mcp")
                candidate = installed.joinpath(*relative.parts)
                if not candidate.is_file() or candidate.read_bytes() != archive.read(entry):
                    raise RuntimeError(
                        "Installed extension differs from the selected package. "
                        "Use the default throwaway profile, or update the extension "
                        "from BLENDER_MCP_INSTALL_ZIP and restart Blender."
                    )
    if MODULE not in bpy.context.preferences.addons:
        bpy.ops.extensions.package_install_files(
            filepath=os.environ["BLENDER_MCP_INSTALL_ZIP"],
            repo="user_default", enable_on_install=True
        )
    if MODULE not in bpy.context.preferences.addons:
        bpy.ops.preferences.addon_enable(module=MODULE)
    bpy.context.preferences.addons[MODULE].preferences.port = int(os.environ["BLENDER_MCP_LIVE_PORT"])
    bpy.ops.scheme_mcp.start_bridge()
    from bl_ext.user_default.scheme_blender_mcp.bridge import get_server
    print("SCHEME_MCP: bridge listening on %s:%s" % get_server().address, file=sys.stderr)
    # A throwaway profile starts with empty Cycles preferences, so a GPU-capable
    # Blender would otherwise render on the CPU without saying so.
    if os.environ["BLENDER_MCP_GPU"] != "0":
        from bl_ext.user_default.scheme_blender_mcp.gpu import configure
        print("SCHEME_MCP: cycles device %s" % (configure(),), file=sys.stderr)
except Exception:
    print("SCHEME_MCP: failed\n" + traceback.format_exc(), file=sys.stderr)
    raise
PYTHON

echo "launching Blender..." >&2
blender_args=()
[[ -n "${blend_file}" ]] && blender_args+=("${blend_file}")
blender_args+=(--log-level "${BLENDER_LOG_LEVEL:-0}" --python-exit-code 1 --python "${startup}")

# Introspecting every operator to build the catalog makes Blender resolve each enum
# default, emitting a few hundred lines of `bpy.rna | WARNING ... matches no enum`.
# `--log-level 0` silences those under `--background`, but a graphical session ignores
# it, so drop exactly that one pattern. Blender logs to stdout while the startup script
# reports on stderr, so filtering stdout cannot swallow our own diagnostics, and every
# other Blender message still comes through. Set BLENDER_NOISE=keep to see them.
readonly ENUM_DEFAULT_NOISE="bpy\.rna .*WARNING.*matches no enum in 'EnumProperty'"
if [[ "${BLENDER_NOISE:-drop}" == "keep" ]]; then
  "${blender}" "${blender_args[@]}" &
else
  "${blender}" "${blender_args[@]}" \
    > >(grep -vE --line-buffered "${ENUM_DEFAULT_NOISE}") &
fi
blender_pid=$!

# Wait for the extension's loopback listener before connecting.
ready=false
for _ in $(seq 1 60); do
  if (exec 3<>"/dev/tcp/127.0.0.1/${bridge_port}") 2>/dev/null; then
    exec 3>&- 3<&-
    ready=true
    break
  fi
  kill -0 "${blender_pid}" 2>/dev/null || { echo "Blender exited during startup" >&2; exit 1; }
  sleep 1
done
if [[ "${ready}" != true ]]; then
  echo "Blender did not expose its bridge within 60 seconds; stopping the failed launch" >&2
  kill "${blender_pid}" 2>/dev/null || true
  exit 1
fi

if [[ "${serve}" != true ]]; then
  echo "bridge is up on 127.0.0.1:${bridge_port}; start the server yourself with:" >&2
  echo "  nix run .#blender-mcp -- --backend live --bridge 127.0.0.1:${bridge_port}" >&2
  wait "${blender_pid}"
  exit 0
fi

echo "serving MCP at http://${mcp_bind}/mcp against the live session" >&2
BLENDER_MCP_LOG="${BLENDER_MCP_LOG:-info}" \
  "${server_binary}" --backend live \
    --bridge "127.0.0.1:${bridge_port}" --bind "${mcp_bind}"
