# Coyote-mesh propagation node

An [LXMF propagation node](https://github.com/markqvist/LXMF#propagation-nodes) is store-and-forward for the
mesh: when a Coyote instance cannot open a direct link to a peer, it posts the message or knock to a propagation
node, which holds it until the recipient fetches it. Coyote is never a propagation node itself; this directory
builds one from the reference daemon, [`lxmd`](https://github.com/markqvist/LXMF#daemon-included), configured
for a private team network.

Status in this build: Coyote discovers propagation nodes from their announces (`.mesh info` lists them under
`propagation_nodes`) and posts to the nearest one when a direct link fails (`mesh__send` reports
`via: store_and_forward`). Fetching held messages back is not yet triggered by any command or schedule, so a
message parked on the node is not picked up yet.

## Build and run

```sh
docker build -t coyote-pn deployment/propagation-node
docker run -d --name coyote-pn -p <lan-or-vpn-ip>:4242:4242 -v coyote-pn-data:/data coyote-pn
```

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
multi-node team, `static_peers = <hashes>` and `from_static_only = yes` are the knobs (`lxmd.py:204-220`; both
are in `lxmd --exampleconfig`).

`reticulum.config` also turns off Reticulum's announce ingress control (`ingress_control = No`), the
per-interface burst limiter for announces and path requests, so a new instance's first announce is not held back.
Keep port 4242 behind a firewall or VPN, and set it back to `Yes` on a host that is reachable from outside.

## Coyote side

Point each instance at the node:

```yaml
mesh:
  enabled: true
  interfaces:
    - type: private
      host: <node host>
      port: 4242
```

Once the node's announce is heard, `.mesh info` shows it under `propagation_nodes`. Selection is nearest by hops;
pinning a specific node is not supported yet, and `.mesh info` prints that line itself.

Deployment recipes and the trust model are in the wiki:
[Mesh Deployment](https://github.com/Dark-Alex-17/coyote/wiki/Mesh-Deployment) and
[Mesh Trust Model](https://github.com/Dark-Alex-17/coyote/wiki/Mesh-Trust-Model).

## Operating notes

- Logs go to stdout; read them with `docker logs coyote-pn`. The daemon is started without `-s`, which would
  send them to a file instead.
- `lxmd --status` is not usable from this image. A second `docker run` against the volume has its own network
  namespace and no Reticulum interface that joins it to the daemon (`reticulum.config` only listens), so the
  status query never finds a path to the node's control destination and exits 200 at the path wait
  (`lxmd.py:639-643`, timeout at `lxmd.py:631-635`). A `docker exec` form is no better: with
  `share_instance = No` it starts a second, isolated Reticulum instance. In both forms the second
  `RNS.Reticulum(configdir=/data/reticulum)` writes into the daemon's live `/data/reticulum/storage/` on exit.
  The log is the status surface.
- lxmd also registers a delivery destination for the daemon itself (the first hash in the smoke output).
  Anything sent to it is written under `/data/lxmd/storage/messages` with no count cap;
  `message_storage_limit` bounds the propagation store only. Size the volume for it, or set `on_inbound` to a
  discard script, if the node is reachable by strangers.
- Upgrade by rebuilding with `--build-arg LXMF_VERSION=<version>` (and `RNS_VERSION` if needed) and starting the
  new image against the same volume; identity, config and held messages carry over.
- `reticulum.config` sets `enable_transport = Yes`, so the container also relays announces and paths between
  the Coyote nodes that dial it. Two Coyote processes on one host cannot both use `type: lan` (the AutoInterface
  port `:42671` is bound exclusively), so a second instance on the same machine reaches the mesh through this
  relay with `type: private`. The interface options are documented in the
  [Reticulum manual](https://markqvist.github.io/Reticulum/manual/interfaces.html#tcp-server-interface).
