#!/usr/bin/env bash
# Checks the chart's monitoring resources:
#   - PrometheusRule: promtool check rules + the unit tests in prometheusrule-test.yaml
#   - dashboard: valid JSON, and every panel query parses as PromQL
# Needs helm, yq, jq and docker (promtool runs from the Prometheus image).
set -euo pipefail

PROMETHEUS_IMAGE="${PROMETHEUS_IMAGE:-prom/prometheus:v3.15.0}"
chart="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

promtool() {
  docker run --rm --user "$(id -u):$(id -g)" -v "$work:/work" -w /work --entrypoint promtool "$PROMETHEUS_IMAGE" "$@"
}

helm template codesearch "$chart" --namespace codesearch \
  --api-versions monitoring.coreos.com/v1/PrometheusRule \
  | yq 'select(.kind == "PrometheusRule") | .spec' >"$work/rules.yaml"
[ -s "$work/rules.yaml" ] || { echo "FAIL: no PrometheusRule rendered" >&2; exit 1; }
cp "$chart/ci/prometheusrule-test.yaml" "$work/"

echo "== promtool check rules"
promtool check rules rules.yaml
echo "== promtool test rules"
promtool test rules prometheusrule-test.yaml

alerts="$(yq '.groups[].rules[].alert' "$work/rules.yaml" | sort -u)"
for alert in $alerts; do
  grep -q "alertname: ${alert}$" "$chart/ci/prometheusrule-test.yaml" \
    || { echo "FAIL: ${alert} has no unit test" >&2; exit 1; }
done
echo "ok   every alert has a unit test"

echo "== dashboard"
dashboard="$chart/dashboards/codesearch.json"
jq -e '.title and .uid and (.panels | length > 0)' "$dashboard" >/dev/null
echo "ok   valid JSON"
# Grafana variables become literal values so promtool can parse each query.
jq -r '[.. | objects | select(has("expr")) | .expr] | unique[]' "$dashboard" \
  | sed -e 's/[$]__rate_interval/5m/g' -e 's/[$][a-z_]*/x/g' \
  | jq -R -s '{groups: [{name: "dashboard", rules: [split("\n")[] | select(length > 0) | {record: "dashboard:query", expr: .}]}]}' \
    >"$work/dashboard-queries.json"
promtool check rules --lint=none dashboard-queries.json
echo "ok   every dashboard query parses"
