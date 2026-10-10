# Coyote-mesh propagation node

An [LXMF propagation node](https://github.com/markqvist/LXMF#propagation-nodes) is store-and-forward for the
mesh: when a Coyote instance cannot open a direct link to a peer, it posts the message to a propagation
node, which holds it until the recipient fetches it. Coyote is never a propagation node itself; this directory
builds one from the reference daemon, [`lxmd`](https://github.com/markqvist/LXMF#daemon-included), configured
for a private team network.

Status in this build: Coyote discovers propagation nodes from their announces (`.mesh info` lists them under
`propagation_nodes`) and posts to the nearest one when a direct link fails (`mesh__send` reports
`via: store_and_forward`). Held messages are fetched back automatically every `mesh.propagation_sync_interval_secs`
seconds (default 300; `0` turns the automatic path off, including the join-time fetch) and once a propagation node is
heard after the node joins; `.mesh sync` runs a fetch now. A `.mesh knock` to an unreachable peer is parked on the
node the same way. Coyote's table of propagation nodes is not kept across a Coyote restart, so after a restart the
first fetch waits for the node's next announce (up to `announce_interval`, 30 minutes with the shipped `lxmd.config`;
`.mesh sync` is refused until then too); a held message whose sender has not announced since the restart is kept on
the node until it has been seen on three fetches and at least 15 minutes (one peer heartbeat) have passed; then it is
dropped. A Coyote node with `announce: false` never fetches on its own; `.mesh sync` does.

## Build and run

```sh
docker build -t coyote-pn deployment/propagation-node
docker run -d --name coyote-pn -p <lan-or-vpn-ip>:4242:4242 -v coyote-pn-data:/data coyote-pn
```

The build needs BuildKit, the default since Docker 23; older daemons need `DOCKER_BUILDKIT=1`.

Publish the port on the LAN or VPN address the team's Coyote nodes dial, not on every address: the node relays for
anyone who can reach 4242 (see "Private network").

Behind a proxy that intercepts TLS, pass the CA bundle pip should trust as a build secret; the Dockerfile exports
it as `PIP_CERT`:

```sh
docker build --secret id=pip_ca,src=/etc/ssl/certs/ca-certificates.crt -t coyote-pn deployment/propagation-node
```

The image runs as uid 1000. A bind mount instead of a named volume has to be owned by that uid, and mode 700
keeps the identity files it will hold from other users on the host:

```sh
install -d -m 700 -o 1000 -g 1000 /srv/coyote-pn
docker run -d --name coyote-pn -p <lan-or-vpn-ip>:4242:4242 -v /srv/coyote-pn:/data coyote-pn
```

Smoke:

```sh
$ docker run --rm coyote-pn --version
lxmd 0.9.6
$ docker logs coyote-pn
[...] [Notice]   LXMF Router ready to receive on <3a901b6ff64c40c924107964b6fd60d5>
[...] [Notice]   LXMF Propagation Node started on <5650cc666e01f4c871281f567321f5d2>
```

The two hashes are the daemon's delivery destination and the propagation node destination; yours will differ.
The second one is what Coyote lists under `propagation_nodes`.

A first argument of `lxmd` is dropped and the rest is appended to the daemon's command line, so
`docker run --rm coyote-pn lxmd --version`, `lxmd --exampleconfig` and a bare `lxmd` all behave exactly like
the same command without the `lxmd`: every path seeds the volume first and then runs lxmd against
`/data/lxmd` and `/data/reticulum`, never against `~/.lxmd`. `rnsd`, `sh` and `bash` run that program instead
of the daemon. The other RNS tools (`rnstatus`, `rnpath`, `rnprobe`, `rnid`) are not passed through: with
`share_instance = No` they would start an isolated second Reticulum instance and report nothing about the
daemon.

## What is on the volume

| Path                       | Purpose                                                                            |
|----------------------------|------------------------------------------------------------------------------------|
| `/data/lxmd/config`        | The daemon config; seeded from `lxmd.config` on first start, never overwritten     |
| `/data/lxmd/allowed`       | Identity hashes allowed to fetch; created empty on first start                     |
| `/data/lxmd/identity`      | The node's Reticulum identity; minted on first start, reused on every restart      |
| `/data/lxmd/storage/`      | Held messages and peer state                                                       |
| `/data/reticulum/config`   | The Reticulum config; seeded from `reticulum.config` on first start                |
| `/data/reticulum/storage/` | Transport identity (minted for `enable_transport = Yes`), destinations, path cache |

Because the identity lives on the volume the node keeps its destination hashes across restarts and upgrades. A
fresh volume mints a fresh identity, and Coyote peers then see a different node. Copying or moving `/data` (named
volume or bind mount) to another host makes that host the same node with the same destination hashes. Copy the
whole directory: `/data/lxmd/identity` alone moves the propagation-node destination but not the relay's
transport identity under `/data/reticulum/storage/`. Two hosts running from copies of the same `/data` collide as
one identity, so copy to move, never to duplicate.

## Private network

The shipped `lxmd.config` sets `enable_node = yes` and `auth_required = yes`, so only identities listed in
`/data/lxmd/allowed` can fetch from the node (LXMF 0.9.6, `LXMF/Utilities/lxmd.py:103-116`).

The file format, as the loader reads it (`lxmd.py:248-268`): one Reticulum identity hash per line, 32 hex
characters and nothing else on the line. LF or CRLF line endings both work; the loader splits on either and then
keeps only lines whose remaining bytes are exactly 32, so a trailing space, inline whitespace, a comment or a
blank-padded line is silently dropped. The file is read once at start, so restart the container after editing it.

The hash to list is a Coyote node's mesh **identity** hash: the `identity:` line printed by `.mesh on` and the
`identity` row of `.mesh info`. It is not the destination hash. Example, with two placeholders:

```
00000000000000000000000000000000
ffffffffffffffffffffffffffffffff
```

This fails closed. With `auth_required = yes` and an empty or missing `allowed`, lxmd starts and logs

```
[Warning]  Clint authentication was enabled, but no identity hashes could be loaded from /data/lxmd/allowed. Nobody will be able to sync messages from this propagation node.
```

(the typo is upstream's), and `LXMRouter.identity_allowed` returns false for everyone, so every fetch is answered
`ERROR_NO_ACCESS` (`LXMF/LXMRouter.py:1417-1429`).

`auth_required` gates fetching only. Posting into the node is open to anyone who can reach port 4242 and is
governed by the node's stamp cost (`propagation_stamp_cost_target`, upstream default 16; Coyote accepts nodes
with a cost up to 26). Network reachability, a firewall or a VPN, is the boundary for who can post.

Peering is off: `lxmd.config` sets `autopeer = no`, where lxmd's default would peer with any propagation node
whose announce it hears within four hops and sync it a copy of every held message (`auth_required` does not gate
peer sync). So `allowed` plus network reachability are the only ways messages leave the node. For a deliberate
multi-node team, `static_peers = <hashes>` is the knob (`lxmd.py:204-210`; it is in `lxmd --exampleconfig`): each
node lists the others' propagation-node destination hashes and they sync held messages between themselves. Do not
set `from_static_only = yes` (`lxmd.py:217-220`) on a node Coyote should post to. It clears the propagation-node
flag in the node's announce (`LXMRouter.py:309`, `node_state = self.propagation_node and not
self.from_static_only`), and Coyote only posts to nodes that announce that flag, so `mesh__send` would answer
`no_propagation_node` even though the node is up and listed. With it off, the same flag is also the only gate on
inbound peer sync (`LXMRouter.py:2089`, `:2157`): a rogue propagation node that reaches port 4242 can sync messages
into this node's store, so the network boundary covers that direction too; egress stays closed by `autopeer = no`.

`reticulum.config` also turns off Reticulum's announce ingress control (`ingress_control = No`), the
per-interface burst limiter for announces and path requests, so a new instance's first announce is not held back.
Keep port 4242 behind a firewall or VPN, and set it back to `Yes` on a host that is reachable from outside.

## Coyote side

Each host runs the local daemon setup once, with this node as its relay. From a checkout:

```sh
scripts/mesh-relay.sh --relay <node host>:4242
```

```powershell
pwsh -File scripts\mesh-relay.ps1 -Relay <node host>:4242
```

Without a checkout:

```sh
curl -fsSL https://raw.githubusercontent.com/Dark-Alex-17/coyote/refs/heads/main/scripts/mesh-relay.sh | bash -s -- --relay <node host>:4242
```

```powershell
iwr -useb https://raw.githubusercontent.com/Dark-Alex-17/coyote/refs/heads/main/scripts/mesh-relay.ps1 -OutFile mesh-relay.ps1
pwsh -NoProfile -ExecutionPolicy Bypass -File .\mesh-relay.ps1 -Relay <node host>:4242
```

On a host with no `~/.reticulum/config` yet the script writes the `[[Team Relay]]` stanza into it. A host that already
ran the setup keeps its config: the script prints the stanza to add by hand, and the daemon then has to be restarted
(`systemctl --user restart coyote-rnsd`, the `launchctl bootout`/`bootstrap` pair, or
`Stop-ScheduledTask`/`Start-ScheduledTask 'Coyote rnsd'`). Coyote keeps the shipped default `mesh.interfaces`, which
dials that daemon; there is nothing to change:

```yaml
mesh:
  enabled: true
  interfaces:
    - type: private
      host: 127.0.0.1
      port: 4242
```

Only a session running without `rnsd` dials the node directly, with `host: <node host>` in place of `127.0.0.1`; the
Docker image (from v0.10.4) has its own `rnsd` and reaches the node through `COYOTE_MESH_RELAY=<node host>:4242` instead.

Once the node's announce is heard, `.mesh info` shows it under `propagation_nodes`. Selection is nearest by hops;
pinning a specific node is not supported yet, and `.mesh info` prints that line itself.

Deployment recipes and the trust model are in the wiki:
[Mesh Deployment](https://github.com/Dark-Alex-17/coyote/wiki/Mesh-Deployment) and
[Mesh Trust Model](https://github.com/Dark-Alex-17/coyote/wiki/Mesh-Trust-Model).

## Operating notes

- Logs go to stdout; read them with `docker logs coyote-pn`. The daemon is started without `-s`, which would
  send them to a file instead.
- `lxmd --status` is not usable from this image, in either form. A second `docker run ... lxmd --status` against
  the volume has its own network namespace and no Reticulum interface that joins it to the daemon
  (`reticulum.config` only listens), so the status query never finds a path to the node's control destination
  and exits 200 at the path wait with `Getting lxmd statistics timed out` (`lxmd.py:639-643`, timeout at
  `lxmd.py:631-635`); on the way out its second `RNS.Reticulum(configdir=/data/reticulum)` rewrites
  `destination_table`, `known_destinations`, `packet_hashlist.raw` and `tunnels` under the daemon's live
  `/data/reticulum/storage/`. `docker exec coyote-pn lxmd --config /data/lxmd --rnsconfig /data/reticulum --status`
  shares the daemon's namespace instead, and with `share_instance = No` its second Reticulum instance tries to
  open its own `Coyote Peers` listener on 4242, which the daemon holds: it dies with
  `[Errno 98] Address already in use`, exit 255, before Reticulum is up, and writes nothing. The daemon keeps
  running through both. The log is the status surface.
- lxmd also registers a delivery destination for the daemon itself (the first hash in the smoke output).
  Anything sent to it is written under `/data/lxmd/storage/messages` with no count cap;
  `message_storage_limit` bounds the propagation store only. Size the volume for it, or set `on_inbound` to a
  discard script, if the node is reachable by strangers.
- Upgrade by rebuilding with `--build-arg LXMF_VERSION=<version>` (and `RNS_VERSION` if needed) and starting the
  new image against the same volume; identity, config and held messages carry over.
- `reticulum.config` sets `enable_transport = Yes`, so the container also relays announces and paths between
  the team's Coyote nodes that dial it; `enable_node = yes` is what makes it store and forward for them. Sessions
  on one host reach each other through the host's local `rnsd` (`scripts/mesh-relay.sh --relay <node host>:4242`),
  which dials this container as its `[[Team Relay]]`; this container is not that path. The interface options are
  documented in the
  [Reticulum manual](https://markqvist.github.io/Reticulum/manual/interfaces.html#tcp-server-interface).
