# syntax=docker/dockerfile:1
# check=error=true
ARG COYOTE_VERSION
FROM docker/sandbox-templates:shell-docker AS build

ARG COYOTE_VERSION
ARG TARGETARCH

ENV PATH="/home/agent/.cargo/bin:/home/agent/.local/bin:${PATH}"

USER root

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      jq curl git \
      build-essential pkg-config \
      cmake \
      clang libclang-dev \
      musl-tools \
      libssl-dev \
      pandoc \
      bzip2 \
      nano && \
    rm -rf /var/lib/apt/lists/*

RUN set -euo pipefail; \
    USQL_VERSION=0.21.4; \
    case "${TARGETARCH}" in \
      amd64) USQL_ARCH=amd64 ;; \
      arm64) USQL_ARCH=arm64 ;; \
      *) echo "Unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    TMPDIR=$(mktemp -d); \
    curl -fsSL --retry 3 \
      "https://github.com/xo/usql/releases/download/v${USQL_VERSION}/usql_static-${USQL_VERSION}-linux-${USQL_ARCH}.tar.bz2" \
      -o "$TMPDIR/usql.tar.bz2"; \
    tar -xjf "$TMPDIR/usql.tar.bz2" -C "$TMPDIR"; \
    install -m 0755 "$TMPDIR/usql_static" /usr/local/bin/usql; \
    rm -rf "$TMPDIR"

RUN set -euo pipefail; \
    DUCKDB_VERSION=1.5.5; \
    case "${TARGETARCH}" in \
      amd64) DUCKDB_ARCH=amd64 ;; \
      arm64) DUCKDB_ARCH=arm64 ;; \
      *) echo "Unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    TMPDIR=$(mktemp -d); \
    curl -fsSL --retry 3 \
      "https://github.com/duckdb/duckdb/releases/download/v${DUCKDB_VERSION}/duckdb_cli-linux-${DUCKDB_ARCH}.gz" \
      -o "$TMPDIR/duckdb.gz"; \
    gunzip "$TMPDIR/duckdb.gz"; \
    install -m 0755 "$TMPDIR/duckdb" /usr/local/bin/duckdb; \
    rm -rf "$TMPDIR"

USER 1000

RUN curl -LsSf https://astral.sh/uv/install.sh | sh && \
    printf '#!/bin/sh\nexec uv tool run "$@"\n' > "$HOME/.local/bin/uvx" && \
    chmod +x "$HOME/.local/bin/uvx"

RUN mkdir -p /usr/local/share/npm-global/lib

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
      sh -s -- -y --default-toolchain stable --profile minimal && \
    . "$HOME/.cargo/env" && \
    cargo install --locked iwec && \
    cargo install --locked ast-grep

# The rns version the mesh interop harness is verified against; the propagation-node
# Dockerfile declares the same value and tests/scripts_pins.rs holds the two together.
ARG RNS_VERSION=1.5.2

# rnsd for the entrypoint. A uv-managed CPython 3.12 (the pinned rns is interop-verified on 3.12)
# lands under ~/.local/share/uv/python and rides the flatten below with the tool venv;
# --no-build makes a missing wheel fail the build instead of compiling under QEMU on arm64.
RUN UV_NO_CACHE=1 uv tool install --no-build --python 3.12 "rns==${RNS_VERSION}"

USER root

RUN set -euo pipefail; \
    case "${TARGETARCH}" in \
      amd64) MUSL_TARGET=x86_64-unknown-linux-musl ;; \
      arm64) MUSL_TARGET=aarch64-unknown-linux-musl ;; \
      *) echo "Unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    TMPDIR=$(mktemp -d); \
    curl -fsSL --retry 3 \
      "https://github.com/Dark-Alex-17/coyote/releases/download/v${COYOTE_VERSION}/coyote-${MUSL_TARGET}.tar.gz" \
      -o "$TMPDIR/coyote.tar.gz"; \
    tar -xzf "$TMPDIR/coyote.tar.gz" -C "$TMPDIR"; \
    install -m 0755 "$TMPDIR/coyote" /home/agent/.cargo/bin/coyote; \
    chown 1000:1000 /home/agent/.cargo/bin/coyote; \
    rm -rf "$TMPDIR"

COPY --chmod=0755 scripts/docker-entrypoint.sh /usr/local/bin/coyote-entrypoint
# Left to COPY, /opt/coyote would be created with the file mode and uid 1000 could not traverse it.
RUN install -d -m 0755 /opt/coyote
COPY --chmod=0644 scripts/reticulum.config.tmpl /opt/coyote/reticulum.config.tmpl

FROM scratch

ARG COYOTE_VERSION

COPY --from=build / /

ENV PATH="/home/agent/.cargo/bin:/home/agent/.local/bin:/usr/local/share/npm-global/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
    NPM_CONFIG_PREFIX="/usr/local/share/npm-global" \
    NO_PROXY="localhost,127.0.0.1,::1,172.17.0.0/16" \
    no_proxy="localhost,127.0.0.1,::1,172.17.0.0/16" \
    BASH_ENV="/etc/sandbox-persistent.sh"

LABEL com.docker.sandboxes="templates" \
      com.docker.sandboxes.base="ubuntu:questing" \
      com.docker.sandboxes.flavor="shell-docker" \
      com.docker.sandboxes.start-docker="true" \
      org.opencontainers.image.title="coyote" \
      org.opencontainers.image.description="The batteries-included runtime for LLMs: Shell Assistant, CLI & REPL mode, RAG, AI tools & agents, MCP servers, skills, and macros." \
      org.opencontainers.image.source="https://github.com/Dark-Alex-17/coyote" \
      org.opencontainers.image.version="${COYOTE_VERSION}"

WORKDIR /home/agent/workspace

USER 1000

# tini as PID 1: -s (subreaper) reaps the orphans a dying main command leaves behind,
# -g delivers TERM/INT to the whole process group so the main command receives them
# directly. coyote-entrypoint starts rnsd in its own session (opt out with
# COYOTE_MESH_RNSD=0), runs coyote, or the given command (e.g. the Docker Sandboxes
# keep-alive) when the first arg is sh/bash/a path, in its foreground, then stops rnsd
# and exits with the main command's code.
ENTRYPOINT ["/usr/bin/tini", "-s", "-g", "--", "/usr/local/bin/coyote-entrypoint"]
