#!/usr/bin/env bash
# Renders the chart with each values combination and checks the rendered output.
set -euo pipefail

chart="$(cd "$(dirname "$0")/.." && pwd)"
monitoring=(--api-versions monitoring.coreos.com/v1/ServiceMonitor --api-versions monitoring.coreos.com/v1/PrometheusRule)
render() { helm template codesearch "$chart" --namespace cs "$@"; }
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
# yq query against one kind/name of the rendered output.
query() { yq "select(.kind == \"$2\" and .metadata.name == \"$3\")" <<<"$1" | yq "$4"; }
expect_eq() {
  local got="$1" want="$2" what="$3"
  [ "$got" = "$want" ] || { echo "FAIL: $what = '$got', want '$want'" >&2; exit 1; }
  echo "ok   $what = $want"
}

echo "== defaults (apiKey mode, no monitoring CRDs)"
out="$(render)"
expect "$out" 'image: "?ghcr.io/ryanmcafee/codesearch:[0-9]+\.[0-9]+\.[0-9]+'
expect "$out" 'value: "codesearch,codesearch.cs,codesearch.cs.svc,codesearch.cs.svc.cluster.local,localhost,127.0.0.1,::1"'
expect_eq "$(query "$out" Deployment codesearch '.spec.strategy.type')" Recreate "Deployment strategy"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.volumes[] | select(.name == "data") | .persistentVolumeClaim.claimName')" codesearch-data "data volume claim"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[0].volumeMounts[] | select(.name == "data") | .mountPath')" /data "serve data mount"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[0].env[] | select(.name == "HOME") | .value')" /data/home "serve HOME"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[0].env[] | select(.name == "CODESEARCH_ALLOWED_ROOTS") | .value')" /data/repos "allowed roots"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[0].args | join(" ")')" "serve --host 0.0.0.0 --port 39725 --no-tui" "serve args"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.metadata.labels["app.kubernetes.io/name"] + "/" + .spec.template.metadata.labels["app.kubernetes.io/instance"]')" codesearch/codesearch "pod labels"
expect_eq "$(query "$out" Service codesearch '.spec.ports[0].port')" 39725 "Service port"
expect_eq "$(query "$out" Secret codesearch-api-key '.kind')" Secret "API key Secret"
expect_eq "$(query "$out" PersistentVolumeClaim codesearch-data '.spec.resources.requests.storage')" 20Gi "PVC size"
expect_eq "$(query "$out" ConfigMap codesearch-dashboard '.metadata.labels.grafana_dashboard')" 1 "dashboard sidecar label"
expect_absent "$out" 'name: (repo-sync|forwarder)$'
expect_absent "$out" 'kind: (Ingress|ServiceMonitor|PrometheusRule|NetworkPolicy)'

echo "== monitoring CRDs present"
out="$(render "${monitoring[@]}")"
expect_eq "$(query "$out" ServiceMonitor codesearch '.spec.endpoints[0].authorization.credentials.name')" codesearch-api-key "ServiceMonitor bearer Secret"
expect_eq "$(query "$out" PrometheusRule codesearch '[.spec.groups[].rules[].alert] | unique | length')" 10 "distinct alerts"
out="$(render "${monitoring[@]}" --set serviceMonitor.enabled=false --set prometheusRule.enabled=false --set dashboard.enabled=false)"
expect_absent "$out" 'kind: (ServiceMonitor|PrometheusRule)'
expect_absent "$out" 'name: codesearch-dashboard'
out="$(render "${monitoring[@]}" --set prometheusRule.rules.down.enabled=false --set 'prometheusRule.additionalLabels.release=kps' \
  --set dashboard.namespace=monitoring --set 'dashboard.annotations.grafana_folder=codesearch')"
expect_eq "$(query "$out" PrometheusRule codesearch '.metadata.labels.release')" kps "PrometheusRule extra label"
expect_absent "$out" 'alert: CodesearchDown'
expect_eq "$(query "$out" ConfigMap codesearch-dashboard '.metadata.namespace + "/" + .metadata.annotations.grafana_folder')" monitoring/codesearch "dashboard namespace and folder"

echo "== networkPolicy auth mode"
out="$(render "${monitoring[@]}" -f "$chart/ci/networkpolicy-values.yaml" \
  --set-string 'networkPolicy.ingress.namespaceSelectors[0].codesearch-client=true' \
  --set 'networkPolicy.ingress.extraPeers[0].namespaceSelector.matchLabels.kubernetes\.io/metadata\.name=monitoring')"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[0].args | join(" ")')" "serve --host 127.0.0.1 --port 39726 --no-tui" "serve args"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[] | select(.name == "forwarder") | .command | join(" ")')" "socat TCP-LISTEN:39725,fork,reuseaddr TCP:127.0.0.1:39726" "forwarder command"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[] | select(.name == "forwarder") | .ports[0].name')" http "forwarder port name"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[] | select(.name == "repo-sync") | .env[] | select(.name == "SERVE_PORT") | .value')" 39726 "repo-sync talks to loopback serve"
