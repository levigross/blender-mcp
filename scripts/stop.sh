#!/usr/bin/env bash
# Stop a running blender-mcp session: the MCP server and the Blender it launched.
#
#   scripts/stop.sh              # default ports
#   MCP_BIND=127.0.0.1:9000 scripts/stop.sh
#
# Only processes actually listening on those ports are signalled, and only when
# they look like ours, so an unrelated service on the same port is reported and
# left alone.
set -euo pipefail

mcp_bind="${MCP_BIND:-127.0.0.1:8000}"
mcp_port="${mcp_bind##*:}"
bridge_port="${BRIDGE_PORT:-9876}"

# PIDs listening on a port, via ss's users:(("name",pid=N,fd=M)) field.
listeners_on() {
  local port="$1"
  ss -lntp 2>/dev/null \
    | awk -v port=":${port}" '$4 ~ port"$" {print}' \
    | grep -oE 'pid=[0-9]+' \
    | cut -d= -f2 \
    | sort -u
}

stopped=0
for entry in "${mcp_port}:the MCP server" "${bridge_port}:the Blender bridge"; do
  port="${entry%%:*}"
  what="${entry#*:}"
  for pid in $(listeners_on "${port}"); do
    command="$(ps -p "${pid}" -o comm= 2>/dev/null || true)"
    argv="$(tr '\0' ' ' < "/proc/${pid}/cmdline" 2>/dev/null || true)"
    # Ours is either the server binary or a Blender running our startup script.
    if [[ "${command}" == *blender-mcp* ]] \
      || [[ "${command}" == *blender* && "${argv}" == *scheme-mcp-startup* ]] \
      || [[ "${command}" == *blender* && "${port}" == "${bridge_port}" ]]; then
      echo "stopping ${what} (pid ${pid}, ${command})" >&2
      kill -TERM "${pid}" 2>/dev/null || true
      stopped=$((stopped + 1))
    else
      echo "port ${port} is held by an unrelated process (pid ${pid}, ${command}); leaving it alone" >&2
    fi
  done
done

if (( stopped == 0 )); then
  echo "nothing to stop on ${mcp_port} or ${bridge_port}" >&2
  exit 0
fi

for _ in $(seq 1 20); do
  remaining=0
  for port in "${mcp_port}" "${bridge_port}"; do
    [[ -n "$(listeners_on "${port}")" ]] && remaining=1
  done
  (( remaining == 0 )) && { echo "stopped" >&2; exit 0; }
  sleep 0.5
done

echo "still listening after 10s; check with: ss -lntp | grep -E ':${mcp_port}|:${bridge_port}'" >&2
exit 1
