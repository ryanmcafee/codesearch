#!/usr/bin/env bash
# Runs `codesearch mcp` over stdio in the image against a directory and checks an
# initialize + tools/call search round trip.
#
#   docker/stdio-test.sh <image> <directory> [query]
set -euo pipefail

IMAGE="${1:?usage: docker/stdio-test.sh <image> <directory> [query]}"
DIR="$(cd "${2:?usage: docker/stdio-test.sh <image> <directory> [query]}" && pwd)"
QUERY="${3:-helm chart deployment}"
TIMEOUT="${STDIO_TIMEOUT:-300}"

coproc MCP { docker run -i --rm --user "$(id -u):$(id -g)" -v "${DIR}:/workspace" "$IMAGE" mcp --mode local /workspace 2>/dev/null; }

send() { printf '%s\n' "$1" >&"${MCP[1]}"; }

# Reads stdout until the response with the given id arrives.
await() {
  local line
  while IFS= read -r -t "$TIMEOUT" line <&"${MCP[0]}"; do
    if [[ "$line" == *"\"id\":$1,"* || "$line" == *"\"id\":$1}"* ]]; then
      printf '%s\n' "$line"
      return 0
    fi
  done
  echo "FAIL: no response with id $1 within ${TIMEOUT}s" >&2
  return 1
}

send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"stdio-test","version":"1"}}}'
init="$(await 1)"
grep -q '"serverInfo"' <<<"$init" || { echo "FAIL: initialize: $init" >&2; exit 1; }
echo "ok   initialize ($(grep -o '"serverInfo":{[^}]*}' <<<"$init"))"
send '{"jsonrpc":"2.0","method":"notifications/initialized"}'

# The first session indexes in the background; retry until the index answers.
deadline=$((SECONDS + TIMEOUT))
id=2
while true; do
  send "{\"jsonrpc\":\"2.0\",\"id\":${id},\"method\":\"tools/call\",\"params\":{\"name\":\"search\",\"arguments\":{\"query\":\"${QUERY}\",\"limit\":3}}}"
  result="$(await "$id")"
  if ! grep -q '"isError":true' <<<"$result" && grep -q '"path' <<<"$result"; then
    break
  fi
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "FAIL: search returned no hits within ${TIMEOUT}s: ${result:0:2000}" >&2
    exit 1
  fi
  id=$((id + 1))
  sleep 3
done
echo "ok   tools/call search \"${QUERY}\" -> $(grep -o '\\"path\\":\\"[^\\]*' <<<"$result" | head -3 | sed 's/.*\\"//' | paste -sd, -)"

stdin_fd="${MCP[1]}"
exec {stdin_fd}>&-
wait "$MCP_PID" 2>/dev/null || true
