# Upstream issue drafts

Issues the mesh work has found in the code it depends on, written so they can be filed as they
stand. Filing needs the maintainer's GitHub identity and is tracked as a follow-up; until then
every entry here has the status "Drafted, not yet filed". Part A holds the findings the
leniency register of `docs/mesh/PROTOCOL.md` (section 18) points at; every entry there is cited
by at least one leniency row. Part B holds drafts inherited from an earlier audit of Reticulum
implementations; none of them corresponds to a leniency in this crate and they are kept here so
the findings are not lost.

Each entry: affected repository and revision, root cause, reproduction, suggested fix, and
what this crate does about it in the meantime.

## Part A: found while integrating the mesh

### A1. rns-transport: an advertisement-time reject deadlocks the transport

- Status: Drafted, not yet filed.
- Repository / revision: FreeTAKTeam/LXMF-rs, crate `reticulum-rs-transport`, rev
  `3ed5932da4420e2dd1b9d36283b0e72a364e3ebe` (where found); re-checked against the 0.12.0
  release this crate depends on: still present (`resource_wire.rs`, both advertisement branches).
- Severity: high. A destination with `set_max_request_size` set, or a link with a response size
  limit set, stops processing every link event the first time an advertisement over the cap
  arrives. Nothing recovers it short of dropping the transport.
- Referenced by: `MESH-LEN-001` in `docs/mesh/PROTOCOL.md`.

#### Root cause

The resource wire handler rejects an advertisement whose declared size is over the cap by
sending the reject through the link's handler channel while it still holds the link mutex
(`resource_wire.rs`, the advertisement branch). The handler on the other end of that channel
re-locks the same link to record the sent packet (`note_link_packet_sent`). With the mutex
held across the `await`, the send never completes and the lock is never released, so every
later event for that link, and the transport loop that dispatches them, waits on it for good.

#### Reproduction

1. Start two transports on a loopback `TcpServer` / `TcpClient` pair and register a
   destination on A with `set_max_request_size(n)`.
2. From B, open a link to A and send a request resource whose packed length exceeds `n`.
3. A's transport stops delivering link events. A later `stop()` or `deregister_destination`
   on A never returns without an external timeout.

This crate reproduces the wedge on purpose in `src/mesh/r3/tests.rs` (`wedge_node_a`, driven by
`mesh_runtime_stop_is_bounded_when_the_transport_is_wedged` and
`rekey_gives_up_within_grace_when_the_transport_is_wedged`) to prove its own waits stay bounded
when the transport is gone.

#### Suggested fix

Release the link mutex before awaiting the handler send, or queue the reject and send it after
the guard drops. A regression test: arm the cap, send one oversize advertisement, then send a
well-formed request on the same link and assert it is served.

#### What this crate does meanwhile

Production code sets no advertisement-time cap and no response size limit. Inbound requests and
responses are bounded on this side after assembly instead, at `MAX_R3_PAYLOAD_BYTES`
(`R3Server::dispatch`, `R3Client::deliver`), with the upstream 64 MiB advertisement cap as the
only bound before that. The cap setter is reachable from tests only (`arm_request_cap_for_test`).
Removal condition: when the pinned transport sends the reject outside the lock, re-arm the cap
and keep the post-assembly bound as the second line.

### A2. rns-transport: a packet proof on an active link surfaces no event

- Status: Drafted, not yet filed.
- Repository / revision: FreeTAKTeam/LXMF-rs, crate `reticulum-rs-transport`, rev
  `3ed5932da4420e2dd1b9d36283b0e72a364e3ebe` (where found); re-checked against the 0.12.0
  release: still present; `handle_proof_packet.rs` is unchanged between the two — the
  active-link branch resolves channel-message delivery state only and returns
  `LinkHandleResult::None` with no event.
- Severity: medium. A client holding a link cannot learn that a link packet it sent was proven
  by the far end: no `LinkEvent` reports it and no per-link receipt resolves. The only surface
  is the transport-global `ReceiptHandler` (`Transport::set_receipt_handler`), keyed by packet
  hash, which a per-link client cannot scope.
- Referenced by: `MESH-LEN-002` in `docs/mesh/PROTOCOL.md`.

#### Root cause

The proof handler for an active link (`link_sections/handle_proof_packet.rs`) validates the
proof against the link and returns without producing a `LinkEvent` or resolving any
per-link-packet receipt (it resolves channel-message state only). The Python reference calls
the packet's delivery callback at this point (`RNS/Link.py`, `Packet.prove` / `PacketReceipt`).
The Rust transport's counterpart is the single `ReceiptHandler` installed on the whole
transport (`transport/wire.rs::handle_proof` → `validated_receipt_hash` → `on_receipt`), which
reports the proven packet hash and nothing else: not the link, not the request it belonged to.

