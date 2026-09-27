# Running codesearch on Kubernetes

The Helm chart in [`charts/codesearch`](../charts/codesearch) runs `codesearch serve` as a shared MCP
server for a team or for in-cluster agents. Chart and image are released together: chart version
`X.Y.Z` deploys image `ghcr.io/ryanmcafee/codesearch:X.Y.Z`.

## Install

```bash
helm repo add codesearch https://ryanmcafee.github.io/codesearch
helm install codesearch codesearch/codesearch -n codesearch --create-namespace \
  --set 'repositories.urls[0]=https://github.com/ryanmcafee/codesearch.git'
# or: helm install codesearch oci://ghcr.io/ryanmcafee/charts/codesearch --version X.Y.Z ...
```

## Connect Claude Code

```bash
export CODESEARCH_API_KEY=$(kubectl get secret -n codesearch codesearch-api-key \
  -o jsonpath='{.data.api-key}' | base64 -d)
kubectl port-forward -n codesearch svc/codesearch 39725:39725 &

claude mcp add --transport http codesearch http://localhost:39725/mcp \
  --header "Authorization: Bearer $CODESEARCH_API_KEY"
```

In-cluster agents use `http://codesearch.codesearch.svc.cluster.local:39725/mcp`. With
`ingress.enabled`, use the Ingress host.

## What the chart deploys

- A **Deployment with one replica** (`strategy.type: Recreate`): MCP sessions live in the serve
  process's memory and each index has a single writer, so there is no replica count.
- One **PersistentVolumeClaim** `<release>-data` (volume `data`, mounted at `/data`; `HOME` is
  `/data/home`): `/data/home/.codesearch` holds `repos.json`, the embedding cache, logs and the
  embedding model (copied from the image by the `seed-model` init container, so the pod starts
  offline); `/data/repos` holds checkouts, each with its index in `.codesearch.db`.
- `CODESEARCH_ALLOWED_HOSTS` set to the Service's short, namespaced and FQDN names, loopback (for
  `kubectl port-forward`), the Ingress hosts and `allowedHosts`; `CODESEARCH_ALLOWED_ROOTS` set to
  `/data/repos` plus `allowedRoots`.
- Probes on the unauthenticated `/healthz`, a read-only root filesystem and a non-root user.

## Authentication modes

| `auth.mode` | How it works | Use when |
|-------------|--------------|----------|
| `apiKey` (default) | serve listens on the pod IP; every route except `/healthz` needs `Authorization: Bearer <key>`. The key comes from `auth.existingSecret` or a generated Secret that is kept across upgrades. | Clients can send a header, or traffic crosses an Ingress |
| `networkPolicy` | serve listens on `127.0.0.1:39726` (loopback needs no key) and a `forwarder` container (socat, from the codesearch image by default) exposes it on port 39725. The chart refuses to render unless `networkPolicy.enabled` is true. | Clients cannot send headers and the admitted namespaces are trusted |

Trade-off of `networkPolicy` mode: the NetworkPolicy is the **only** access control. Any pod it
admits can search every indexed repo and call the management API (`POST /repos`,
`DELETE /repos/<alias>`, reindex). It relies on a CNI that enforces NetworkPolicy, and
`kubectl port-forward` or node-level processes bypass it. `CODESEARCH_ALLOWED_ROOTS` and Host
validation still apply.

```yaml
auth:
  mode: networkPolicy
networkPolicy:
  enabled: true
  ingress:
    namespaceSelectors:
      - codesearch-client: "true"           # label client namespaces
    extraPeers:
      - namespaceSelector:                   # let Prometheus scrape
          matchLabels:
            kubernetes.io/metadata.name: monitoring
```

The Service's `targetPort` is the pod's `http` port (39725) in both modes, so client-side policies
keyed to 39725 keep working.

## Repositories

The `repo-sync` sidecar keeps `repositories.urls` cloned under `/data/repos/<alias>`:

- clones (shallow by default), fetches every `repositories.intervalSeconds`, registers each repo with
  `POST /repos` (`202`, or `409` when already registered) and requests an incremental reindex when
  `HEAD` moves;
- with `repositories.prune` (default), unregisters (`DELETE /repos/<alias>`) and deletes checkouts
  that are no longer listed. It owns `/data/repos`: do not put other checkouts there;
- sends `repositories.tokenSecret` as an HTTP `Authorization` header through git's environment,
  so the token is never written to disk;
- when `/data/home/.codesearch/force-reindex` exists (for example after a restore), force-reindexes
  every configured repo once and deletes the file.

```yaml
repositories:
  urls:
    - https://github.com/example/service.git
    - url: https://github.com/example/monorepo.git
      branch: main
      depth: 0          # full history
      alias: mono
  intervalSeconds: 300
  tokenSecret:
    name: git-token     # Secret with key `token`
    key: token
```

## Monitoring

When the cluster serves the Prometheus Operator CRDs, the chart also renders (each can be turned off):

- `serviceMonitor`: scrapes `/metrics`, with the API key as bearer token in `apiKey` mode.
- `prometheusRule`: `CodesearchDown`, `CodesearchDegraded`, `CodesearchToolCallErrors`,
  `CodesearchToolCallLatencyHigh`, `CodesearchRepoIndexFailing`, `CodesearchRepoIndexStale`,
  `CodesearchIndexQueueBacklog`, `CodesearchHighMemory`, `CodesearchPVCAlmostFull` and
  `CodesearchRestarting`, each with thresholds under `prometheusRule.rules.*` and a `runbook_url` into
  the [runbooks](runbooks/README.md). `prometheusRule.additionalLabels` adds the labels your
  Prometheus selects rules by (e.g. `release: kube-prometheus-stack`).

`dashboard.enabled` (default) renders a ConfigMap with the Grafana dashboard
([`dashboards/codesearch.json`](../charts/codesearch/dashboards/codesearch.json)) labelled
`grafana_dashboard: "1"` for the kube-prometheus-stack sidecar; `dashboard.namespace`,
`dashboard.labels` and `dashboard.annotations` (e.g. `grafana_folder`) fit other sidecar setups.

Backup, restore and upgrade procedures are in the [runbooks](runbooks/README.md#operations).

## Extension points

`extraInitContainers` and `extraContainers` are rendered through `tpl`, so they can reference values
(`image: '{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}'`) and
mount the `data` volume. `extraVolumes`, `extraVolumeMounts` (serve container), `extraEnv`,
`extraEnvFrom`, `podAnnotations`, `podLabels`, `persistence.existingClaim`,
`persistence.storageClassName` and `persistence.annotations` cover the rest. All values are listed in
the [chart README](../charts/codesearch/README.md).
