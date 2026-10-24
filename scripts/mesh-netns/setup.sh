#!/usr/bin/env bash
set -euo pipefail

# Creates the Linux network namespaces for the mesh reachability suite
# (src/mesh/conformance/netns.rs): a relay namespace holding a bridge, and two node
# namespaces each joined to that bridge over a veth pair whose bridge port is isolated,
# so either node reaches the relay's address and neither reaches the other directly.
#
#   coyote-a [10.77.0.2/24] --veth--+
#                                   |  coyote-br0 [10.77.0.1/24], isolated ports
#                                   +--  in coyote-relay
#   coyote-b [10.77.0.3/24] --veth--+
#
# Usage:
#   scripts/mesh-interop/setup.sh
#   sudo scripts/mesh-netns/setup.sh
#   COYOTE_MESH_INTEROP=1 cargo test --all mesh::conformance -- --include-ignored
#   sudo scripts/mesh-netns/teardown.sh
#
# The suite spawns the pinned Python relay that scripts/mesh-interop/setup.sh provides,
# and panics naming that script when the clones are missing.
#
# Env: none. The namespace names and the relay address are fixed; the suite pins them
# against the assignments below.
#
# Requirements: root (CAP_NET_ADMIN for the links, CAP_SYS_ADMIN for the namespace
# mounts), iproute2 (`ip` and `bridge`). Linux 4.18 or newer: isolated bridge ports are
# that old, and network namespaces are a Linux kernel feature.
#
# Idempotent: a namespace, bridge, veth pair or address that already exists is left as
# it is, and a second run prints no "creating" lines. Diagnostics go to stderr; stdout
# is the relay address, for humans and shell scripts.

RELAY_NS="coyote-relay"
NODE_A_NS="coyote-a"
NODE_B_NS="coyote-b"
RELAY_IP="10.77.0.1"
RELAY_PORT="4242"
NODE_A_IP="10.77.0.2"
NODE_B_IP="10.77.0.3"
PREFIX_LEN="24"
BRIDGE="coyote-br0"

log() {
  echo "[mesh-netns] $*" >&2
}

fail() {
  echo "[mesh-netns] ERROR: $*" >&2
  exit 1
}

if [ "$(id -u)" -ne 0 ]; then
  fail "creating network namespaces needs root (CAP_NET_ADMIN and CAP_SYS_ADMIN); run: sudo $0"
fi
command -v ip >/dev/null 2>&1 || fail "ip (iproute2) is not installed"
command -v bridge >/dev/null 2>&1 || fail "bridge (iproute2) is not installed"

ns_exists() {
  ip netns list | awk -v ns="$1" '$1 == ns { found = 1 } END { exit !found }'
}

link_exists() {
  ip -n "$1" link show "$2" >/dev/null 2>&1
}

# Deletes a link wherever an interrupted run may have left it: in the namespace it
# belongs to, the relay namespace, or the root namespace.
delete_link_anywhere() {
  local ns="$1" dev="$2"
  if link_exists "$ns" "$dev"; then
    ip -n "$ns" link del "$dev"
  fi
  if link_exists "$RELAY_NS" "$dev"; then
    ip -n "$RELAY_NS" link del "$dev"
  fi
  if ip link show "$dev" >/dev/null 2>&1; then
    ip link del "$dev"
  fi
}

ensure_ns() {
  if ! ns_exists "$1"; then
    log "creating namespace $1"
    ip netns add "$1"
  fi
  ip -n "$1" link set lo up
}

ensure_bridge() {
  if ! link_exists "$RELAY_NS" "$BRIDGE"; then
    log "creating bridge $BRIDGE in $RELAY_NS"
    ip -n "$RELAY_NS" link add "$BRIDGE" type bridge
  fi
  ip -n "$RELAY_NS" addr replace "$RELAY_IP/$PREFIX_LEN" dev "$BRIDGE"
  ip -n "$RELAY_NS" link set "$BRIDGE" up
}

# `<ns>-h` is the node's end, `<ns>-r` the relay's end enslaved to the bridge as an
# isolated port: the bridge forwards between an isolated port and the bridge itself,
# never between two isolated ports, which is what keeps the nodes apart. The pair counts
# as present only with both ends where they belong; deleting one end of a veth pair
# takes the other with it, so a half-present pair is a stray end to clear and recreate.
# Everything on the rerun path (`addr replace`, `link set up`, `set master`, `isolated on`)
# must stay a kernel no-op for a link already in that state: the idempotency test re-runs
# this script while the reachability test holds live connections in these namespaces.
ensure_node() {
  local ns="$1" ip4="$2" host_end="$1-h" relay_end="$1-r"
  if ! { link_exists "$ns" "$host_end" && link_exists "$RELAY_NS" "$relay_end"; }; then
    delete_link_anywhere "$ns" "$host_end"
    delete_link_anywhere "$ns" "$relay_end"
    log "creating veth pair $host_end/$relay_end"
    ip link add "$host_end" type veth peer name "$relay_end"
    ip link set "$host_end" netns "$ns"
    ip link set "$relay_end" netns "$RELAY_NS"
  fi
  ip -n "$RELAY_NS" link set "$relay_end" master "$BRIDGE"
  ip -n "$RELAY_NS" link set "$relay_end" up
  ip netns exec "$RELAY_NS" bridge link set dev "$relay_end" isolated on \
    || fail "bridge port isolation needs Linux 4.18 or newer and a matching iproute2"
  ip -n "$ns" addr replace "$ip4/$PREFIX_LEN" dev "$host_end"
  ip -n "$ns" link set "$host_end" up
}

# A `lan` node binds the link-local IPv6 address of its interface, and the kernel refuses
# that (EADDRNOTAVAIL) while duplicate address detection still has it tentative, about
# two seconds after the link comes up.
wait_for_dad() {
  local ns="$1" dev="$2" waited=0
  while [ -n "$(ip -n "$ns" -6 addr show dev "$dev" tentative)" ]; do
    if [ "$waited" -ge 100 ]; then
      fail "$dev in $ns still has a tentative address after 10 s"
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
}

ensure_ns "$RELAY_NS"
ensure_ns "$NODE_A_NS"
ensure_ns "$NODE_B_NS"
ensure_bridge
ensure_node "$NODE_A_NS" "$NODE_A_IP"
ensure_node "$NODE_B_NS" "$NODE_B_IP"
wait_for_dad "$NODE_A_NS" "$NODE_A_NS-h"
wait_for_dad "$NODE_B_NS" "$NODE_B_NS-h"

log "$RELAY_NS: $BRIDGE at $RELAY_IP/$PREFIX_LEN; $NODE_A_NS at $NODE_A_IP; $NODE_B_NS at $NODE_B_IP"
echo "RELAY_IP=$RELAY_IP"
echo "RELAY_PORT=$RELAY_PORT"
