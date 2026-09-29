#!/bin/sh
set -eu

case "${1:-}" in
    lxmd|rnsd|rnstatus|rnpath|rnprobe|rnid|sh|bash)
        exec "$@"
        ;;
esac

mkdir -p /data/lxmd /data/reticulum

# Seed the configs on first start only; an operator's edits are never overwritten.
if [ ! -f /data/lxmd/config ]; then
    cp /opt/coyote-pn/lxmd.config /data/lxmd/config
fi
if [ ! -f /data/reticulum/config ]; then
    cp /opt/coyote-pn/reticulum.config /data/reticulum/config
fi
if [ ! -f /data/lxmd/allowed ]; then
    : > /data/lxmd/allowed
fi

# --config bypasses lxmd's default search (/etc/lxmd, ~/.config/lxmd, ~/.lxmd; lxmd.py:307-313)
# so every piece of state lands on the volume. The node identity is minted at
# /data/lxmd/identity on first start (lxmd.py:366-371) and reused after.
exec lxmd --config /data/lxmd --rnsconfig /data/reticulum "$@"
