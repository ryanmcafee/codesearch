# Running codesearch in Docker

`ghcr.io/ryanmcafee/codesearch` is a multi-arch (`linux/amd64`, `linux/arm64`) image built from
[`docker/Dockerfile`](../docker/Dockerfile) on every release. It runs the `codesearch` binary as
a non-root user (uid 10001) with the default embedding model baked in, so it starts without
network access.

| Tag | Meaning |
|-----|---------|
| `vX.Y.Z`, `X.Y.Z` | One release |
| `X.Y` | Latest patch of a minor release |
| `latest` | Latest release |

The entrypoint is `codesearch`, so any subcommand works: `docker run IMAGE <subcommand> ...`.
Without arguments it runs `serve --host 0.0.0.0 --no-tui`.

## Where state lives

| Path | Content | Mount |
|------|---------|-------|
| `<repo>/.codesearch.db` | The repo's index | The repo itself must be writable |
| `/home/app/.codesearch` | `repos.json`, embedding cache, logs, the embedding model | A named volume keeps it across containers |

A **named** volume on `/home/app/.codesearch` starts with the baked-in model; a bind mount
there hides it, and codesearch then downloads the model on first use.

On Linux, bind-mounted repos are owned by your user, not uid 10001. Run the container as
yourself with `--user "$(id -u):$(id -g)"`; `/home/app/.codesearch` is writable by any uid.

## stdio MCP (one repo, no server)

The agent starts the container per session and talks MCP over stdin/stdout:

```bash
claude mcp add codesearch -- \
  docker run -i --rm --user "$(id -u):$(id -g)" \
    -v "$PWD:/workspace" -v codesearch-data:/home/app/.codesearch \
    ghcr.io/ryanmcafee/codesearch mcp --mode local /workspace
```

`-i` keeps stdin open; do not add `-t`. The first session indexes the repo into
`$PWD/.codesearch.db`; later sessions refresh it incrementally.

## HTTP MCP (many repos, one server)

```bash
export CODESEARCH_SERVE_API_KEY=$(openssl rand -hex 32)

docker run -d --name codesearch --restart unless-stopped \
  -p 127.0.0.1:39725:39725 \
  --user "$(id -u):$(id -g)" \
  -e CODESEARCH_SERVE_API_KEY \
  -e CODESEARCH_ALLOWED_ROOTS=/repos \
  -v codesearch-data:/home/app/.codesearch \
  -v "$HOME/Projects:/repos" \
  ghcr.io/ryanmcafee/codesearch
```

The server binds `0.0.0.0` inside the container, so it refuses to start without
`CODESEARCH_SERVE_API_KEY`; every route except `/healthz` then needs
`Authorization: Bearer <key>` (or `X-API-Key`). Publish the port on `127.0.0.1` unless other
hosts should reach it. [`docker/compose.yaml`](../docker/compose.yaml) is the same setup for
`docker compose`.

Register repositories by their path **inside** the container (indexing runs in the
background; `409` means the path is already registered):

```bash
curl -X POST http://localhost:39725/repos \
  -H "Authorization: Bearer $CODESEARCH_SERVE_API_KEY" -H 'Content-Type: application/json' \
  -d '{"path": "/repos/my-service"}'
```

Registrations persist in the `codesearch-data` volume. Connect Claude Code:

```bash
claude mcp add --transport http codesearch http://localhost:39725/mcp \
  --header "Authorization: Bearer $CODESEARCH_SERVE_API_KEY"
```

`/mcp` accepts the `Host` headers `localhost`, `127.0.0.1` and `::1` by default. When clients
use another name (a reverse proxy, another container), list it in `CODESEARCH_ALLOWED_HOSTS`
(comma-separated; an entry without a port matches any port).

## Backups

The image also ships `mdb_copy`/`mdb_stat` built from the LMDB sources codesearch links (Debian's
`lmdb-utils` cannot open codesearch's LMDB files) and GNU tar, so it can back up its own state while
serve runs:

```bash
docker exec codesearch sh -ec 'mkdir -p /tmp/bk/minilm-l6-q
  mdb_copy -c ~/.codesearch/embedding_cache/minilm-l6-q /tmp/bk/minilm-l6-q
  cp ~/.codesearch/repos.json /tmp/bk/ && tar -C /tmp/bk -czf /tmp/codesearch-backup.tgz .'
docker cp codesearch:/tmp/codesearch-backup.tgz .
```

Indexes (`<repo>/.codesearch.db`) are derived from git and are cheaper to rebuild than to back up.
See [runbooks](runbooks/README.md#operations) for restore steps.

## Stopping

codesearch shuts down cleanly on `SIGINT`; the image sets `STOPSIGNAL SIGINT`, so
`docker stop` works as expected.

## Building locally

```bash
docker build -f docker/Dockerfile -t codesearch:local .
docker/smoke-test.sh codesearch:local
docker/stdio-test.sh codesearch:local /path/to/a/small/repo
```

The root [`Dockerfile`](../Dockerfile) is a different image: the Azure federation deployment
([`integrations/cloud`](../integrations/cloud/README.md)).