expect_eq "$(query "$out" NetworkPolicy codesearch '.spec.ingress[0].from[0].namespaceSelector.matchLabels.codesearch-client')" true "client namespace selector"
expect_eq "$(query "$out" NetworkPolicy codesearch '.spec.ingress[0].from[1].namespaceSelector.matchLabels["kubernetes.io/metadata.name"]')" monitoring "extra peer"
expect_eq "$(query "$out" ServiceMonitor codesearch '.spec.endpoints[0] | has("authorization")')" false "ServiceMonitor without bearer"
expect_eq "$(query "$out" Deployment codesearch '[.spec.template.spec.containers[].env[]? | select(.name == "CODESEARCH_SERVE_API_KEY")] | length')" 0 "API key env vars"
expect_absent "$out" 'kind: Secret'

echo "== repositories"
out="$(render -f "$chart/ci/repositories-values.yaml" \
  --set 'repositories.urls[1].url=https://example.com/org/private.git' --set 'repositories.urls[1].branch=main' \
  --set 'repositories.urls[1].depth=0' --set repositories.tokenSecret.name=git-token --set repositories.prune=false)"
expect "$out" 'Hello-World\|https://github.com/octocat/Hello-World.git\|\|1'
expect "$out" 'private\|https://example.com/org/private.git\|main\|0'
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[] | select(.name == "repo-sync") | .env[] | select(.name == "FORCE_REINDEX_MARKER") | .value')" /data/home/.codesearch/force-reindex "force-reindex marker"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[] | select(.name == "repo-sync") | .env[] | select(.name == "GIT_TOKEN") | .valueFrom.secretKeyRef.name')" git-token "token Secret"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[] | select(.name == "repo-sync") | .env[] | select(.name == "PRUNE") | .value')" false "prune toggle"

echo "== extension points and storage"
out="$(render -f "$chart/ci/extensions.yaml")"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.initContainers[1].image')" "ghcr.io/ryanmcafee/codesearch:$(yq .appVersion "$chart/Chart.yaml")" "extraInitContainers rendered through tpl"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[-1].name')" sidecar "extraContainers"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.volumes[-1].name')" scratch "extraVolumes"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.containers[0].volumeMounts[-1].mountPath')" /scratch "extraVolumeMounts"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.volumes[] | select(.name == "data") | .persistentVolumeClaim.claimName')" restored-data "existingClaim"
expect_absent "$out" 'kind: PersistentVolumeClaim'
out="$(render --set persistence.storageClassName=fast --set 'persistence.annotations.argocd\.argoproj\.io/sync-options=Delete=false')"
expect_eq "$(query "$out" PersistentVolumeClaim codesearch-data '.spec.storageClassName + " " + .metadata.annotations["argocd.argoproj.io/sync-options"]')" "fast Delete=false" "PVC class and annotations"
out="$(render --set persistence.enabled=false)"
expect_eq "$(query "$out" Deployment codesearch '.spec.template.spec.volumes[] | select(.name == "data") | has("emptyDir")')" true "ephemeral data volume"

echo "== ingress and existing Secret"
out="$(render --set ingress.enabled=true --set 'ingress.hosts[0].host=codesearch.example.com' \
  --set auth.existingSecret=codesearch-key --set 'allowedHosts[0]=codesearch.internal' --set 'allowedRoots[0]=/srv/code')"
expect "$out" 'value: ".*,codesearch.example.com,codesearch.internal"'
expect "$out" 'value: "/data/repos;/srv/code"'
expect "$out" 'name: codesearch-key'
expect_absent "$out" 'kind: Secret'

echo "== invalid values are rejected"
for invalid in "ingress.enabled=true" "auth.mode=networkPolicy" "auth.mode=none" \
  "repositories.urls[0]=git@github.com:org/repo.git" "repositories.urls[0].url=https://x/y.git,repositories.urls[0].alias=bad/alias" \
  "image.pullPolicy=Sometimes" "strategy.type=Blue"; do
  if render --set "$invalid" >/dev/null 2>&1; then
    echo "FAIL: accepted $invalid" >&2
    exit 1
  fi
  echo "ok   rejected $invalid"
done
