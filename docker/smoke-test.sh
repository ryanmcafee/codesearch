#!/usr/bin/env bash
# Smoke-tests a codesearch image: serve starts on a network bind with an API key,
# /healthz answers without auth, /metrics and MCP initialize answer with it.
#
#   docker/smoke-test.sh <image>
set -euo pipefail

IMAGE="${1:?usage: docker/smoke-test.sh <image>}"
PORT="${SMOKE_PORT:-39725}"
KEY="smoke-$(date +%s)-$RANDOM"
NAME="codesearch-smoke-$$"
BASE="http://127.0.0.1:${PORT}"

cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  docker logs "$NAME" 2>&1 | tail -50 >&2 || true
  exit 1
}

docker run -d --name "$NAME" -p "127.0.0.1:${PORT}:39725" \
  -e CODESEARCH_SERVE_API_KEY="$KEY" "$IMAGE" >/dev/null

for _ in $(seq 1 60); do
  curl -fsS -o /dev/null "${BASE}/healthz" 2>/dev/null && break
  [ "$(docker inspect -f '{{.State.Running}}' "$NAME")" = true ] || fail "container exited"
  sleep 1
done

status() { curl -sS -o /dev/null -w '%{http_code}' "$@"; }

[ "$(status "${BASE}/healthz")" = 200 ] || fail "/healthz did not return 200"
echo "ok   GET /healthz -> 200 (no auth)"

[ "$(status "${BASE}/metrics")" = 401 ] || fail "/metrics without a key did not return 401"
echo "ok   GET /metrics -> 401 (no auth)"

[ "$(status -H "Authorization: Bearer ${KEY}" "${BASE}/metrics")" = 200 ] || fail "/metrics with a key did not return 200"
echo "ok   GET /metrics -> 200 (bearer)"

init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke-test","version":"1"}}}'
body="$(curl -sS -w '\n%{http_code}' -H "Authorization: Bearer ${KEY}" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d "$init" "${BASE}/mcp")"
[ "$(tail -n1 <<<"$body")" = 200 ] || fail "MCP initialize did not return 200: ${body}"
grep -q '"serverInfo"' <<<"$body" || fail "MCP initialize response has no serverInfo: ${body}"
echo "ok   POST /mcp initialize -> 200 ($(grep -o '"serverInfo":{[^}]*}' <<<"$body"))"

docker stop "$NAME" >/dev/null
[ "$(docker inspect -f '{{.State.ExitCode}}' "$NAME")" = 0 ] || fail "serve did not exit 0 on docker stop"
echo "ok   docker stop -> exit 0"
