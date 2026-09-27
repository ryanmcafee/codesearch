# codesearch Helm chart

Runs [codesearch](https://github.com/ryanmcafee/codesearch) `serve`: a semantic code search MCP
server over streamable HTTP, with a sidecar that clones, syncs and registers git repositories, and
Prometheus/Grafana monitoring. Guide: [docs/kubernetes.md](../../docs/kubernetes.md); alert runbooks:
[docs/runbooks](../../docs/runbooks/README.md).

```bash
helm repo add codesearch https://ryanmcafee.github.io/codesearch
helm install codesearch codesearch/codesearch -n codesearch --create-namespace
```

The chart version equals the codesearch version and the default image tag is the chart's
`appVersion`. The chart always runs one replica: MCP sessions are held in memory and each index has
a single writer.

## Values

| Key | Default | Description |
|-----|---------|-------------|
| `image.repository` / `tag` / `digest` | `ghcr.io/ryanmcafee/codesearch` / appVersion / `""` | Image; `digest` overrides `tag` |
| `strategy.type` | `Recreate` | Deployment strategy |
| `auth.mode` | `apiKey` | `apiKey`, or `networkPolicy` (loopback serve behind a forwarder, no key; requires `networkPolicy.enabled`) |
| `auth.existingSecret` / `existingSecretKey` | `""` / `api-key` | Secret holding the API key; empty creates one, kept on upgrade |
| `auth.apiKey` | `""` | Key for the chart-created Secret; random when empty |
| `forwarder.image.*` | codesearch image | networkPolicy mode: image running `socat` |
| `forwarder.resources` | 10m / 16Mi | networkPolicy mode: forwarder resources |
| `allowedHosts` | `[]` | Extra `Host` values `/mcp` accepts (Service names, loopback and Ingress hosts are always included) |
| `allowedRoots` | `[]` | Extra roots `POST /repos` may register besides `/data/repos` |
| `serve.port` | `39725` | Service port and the pod's `http` port |
| `serve.internalPort` | `39726` | networkPolicy mode: loopback port of serve |
| `serve.extraArgs` | `[]` | Extra `codesearch serve` arguments, e.g. `--model` |
| `extraEnv`, `extraEnvFrom` | `[]` | Extra environment for serve (e.g. `CODESEARCH_INDEX_THREADS`) |
| `repositories.urls` | `[]` | HTTPS URLs, or `{url, alias, branch, depth}`; cloned into `/data/repos/<alias>` |
| `repositories.intervalSeconds` | `300` | Seconds between fetches |
| `repositories.depth` | `1` | Default clone depth; 0 = full history |
| `repositories.tokenSecret.name` / `key` | `""` / `token` | Token for private remotes, sent as an HTTP header, never written to disk |
| `repositories.username` | `x-access-token` | Username sent with the token |
| `repositories.prune` | `true` | Unregister and delete checkouts no longer listed |
| `repositories.resources` | 10m / 32Mi | repo-sync and seed-model resources |
| `persistence.enabled` | `true` | PVC for volume `data` (`/data`); `false` uses an emptyDir |
| `persistence.existingClaim` | `""` | Use an existing PVC |
| `persistence.storageClassName` | `""` | StorageClass; `-` sets `""` |
| `persistence.size` / `accessModes` / `annotations` | `20Gi` / `[ReadWriteOnce]` / `{}` | PVC settings |
| `service.type` / `annotations` | `ClusterIP` / `{}` | Service |
| `ingress.*` | disabled | Ingress; `ingress.hosts[].host` required when enabled |
| `serviceMonitor.enabled` | `true` | ServiceMonitor, rendered only when the CRD exists; bearer auth in apiKey mode |
| `serviceMonitor.interval` / `scrapeTimeout` / `labels` / `relabelings` / `metricRelabelings` | `30s` / `10s` / `{}` / `[]` / `[]` | Scrape settings |
| `prometheusRule.enabled` | `true` | PrometheusRule, rendered only when the CRD exists |
| `prometheusRule.additionalLabels` | `{}` | Labels Prometheus selects rules by |
| `prometheusRule.runbookBaseUrl` | docs/runbooks/README.md on GitHub | Base of every `runbook_url` |
| `prometheusRule.rules.<alert>.*` | see `values.yaml` | Per alert: `enabled`, `for` and thresholds |
| `dashboard.enabled` | `true` | Grafana dashboard ConfigMap |
| `dashboard.namespace` / `labels` / `annotations` | release ns / `grafana_dashboard: "1"` / `{}` | For the Grafana sidecar (e.g. `grafana_folder`) |
| `networkPolicy.enabled` | `false` | NetworkPolicy for the serve port |
| `networkPolicy.ingress.namespaceSelectors` | `[]` | `matchLabels` maps of client namespaces |
| `networkPolicy.ingress.extraPeers` | `[]` | Extra peers (e.g. the monitoring namespace); with neither, the release namespace is allowed |
| `networkPolicy.egress` | `[]` | Egress rules; when set, egress is restricted to these plus DNS |
| `extraInitContainers`, `extraContainers` | `[]` | Extra containers, rendered through `tpl` |
| `extraVolumes`, `extraVolumeMounts` | `[]` | Extra pod volumes / serve mounts |
| `podAnnotations`, `podLabels` | `{}` | Pod metadata |
| `resources` | 250m / 1Gi requests, 4Gi limit | serve resources; indexing a large repo needs about 2Gi |
| `podSecurityContext`, `securityContext` | non-root 10001, read-only root filesystem, no capabilities | |
| `nodeSelector`, `tolerations`, `affinity`, `priorityClassName` | | Scheduling |

`values.schema.json` validates every value.

## Testing

```bash
charts/codesearch/ci/template-test.sh     # helm template assertions (modes, gating, extension points)
charts/codesearch/ci/monitoring-test.sh   # promtool check/test rules, dashboard JSON and queries
helm test codesearch -n codesearch        # healthz, metrics and MCP initialize against a release
```
