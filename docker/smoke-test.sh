#!/usr/bin/env bash
# Smoke-tests a codesearch image: serve starts on a network bind with an API key,
# /healthz answers without auth, /metrics and MCP initialize answer with it, and
# a hot mdb_copy of the embedding cache taken while serve runs restores cleanly.
#
#   docker/smoke-test.sh <image>
set -euo pipefail

IMAGE="${1:?usage: docker/smoke-test.sh <image>}"
PORT="${SMOKE_PORT:-39725}"
KEY="smoke-$(date +%s)-$RANDOM"
NAME="codesearch-smoke-$$"
BASE="http://127.0.0.1:${PORT}"

RESTORE_VOLUME="${NAME}-restore"
WORK="$(mktemp -d)"

cleanup() {
  docker rm -f "$NAME" "${NAME}-restored" >/dev/null 2>&1 || true
  docker volume rm "$RESTORE_VOLUME" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
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

# Index a tiny repo so the embedding cache LMDB exists, then back it up while serve runs.
docker exec "$NAME" sh -c 'mkdir -p /tmp/demo && printf "fn greet() { println!(\"hello\"); }\n" > /tmp/demo/main.rs'
[ "$(status -X POST -H "Authorization: Bearer ${KEY}" -H 'Content-Type: application/json' \
  -d '{"path":"/tmp/demo"}' "${BASE}/repos")" = 202 ] || fail "POST /repos did not return 202"
cache_entries() {
  docker exec "$NAME" sh -c "mdb_stat -a \"\$1\" 2>/dev/null | sed -n '/Status of embeddings/,/Entries/s/.*Entries: //p'" _ "$1"
}
cache_dir=""
for _ in $(seq 1 120); do
  cache_dir="$(docker exec "$NAME" sh -c 'dirname "$(ls -1 "$HOME"/.codesearch/embedding_cache/*/data.mdb 2>/dev/null | head -1)"')"
  [ "$cache_dir" != . ] && [ "$(cache_entries "$cache_dir")" -gt 0 ] 2>/dev/null && break
  sleep 1
done
entries="$(cache_entries "$cache_dir")"
[ "${entries:-0}" -gt 0 ] || fail "embedding cache was not populated"
model="$(basename "$cache_dir")"
docker exec "$NAME" sh -c 'set -e
  mkdir -p /tmp/backup/embedding_cache/"$1"
  mdb_copy -c "$HOME/.codesearch/embedding_cache/$1" /tmp/backup/embedding_cache/"$1"
  cp "$HOME/.codesearch/repos.json" /tmp/backup/
  tar -C /tmp/backup -czf /tmp/backup.tgz .' _ "$model" || fail "mdb_copy of the live cache failed"
docker cp -q "$NAME:/tmp/backup.tgz" "$WORK/backup.tgz"
echo "ok   mdb_copy -c of the live ${model} cache (${entries} embeddings) + tar"

docker volume create "$RESTORE_VOLUME" >/dev/null
restored="$(docker run --rm -v "$RESTORE_VOLUME:/home/app/.codesearch" -v "$WORK/backup.tgz:/backup.tgz:ro" \
  --entrypoint sh "$IMAGE" -c "tar -C /home/app/.codesearch -xzf /backup.tgz && codesearch cache stats $model 2>/dev/null" \
  | sed -n 's/.*Total Entries: //p')"
[ "$restored" = "$entries" ] || fail "restored cache has ${restored:-no} entries, want ${entries}"
docker run -d --name "${NAME}-restored" -p "127.0.0.1:$((PORT + 1)):39725" -e CODESEARCH_SERVE_API_KEY="$KEY" \
  -v "$RESTORE_VOLUME:/home/app/.codesearch" "$IMAGE" >/dev/null
for _ in $(seq 1 60); do
  curl -fsS -o /dev/null "http://127.0.0.1:$((PORT + 1))/healthz" 2>/dev/null && break
  sleep 1
done
[ "$(status "http://127.0.0.1:$((PORT + 1))/healthz")" = 200 ] || fail "serve did not start on the restored cache"
echo "ok   restored copy: codesearch cache stats reads ${restored} entries, serve starts on it"

docker stop "$NAME" >/dev/null
[ "$(docker inspect -f '{{.State.ExitCode}}' "$NAME")" = 0 ] || fail "serve did not exit 0 on docker stop"
echo "ok   docker stop -> exit 0"
