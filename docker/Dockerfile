# syntax=docker/dockerfile:1
#
# General-purpose codesearch image (ghcr.io/ryanmcafee/codesearch).
#
#   docker run -i --rm -v "$PWD:/workspace" ghcr.io/ryanmcafee/codesearch mcp /workspace
#   docker run -d -p 127.0.0.1:39725:39725 -e CODESEARCH_SERVE_API_KEY=... ghcr.io/ryanmcafee/codesearch
#
# Separate from the root Dockerfile (the Azure federation image): that one must
# build on ACR's classic builder and ships azcopy + docker/entrypoint.sh. This
# one uses BuildKit cache mounts and runs the plain `codesearch` binary.
# Docs: docs/docker.md, docs/kubernetes.md.

# trixie (glibc 2.41): the prebuilt onnxruntime needs glibc >= 2.38.
FROM rust:1-trixie AS builder
WORKDIR /src

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev cmake \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
# Cargo.toml declares [[bench]] search_fanout; the manifest does not parse without it.
COPY benches ./benches
# Embedded via include_str! in src/cli/claude_hooks.rs.
COPY integrations/claude-code/hooks ./integrations/claude-code/hooks

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin codesearch \
    && install -m 0755 target/release/codesearch /usr/local/bin/codesearch \
    && mkdir -p /out/lib \
    && find target/release -maxdepth 2 -name 'libonnxruntime*.so*' -exec cp {} /out/lib/ \;

# Bake the default embedding model: indexing a throwaway repo makes fastembed
# download it into ~/.codesearch/models, so containers start offline.
ENV HOME=/home/app
RUN set -eux; \
    mkdir -p /tmp/warm; \
    printf '# warmup\nhello world\n' > /tmp/warm/README.md; \
    LD_LIBRARY_PATH=/out/lib codesearch index add /tmp/warm > /dev/null 2>&1 || true; \
    test -n "$(ls -A /home/app/.codesearch/models 2>/dev/null)" \
        || { echo 'ERROR: model warmup did not populate ~/.codesearch/models' >&2; exit 1; }

FROM debian:trixie-slim AS runtime

ARG VERSION=dev
ARG REVISION=unknown
LABEL org.opencontainers.image.title="codesearch" \
      org.opencontainers.image.description="Local semantic code search MCP server" \
      org.opencontainers.image.source="https://github.com/ryanmcafee/codesearch" \
      org.opencontainers.image.url="https://github.com/ryanmcafee/codesearch" \
      org.opencontainers.image.documentation="https://github.com/ryanmcafee/codesearch/blob/master/docs/docker.md" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

# git: repo metadata and the Helm chart's clone sidecar; curl: health probes and
# the chart's registration calls; libgomp1: onnxruntime; tini: reaps git children.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git libgomp1 tini \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 10001 app \
    && useradd --uid 10001 --gid app --create-home --home-dir /home/app --shell /usr/sbin/nologin app \
    && mkdir -p /workspace /repos \
    && chown app:app /workspace /repos

COPY --from=builder /usr/local/bin/codesearch /usr/local/bin/codesearch
COPY --from=builder /out/lib/ /usr/local/lib/
# Only the model cache: the warmup's repos.json must not register /tmp/warm.
COPY --from=builder --chown=app:app /home/app/.codesearch/models /home/app/.codesearch/models
# Lets `docker run --user "$(id -u):$(id -g)"` keep state here and read the model.
RUN chmod 0755 /home/app && chmod 1777 /home/app/.codesearch && chmod -R a+rX /home/app/.codesearch/models

ENV HOME=/home/app \
    LD_LIBRARY_PATH=/usr/local/lib \
    CODESEARCH_SERVE_PORT=39725

USER 10001:10001
WORKDIR /workspace
EXPOSE 39725

# codesearch handles SIGINT only (ctrlc); tini forwards it.
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/bin/tini", "--", "codesearch"]
CMD ["serve", "--host", "0.0.0.0", "--no-tui"]
