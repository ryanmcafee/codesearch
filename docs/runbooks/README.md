# codesearch runbooks

Runbooks for the alerts in the Helm chart's PrometheusRule (`charts/codesearch`), plus backup,
restore and upgrade procedures. Every alert's `runbook_url` points at its section here.

## Before you start

The examples assume the release is named `codesearch`; set the namespace first:

```bash
NS=codesearch
# cs <path> [curl args]: calls serve from inside the pod. 127.0.0.1:39725 works in both
# auth modes (in networkPolicy mode it is the forwarder in front of serve).
cs() {
  kubectl -n "$NS" exec deploy/codesearch -c codesearch -- sh -c '
    path=$1; shift
    [ -n "${CODESEARCH_SERVE_API_KEY:-}" ] && set -- "$@" -H "Authorization: Bearer $CODESEARCH_SERVE_API_KEY"
    curl -sS "$@" "http://127.0.0.1:39725$path"' sh "$@"
}
cs /api/summary        # ok/degraded and the reasons, latency, governor, repo counts
cs /api/repos          # per-repo index health (?status=failing)
cs '/api/events?limit=50'  # recent index and governor events
```

Where to look:

| What | Where |
|------|-------|
| Pod state and events | `kubectl -n $NS get pods,pvc`; `kubectl -n $NS describe pod -l app.kubernetes.io/instance=codesearch` |
| Serve log (detailed; file-only) | `/data/home/.codesearch/logs/` in the `codesearch` container |
| Per-repo index log | `/data/repos/<alias>/.codesearch.db/logs/` |
| Repo sync (clone, fetch, register) | `kubectl -n $NS logs deploy/codesearch -c repo-sync` |
| Container stdout (startup, panics) | `kubectl -n $NS logs deploy/codesearch -c codesearch --previous` |
| Dashboard | Grafana, dashboard "codesearch" (uid `codesearch`) |

```bash
kubectl -n "$NS" exec deploy/codesearch -c codesearch -- sh -c 'tail -n 200 "$(ls -t /data/home/.codesearch/logs/* | head -1)"'
```

Escalation: codesearch is a developer tool; nothing downstream depends on it for correctness.
Escalate to the codesearch owner (open an issue at <https://github.com/ryanmcafee/codesearch/issues>
with `/api/summary`, `/api/events` and the log tail) when the mitigation below does not help.

## CodesearchDown

**Meaning:** Prometheus cannot scrape `/metrics`, or the scrape target has disappeared.

**Impact:** MCP clients cannot search; agents fall back to grep.

**Diagnosis:**
1. `kubectl -n $NS get pods -l app.kubernetes.io/instance=codesearch` -- Pending, CrashLoopBackOff, or not Ready?
2. `kubectl -n $NS describe pod ...` -- look for OOMKilled, failed volume mounts (`codesearch-data` is ReadWriteOnce), image pull errors.
3. `kubectl -n $NS logs deploy/codesearch -c codesearch --previous` and the serve log.
4. If the pod is Ready but the target is down, check the ServiceMonitor selector and, with
   `networkPolicy.enabled`, that the Prometheus namespace is in `networkPolicy.ingress.extraPeers`.

