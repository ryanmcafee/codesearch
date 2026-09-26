# codesearch Helm chart

Runs [codesearch](https://github.com/ryanmcafee/codesearch) `serve`: a semantic code search MCP
server over streamable HTTP, with an optional sidecar that clones and indexes git repositories.
Guide: [docs/kubernetes.md](../../docs/kubernetes.md).

```bash
helm repo add codesearch https://ryanmcafee.github.io/codesearch
helm install codesearch codesearch/codesearch -n codesearch --create-namespace
```

The chart version equals the codesearch version; the default image tag is the chart's
`appVersion`. The chart always runs one replica: MCP sessions are held in memory and each index
has a single writer.

## Values

| Key | Default | Description |
|-----|---------|-------------|
| `image.repository` | `ghcr.io/ryanmcafee/codesearch` | Image |
| `image.tag` | `""` (appVersion) | Image tag |
| `image.digest` | `""` | Pin by digest; overrides `tag` |
| `auth.existingSecret` | `""` | Secret holding the API key; empty creates one |
| `auth.existingSecretKey` | `api-key` | Key inside that Secret |
| `auth.apiKey` | `""` | Key for the chart-created Secret; random (and kept on upgrade) when empty |
| `allowedHosts` | `[]` | Extra `Host` values `/mcp` accepts (Service names, loopback and Ingress hosts are always included) |
| `allowedRoots` | `[]` | Extra roots `POST /repos` may register besides `persistence.repos.mountPath` |
| `serve.port` | `39725` | Container port |
| `serve.extraArgs` | `[]` | Extra `codesearch serve` arguments, e.g. `--model` |
| `extraEnv`, `extraEnvFrom` | `[]` | Extra environment for the serve container (e.g. `CODESEARCH_INDEX_THREADS`) |
| `repositories` | `[]` | Git repos to clone, keep fetched and register: `url` (required), `alias`, `branch`, `depth` (default 1, 0 = full) |
| `repoSync.interval` | `300` | Seconds between fetches |
| `repoSync.git.existingSecret` | `""` | Secret with a token for private HTTPS remotes |
| `repoSync.git.tokenKey` | `token` | Key inside that Secret |
| `repoSync.git.username` | `x-access-token` | Username sent with the token |
| `persistence.data.*` | enabled, `5Gi`, `/home/app/.codesearch` | `repos.json`, embedding cache, logs, model |
| `persistence.repos.*` | enabled, `20Gi`, `/repos` | Checkouts and their `.codesearch.db` indexes |
| `persistence.<volume>.existingClaim` | `""` | Use an existing PVC |
| `persistence.<volume>.storageClass` | `""` | StorageClass; `-` sets `""` |
| `service.type`, `service.port` | `ClusterIP`, `39725` | Service |
| `ingress.enabled` | `false` | Ingress; `ingress.hosts[].host` required when enabled |
| `metrics.serviceMonitor.enabled` | `false` | Prometheus Operator ServiceMonitor with bearer auth from the API key Secret |
| `networkPolicy.enabled` | `false` | Allow ingress only from `networkPolicy.from` (default: the release namespace) |
| `networkPolicy.egress` | `[]` | Egress rules; when set, egress is restricted to these plus DNS |
| `resources` | 250m / 1Gi requests, 4Gi limit | Indexing a large repo needs about 2Gi |
| `podSecurityContext`, `securityContext` | non-root 10001, read-only root filesystem, no capabilities | |
| `nodeSelector`, `tolerations`, `affinity`, `priorityClassName` | | Scheduling |

`values.schema.json` validates every value.

## Testing

```bash
charts/codesearch/ci/template-test.sh   # helm template assertions
helm test codesearch -n codesearch      # healthz, metrics and MCP initialize against a release
```
