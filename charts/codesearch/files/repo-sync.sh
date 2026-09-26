#!/bin/bash
# Clones or fast-forwards each configured repository, then registers it with the
# codesearch serve container (or reindexes it when HEAD moved).
set -uo pipefail

: "${REPOS_ROOT:?}" "${REPOS_FILE:?}" "${SERVE_PORT:?}" "${CODESEARCH_SERVE_API_KEY:?}"
SYNC_INTERVAL="${SYNC_INTERVAL:-300}"
API="http://127.0.0.1:${SERVE_PORT}"
export GIT_TERMINAL_PROMPT=0

log() { printf '%s repo-sync: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }

trap 'log "stopping"; exit 0' TERM INT

git_cmd() {
  if [ -n "${GIT_TOKEN:-}" ]; then
    git -c credential.helper= -c "credential.helper=${GIT_CREDENTIAL_HELPER:?}" "$@"
  else
    git "$@"
  fi
}

api() {
  curl -sS -o /tmp/repo-sync-response -w '%{http_code}' \
    -H "Authorization: Bearer ${CODESEARCH_SERVE_API_KEY}" \
    -H 'Content-Type: application/json' "$@"
}

wait_for_serve() {
  until curl -fsS -o /dev/null "${API}/healthz"; do
    log "waiting for codesearch serve on ${API}"
    sleep 5 & wait $!
  done
}

sync_repo() {
  local alias="$1" url="$2" branch="$3" depth="$4"
  local dir="${REPOS_ROOT}/${alias}" before="" after depth_args=()
  [ "$depth" -gt 0 ] && depth_args=(--depth "$depth")

  if [ -d "${dir}/.git" ]; then
    before="$(git -C "$dir" rev-parse HEAD 2>/dev/null || true)"
    if ! git_cmd -C "$dir" fetch --quiet "${depth_args[@]}" origin "${branch:-HEAD}"; then
      log "${alias}: fetch failed"
      return 1
    fi
    git -C "$dir" reset --quiet --hard FETCH_HEAD
  else
    local branch_args=()
    [ -n "$branch" ] && branch_args=(--branch "$branch")
    rm -rf "$dir"
    git_cmd clone --quiet --single-branch "${depth_args[@]}" "${branch_args[@]}" "$url" "$dir" \
      || { log "${alias}: clone of ${url} failed"; return 1; }
    log "${alias}: cloned ${url}"
  fi
  after="$(git -C "$dir" rev-parse HEAD)"

  local code
  code="$(api -X POST "${API}/repos" -d "{\"path\":\"${dir}\",\"alias\":\"${alias}\"}")"
  case "$code" in
    200 | 201 | 202) log "${alias}: registered at ${after}" ;;
    409)
      if [ "$before" != "$after" ]; then
        code="$(api -X POST "${API}/repos/${alias}/reindex")"
        case "$code" in
          202 | 409) log "${alias}: ${before:-none} -> ${after}, reindex requested (${code})" ;;
          *) log "${alias}: reindex failed (HTTP ${code}): $(cat /tmp/repo-sync-response)"; return 1 ;;
        esac
      fi
      ;;
    *) log "${alias}: registration failed (HTTP ${code}): $(cat /tmp/repo-sync-response)"; return 1 ;;
  esac
}

while true; do
  wait_for_serve
  while IFS='|' read -r alias url branch depth; do
    [ -n "$alias" ] && sync_repo "$alias" "$url" "$branch" "$depth" </dev/null
  done < "$REPOS_FILE"
  sleep "$SYNC_INTERVAL" & wait $!
done
