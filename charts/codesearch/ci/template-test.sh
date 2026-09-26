#!/usr/bin/env bash
# Renders the chart with each values combination and checks the rendered output.
set -euo pipefail

chart="$(cd "$(dirname "$0")/.." && pwd)"
render() { helm template t "$chart" --namespace cs "$@"; }
expect() {
  local out="$1" pattern="$2"
  grep -qE -- "$pattern" <<<"$out" || { echo "FAIL: rendered output lacks /$pattern/" >&2; exit 1; }
  echo "ok   /$pattern/"
}
expect_absent() {
  local out="$1" pattern="$2"
  ! grep -qE -- "$pattern" <<<"$out" || { echo "FAIL: rendered output has /$pattern/" >&2; exit 1; }
  echo "ok   no /$pattern/"
}

echo "== defaults"
out="$(render)"
expect "$out" 'image: "?ghcr.io/ryanmcafee/codesearch:[0-9]+\.[0-9]+\.[0-9]+'
expect "$out" 'value: "t-codesearch,t-codesearch.cs,t-codesearch.cs.svc,t-codesearch.cs.svc.cluster.local,localhost,127.0.0.1,::1"'
expect "$out" 'value: "/repos"'
expect "$out" 'kind: Secret'
expect "$out" 'claimName: t-codesearch-data'
expect "$out" 'claimName: t-codesearch-repos'
expect "$out" 'type: Recreate'
expect_absent "$out" 'name: repo-sync'
expect_absent "$out" 'kind: (Ingress|ServiceMonitor|NetworkPolicy)'

for f in "$chart"/ci/*-values.yaml; do
  echo "== $(basename "$f")"
  render -f "$f" >/dev/null
done

echo "== repositories"
out="$(render -f "$chart/ci/repositories-values.yaml")"
expect "$out" 'Hello-World\|https://github.com/octocat/Hello-World.git\|\|1'
expect "$out" 'name: repo-sync'
expect "$out" 'kind: NetworkPolicy'

echo "== ingress, ServiceMonitor, NetworkPolicy egress, existing Secret, extra hosts"
out="$(render \
  --set ingress.enabled=true --set 'ingress.hosts[0].host=codesearch.example.com' \
  --set metrics.serviceMonitor.enabled=true \
  --set networkPolicy.enabled=true --set 'networkPolicy.egress[0].to[0].ipBlock.cidr=10.0.0.0/8' \
  --set auth.existingSecret=codesearch-key \
  --set 'allowedHosts[0]=codesearch.internal' --set 'allowedRoots[0]=/srv/code' \
  --set 'repositories[0].url=https://example.com/org/private.git' --set repoSync.git.existingSecret=git-token)"
expect "$out" 'kind: Ingress'
expect "$out" 'kind: ServiceMonitor'
expect "$out" '- Egress'
expect "$out" 'value: ".*,codesearch.example.com,codesearch.internal"'
expect "$out" 'value: "/repos;/srv/code"'
expect "$out" 'name: codesearch-key'
expect "$out" 'name: GIT_TOKEN'
expect_absent "$out" 'kind: Secret'

echo "== ephemeral storage"
out="$(render --set persistence.data.enabled=false --set persistence.repos.enabled=false)"
expect_absent "$out" 'kind: PersistentVolumeClaim'
expect "$out" 'emptyDir: \{\}'

echo "== schema rejects invalid values"
for invalid in "ingress.enabled=true" "repositories[0].alias=bad/alias" "image.pullPolicy=Sometimes"; do
  if render --set "$invalid" >/dev/null 2>&1; then
    echo "FAIL: schema accepted $invalid" >&2
    exit 1
  fi
  echo "ok   rejected $invalid"
done
