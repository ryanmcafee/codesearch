# Running codesearch on Kubernetes

The Helm chart in [`charts/codesearch`](../charts/codesearch) runs `codesearch serve` as a
shared, authenticated MCP server for a team or a cluster of agents. Chart and image are released
together: chart version `X.Y.Z` deploys image `ghcr.io/ryanmcafee/codesearch:X.Y.Z`.

## Install

```bash
helm repo add codesearch https://ryanmcafee.github.io/codesearch
helm install codesearch codesearch/codesearch -n codesearch --create-namespace \
  --set 'repositories[0].url=https://github.com/ryanmcafee/codesearch.git'
```

or from the OCI registry:

```bash
helm install codesearch oci://ghcr.io/ryanmcafee/charts/codesearch --version X.Y.Z -n codesearch --create-namespace
```

## Connect Claude Code

```bash
export CODESEARCH_API_KEY=$(kubectl get secret -n codesearch codesearch-api-key \
  -o jsonpath='{.data.api-key}' | base64 -d)
kubectl port-forward -n codesearch svc/codesearch 39725:39725 &

claude mcp add --transport http codesearch http://localhost:39725/mcp \
  --header "Authorization: Bearer $CODESEARCH_API_KEY"
```

In-cluster agents use `http://codesearch.codesearch.svc.cluster.local:39725/mcp` with the same
header. With `ingress.enabled`, use the Ingress host instead.

## What the chart deploys

- A **Deployment with one replica** (`Recreate`): MCP sessions live in the serve process's
  memory and each index has a single writer, so the chart does not expose `replicaCount`.
- Two **PersistentVolumeClaims**: `data` (`/home/app/.codesearch`: `repos.json`, embedding
  cache, logs, the embedding model) and `repos` (`/repos`: checkouts, each holding its index in
  `.codesearch.db`). An init container copies the baked-in embedding model into `data`, so the
  pod starts without network access.
- The **API key** Secret. The chart generates a random key once and reuses it on upgrade, or
  reads `auth.existingSecret`. Every route except `/healthz` requires it; probes use `/healthz`.
- `CODESEARCH_ALLOWED_HOSTS` set to the Service's short, namespaced and FQDN names, loopback
  (for `kubectl port-forward`), the Ingress hosts and `allowedHosts`.
- `CODESEARCH_ALLOWED_ROOTS` set to `/repos` plus `allowedRoots`, so `POST /repos` cannot
  register paths elsewhere in the container.

## Repositories

Each entry in `repositories` is cloned into `/repos/<alias>` by the `repo-sync` sidecar, which
fetches every `repoSync.interval` seconds, registers new repos through `POST /repos` and
requests a reindex when `HEAD` moves:

```yaml
repositories:
  - url: https://github.com/example/service.git
    branch: main   # default: the remote HEAD
    depth: 1       # default 1; 0 clones full history
    alias: service # default: repository name
repoSync:
  git:
    existingSecret: git-token  # Secret with key `token`, for private HTTPS remotes
```

Without `repositories`, put checkouts on the `repos` volume yourself and register them with
`POST /repos` (see the chart's NOTES output).

## Monitoring and network access

- `metrics.serviceMonitor.enabled` creates a Prometheus Operator ServiceMonitor that scrapes
  `/metrics` with the API key as bearer token.
- `networkPolicy.enabled` limits ingress to the serve port from `networkPolicy.from` (default:
  pods in the release namespace). Add the Ingress controller's and Prometheus's namespaces
  there when they are elsewhere. `networkPolicy.egress` restricts egress (DNS is always
  allowed); the sidecar needs HTTPS to your git hosts.

All values are documented in the [chart README](../charts/codesearch/README.md).
