#!/usr/bin/env bash
set -euo pipefail

# Removes the network namespaces scripts/mesh-netns/setup.sh created for the mesh
# reachability suite (src/mesh/conformance/netns.rs), stopping anything still running
# inside them first. Deleting a namespace destroys its veth ends and the bridge with it;
# an end an interrupted setup left in the root namespace is deleted here too, as is any
# `coyote-probe-*` namespace the suite's privilege probe creates and a killed test may
# have stranded.
#
# Usage:
#   sudo scripts/mesh-netns/teardown.sh
#
# Env: none.
#
# Requirements: root (CAP_NET_ADMIN and CAP_SYS_ADMIN), iproute2. Linux only.
#
# Idempotent: a namespace or link that is already gone is skipped, and a second run
# prints nothing but the closing summary.

RELAY_NS="coyote-relay"
NODE_A_NS="coyote-a"
NODE_B_NS="coyote-b"
BRIDGE="coyote-br0"

log() {
  echo "[mesh-netns] $*" >&2
}

fail() {
  echo "[mesh-netns] ERROR: $*" >&2
  exit 1
}

if [ "$(id -u)" -ne 0 ]; then
  fail "deleting network namespaces needs root (CAP_NET_ADMIN and CAP_SYS_ADMIN); run: sudo $0"
fi
command -v ip >/dev/null 2>&1 || fail "ip (iproute2) is not installed"

ns_exists() {
  ip netns list | awk -v ns="$1" '$1 == ns { found = 1 } END { exit !found }'
}

probe_namespaces() {
  ip netns list | awk '$1 ~ /^coyote-probe-/ { print $1 }'
}

# SIGTERM, up to five seconds for the processes to leave, then SIGKILL.
stop_processes_in() {
  local ns="$1" pids waited=0
  pids="$(ip netns pids "$ns" || true)"
  [ -n "$pids" ] || return 0
  log "stopping processes in $ns: $(echo "$pids" | tr '\n' ' ')"
  echo "$pids" | xargs -r kill -TERM 2>/dev/null || true
  while [ "$waited" -lt 50 ] && [ -n "$(ip netns pids "$ns" || true)" ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
  pids="$(ip netns pids "$ns" || true)"
  if [ -n "$pids" ]; then
    echo "$pids" | xargs -r kill -KILL 2>/dev/null || true
  fi
}

for ns in "$RELAY_NS" "$NODE_A_NS" "$NODE_B_NS"; do
  if ns_exists "$ns"; then
    stop_processes_in "$ns"
    log "deleting namespace $ns"
    ip netns del "$ns"
  fi
done

for ns in $(probe_namespaces); do
  log "deleting stray probe namespace $ns"
  ip netns del "$ns" 2>/dev/null || log "probe namespace $ns already gone"
done

for dev in "$NODE_A_NS-h" "$NODE_A_NS-r" "$NODE_B_NS-h" "$NODE_B_NS-r" "$BRIDGE"; do
  if ip link show "$dev" >/dev/null 2>&1; then
    log "deleting stray link $dev"
    ip link del "$dev"
  fi
done

log "no mesh namespaces remain"