**Mitigation:** fix the cause shown above. OOMKilled: raise `resources.limits.memory` (indexing large
repos needs about 2 GiB). A corrupted index shows up as repeated open errors for one repo: delete
`/data/repos/<alias>/.codesearch.db` and let repo-sync re-register it, or touch the force-reindex marker
(see [Force a reindex](#force-a-reindex)).

## CodesearchDegraded

**Meaning:** `codesearch_status{status="degraded"}` has been 1 for 15 minutes. codesearch judges
itself degraded when tool-call latency breaks the SLO, repos fail to index, or indexing is paused.

**Impact:** searches still work but may be slow or miss recent changes.

**Diagnosis:** `cs /api/summary` lists the reasons; follow the matching section below
(latency -> [CodesearchToolCallLatencyHigh](#codesearchtoolcalllatencyhigh), failing repos ->
[CodesearchRepoIndexFailing](#codesearchrepoindexfailing), governor paused on memory pressure ->
[CodesearchHighMemory](#codesearchhighmemory)).

**Mitigation:** resolve the listed reason. The status recovers on its own once it clears.

## CodesearchToolCallErrors

**Meaning:** more than 5% (`prometheusRule.rules.toolCallErrors.ratio`) of MCP tool calls failed over 15 minutes.

**Impact:** agents get errors instead of results.

**Diagnosis:**
1. Dashboard "Tool-call failures by tool": one tool or all?
2. Serve log: errors are logged with the tool name and the full error chain.
3. `cs /api/repos` -- a failing or missing repo makes every scoped call to it fail.
4. Client misuse (unknown `project`, missing scope) also counts; check whether one client dominates.

**Mitigation:** reindex the failing repo (`cs '/repos/<alias>/reindex?force=true' -X POST`);
restart the pod if every tool fails with store errors.

## CodesearchToolCallLatencyHigh

**Meaning:** p95 tool-call latency over 15 minutes is above `codesearch_slo_seconds` (`CODESEARCH_SLO_MS`, default 5 s).

**Impact:** agents wait; some clients time out.

**Diagnosis:**
1. Dashboard "p95 latency by tool" and "Governor jobs": does latency track indexing?
2. `cs /api/summary` -- governor state, QoS, recent read max.
3. CPU throttling: compare "CPU" with the container's CPU limit.

**Mitigation:** let a large initial index finish; reduce indexing pressure
(`extraEnv: CODESEARCH_INDEX_THREADS=1`, `CODESEARCH_INDEX_MAX_JOBS=1`); raise CPU requests/limits;
narrow group searches (`group="all"` fans out to every repo).

## CodesearchRepoIndexFailing

**Meaning:** the last 3 (`consecutiveFailures`) index jobs of one repo failed.

**Impact:** that repo's results are stale or missing.

**Diagnosis:** `cs '/api/repos?status=failing'` and `cs '/api/events?limit=50'` show the error;
the repo's own log is `/data/repos/<alias>/.codesearch.db/logs/`. Common causes: the volume is full
([CodesearchPVCAlmostFull](#codesearchpvcalmostfull)), memory (OOM during embedding), a corrupt index,
or the checkout was removed.

**Mitigation:** fix the cause, then force a reindex of that repo:

```bash
cs '/repos/<alias>/reindex?force=true' -X POST
```

If it keeps failing, delete `/data/repos/<alias>` entirely; repo-sync re-clones and re-registers it.

## CodesearchRepoIndexStale

**Meaning:** a repo has unindexed changes (`codesearch_repo_pending_changes > 0`) and its last
successful index is older than `maxAgeSeconds` (default 1 h).

**Impact:** search misses recent commits in that repo.

**Diagnosis:** is the repo queued behind others ([CodesearchIndexQueueBacklog](#codesearchindexqueuebacklog)),
failing ([CodesearchRepoIndexFailing](#codesearchrepoindexfailing)), or is the governor paused
(`cs /api/summary`, "Paused jobs by reason")?

**Mitigation:** request a reindex (`cs /repos/<alias>/reindex -X POST`); resolve the pause reason.

## CodesearchIndexQueueBacklog

**Meaning:** more than 5 (`queuedRepos`) repos have waited for an indexing slot for 30 minutes.

**Impact:** indexes lag behind the repositories.

**Diagnosis:** dashboard "Governor jobs" and "Paused jobs by reason". A paused governor (memory
pressure, other CPU load) or one very large repo holding the only slot are typical.

**Mitigation:** raise `CODESEARCH_INDEX_MAX_JOBS` (more memory per job) and CPU; stagger initial
registration of many repos; resolve the pause reason.

## CodesearchHighMemory

**Meaning:** the `codesearch` container's working set is above 90% (`limitRatio`) of its memory
limit (`basis="limit"`), or, without kube-state-metrics, resident memory is above `residentBytes`
(`basis="resident"`).

**Impact:** the next indexing burst may be OOM-killed, which restarts the pod and drops MCP sessions.

**Diagnosis:** dashboard "Resident memory"; is an index job running (`cs /api/summary`)? The ONNX
runtime keeps its arena allocation after large embedding batches, so memory stays high after indexing.

**Mitigation:** raise `resources.limits.memory`; set `CODESEARCH_INDEX_MAX_JOBS=1` and a smaller
`CODESEARCH_BATCH_SIZE`; a restart returns the arena memory.

## CodesearchPVCAlmostFull

**Meaning:** the data volume (`codesearch-data`) is above 85% (warning) or 95% (critical).

**Impact:** indexing and the embedding cache fail when it fills; LMDB cannot grow.

**Diagnosis:**

```bash
kubectl -n "$NS" exec deploy/codesearch -c codesearch -- du -xh -d 2 /data | sort -h | tail -20
```

Usually checkouts with full history (`repositories.depth: 0`), many repos, or a large embedding cache.

**Mitigation:** expand the PVC (`persistence.size`, if the StorageClass allows expansion); use shallow
clones; remove repos you no longer need from `repositories.urls` (prune deletes them); clear the
embedding cache (`codesearch cache clear -y`, it is rebuilt on demand).

## CodesearchRestarting

**Meaning:** a container in the codesearch pod restarted more than 3 times within an hour.

**Impact:** every restart drops MCP sessions and re-warms indexes.

**Diagnosis:** `kubectl -n $NS describe pod ...` (last state, exit code, OOMKilled), then the
container's `--previous` logs. For `repo-sync`, clone failures (auth, URL) only log; restarts there
mean the script itself died.

**Mitigation:** see [CodesearchDown](#codesearchdown) and [CodesearchHighMemory](#codesearchhighmemory).

## Operations

### What is on the data volume

| Path | Content | Recoverable from |
|------|---------|------------------|
| `/data/home/.codesearch/repos.json` | Registered repos, groups, remotes | Backup, or repo-sync re-registers configured repos |
| `/data/home/.codesearch/embedding_cache/<model>/` | LMDB cache of chunk embeddings | Backup; otherwise recomputed (slow, CPU-heavy) |
| `/data/home/.codesearch/models/` | Embedding model | The image (seeded by the `seed-model` init container) |
| `/data/repos/<alias>/` | Git checkouts | The git remotes |
| `/data/repos/<alias>/.codesearch.db/` | Per-repo index | Reindexing the checkout |

Only `repos.json` and the embedding cache are worth backing up; indexes are derived from git.

### Backup

`mdb_copy -c` takes a consistent copy of a live LMDB environment, so no downtime is needed. The
image ships `mdb_copy`/`mdb_stat` built from the same LMDB sources codesearch links (Debian's
`lmdb-utils` is a different LMDB version and fails with `MDB_VERSION_MISMATCH`) and GNU tar:

```bash
kubectl -n "$NS" exec deploy/codesearch -c codesearch -- sh -ec '
  out=/tmp/backup; rm -rf "$out"; mkdir -p "$out/embedding_cache"
  for dir in /data/home/.codesearch/embedding_cache/*/; do
    model=$(basename "$dir"); mkdir -p "$out/embedding_cache/$model"
    mdb_copy -c "$dir" "$out/embedding_cache/$model"
  done
  cp /data/home/.codesearch/repos.json "$out/"
  tar -C "$out" -czf /tmp/codesearch-backup.tgz . && rm -rf "$out"'
kubectl -n "$NS" cp -c codesearch "$(kubectl -n "$NS" get pod -l app.kubernetes.io/instance=codesearch -o name | head -1 | cut -d/ -f2)":/tmp/codesearch-backup.tgz ./codesearch-backup.tgz
```

A scheduled backup can run the same commands in a CronJob with the codesearch image (it has `sh`,
`tar`, `mdb_copy`) and the data PVC mounted read-only, as long as the volume supports it
(ReadWriteMany, or scheduling on the same node for ReadWriteOnce). Volume snapshots work too; LMDB is
crash-consistent, so a snapshot restores like an unclean shutdown.

### Restore

1. Put the files back while serve is stopped, e.g. from an `extraInitContainers` entry that unpacks
   the archive into `/data/home/.codesearch/` (mount the `data` volume at `/data`).
2. Touch `/data/home/.codesearch/force-reindex`. On its next cycle repo-sync force-reindexes every
   configured repo once and deletes the file, so indexes match the restored registrations.
3. Check `cs /api/repos` and the dashboard until every repo is indexed.

### Force a reindex

```bash
kubectl -n "$NS" exec deploy/codesearch -c codesearch -- touch /data/home/.codesearch/force-reindex
kubectl -n "$NS" logs -f deploy/codesearch -c repo-sync   # "force-reindex marker removed"
```

### Upgrades

- Chart and image versions are the codesearch version; upgrading the chart upgrades codesearch.
- The Deployment uses `Recreate`: expect a short outage while the pod restarts; MCP clients reconnect.
- Read the release's `CHANGELOG.md` entry. An index format change reindexes repos on first open;
  budget CPU and time for it on large installations (watch "Governor jobs").
- A new default embedding model is added to the data volume by the `seed-model` init container;
  existing indexes keep the model they record.
- Take a backup first if the embedding cache is large; it is the only state that is slow to rebuild.

### Alertmanager routing example

Not part of the chart. Routes codesearch alerts to a team receiver, critical ones paged:

```yaml
route:
  routes:
    - matchers: ['alertname=~"Codesearch.*"', 'severity="critical"']
      receiver: dev-tools-pager
    - matchers: ['alertname=~"Codesearch.*"']
      receiver: dev-tools-chat
      group_by: [alertname, namespace, repo]
      repeat_interval: 12h
```
