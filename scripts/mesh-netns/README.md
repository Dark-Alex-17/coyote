# Mesh network-namespace harness

The reachability half of the mesh conformance suite (`src/mesh/conformance/netns.rs`): two
of this crate's nodes, each in its own Linux network namespace, reaching each other through
an external Reticulum relay. It is the two-host production topology on one machine, with the
kernel guaranteeing that the only path between the nodes is the relay. The interop half,
which proves the wire format against the Python reference, lives next to it in
`scripts/mesh-interop/`.

## Topology

`setup.sh` creates three namespaces. `coyote-relay` holds a bridge; `coyote-a` and `coyote-b`
each hang off it over a veth pair whose bridge port is isolated. An isolated port forwards to
the bridge itself and never to another isolated port, so both nodes reach `10.77.0.1` and a
TCP connection from `10.77.0.2` to `10.77.0.3` fails (the ARP request is never forwarded, so
the caller's 2 s timeout usually fires before the kernel gives up soliciting; occasionally
the neighbour lookup fails first and it reports no route to host). The suite asserts that
connection fails with a timeout or no route, never `ConnectionRefused`, before it asserts
anything about the relay: each node child holds a throwaway listener on `:4242` that nothing
accepts on, so the connect would succeed if the ports were not isolated.

```
coyote-a  [10.77.0.2/24] --coyote-a-h ... coyote-a-r--+
                                                      |  coyote-br0 [10.77.0.1/24]
                                                      +--  rnsd listens on :4242
coyote-b  [10.77.0.3/24] --coyote-b-h ... coyote-b-r--+
```

The test process stays in the root namespace and spawns the pinned Python `rnsd`
(`scripts/mesh-interop/setup.sh` provides it) inside `coyote-relay`, and this very test
binary twice, once inside each node namespace, driven over JSON lines on stdin/stdout. Each
child is entered with `ip netns exec` and dropped back to the invoking user with `setpriv`
before the program starts. The suite asserts that each node files the other's announce, that
a `/status` request before trust is refused with `NoAccess`, and that after mutual trust each
node reads the other's card, every byte of it relayed by `rnsd`. A second test starts two
`lan` nodes in one namespace and asserts the second fails to bind with the message the
product prints, without touching the host's own discovery port.

## Running locally

```sh
scripts/mesh-interop/setup.sh
sudo scripts/mesh-netns/setup.sh
COYOTE_MESH_INTEROP=1 cargo test --all mesh::conformance -- --include-ignored
sudo scripts/mesh-netns/teardown.sh
```

`scripts/mesh-interop/setup.sh` provides the pinned Python relay; without the clones the
suite panics naming that script.
`setup.sh` is idempotent: a second run finds everything in place and creates nothing.
`teardown.sh` stops whatever still runs inside the namespaces, deletes them (which takes the
veth pairs and the bridge with them) and is idempotent too. A test killed before its own
teardown leaves `rnsd` running as the invoking user inside `coyote-relay` until `teardown.sh`
stops it. The tests are `#[ignore]`d, so a
plain `cargo test` never touches a namespace; without `COYOTE_MESH_INTEROP=1` they print
`skipping: ...` and pass. With it set, the tests first probe whether this host can create a
namespace at all (`sudo -n ip netns add`); when it cannot they skip with a line naming
root (`CAP_NET_ADMIN` and `CAP_SYS_ADMIN`) and `setup.sh`. Once it can, a missing namespace
or a missing reference clone is a failure that names the script to run, so CI cannot pass by
skipping.

| Variable | Meaning |
|---|---|
| `COYOTE_MESH_INTEROP` | Read by value: unset, empty or whitespace-only, `0`, and `false`/`no`/`off` (any case) leave the suite off; any other value (for example `1`) switches it on. Shared between the interop and netns suites. |
| `COYOTE_MESH_INTEROP_DIR`, `COYOTE_MESH_INTEROP_PYTHON` | Where the pinned reference lives and which interpreter runs `rnsd`; as for the interop suite. A bare interpreter name is resolved against the test process's own `PATH` before the relay is launched, because `sudo -n` replaces `PATH` with its `secure_path`. |
| `COYOTE_MESH_INTEROP_DEBUG` | Set to print this crate's captured `mesh` debug log and the relay's log to stderr when the harness shuts down. |

## Privileges

Creating a network namespace needs `CAP_SYS_ADMIN` (the `/var/run/netns` mount) and wiring
it needs `CAP_NET_ADMIN`; in practice, root. The scripts refuse to run otherwise and say to
use `sudo`. The primary mode, and CI's, is an unprivileged test process with passwordless
`sudo`: the tests call `sudo -n ip netns exec`, and every child drops back to the caller's
uid and gid with `setpriv` before the relay or the node starts, so no relay or node of ours
runs as root. A test process that is itself root (a root-only container) needs no `sudo`:
it enters the namespaces directly and runs the relay and the nodes as root. The idempotency
test re-runs `setup.sh` the same way. CI's Ubuntu runners provide passwordless `sudo`; the
`Mesh Interop` job runs `setup.sh` before the suite and `teardown.sh` after it, whatever
the outcome.

## Platform

Linux 4.18 or newer (isolated bridge ports arrived then). The `netns` module is Linux-only
too (`#[cfg(target_os = "linux")]`, narrower than the interop module's `#[cfg(unix)]`)
because network namespaces are a Linux kernel feature.
As with the interop harness, that says nothing about the mesh: product code under
`src/mesh/` is never cfg-gated, and the other platforms are covered by the unit tests and the
platform-independent vectors.