#### Reproduction

1. Open a link from A to B and register a link event stream on A.
2. Send a link packet from A that B answers with a proof (an LXMF propagation node does this
   for every accepted message packet; `Link.request` does it for a plain packet).
3. Observe that A's event stream carries nothing for the proof, while a resource sent on the
   same link reports its completion.

#### Suggested fix

Emit a link event (for example `PacketProven { link_id, packet_hash }`) from the proof handler,
or resolve a receipt handed back by the packet send. Either lets a client distinguish "proven"
from "no answer".

#### What this crate does meanwhile

When posting to a propagation node, acceptance is inferred: a packet that draws no rejection
signal inside `PROPAGATION_REJECT_WINDOW` after the transfer completes counts as accepted
(`MESH-PROP-016`; `a_completed_transfer_is_accepted_one_window_later_not_at_the_deadline`,
`src/mesh/propagation.rs`). A resource is at least confirmed received by its completion event.
The transport-global receipt handler is not used for this today: a handler keyed by packet hash
cannot be scoped to one link or one request without tracking the hash of every packet sent
through the transport, so reading acceptance from it is a propagation follow-up rather than a
drop-in.
Removal condition: when the transport surfaces the proof, read acceptance from it and stop
inferring it from silence.

### A3. rns-transport: `AddressHash::new_from_hex_string` panics on a 32-byte string with a multibyte character

- Status: Drafted, not yet filed.
- Repository / revision: FreeTAKTeam/LXMF-rs, crate `reticulum-rs-transport`, rev
  `3ed5932da4420e2dd1b9d36283b0e72a364e3ebe` (where found); re-checked against the 0.12.0
  release: still present, `hash.rs` is unchanged between the two.
- Severity: low for this crate, medium for a caller that hands user text to the parser: a
  panic where an `Err` is documented.
- Referenced by: `MESH-LEN-006` in `docs/mesh/PROTOCOL.md`.

#### Root cause

`hash.rs:107` checks `hex_string.len() != ADDRESS_HASH_SIZE * 2`, a byte length, and
`hash.rs:114` then slices `&hex_string[i * 2..(i * 2) + 2]` by byte index. A 32-byte string
that contains a multibyte UTF-8 character passes the length check and the slice lands on a
char boundary inside that character, which panics before `from_str_radix` can refuse it.

#### Reproduction

Call `AddressHash::new_from_hex_string` on one 2-byte UTF-8 character followed by 30 ASCII
`a` (32 bytes in all). Expected: `Err(RnsError::IncorrectHash)`. Observed: a panic at the
byte slice.

#### Suggested fix

Check `hex_string.chars().all(|c| c.is_ascii_hexdigit())` (and so the character count)
before slicing, or guard each slice with `is_char_boundary`. A regression test: the string
above returns an error.

#### What this crate does meanwhile

Every identity or destination hash given as text passes through `canonical_hash` first
(`MESH-CANON-002`; `malformed_hashes_are_refused_without_panicking`, `src/mesh/trust.rs`),
which refuses anything that is not 32 ASCII hex digits, so the panicking path is unreachable
from this crate. Removal condition: when the pinned parser validates its input, the guard
becomes defence in depth and the leniency row is retired.

### A4. rns-transport: follow-up Resource segments are dispatched through the path table and dropped on a non-broadcast link

- Status: Drafted, not yet filed.
- Repository / revision: FreeTAKTeam/LXMF-rs, crate `reticulum-rs-transport`, release 0.12.0
  (crates.io), where found; `transport/resource_wire.rs` (`handle_resource_proof`) and the
  `resource-retry` worker in `transport/jobs.rs`.
- Severity: medium. A Resource longer than one segment (`MAX_EFFICIENT_SIZE`, one byte under
  1 MiB) never completes between two nodes that keep `broadcast: false`: the first segment
  transfers, the second segment's advertisement is dropped as unroutable, and the requester
  waits out its deadline. The same transfer completes with `broadcast: true`.
- Referenced by: `MESH-LEN-007` in `docs/mesh/PROTOCOL.md`.

#### Root cause

