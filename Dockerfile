ARG COYOTE_VERSION
FROM docker/sandbox-templates:shell-docker

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

USER 1000

ENTRYPOINT ["coyote"]
