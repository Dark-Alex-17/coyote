# Mesh interop harness

The interop half of the mesh conformance suite (`src/mesh/conformance/interop.rs`). Each
test spawns the pinned Python Reticulum/LXMF reference as a subprocess, joins this crate's
node to it as a TCP client (the production `rnsd` relay topology) and proves the two agree
on the wire: announce filing, request replies, message delivery and store-and-forward
through a real LXMF propagation node. The vectors half, which needs nothing but Rust, lives
next to it in the `*_vectors.rs` modules of that directory (and, for the REPL's attachment
rules, in `src/repl/mesh_share_vectors.rs`).

## Pins

| Repository | Commit |
|---|---|
| https://github.com/markqvist/Reticulum | `ea98db4f53dcf0defc0e71a16e60d28b1229c4e6` |
| https://github.com/markqvist/LXMF | `727830cefda83d9c6e3982b48675425f3f988f9c` |

### Verification record

Last verified 2026-10-14, on macOS (the netns rows do not exist on this host; they and the
Linux leg are covered by the `Mesh Interop` CI job), with the Rust side at
[`coyote-rns-transport`](https://github.com/Dark-Alex-17/coyote-rns-transport) `0.14.0`
(Coyote's fork of `reticulum-rs-transport`), `lxmf-wire` `0.14.0` and `reticulum-rs-core`
`0.14.0`, and the Python side at RNS `1.5.2` / LXMF `0.9.6` (the two commits pinned above):

| Invocation | Result |
|---|---|
| `COYOTE_MESH_INTEROP=1 cargo test --all mesh::conformance -- --include-ignored` (vectors + interop) | 76 passed, 0 ignored |
| `cargo test --all mesh::fuzz` | 21 passed |
| `cargo test --all` | 6143 passed, 1 failed, 11 ignored in the unit binary; 276 passed across the integration binaries. The failure is `config::envoy_tools_tests::the_exec_probe_is_not_fooled_by_concurrent_process_spawns`, not a mesh test: this host kills the probe binary it spawns with SIGKILL, and the test fails the same way with the previous pin checked out, so the failure is the host's and not the bump's |

The Linux and Windows legs are covered by the CI matrix rather than by this record.
Re-run the three commands and refresh this table whenever either side's pin moves.

The reference is imported straight from the clones via `PYTHONPATH`, never from a `pip
install rns` or `lxmf`: a wheel would float with whatever PyPI serves that day, while the
protocol document cites these commits by line number. `setup.sh` refuses to continue when a
clone is not at its pin, and `Reference::spawn` re-checks the two `HEAD`s before every run.
Only `cryptography` and `pyserial` (the reference's own dependencies) are installed, into a
venv under the interop directory.

## Running locally

```sh
scripts/mesh-interop/setup.sh
source "${COYOTE_MESH_INTEROP_DIR:-$HOME/.cache/coyote/mesh-interop}/env.sh"
COYOTE_MESH_INTEROP=1 cargo test --all mesh::conformance -- --include-ignored
```

Independently of the interop setup, the wiki lint runs with
`COYOTE_WIKI_DIR=../coyote.wiki cargo test --test mesh_wiki_docs`.

`setup.sh` is idempotent: a second run verifies the pins and the imports and does nothing
else. The interop tests are `#[ignore]`d, so a plain `cargo test` never spawns Python; without
`COYOTE_MESH_INTEROP=1` they print `skipping: ...` and pass. With it set, every missing
prerequisite is a failure that names `setup.sh`. The script also byte-compiles both clones:
the tests run five references in parallel with 15 s waits, and a reference compiling
Reticulum's interfaces and LXMF's router on first import starts late enough to miss them
(`timed out waiting for the node to file the reference` on a cold clone is this).

| Variable | Meaning |
|---|---|
| `COYOTE_MESH_INTEROP` | Read by value: unset, empty or whitespace-only, `0`, and `false`/`no`/`off` (any case) leave the suite off; any other value (for example `1`) switches it on. Shared between the interop and netns suites. |
| `COYOTE_MESH_INTEROP_DIR` | Where the clones and venv live (default `~/.cache/coyote/mesh-interop`). |
| `COYOTE_MESH_INTEROP_PYTHON` | The interpreter to spawn (default `<dir>/venv/bin/python`, else `python3`). |
| `COYOTE_MESH_INTEROP_DEBUG` | Set to have the reference log at `RNS.LOG_DEBUG` on stderr, and to print this crate's captured `mesh` debug log to stderr when each reference shuts down. |
| `COYOTE_WIKI_DIR` | A checkout of the project wiki (relative paths resolve against the crate root). Switches on `tests/mesh_wiki_docs.rs`, which pins the `Mesh*`, `Hooks` and `Home` wiki pages and the README to the REPL verb table, hook events, `mesh__*` tools and staging-inbox layout in the source; unset or blank, those tests print `skipping: ...` and pass. |

## `reference_peer.py`

One Python process, driven over stdin/stdout with one JSON object per line. Commands are
`{"id": N, "cmd": "...", ...}` and are answered with `{"id": N, "ok": true, ...}` or
`{"id": N, "ok": false, "error": "..."}`; what the reference observes on its own arrives as
`{"event": "...", ...}`. Bytes are lowercase hex. On start it writes a Reticulum config
(transport enabled, one `TCPServerInterface` on a free loopback port, no shared instance),
creates a Coyote-shaped destination `scope.session.<instance_id>` serving `/status` and
`/message` to anyone, and prints `READY {json}` with `relay_port`, `identity_hash`,
`destination_hash`, `name_hash` and `instance_id`. The reference peer does not implement
`/knock`, `/list`, `/fetch` or `/access`; the file-sharing paths are covered by Rust-only
in-process conformance vectors in `src/mesh/conformance/share_vectors.rs`,
`access_vectors.rs` and `live_vectors.rs` (the last runs its node-pair rows over the
loopback pair of `src/mesh/request/tests.rs` and its symlink rows on unix only), so the
interop matrix exercises only `/status` and `/message`.

| Command | Arguments | Reply / effect |
|---|---|---|
| `announce` | `display_name: str \| null` | Announces the peer destination with `SCOPE`, version `1` and the name. |
| `watch` | `instance_id` | Registers an announce handler for `scope.session.<instance_id>`; each hit is an `announce` event with `destination_hash`, `identity_hash`, `app_data`, `derived_destination_hash` (the reference's own derivation from the announced identity) and `decoded` (`magic_ok`, `version`, `display_name`). |
| `wait_path` | `destination_hash`, `timeout_secs` | Requests a path once and waits until the transport has one and the identity is known; replies with `hops`. |
| `request` | `destination_hash`, `instance_id`, `path`, `envelope` or `raw_envelope`, `timeout_secs` | Opens a link to `scope.session.<instance_id>`, identifies as the peer identity, sends the request and replies with `status` (`ready`/`failed`), `response` (an integer for a refusal code, a map otherwise) and `response_type`. In `envelope`, a string `name_hash` is hex; keys are emitted in the given order. |
| `silence` | `path` | Deregisters that request handler on the peer destination, so a request to it goes unanswered. Fails when nothing is registered at `path`. |
| `pn_start` | `cost` | Starts an `LXMRouter` propagation node at that stamp cost and announces it at once; replies with `destination_hash`, `stamp_cost`, `stamp_cost_flexibility`. |
| `pn_announce` | | Announces the running propagation node again, so a peer that restarted after `pn_start` files it; replies with `destination_hash`. |
| `pn_count` | | `count` of messages the node holds. |
| `pn_messages` | | Every held message as `transient_id`, `destination_hash`, `stamp_value`; those addressed to the peer's `lxmf.delivery` are also decrypted and unpacked (`title`, `content`, `fields`, `source_hash`, `timestamp`). |
| `quit` | | Shuts Reticulum down and exits. |

Every request served on the peer destination is reported as a `request` event with `path`,
`remote_identity` and the decoded `envelope`.

## Platform

The harness is unix-only, and so is the `interop` module (`#[cfg(unix)]`). That says nothing
about the mesh: product code under `src/mesh/` is never cfg-gated, and Windows mesh behaviour
is covered by the unit tests and the platform-independent vectors. CI runs this suite in the
informational `Mesh Interop` job on Linux only.