The first segment's advertisement leaves on the link's bound interface
(`send_link_packet_on_bound_iface`, the `send_resource_*` family in
`transport/links_parts/transport_sections/reset_out_link.rs`). When the proof for a completed
segment arrives, `handle_resource_proof` (`transport/resource_wire.rs`) lets the resource
manager build the next segment's advertisement and hands it to `handler.send_packet`; the
`resource-retry` worker in `transport/jobs.rs` re-sends pending advertisements
(`poll_outgoing`) the same way. `send_packet` routes through the path table
(`route_outbound_packet`, `transport/handler.rs`), and a link id is never a path-table key, so
the lookup finds no next interface; only an announce is broadcast without a route, and a node
with `broadcast: false` records `DroppedNoRoute` and sends nothing. The requester, which
received segment one, waits for an advertisement that never leaves the responder.

#### Reproduction

1. Start two transports on a loopback `TcpServer` / `TcpClient` pair with `broadcast: false`
   on both, open a link from B to A and register a request handler on A that answers with a
   Resource of the requested size.
2. Request a 300 KiB body. It travels as one segment and completes.
3. Request a 1 MiB + 64 KiB body. A's side reports `OutboundFailed` after its retries, each
   of them `DroppedNoRoute`; B's request times out.
4. Repeat step 3 with `broadcast: true` on A. The two segments arrive and the request
   completes.

#### Suggested fix

Dispatch the advertisement of every segment after the first, and each retry of it, on the
link's bound interface as the first segment's is, instead of through `send_packet`. A
regression test: a two-segment Resource over a `broadcast: false` pair completes.

#### What this crate does meanwhile

The responder caps its serving limit at `SINGLE_SEGMENT_FETCH_CEILING` = `MAX_EFFICIENT_SIZE`
less `OK_REPLY_FRAMING_BYTES` (1 048 575 − 128 = 1 048 447 bytes; `serving_limit`,
`src/mesh/fetch.rs`), so every `ok` reply fits one segment, and answers `too_large` with that
limit for a larger file (`MESH-FETCH-027`, `MESH-LEN-007`;
`a_file_above_the_single_segment_ceiling_is_too_large_with_that_limit`,
`an_ok_reply_at_the_ceiling_fits_one_resource_segment`, `src/mesh/fetch.rs`). The ceiling only
ever lowers `mesh.fetch.max_bytes`; a compile-time assertion keeps it under
`MAX_FETCH_FILE_BYTES`. Removal condition: the upstream release that sends follow-up
advertisements the way it sends the first; then the ceiling, its test and the README/config
clauses go.

## Part B: inherited drafts against Reticulum implementations

These were validated against RNS 1.3.1 and LXMF 0.9.9. Line numbers refer to those releases;
cross-check before filing if upstream HEAD has moved.

### B1. Reticulum: `_synthesize_interface` raises `KeyError('mode')` for `interface_mode = gateway`

- Status: Drafted, not yet filed.
- Repository / version: markqvist/Reticulum, confirmed in 1.3.1 (`RNS/Reticulum.py`); also
  present in 1.1.3.
- Severity: medium. Any configuration that sets `interface_mode = gateway` (or `gw`) on an
  interface without also setting the legacy `mode` key crashes Reticulum startup with an
  uncaught `KeyError`.
- Referenced by: none (no Coyote leniency).

#### Root cause

`RNS/Reticulum.py`, `_synthesize_interface`, the `interface_mode` branch (lines 689-702, the
gateway test at line 701):

```python
if "interface_mode" in c:
    c["interface_mode"] = str(c["interface_mode"]).lower()
    if c["interface_mode"] == "full":
        interface_mode = Interface.Interface.MODE_FULL
    elif c["interface_mode"] == "access_point" or c["interface_mode"] == "accesspoint" or c["interface_mode"] == "ap":
        interface_mode = Interface.Interface.MODE_ACCESS_POINT
    elif c["interface_mode"] == "pointtopoint" or c["interface_mode"] == "ptp":
        interface_mode = Interface.Interface.MODE_POINT_TO_POINT
    elif c["interface_mode"] == "roaming":
        interface_mode = Interface.Interface.MODE_ROAMING
    elif c["interface_mode"] == "boundary":
        interface_mode = Interface.Interface.MODE_BOUNDARY
    elif c["mode"] == "gateway" or c["mode"] == "gw":     # reads c["mode"]
        interface_mode = Interface.Interface.MODE_GATEWAY
```

