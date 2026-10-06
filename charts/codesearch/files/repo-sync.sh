#!/bin/bash
# Keeps the configured repositories cloned under REPOS_ROOT and registered with
# the codesearch serve container: clone or fetch, POST /repos, reindex when HEAD
# moved, prune checkouts no longer configured, and force-reindex everything once
# when FORCE_REINDEX_MARKER exists.
set -uo pipefail

: "${REPOS_ROOT:?}" "${REPOS_FILE:?}" "${SERVE_PORT:?}" "${FORCE_REINDEX_MARKER:?}"
SYNC_INTERVAL="${SYNC_INTERVAL:-300}"
PRUNE="${PRUNE:-true}"
API="http://127.0.0.1:${SERVE_PORT}"
export GIT_TERMINAL_PROMPT=0

# The token travels as an HTTP header in git's environment config, never on disk.
if [ -n "${GIT_TOKEN:-}" ]; then
  export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.extraHeader
  GIT_CONFIG_VALUE_0="Authorization: Basic $(printf '%s:%s' "${GIT_USERNAME:-x-access-token}" "$GIT_TOKEN" | base64 -w0)"
  export GIT_CONFIG_VALUE_0
fi

log() { printf '%s repo-sync: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }

trap 'log "stopping"; exit 0' TERM INT

api() {
  local auth=()
  [ -n "${CODESEARCH_SERVE_API_KEY:-}" ] && auth=(-H "Authorization: Bearer ${CODESEARCH_SERVE_API_KEY}")
  curl -sS -o /tmp/repo-sync-response -w '%{http_code}' "${auth[@]}" \
    -H 'Content-Type: application/json' "$@"
}

response() { cat /tmp/repo-sync-response 2>/dev/null; }

wait_for_serve() {
  until curl -fsS -o /dev/null "${API}/healthz"; do
    log "waiting for codesearch serve on ${API}"
    sleep 5 &
    wait $!
  done
}

sync_repo() {
  local alias="$1" url="$2" branch="$3" depth="$4"
  local dir="${REPOS_ROOT}/${alias}" before="" after depth_args=()
  [ "$depth" -gt 0 ] && depth_args=(--depth "$depth")

  if [ -d "${dir}/.git" ]; then
    before="$(git -C "$dir" rev-parse HEAD 2>/dev/null || true)"
    if ! git -C "$dir" fetch --quiet "${depth_args[@]}" origin "${branch:-HEAD}"; then
      log "${alias}: fetch failed"
      return 1
    fi
    git -C "$dir" reset --quiet --hard FETCH_HEAD
  else
    local branch_args=()
    [ -n "$branch" ] && branch_args=(--branch "$branch")
    rm -rf "$dir"
    if ! git clone --quiet --single-branch "${depth_args[@]}" "${branch_args[@]}" "$url" "$dir"; then
      log "${alias}: clone of ${url} failed"
      return 1
    fi
    log "${alias}: cloned ${url}"
  fi
  after="$(git -C "$dir" rev-parse HEAD)"

  local code
  code="$(api -X POST "${API}/repos" -d "{\"path\":\"${dir}\",\"alias\":\"${alias}\"}")"
  case "$code" in
    200 | 201 | 202) log "${alias}: registered at ${after}" ;;
    409)
      [ "$before" = "$after" ] && return 0
      code="$(api -X POST "${API}/repos/${alias}/reindex")"
      case "$code" in
        202 | 409) log "${alias}: ${before:-none} -> ${after}, reindex requested (HTTP ${code})" ;;
        *)
          log "${alias}: reindex failed (HTTP ${code}): $(response)"
          return 1
          ;;
      esac
      ;;
    *)
      log "${alias}: registration failed (HTTP ${code}): $(response)"
      return 1
      ;;
  esac
}

prune() {
  local dir alias code
  for dir in "${REPOS_ROOT}"/*/; do
    [ -d "$dir" ] || continue
    alias="$(basename "$dir")"
    grep -q "^${alias}|" "$REPOS_FILE" && continue
    code="$(api -X DELETE "${API}/repos/${alias}")"
    case "$code" in
      200 | 202 | 204 | 404)
        rm -rf "$dir"
        log "${alias}: no longer configured; unregistered (HTTP ${code}) and deleted"
        ;;
      *) log "${alias}: unregister failed (HTTP ${code}): $(response)" ;;
    esac
  done
}

# Deletes the marker only once every configured repo accepted a forced reindex.
force_reindex() {
  local alias url branch depth code ok=true
  log "force-reindex marker found; reindexing every repository"
  while IFS='|' read -r alias url branch depth; do
    [ -n "$alias" ] || continue
    code="$(api -X POST "${API}/repos/${alias}/reindex?force=true" </dev/null)"
    if [ "$code" = 202 ]; then
      log "${alias}: forced reindex started"
    else
      log "${alias}: forced reindex not started (HTTP ${code}): $(response); retrying next cycle"
      ok=false
    fi
  done <"$REPOS_FILE"
  if [ "$ok" = true ]; then
    rm -f "$FORCE_REINDEX_MARKER"
    log "force-reindex marker removed"
  fi
}

mkdir -p "$REPOS_ROOT"
while true; do
  wait_for_serve
  while IFS='|' read -r alias url branch depth; do
    [ -n "$alias" ] && sync_repo "$alias" "$url" "$branch" "$depth" </dev/null
  done <"$REPOS_FILE"
  [ "$PRUNE" = true ] && prune
  [ -f "$FORCE_REINDEX_MARKER" ] && force_reindex
  sleep "$SYNC_INTERVAL" &
  wait $!
done
