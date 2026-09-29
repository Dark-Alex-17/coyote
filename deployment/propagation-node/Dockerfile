# syntax=docker/dockerfile:1.7
FROM python:3.12-slim

# The versions the mesh interop harness is verified against
# (scripts/mesh-interop/README.md: RNS 1.5.2 / LXMF 0.9.6).
ARG RNS_VERSION=1.5.2
ARG LXMF_VERSION=0.9.6

# lxmd logs with print(); without this the lines sit in Python's block buffer until exit.
ENV PYTHONUNBUFFERED=1

# The pip_ca secret is only for builds behind a TLS-intercepting proxy; pip reads it as PIP_CERT.
RUN --mount=type=secret,id=pip_ca,required=false \
    sh -c 'if [ -f /run/secrets/pip_ca ]; then export PIP_CERT=/run/secrets/pip_ca; fi; \
           pip install --no-cache-dir "rns==${RNS_VERSION}" "lxmf==${LXMF_VERSION}"'

# uid 1000 matches Coyote's own image, so a bind mount chowned 1000:1000 serves both.
RUN useradd --uid 1000 --create-home --shell /usr/sbin/nologin lxmd \
    && mkdir -p /data /opt/coyote-pn && chown lxmd:lxmd /data

COPY --chmod=0755 entrypoint.sh /usr/local/bin/coyote-pn-entrypoint
COPY --chmod=0644 lxmd.config reticulum.config /opt/coyote-pn/

USER lxmd

# /data/lxmd: config, allowed, identity, storage/; /data/reticulum: config.
VOLUME ["/data"]
EXPOSE 4242/tcp

ENTRYPOINT ["/usr/local/bin/coyote-pn-entrypoint"]