Inside the `if "interface_mode" in c:` block, the final `elif` for the gateway mode reads
`c["mode"]` instead of `c["interface_mode"]`. When a configuration specifies only
`interface_mode` (the documented, current key) and the value is `gateway` or `gw`, none of the
preceding comparisons match, control reaches line 701, and `c["mode"]` raises `KeyError`
because that key is absent. The parallel `elif "mode" in c:` branch (lines 704-717) is
internally consistent and tests `c["mode"] == "gateway"` at line 716, so the defect is
isolated to the `interface_mode` branch.

#### Reproduction

```bash
python3 - <<'PY'
c = {"interface_mode": "gateway"}
c["interface_mode"] = str(c["interface_mode"]).lower()
if c["interface_mode"] == "full":          pass
elif c["interface_mode"] in ("access_point","accesspoint","ap"): pass
elif c["interface_mode"] in ("pointtopoint","ptp"): pass
elif c["interface_mode"] == "roaming":     pass
elif c["interface_mode"] == "boundary":    pass
elif c["mode"] == "gateway" or c["mode"] == "gw":  # KeyError('mode')
    pass
PY
```

End to end this is a crash on `RNS.Reticulum()` startup with a configuration containing:

```ini
[[Some Interface]]
  type = TCPServerInterface
  listen_ip = 0.0.0.0
  listen_port = 4242
  interface_mode = gateway
```

#### Suggested fix

Change line 701 to test the correct key:

```python
elif c["interface_mode"] == "gateway" or c["interface_mode"] == "gw":
    interface_mode = Interface.Interface.MODE_GATEWAY
```

A workaround until then: write `mode = <name>` instead of `interface_mode = <name>`, which
routes through the consistent fallback branch and assigns the same `MODE_*` constant.

### B2. IFAC `ifac_key` derivation in third-party ports drops the final `full_hash` over the netname/netkey origin

- Status: Drafted, not yet filed against reticulum-swift / microReticulum. Originally surfaced
  as reticulum-kt issue #29.
- Repositories: reticulum-kt (Kotlin), reticulum-swift (Swift) and microReticulum (C++), which
  historically copied the same derivation. The correct behaviour is RNS 1.3.1
  (`RNS/Reticulum.py`).
- Severity: high for interop. A wrong `ifac_key` produces wire bytes that a correct RNS peer's
  IFAC unmasker silently rejects (the packet is dropped, no error surfaced), so an
  IFAC-protected link between an affected port and stock RNS never passes traffic and gives no
  diagnostic.
- Referenced by: none (no Coyote leniency).

#### Root cause

The reference chain in `RNS/Reticulum.py`, `_synthesize_interface` (lines 898-916):

```python
ifac_origin = b""
if interface.ifac_netname != None:
    ifac_origin += RNS.Identity.full_hash(interface.ifac_netname.encode("utf-8"))
if interface.ifac_netkey != None:
    ifac_origin += RNS.Identity.full_hash(interface.ifac_netkey.encode("utf-8"))

ifac_origin_hash = RNS.Identity.full_hash(ifac_origin)        # the final full_hash
interface.ifac_key = RNS.Cryptography.hkdf(
    length=64,
    derive_from=ifac_origin_hash,                             # HKDF over the hash of the origin
    salt=self.ifac_salt,                                      # Reticulum.IFAC_SALT
    context=None,
)
interface.ifac_identity  = RNS.Identity.from_bytes(interface.ifac_key)
interface.ifac_signature = interface.ifac_identity.sign(RNS.Identity.full_hash(interface.ifac_key))
```

The correct derivation is:

```text
ifac_origin       = full_hash(netname) || full_hash(netkey)
ifac_origin_hash  = full_hash(ifac_origin)                      # the step that was dropped
ifac_key          = HKDF(length=64, derive_from=ifac_origin_hash, salt=IFAC_SALT)
```

The affected ports fed the concatenation `ifac_origin` directly into HKDF's `derive_from`,
skipping the final `full_hash(ifac_origin)`. HKDF is deterministic and the salt
(`Reticulum.IFAC_SALT = adf54d882c9a9b80771eb4995d702d4a3e733391b2a0f53f416d9f907e55cff8`) is
shared, so the two sides agree on everything except this one hash step, yielding a divergent
64-byte `ifac_key`, hence a divergent `ifac_identity` and IFAC signature and mask. The
receiving RNS recomputes the expected IFAC, the comparison `ifac == expected_ifac`
(`RNS/Transport.py:1432`) fails, and the packet is discarded without error.

#### Reproduction

```bash
python3 - <<'PY'
import RNS
from RNS.Cryptography import hkdf
IFAC_SALT = bytes.fromhex("adf54d882c9a9b80771eb4995d702d4a3e733391b2a0f53f416d9f907e55cff8")
netname, netkey = "conformance-net", "test-pass"

origin      = RNS.Identity.full_hash(netname.encode()) + RNS.Identity.full_hash(netkey.encode())
origin_hash = RNS.Identity.full_hash(origin)

correct = hkdf(length=64, derive_from=origin_hash, salt=IFAC_SALT, context=None)  # RNS 1.3.1
buggy   = hkdf(length=64, derive_from=origin,      salt=IFAC_SALT, context=None)  # missing final full_hash
print("correct ifac_key[:8] =", correct[:8].hex())
print("buggy   ifac_key[:8] =", buggy[:8].hex())
print("keys differ          =", correct != buggy)
PY
# correct ifac_key[:8] = c008469aff09d54d
# buggy   ifac_key[:8] = b2be7d5183e94d52
# keys differ          = True
```

End to end: two RNS instances with matching `network_name` and `passphrase` exchange
IFAC-masked announces; swap one side for a port that omits the final hash and the announce
never crosses the boundary.

#### Suggested fix (each port)

Insert the missing `full_hash` over the concatenated origin before HKDF:

```text
ifac_origin_hash = full_hash( full_hash(netname) || full_hash(netkey) )
ifac_key         = HKDF(length=64, derive_from=ifac_origin_hash, salt=IFAC_SALT)
```

Add a known-answer test pinning `(netname, passphrase) -> ifac_key` and the resulting Ed25519
IFAC signature against stock RNS, so a self-consistent but wrong derivation on both test sides
cannot hide behind an end-to-end "did the announce cross" check.

### B3. reticulum-kt: path-table replacement lacks the emission-time gate (a stale path overwrites a fresh one)

- Status: Drafted, not yet filed.
- Repository: reticulum-kt (Kotlin). The correct behaviour is RNS 1.3.1 `RNS/Transport.py`
  (announce handling, path-table update); the port mirrors the Python `Transport` region around
  lines 1620-1681 of the version it was ported from. Addressed in kt commit `25ae62c`.
- Severity: high. Without the gate, a stale announce or `PATH_RESPONSE` (older emission
  timestamp) that arrives with an equal or lower hop count overwrites a fresh path-table entry,
  corrupting the next hop and hop count for a destination and silently degrading or
  black-holing routing.
- Referenced by: none (no Coyote leniency).

#### Root cause

RNS gates every path-table replacement on the announce emission timestamp (derived from the
announce `random_blob`), not on hop count alone. In RNS 1.3.1 `RNS/Transport.py` (the
`should_add` computation, lines 1743-1823):

- Equal or lower hop count (line 1762): replace only if the announce is not a replay and
  `announce_emitted > path_timebase` (line 1769), that is, the incoming announce was emitted
  more recently than the entry already held.
- Higher hop count (lines 1774-1823): ignore the announce unless the existing path has expired
  (line 1790) or the incoming emission is strictly more recent than the stored emission
  (`announce_emitted > path_announce_emitted`, line 1806).

The port omitted the emission-time comparison, so it accepted a stale path whenever the hop
count was acceptable, the exact condition the gate exists to prevent.

#### Reproduction (behavioural)

1. Inject announce A for destination D with hops = 1 and a recent emission timestamp. The path
   table records `D -> hops 1`, fresh emission.
2. Inject announce B for the same D with hops = 1 (or fewer) but an older emission timestamp
   (a replayed or stale path response).
3. Correct (RNS): B is rejected by the emission-time gate and `path_table[D]` still reflects A.
   Affected port: B overwrites the entry.
4. Observe through a `path_request` answer for D: a correct implementation reports the fresh
   path, an affected one reports the stale one.

#### Suggested fix

Port the RNS emission-time gate into the path-table update: before replacing an existing entry,
compare the incoming announce's emission timestamp against the stored path's timebase and
replace only when the incoming announce is strictly more recent (or the stored path has expired
or been marked unresponsive), in addition to the hop-count check. A test for it observes the
path table itself (through a `path_request` answer asserting the fresh hop count) rather than
the announce-table retransmit, which passes even when replacement is broken.

### B4. Reticulum: the blackhole check on path-table reload compares an `Identity` object against identity-hash bytes

- Status: Drafted, not yet filed.
- Repository / version: markqvist/Reticulum, confirmed in 1.3.1 (`RNS/Transport.py`).
- Severity: low to medium. The blackhole mechanism's path-table-reload guard is a no-op: paths
  to blackholed identities survive a restart and are re-inserted into the path table. The
  runtime blackhole filters still apply to new traffic, so this is a bypass of the reload skip
  only.
- Referenced by: none (no Coyote leniency).

#### Root cause

`RNS/Transport.py`, path-table load in `start()` (lines 313-315):

```python
if len(Transport.blackholed_identities) > 0:
    path_identity = RNS.Identity.recall(destination_hash, _no_use=True)
    if path_identity in Transport.blackholed_identities: blackholed = True
```

`Identity.recall()` returns an `RNS.Identity` object (or `None`), but
`Transport.blackholed_identities` is a dict keyed by identity-hash bytes (`blackhole_identity()`,
lines 3417-3419, inserts `identity_hash` keys; the type-correct membership idiom is used
elsewhere, for example line 3497: `associated_identity.hash in Transport.blackholed_identities`).
`Identity` defines no `__eq__` or `__hash__`, so default object identity applies and an
`Identity` instance never equals a `bytes` key. The membership test is always `False`,
`blackholed` is never set, and the entry is loaded anyway.

#### Reproduction

```bash
python3 - <<'PY'
import RNS
ident = RNS.Identity()
blackholed_identities = {ident.hash: {"until": None}}   # how Transport keys the table
recalled = ident                                         # stands in for Identity.recall(...)
print(recalled in blackholed_identities)                 # False: object against bytes keys
print(recalled.hash in blackholed_identities)            # True: the intended check
PY
```

On a live node: blackhole an identity (`Transport.blackhole_identity(h)`), let a path to one of
its destinations be saved to the path table, restart. The path entry is reloaded despite the
blackhole (the `blackholed == False` branch at line 318 runs).

#### Suggested fix

```python
if len(Transport.blackholed_identities) > 0:
    path_identity = RNS.Identity.recall(destination_hash, _no_use=True)
    if path_identity != None and path_identity.hash in Transport.blackholed_identities:
        blackholed = True
    del path_identity
```

This also handles the `recall()` returning `None` case, which today only works by accident of
the same broken comparison.

### B5. Reticulum: `Resource.__advertise_job` can overwrite a concurrent `cancel()`

- Status: Drafted, not yet filed.
- Repository / version: markqvist/Reticulum, RNS 1.3.1 (`RNS/Resource.py`); also present in
  1.1.9.
- Severity: medium. A cancelled outgoing resource can keep advertising and transferring after
  the application's cancel callback has fired.
- Referenced by: none (no Coyote leniency).

#### Root cause

`Resource.__advertise_job` (`RNS/Resource.py:520-541`) runs on a daemon thread. After the
one-resource-at-a-time spin-wait

```python
while not self.link.ready_for_new_resource():
    self.status = Resource.QUEUED
    sleep(0.25)
```

it proceeds unconditionally, with no re-check of `self.status` and no lock:

```python
self.advertisement_packet.send()
...
self.status = Resource.ADVERTISED
self.retries_left = self.max_adv_retries
self.link.register_outgoing_resource(self)
```

`Resource.cancel()` (`:1086-1097`) concurrently does, also unlocked:

```python
self.status = Resource.FAILED
...
self.link.cancel_outgoing_resource(self)
```

If `cancel()` interleaves with `__advertise_job` after the link becomes ready (or while the
job is leaving the spin loop), `__advertise_job` sets `status = ADVERTISED` again and re-runs
`register_outgoing_resource(self)`, overwriting the `FAILED` set by `cancel()` and
re-registering a resource the caller already cancelled. The GIL does not help: the window spans
`send()` (I/O) and several method calls, none atomic together.

#### Reproduction

Cancel a queued outgoing resource at the moment its link frees up
(`link.ready_for_new_resource()` flips true): `cancel()` sets `FAILED`, then the daemon
`__advertise_job` overwrites it with `ADVERTISED` and re-registers. The resource transfers
despite the cancellation.

#### Suggested fix

Re-check the status (or hold a short lock) after the spin-wait, before send and register:

```python
def __advertise_job(self):
    self.advertisement_packet = RNS.Packet(...)
    while not self.link.ready_for_new_resource():
        self.status = Resource.QUEUED
        sleep(0.25)
    if self.status != Resource.QUEUED:   # cancel()/reject() won during the wait
        return
    try:
        ...
        self.status = Resource.ADVERTISED
        self.link.register_outgoing_resource(self)
```

A `threading.Lock` shared with `cancel()` around the guard, register and status advance is the
thorough fix.
