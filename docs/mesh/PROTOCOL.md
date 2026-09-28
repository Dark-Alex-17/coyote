# Coyote Mesh Protocol, version 1: wire format

Status: normative. This document specifies the bytes that Coyote instances exchange over Reticulum. The reference implementation is `src/mesh/` in this repository; every value below is the code's value, named by its constant and by the test that pins it. Companion documents: the Security Considerations document (forthcoming under docs/mesh/) and the conformance vector suite (forthcoming as cfg(test) modules under src/mesh/).

## 1. Introduction and scope

Coyote Mesh lets Coyote instances discover and message each other over Reticulum. A node joins through the `.mesh` REPL family and the `mesh__*` tools, announces one destination per instance, answers peers through the envoy (a bounded model run that replies on the human's behalf), and serves its brief as a status card to trusted peers.

This document specifies:

(a) the derivation of every hash and destination a node names (section 4);
(b) the announce application data and its timing (section 5);
(c) the R3 request/response transport over Reticulum Links: frames, the Envelope, dispatch order, refusal codes (section 6);
(d) version negotiation (section 7);
(e) the request paths `/knock`, `/status` and `/message` (sections 8 to 10);
(f) store-and-forward through LXMF propagation nodes, outbound and inbound (section 11);
(g) extensibility and code-point rules (sections 12 and 13);
(h) the canonical forms applied before any peer datum is compared or displayed (section 3).

This document does not specify:

- Security. Security considerations, invariants and the threat model are specified in the companion Security Considerations document (forthcoming under docs/mesh/), not here. Cryptographic agility is inherited from Reticulum and out of scope.
- The human-facing `.mesh` REPL surface and its output text, except where a stored or shown value fixes a wire form.
- On-disk formats, except where a stored form fixes a canonical form (section 3) or a wire value (the knock record, section 8.4).

Section 15 is the single authoritative listing of constants; every constant named in prose is written as `NAME` with its value and is listed there.

## 2. Conventions and requirements language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174] when, and only when, they appear in all capitals, as shown here.

Notation:

- `H(x)`: SHA-256 of the byte string `x`.
- `trunc_n(x)`: the first `n` bytes of `x`.
- `x || y`: byte concatenation.
- msgpack type names: `nil`, `bool`, `uint`, `f64`, `str`, `bin`, `array`, `map`. `uint` is a msgpack integer in either encoding family (`uint` or `int`) whose value is non-negative; the reference reads it with rmpv `as_u64`. `bin(n)` is a `bin` of exactly `n` bytes. `text` is `str` or `bin`; a `bin` is decoded as lossy UTF-8 (section 3.4).
- "hex": lowercase hexadecimal, two digits per byte.
- "silence": the receiver sends no response of any kind.
- "the window": the closed interval `[MESH_PROTOCOL_MIN_SUPPORTED, MESH_PROTOCOL_VERSION]` (section 7).
- "standing": where an identity stands in the receiver's trust store: `Unknown` (no record), `Trusted` (a record on disk or for the session), `Blocked` (`identity_standing`, src/mesh/trust.rs).
- Per-field tables for maps have the columns `Field`, `Type`, `Sender puts`, `Receiver action on any other value`. The last column states what the receiver does for a missing key, a wrong type, an out-of-range value, and (final row) an unknown key. Positional layouts (byte offsets, array elements, announce slots) use `Bytes`, `Element` or `Slot` in place of `Field` and carry the same three remaining columns; their final row covers the bytes, elements or slots this document does not name.
- Byte listings are hex with `<n>` standing for `n` bytes of variable content, for example `92 c4 10 <16>`.

Requirement ids have the form `MESH-<AREA>-<NNN>`. The areas are `DEST` (section 4), `ANN` and `TIME` (sections 5 and 6.8), `ENV` (section 6), `VER` (section 7), `KNOCK` (section 8), `STATUS` (section 9), `MSG` (section 10), `PROP` (section 11), `CANON` (section 3), `EXT` (section 12) and `CODE` (section 13). An id is defined once, in bold brackets at the start of the sentence it governs, and referenced elsewhere in plain form. Ids are stable under the rules of section 14. The conformance vector suite (forthcoming as cfg(test) modules under src/mesh/) is keyed by requirement id: each vector names the ids it exercises, and the byte vectors there are the only place where emitted map key order is compared.

## 3. Canonical forms

### 3.1 Hashes as text

| Quantity | Bytes | Hex digits |
|---|---|---|
| identity hash | 16 | 32 |
| destination hash | 16 | 32 |
| name hash | 10 | 20 |
| transient id | 32 | 64 |
| message id | 32 | 64 |

**[MESH-CANON-001]** An implementation MUST write every hash as lowercase hex (`hex_lower`, src/mesh/mod.rs).

**[MESH-CANON-002]** An identity or destination hash given as text MUST be accepted only when it is exactly 32 ASCII hexadecimal digits, and is lowercased before it is stored or compared (`canonical_hash`, src/mesh/mod.rs).

**[MESH-CANON-003]** Every identity and destination key in the trust store on disk MUST be 32 lowercase hex digits.

**[MESH-CANON-004]** Two identity hashes MUST be compared byte-wise over the decoded 16 bytes; the reference compares in constant time (`authorize`, src/mesh/trust.rs).

### 3.2 Peer text

**[MESH-CANON-005]** An implementation MUST NOT display or compare a peer-supplied text field before cleaning it with `display_text` at that field's cap (`display_text`, src/mesh/mod.rs).

`display_text(text, max_chars)` is, in order:

1. Strip terminal escape sequences (`sanitize_display_text`, src/config/tool_scope.rs): `ESC [` (CSI) is skipped through the first character in `0x40..=0x7e`, scanning at most 128 characters; `ESC ]` (OSC) is skipped through `BEL` (`0x07`) or through the next `ESC` (a `\` directly after that `ESC` is consumed too), scanning at most 128 characters; any other `ESC` and the one character after it are dropped.
2. Replace every other control character (`char::is_control`), and `U+2028` and `U+2029`, with a space.
3. Drop every character in the table of section 3.3 and every variation selector (`U+FE00..=U+FE0F`, `U+E0100..=U+E01EF`).
4. Trim leading and trailing whitespace.
5. Cut after `max_chars` characters, on a character boundary.
6. Trim trailing whitespace.
7. An empty result is absent (`None`), not an empty string.

### 3.3 Control and invisible characters

`is_control_or_invisible` (src/mesh/announce.rs) is true for `char::is_control()` and for every code point below. An announce carrying one of these is ignored (section 5.1); `display_text` drops them.

| Code points |
|---|
| `U+00AD` |
| `U+0600..=U+0605`, `U+061C`, `U+06DD`, `U+070F` |
| `U+0890..=U+0891`, `U+08E2` |
| `U+180E` |
| `U+200B..=U+200F`, `U+2028..=U+2029`, `U+202A..=U+202E`, `U+2060..=U+206F` |
| `U+FEFF`, `U+FFF9..=U+FFFB` |
| `U+110BD`, `U+110CD`, `U+13430..=U+1343F`, `U+1BCA0..=U+1BCA3`, `U+1D173..=U+1D17A` |
| `U+E0000..=U+E007F` |

Variation selectors (`is_variation_selector`: `U+FE00..=U+FE0F`, `U+E0100..=U+E01EF`) are not in this set: they pass the wire and are dropped only by `display_text`.

### 3.4 msgpack

**[MESH-CANON-006]** A sender MUST emit each `uint` in the shortest encoding that holds its value (the reference serialises with rmpv).

**[MESH-CANON-007]** A receiver MUST accept a `uint` of any encoded width whose value fits the field's declared range.

**[MESH-CANON-008]** Where a table in this document states an emission order, a sender MUST emit the map keys in that order (the Envelope, section 6.5; the version refusal, section 7; the status card, section 9.2; the message body, section 10.1).

**[MESH-CANON-009]** A receiver MUST look map keys up by name and MUST NOT depend on their order.

**[MESH-CANON-010]** Where a field's type is `text`, a receiver MUST accept either `str` or `bin`, decoding a `bin` as lossy UTF-8 (`text_of`, src/mesh/message.rs, for the `/message` body fields `kind`, `id`, `in_reply_to`, `title` and `content` of section 10.1 and the LXMF custom-data fields `kind`, `id` and `in_reply_to` of section 10.8; the LXMF type tags of sections 8.6 and 10.8 are compared in either form).

**[MESH-CANON-011]** A receiver MUST accept a non-negative integer in either msgpack integer family wherever this document says `uint`.

**[MESH-CANON-012]** A receiver MUST use the last occurrence of a duplicated key in the Envelope (MESH-ENV-019) and inside the message body's `fields` map, including the typed refusal of section 10.7 (the reference converts `fields` to a JSON object), and the first occurrence in every other map this document defines: the status card and its sub-maps, the `/message` body, the `/knock` body, the version refusal, the dispatch error, the acknowledgement, the LXMF fields map and its custom data (`entry`, src/mesh/message.rs; `Fields::get`, src/mesh/card.rs; `intro_from_r3_body`, src/mesh/knock.rs; `VersionRefusal::from_value`, src/mesh/protocol.rs; `DispatchError::from_value`, src/mesh/r3/dispatch.rs).

**[MESH-CANON-013]** A sender MUST NOT emit a key twice.

The exact byte strings that are hashed are given in section 4.

## 4. Destination naming

Derivations, from the identity's public keys outward:

- identity hash = `trunc_16(H(x25519_public(32) || ed25519_public(32)))`, 16 bytes.
- instance id = a UUIDv4 in simple form, 32 lowercase hex digits.
- destination name = application `coyote`, aspect `mesh.<instance_id>`.
- name hash = `trunc_10(H(name_bytes))` where `name_bytes` is the ASCII string `coyote.mesh.<instance_id>`: the bytes `coyote`, `.`, `mesh.`, then the 32 hex digits of the instance id. 10 bytes.
- destination hash = `trunc_16(H(name_hash(10) || identity_hash(16)))`, 16 bytes.
- LXMF delivery hash = `trunc_16(H(trunc_10(H("lxmf.delivery")) || identity_hash(16)))`, 16 bytes.
- propagation node name hash = `trunc_10(H("lxmf.propagation"))`, 10 bytes.

**[MESH-DEST-001]** An identity hash MUST be `trunc_16(H(x25519_public || ed25519_public))` over the two 32-byte public keys in that order (Reticulum's identity address hash).

**[MESH-DEST-002]** A node's instance id MUST be 32 lowercase hex digits, minted once per session lineage and reused by every session of that lineage (`mesh_instance_id`, `is_valid_mesh_instance_id`, src/config/session.rs).

**[MESH-DEST-003]** A Coyote instance's destination name MUST be application `coyote`, aspect `mesh.<instance_id>` (`DestinationName::new("coyote", "mesh.<instance_id>")`, src/mesh/node.rs).

**[MESH-DEST-004]** The name hash MUST be the first 10 bytes of the SHA-256 of the ASCII string `coyote.mesh.<instance_id>` (upstream `DestinationName::new`, which hashes `app || "." || aspects`).

**[MESH-DEST-005]** The destination hash MUST be `trunc_16(H(name_hash || identity_hash))` (`destination_address`, src/mesh/mod.rs; pinned by `destination_address_matches_upstream_derivation`).

**[MESH-DEST-006]** A receiver MUST attribute a destination hash to an identity only when the derivation of MESH-DEST-005, from the name hash carried with it and the identity hash proven for it, reproduces that destination hash (`verify_binding`, src/mesh/trust.rs; `trust_destination_records_the_identity_the_formula_proves`).

**[MESH-DEST-007]** A receiver MUST NOT trust a destination whose carried name hash fails MESH-DEST-006; nothing is trusted and the failure names the destination the identity would announce (`trust_destination_refuses_a_forged_name_hash`).

**[MESH-DEST-008]** A name hash MUST be combined only with the identity hash that proved itself on the link or signed the message; a peer can name only its own instances.

**[MESH-DEST-009]** Every store-and-forward message MUST carry the sender's LXMF delivery hash as source and the recipient's LXMF delivery hash as destination, where the delivery hash of an identity is the Reticulum destination of application `lxmf`, aspect `delivery`, for that identity (`lxmf_delivery_hash`, src/mesh/propagation.rs).

**[MESH-DEST-010]** A node MUST recognise a propagation node solely by the name hash of application `lxmf`, aspect `propagation` (section 5.4).

## 5. Announce

A Coyote instance announces its destination (section 4) over Reticulum. The announce's application data is the only Coyote-defined content; everything else in the announce is Reticulum's.

### 5.1 Application data

Layout: `magic(4) || version(2) || display_name(0..=64)`; total length 6 to 70 bytes. There is no length prefix.

| Bytes | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| magic, bytes 0..4 | 4 bytes | `ANNOUNCE_MAGIC` = `"COYM"` | **[MESH-ANN-001]** The receiver MUST treat application data shorter than 6 bytes, or whose first 4 bytes are not `"COYM"`, as not a Coyote announce and MUST NOT record it. |
| version, bytes 4..6 | u16 big-endian | its own `MESH_PROTOCOL_VERSION` = `1` | **[MESH-ANN-002]** The receiver MUST record the announce for every value and MUST mark the peer `Incompatible` with the found version when it lies outside the receiver's window (section 7; `Compatibility::of`, src/mesh/protocol.rs; `observe_marks_an_unsupported_announce_version_incompatible`). |
| display_name, bytes 6..end | UTF-8, 0 to `MAX_DISPLAY_NAME_BYTES` = `64` bytes | the configured display name, or nothing (section 5.2) | **[MESH-ANN-003]** The receiver MUST ignore the whole announce when the name is longer than 64 bytes, is not valid UTF-8, or contains any character of the section 3.3 table. **[MESH-ANN-004]** The receiver MUST read an empty name as no display name. |
| any other byte | none | nothing | **[MESH-ANN-005]** There is no other field: the receiver MUST read every byte from offset 6 to the end as the display name (`app_data_carries_only_version_and_display_name`). |

Examples (`encode_layout_is_magic_version_name`, `decode_reads_version_big_endian`): `43 4f 59 4d 00 01 41 6c 65 78` is version 1, display name `Alex`; `43 4f 59 4d 01 02` is version `0x0102`, no display name.

**[MESH-ANN-006]** A sender MUST NOT emit a display name longer than 64 bytes or containing a character of the section 3.3 table (`AnnounceAppData::encode` refuses both).

**[MESH-ANN-007]** Variation selectors pass the wire verbatim: a receiver MUST NOT reject a display name for containing them; they are dropped only by `display_text` at display time (section 3.2).

### 5.2 Display name policy and receiver record

**[MESH-ANN-008]** A sender MUST carry a display name only when `mesh.display_name` is configured.

**[MESH-ANN-009]** A sender MUST withhold the display name on every interface when any configured interface is `type: public`, unless `mesh.display_name_on_public` is `true` (`announce_app_data`, src/mesh/announce.rs; `public_interface_withholds_display_name_unless_opted_in`).

**[MESH-ANN-010]** For each Coyote announce a receiver MUST record the destination hash, the identity hash, the name hash, the display name (or its absence), the announced version and the hop count.

**[MESH-ANN-011]** Every announce MUST re-judge the peer's compatibility from the announced version, overriding a mark learned from a version refusal on the wire (`observe`, src/mesh/peers.rs; `an_announce_refresh_rejudges_a_wire_learned_mark`).

### 5.3 Timing

**[MESH-TIME-001]** A node configured to announce MUST announce once at start.

**[MESH-TIME-002]** A node configured to announce MUST re-announce every `HEARTBEAT_SECS` = `900` seconds (the heartbeat).

**[MESH-TIME-003]** An announce requested less than `REANNOUNCE_FLOOR_SECS` = `300` seconds after the previous one MUST NOT be sent (`announce_now` returns without sending).

**[MESH-TIME-004]** A receiver MUST age a peer out when `now - last_seen >= PEER_TTL`, with `PEER_TTL` = `2700` seconds = `PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT` (`3`) heartbeats, the bound inclusive (`ttl_is_three_heartbeats`).

**[MESH-TIME-005]** A receiver MUST mark a peer stale when its last sighting is `PEER_STALE_AFTER` = `1800` seconds old or older; a sighting in the future is never stale (`stale_is_two_heartbeats_and_never_for_a_future_sighting`).

**[MESH-TIME-006]** A receiver MUST sweep expired peers at least once per `HEARTBEAT_SECS`.

**[MESH-TIME-007]** A receiver MUST bound its peer table at `PEER_TABLE_MAX_ENTRIES` = `1024` peers.

### 5.4 Propagation node announces

LXMF propagation nodes announce a destination of application `lxmf`, aspect `propagation`, whose application data is a msgpack array. Coyote reads it and never emits it. Reference layout: `[false, timebase, enabled, per_transfer_kb, per_sync_kb, [cost, flex, peering], {}]`. The layout check is upstream's `lxmf_core::announce::validate_pn_announce_data` (lxmf-rs rev 3ed5932), which `PropagationNode::from_announce` (src/mesh/propagation.rs) runs before reading any slot; each refusal it produces is mapped to `InvalidAnnounce` and the node is not filed. In this section `int` is a msgpack integer of either family whose value fits `i64`, which is how upstream reads every integer slot.

**[MESH-ANN-012]** A receiver MUST file into its propagation node table only an announce whose name hash equals `trunc_10(H("lxmf.propagation"))` (`PropagationNode::from_announce`, src/mesh/propagation.rs).

**[MESH-ANN-013]** When the application data does not decode as msgpack, or decodes as anything other than an `array`, the receiver MUST refuse the announce as `InvalidAnnounce` and MUST NOT file the node (`from_announce_refuses_malformed_app_data`).

**[MESH-ANN-033]** Bytes following the array MUST be ignored; both the upstream validator (`validate_pn_announce_data`) and the reference (`PropagationNode::from_announce`, src/mesh/propagation.rs) read one msgpack value and never examine the remainder.

**[MESH-ANN-014]** An array shorter than 7 elements MUST be refused as `InvalidAnnounce` and MUST NOT be filed (`from_announce_refuses_malformed_app_data`).

| Slot | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| slot `[0]` | any | not emitted by Coyote | **[MESH-ANN-015]** The receiver MUST ignore it. |
| slot `[1]` | int | not emitted by Coyote | **[MESH-ANN-016]** Not an `int` (the node's timebase, which Coyote does not read): the receiver MUST refuse the announce as `InvalidAnnounce` and MUST NOT file the node. |
| slot `[2]` | bool | not emitted by Coyote | **[MESH-ANN-017]** The receiver MUST read this slot as whether the node accepts propagation (`PropagationNode::propagation_enabled`). **[MESH-ANN-018]** Not a `bool`: the receiver MUST refuse the announce as `InvalidAnnounce` and MUST NOT file the node. |
| slot `[3]` | int, non-negative | not emitted by Coyote | **[MESH-ANN-019]** The receiver MUST read this slot as the node's per-transfer limit in kilobytes (`PropagationNode::per_transfer_limit_kb`). **[MESH-ANN-020]** Not an `int`: the receiver MUST refuse the announce as `InvalidAnnounce` and MUST NOT file the node. **[MESH-ANN-021]** A negative `int`: the receiver MUST refuse the announce as `InvalidAnnounce` (`per-transfer limit is not a non-negative integer`, `from_announce_refuses_malformed_app_data`) and MUST NOT file the node. |
| slot `[4]` | int | not emitted by Coyote | **[MESH-ANN-022]** Not an `int` (the node's per-sync limit, which Coyote does not read): the receiver MUST refuse the announce as `InvalidAnnounce` and MUST NOT file the node. |
| slot `[5]` | array of at least 3 `int` | not emitted by Coyote | **[MESH-ANN-023]** Not an `array`, shorter than 3 elements, or with any of `[5][0]`, `[5][1]`, `[5][2]` not an `int`: the receiver MUST refuse the announce as `InvalidAnnounce` and MUST NOT file the node (`from_announce_refuses_malformed_app_data`). |
| slot `[5][0]` | int | not emitted by Coyote | **[MESH-ANN-024]** The receiver MUST read this element as the node's stamp cost (upstream `lxmf_core::announce::pn_stamp_cost_from_app_data`). **[MESH-ANN-025]** Negative: the receiver MUST refuse the announce as `NegativeStampCost` and MUST NOT file the node. **[MESH-ANN-026]** Greater than `u32::MAX`: the receiver MUST refuse the announce as `InvalidAnnounce` (`stamp cost does not fit a u32`) and MUST NOT file the node. **[MESH-ANN-027]** The receiver MUST file any other value, including one above `MAX_ACCEPTED_STAMP_COST` (section 11.1, `from_announce_refuses_negative_costs_and_files_any_other`). |
| slot `[6]` | map | not emitted by Coyote | **[MESH-ANN-028]** Not a `map` (the node's metadata, which Coyote does not read): the receiver MUST refuse the announce as `InvalidAnnounce` and MUST NOT file the node. |
| any other slot | any | nothing | **[MESH-ANN-029]** Elements beyond `[6]`: the receiver MUST ignore them. |

**[MESH-ANN-030]** The propagation node table MUST hold at most `PROPAGATION_NODE_TABLE_MAX_ENTRIES` = `32` nodes; at the cap the least recently heard node is evicted (`cap_evicts_the_least_recently_heard_and_logs_it`, src/mesh/propagation_nodes.rs).

**[MESH-ANN-031]** Entries of the propagation node table MUST NOT be aged out.

**[MESH-ANN-032]** For fetching (section 11.3) a node MUST select the propagation node with the fewest hops, ties broken by the most recent sighting.

## 6. R3 transport

R3 is Coyote's request/response layer over a Reticulum Link. Its frames are byte-identical to Reticulum's Link request and response, so a Coyote request is a Reticulum request whose `data` is the Envelope of section 6.5.

### 6.1 Request frame

A request is the msgpack array `[time, path_hash, data]`. Its encoding begins `93 cb <8> c4 10 <16>`; with `nil` data the whole frame is 29 bytes (`request_frame_layout_is_fixed_width_apart_from_the_body`, `request_frame_matches_upstream_link_request_byte_for_byte`).

| Element | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `[0]` time | f64 | the current Unix time in seconds | **[MESH-ENV-001]** The receiver MUST reject the frame (silence, stage 5 of section 6.6) when this element is not an `f64`. |
| `[1]` path_hash | bin(16) | `trunc_16(H(path))` over the ASCII bytes of the path (`PathHash::of`, src/mesh/r3/frame.rs) | **[MESH-ENV-002]** The receiver MUST reject the frame when this element is not a `bin` of exactly 16 bytes. |
| `[2]` data | any | the Envelope (section 6.5) | **[MESH-ENV-003]** The receiver MUST reject a frame that is not an `array` of exactly three elements. |
| trailing bytes | none | nothing | **[MESH-ENV-004]** The receiver MUST reject a frame followed by any further byte (`decode_whole`; `decode_refuses_malformed_frames`). |

### 6.2 Response frame

A response is the msgpack array `[request_id, value]`; its encoding begins `92 c4 10` (`response_frame_is_accepted_by_upstream_envelope_unpacker`).

| Element | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `[0]` request_id | bin(16) | the request id of the request being answered (section 6.3) | **[MESH-ENV-005]** A responder MUST put the request id computed by section 6.3 for the request it answers; a response naming no outstanding request answers nothing. |
| `[1]` value | any | a reply value, a refusal code, a version refusal map or a dispatch error map | **[MESH-ENV-006]** The requester MUST decode this element in the order given in section 6.7. |
| any other element or trailing bytes | none | nothing | **[MESH-ENV-007]** The requester MUST drop a response that is longer than `MAX_R3_PAYLOAD_BYTES`, that is not an `array` of exactly two elements, whose `[0]` is not a `bin` of exactly 16 bytes, that is followed by any further byte, that names no outstanding request, or that arrives on a link other than the one its request went out on; the request then ends in `Timeout` (`ResponseFrame::decode`, src/mesh/r3/frame.rs; `R3Client::deliver`, src/mesh/r3/client.rs; `response_on_the_wrong_link_is_ignored`). |

### 6.3 Size branches

**[MESH-ENV-008]** An encoded frame no longer than the link MDU MUST travel as a single link packet, and its request id is `trunc_16` of that packet's hash.

**[MESH-ENV-009]** An encoded frame longer than the link MDU MUST travel as a Reticulum resource, and its request id is `trunc_16(H(encoded_frame))`.

**[MESH-ENV-010]** The size branch MUST be chosen the same way for requests and responses (`boundary_sizes_pick_packet_or_resource_on_both_halves`; the reference link MDU is 431 bytes at MTU 500).

**[MESH-ENV-011]** A sender MUST refuse locally, without sending, any frame longer than `MAX_R3_PAYLOAD_BYTES` = `262144` bytes (`R3Error::Oversize`); the inbound bound is stage 1 of section 6.6.

### 6.4 Identity

**[MESH-ENV-012]** Every request MUST travel on a Link on which the requester has identified (Reticulum `identify()`).

**[MESH-ENV-013]** A responder MUST NOT answer an anonymous request; it hears silence.

### 6.5 The Envelope

The `data` of every request is the map below (`Envelope::into_value`, `Envelope::from_value`, src/mesh/r3/frame.rs; `envelope_round_trips_and_rejects_anything_that_names_no_origin`).

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `v` | uint whose value fits u16 | its own `MESH_PROTOCOL_VERSION` = `1` | **[MESH-ENV-014]** When `v` is missing, is not a `uint`, exceeds 65535 or lies outside the receiver's window, the receiver MUST answer with the version refusal of section 7 and MUST NOT file a knock. |
| `name_hash` | bin(10) | the requester's own name hash (section 4) | **[MESH-ENV-015]** When `name_hash` is missing, is not a `bin` or is not 10 bytes, the receiver MUST answer `NoAccess` and MUST NOT file a knock. |
| `body` | any | the path's request body (sections 8 to 10) | **[MESH-ENV-016]** When `body` is missing the receiver MUST answer `NoAccess`. |
| any other key | any | nothing | **[MESH-ENV-017]** The receiver MUST ignore it. |

**[MESH-ENV-018]** A sender MUST emit the keys in the order `v`, `name_hash`, `body`.

**[MESH-ENV-019]** When a key occurs more than once, the receiver MUST use the last occurrence.

**[MESH-ENV-020]** The receiver MUST judge `v` before it examines `name_hash` or `body`.

**[MESH-ENV-021]** When `data` is not a `map`, the receiver MUST answer `NoAccess` (stage 7 of section 6.6); `v` is judged only inside a `map`.

**[MESH-ENV-022]** The receiver MUST compute the requester's instance destination as `trunc_16(H(name_hash || identity_hash))` from the Envelope's `name_hash` and the identity proven on the link, and authorize the request against that destination.

**[MESH-ENV-023]** The receiver MUST NOT accept a destination hash supplied by the requester in place of MESH-ENV-022.

### 6.6 Dispatch order

Stages run in this order; the first that fires decides the response (src/mesh/r3/server.rs pre-stages, then `Dispatcher::handle`, src/mesh/r3/dispatch.rs).

| Stage | Condition | Response |
|---|---|---|
| 1 | the encoded frame is longer than `MAX_R3_PAYLOAD_BYTES` | **[MESH-ENV-024]** The receiver MUST drop it with silence, after assembly when it arrived as a resource (`oversized_request_resource_is_dropped_before_the_handler_runs`). |
| 2 | all `MAX_CONCURRENT_INBOUND_REQUESTS` = `16` handler slots are busy | **[MESH-ENV-025]** The receiver MUST drop it with silence (`requests_beyond_the_handler_slots_are_dropped_silently`). |
| 3 | no identity is proven on the link | **[MESH-ENV-026]** The receiver MUST answer with silence. |
| 4 | the identity's standing is `Unknown` or `Blocked` | **[MESH-ENV-027]** The receiver MUST answer with silence, before decoding any byte of the frame data. |
| 5 | the frame does not decode (section 6.1) | **[MESH-ENV-028]** The receiver MUST answer with silence. |
| 6 | the Envelope is a `map` whose `v` is missing or unsupported | **[MESH-ENV-029]** The receiver MUST answer with the version refusal value of section 7, on every path including unknown ones, and MUST NOT file a knock (`wrong_version_requests_are_refused_on_every_path_without_reaching_a_handler`). |
| 7 | the Envelope is not a `map`, or is a `map` whose `name_hash` or `body` is malformed | **[MESH-ENV-030]** The receiver MUST answer `NoAccess`. |
| 8a | the trust verdict is Refuse by rule identity blocked | **[MESH-ENV-031]** The receiver MUST answer with silence. |
| 8b | the trust verdict is Refuse by rule default closed | **[MESH-ENV-032]** The receiver MUST file a knock (section 8.3), retaining the body only when the path is `/knock`, and then answer `NoAccess`. |
| 8c | the trust verdict is Refuse by any other rule (destination denied) | **[MESH-ENV-033]** The receiver MUST answer `NoAccess`. |
| 9a | the verdict is Allow and the path is unknown | **[MESH-ENV-034]** The receiver MUST answer the `unknown_path` map below. |
| 9b | the verdict is Allow and the path is known but has no provider | **[MESH-ENV-035]** The receiver MUST answer the `no_provider` map below. |
| 9c | the verdict is Allow and the path has a provider | **[MESH-ENV-036]** The receiver MUST answer with the handler's reply; a `Silent` reply sends nothing. |
| 10 | the handler has not replied within `HANDLER_TIMEOUT` = `20` seconds | **[MESH-ENV-037]** The receiver MUST answer with silence. |

**[MESH-ENV-038]** Every `NoAccess` refusal MUST be byte-identical whichever stage produced it: `92 c4 10 <16> cc f1` (`every_refusal_is_the_same_bytes_on_the_wire`; `NoAccess` is built at one site, `refuse`, `no_access_is_named_at_exactly_one_site_outside_the_error_module`).

**[MESH-ENV-039]** The trust verdict MUST be evaluated in this precedence: destination deny, identity block, destination allow (the identity recorded for that destination equal to the proven identity, compared per MESH-CANON-004), identity allow for all destinations, default closed (`authorize`, src/mesh/trust.rs).

Dispatch error maps (`DispatchError`, src/mesh/r3/dispatch.rs; `dispatch_errors_round_trip_as_maps_and_never_read_as_refusal_codes`):

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `error` | str | `"unknown_path"` or `"no_provider"` | **[MESH-ENV-040]** A requester MUST read a map without a `str` `error` equal to one of these as the path's reply value, never as a refusal code. |
| `path_hash` | str, 32 hex | with `unknown_path`: the hex of the request's path hash | **[MESH-ENV-041]** When `error` is `"unknown_path"`, the requester MUST read the map as a dispatch error only when this key is a `str` of exactly 32 ASCII hexadecimal digits, and otherwise as the path's reply value (`unknown_path_with_a_malformed_hash_is_not_a_dispatch_error`, src/mesh/r3/dispatch.rs). |
| `path` | str | with `no_provider`: the known path, for example `"/status"` | **[MESH-ENV-042]** When `error` is `"no_provider"`, the requester MUST read the map as a dispatch error only when this key is a `str` equal to one of `/knock`, `/status` and `/message`, and otherwise as the path's reply value (`no_provider_with_an_unknown_path_is_not_a_dispatch_error`, src/mesh/r3/dispatch.rs). |
| any other key | any | nothing | **[MESH-ENV-043]** The receiver MUST ignore it. |

### 6.7 Refusal codes and client decoding

A refusal is a bare msgpack `uint` as the response value (`RefusalCode`, src/mesh/r3/error.rs; `refusal_codes_round_trip_the_wire_and_reject_other_values`).

| Code | Value | Wire bytes | Built by Coyote | Read by Coyote |
|---|---|---|---|---|
| `NoIdentity` | `0xf0` | `cc f0` | never | propagation node sentinel (section 11.3) |
| `NoAccess` | `0xf1` | `cc f1` | dispatcher (section 6.6) | request refused; propagation node sentinel |
| `InvalidKey` | `0xf3` | `cc f3` | never | propagation node sentinel |
| `InvalidData` | `0xf4` | `cc f4` | `/message` handler (section 10.3) | request refused |
| `InvalidStamp` | `0xf5` | `cc f5` | never | propagation node verdict (section 11.2) |
| `Throttled` | `0xf6` | `cc f6` | `/message` handler (section 10.3) | request refused |
| `NotFound` | `0xfd` | `cc fd` | never | propagation node sentinel |
| `Timeout` | `0xfe` | `cc fe` | never | propagation node sentinel |

**[MESH-ENV-044]** A refusal MUST be sent as the bare `uint` of its value as the response value (`cc XX`).

**[MESH-ENV-045]** Coyote MUST build only `NoAccess` (the dispatcher), `InvalidData` and `Throttled` (the `/message` handler); the other five codes are read, never built.

**[MESH-ENV-046]** A requester MUST decode a response value in this order: a `uint` equal to one of the eight values is that refusal; otherwise a map matching section 7 is a version refusal; otherwise the value is the path's reply value.

**[MESH-ENV-047]** A requester MUST read a `uint` that is none of the eight values as the path's reply value (for `/message` that reply is not the acknowledgement, section 10.4).

### 6.8 Timeouts

| Timer | Constant | Value | Side |
|---|---|---|---|
| link open and identify | `DEFAULT_LINK_TIMEOUT` | `10` s | requester |
| request, default | `DEFAULT_REQUEST_TIMEOUT` | `30` s | requester |
| request, `/knock` | `KNOCK_REQUEST_TIMEOUT` | `15` s | requester |
| request, `/message` | `PEER_REQUEST_TIMEOUT` | `15` s | requester |
| link open for `/knock` and `/message` | `KNOCK_LINK_TIMEOUT`, `PEER_LINK_TIMEOUT` | `10` s | requester |
| identity resolution on the link | `PEER_RESOLVE_TIMEOUT` | `2` s | responder |
| handler | `HANDLER_TIMEOUT` | `20` s | responder |
| response send | `DEFAULT_RESPONSE_SEND_TIMEOUT` | `10` s | responder |

**[MESH-TIME-008]** A requester that hears no response within its request timeout MUST treat the request as `Timeout`; this is one of the outcomes that permit store-and-forward fallback (sections 8.5 and 10.4).

**[MESH-TIME-009]** A requester whose link is not open and identified within `DEFAULT_LINK_TIMEOUT` MUST treat the request as `LinkFailed`.

**[MESH-TIME-010]** A responder MUST abandon a handler that has not replied within `HANDLER_TIMEOUT` (stage 10 of section 6.6).

## 7. Version negotiation

**[MESH-VER-001]** The protocol version MUST be an unsigned 16-bit integer: big-endian in the announce (section 5.1), a msgpack `uint` whose value fits u16 in the Envelope (section 6.5).

**[MESH-VER-002]** A node MUST accept exactly the versions in its window `[MESH_PROTOCOL_MIN_SUPPORTED, MESH_PROTOCOL_VERSION]` = `[1, 1]` (`protocol_supported`, src/mesh/protocol.rs; `protocol_version_constants_are_pinned`).

**[MESH-VER-003]** A sender MUST put its own `MESH_PROTOCOL_VERSION` in every announce and every Envelope.

**[MESH-VER-004]** In version 1 the Envelope key `v` is REQUIRED.

**[MESH-VER-005]** A receiver MUST NOT read a missing `v` as version 1; it refuses with `found` = `nil` (`a_missing_version_key_is_refused_not_tolerated`).

The version refusal value (`VersionRefusal::to_value`, `VersionRefusal::from_value`, src/mesh/protocol.rs; `unsupported_version_refusal_shape_is_pinned`):

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `refusal` | str | `"unsupported_version"` | **[MESH-VER-006]** A requester MUST NOT read a map as a version refusal unless `refusal` is exactly this string; the value falls through to the path's reply value (section 6.7). |
| `found` | uint or nil | the `v` it received; `nil` when `v` was missing or not a u16 | **[MESH-VER-007]** When absent, or present and neither `nil` nor a `uint` fitting u16, the requester MUST NOT read the map as a version refusal (`VersionRefusal::from_value`; `found` = `"2"` is refused in `unsupported_version_refusal_shape_is_pinned`). |
| `min` | uint | its `MESH_PROTOCOL_MIN_SUPPORTED` | **[MESH-VER-008]** When absent or not a `uint`, the requester MUST NOT read the map as a version refusal. |
| `max` | uint | its `MESH_PROTOCOL_VERSION` | **[MESH-VER-009]** When absent or not a `uint`, the requester MUST NOT read the map as a version refusal. |
| any other key | any | nothing | **[MESH-VER-010]** The receiver MUST ignore it. |

**[MESH-VER-011]** A responder MUST emit the keys in the order `refusal`, `found`, `min`, `max`.

**[MESH-VER-012]** A responder MUST emit the version refusal after judging identity standing and before parsing any body, on every path (stage 6 of section 6.6; `an_untrusted_identity_on_a_wrong_version_still_hears_silence`).

**[MESH-VER-013]** A requester that receives a version refusal MUST mark the peer `Incompatible` with `found` = `max` only when `min <= max` and the version it sent lies outside `[min, max]`; any other refusal leaves the mark unchanged (`note_version_refusal`, src/mesh/node.rs; `version_refusal_marks_only_a_consistent_window_that_excludes_our_version`).

**[MESH-VER-014]** A node MUST refuse locally, without opening a link, any outbound request to a peer marked `Incompatible` (`SendError::IncompatibleVersion`; `an_incompatible_announce_is_recorded_but_refused_outbound`).

The announce is authoritative: every announce re-judges the mark from its version (MESH-ANN-011), and an announce outside the window records the peer and marks it (MESH-ANN-002).

Schema versions are distinct from the protocol version and travel inside bodies: `STATUS_CARD_VERSION` = `1` (card `v`, section 9.2), `PEER_WIRE_VERSION` = `1` (message body `v`, section 10.1), and the LXMF type tags `"coyote.knock/1"` and `"coyote.peer/1"` (sections 8.6 and 10.8).

Two nodes interoperate exactly when their windows intersect. Failure is visible as the refusal map above over R3, and as a local `Incompatible` mark with outbound refused after an announce.

## 8. /knock

A knocking node (the knocker) asks a receiver whose trust is default closed to surface the knocker's identity and instance so that the receiver's human can trust that destination. A knock is a request on `KNOCK_PATH` = `"/knock"`. The route is reserved by the dispatcher (`KnockHandler`, src/mesh/r3/dispatch.rs).

### 8.1 Request body

`KnockIntro::to_r3_body` (src/mesh/knock.rs):

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `intro` | str | the intro after `display_text` cleaning, at most `KNOCK_INTRO_MAX_CHARS` = `200` characters | **[MESH-KNOCK-001]** The receiver MUST treat the intro as absent and still count the knock when the body is not a `map` or `intro` is absent or not a `str`, and MUST cut an intro longer than 200 characters to 200 characters on a character boundary after cleaning (`intro_from_r3_body`; `intro_is_refused_over_the_cap_and_cleaned_under_it`). |
| any other key | any | nothing | **[MESH-KNOCK-002]** The receiver MUST ignore it. |

**[MESH-KNOCK-003]** A sender MUST NOT send an intro longer than 200 characters after cleaning; the reference refuses it locally (`IntroTooLong`).

**[MESH-KNOCK-004]** A sender MUST clean the intro with `display_text` before it is sent.

### 8.2 Reply

| Caller | Reply value |
|---|---|
| default closed, admitted by the gate (section 8.3) | **[MESH-KNOCK-005]** The receiver MUST answer `NoAccess` (`0xf1`); this is the normal, successful outcome of a knock from a known but untrusted instance. |
| default closed, rate limited by the gate | **[MESH-KNOCK-006]** The receiver MUST answer `NoAccess` and MUST NOT cache or surface the knock. |
| already trusted (verdict Allow) | **[MESH-KNOCK-007]** The receiver MUST answer `nil` and MUST NOT feed the knock sink (`a_trusted_instance_calling_knock_is_acknowledged_and_never_reaches_the_sink`). |
| destination denied | **[MESH-KNOCK-008]** The receiver MUST answer `NoAccess`; this is not a knock. |
| identity `Blocked` or `Unknown` | **[MESH-KNOCK-009]** The receiver MUST answer with silence (stages 4 and 8a of section 6.6; `a_blocked_identity_knocking_over_a_link_is_answered_with_silence`). |

### 8.3 Gate

After the wire, the gate (`KnockGate::admit`, src/mesh/knock.rs) decides in this order: a blocked identity yields nothing; an `Unknown` identity yields nothing and is never a knock; a verdict Allow is already trusted and not a knock; a Refuse by identity block yields nothing; a Refuse by any rule other than default closed (destination denied) is not a knock; a Refuse by default closed proceeds to the rate limit.

**[MESH-KNOCK-010]** The receiver MUST rate-limit knocks per identity with a token bucket of burst `KNOCK_BUCKET_BURST` = `3` refilled one token per `KNOCK_BUCKET_REFILL_INTERVAL` = `600` seconds and never past the burst (`one_identity_is_rate_limited_per_identity_and_surfaced_once`, `the_bucket_refills_one_token_per_interval_and_never_past_the_burst`).

**[MESH-KNOCK-011]** The gate MUST track at most `KNOCK_GATE_MAX_IDENTITIES` = `256` identities, forgetting the least recently seen past the cap (`the_gate_forgets_the_least_recently_seen_identity_past_its_cap`).

**[MESH-KNOCK-012]** The receiver MUST surface an admitted knock to its human at most once per identity per process.

**[MESH-KNOCK-013]** The knock cache MUST hold at most `KNOCK_CACHE_MAX_ENTRIES` = `256` records, newest wins (`append_caps_the_cache_at_max_entries_newest_wins`, src/mesh/knocks.rs).

**[MESH-KNOCK-014]** The knock cache MUST hold at most `KNOCK_CACHE_MAX_PER_IDENTITY` = `16` records per identity, keeping the newest (`append_keeps_only_the_newest_per_identity_and_never_warns_for_it`).

### 8.4 Knock record and hint

An admitted knock is appended to the cache as a record (`KnockRecord`, src/mesh/knocks.rs):

| Member | Content |
|---|---|
| `version` | `1` |
| `received_at` | RFC 3339 UTC |
| `identity_hash` | 32 hex |
| `destination_hash` | 32 hex, the instance computed per MESH-ENV-022 |
| `name_hash` | 20 hex (empty only for rows written before the field existed) |
| `display_name` | the knocker's announced display name, if any |
| `intro` | the cleaned intro, if any |
| `hops` | the announced hop count |

**[MESH-KNOCK-015]** The receiver MUST file the knocker's name hash with the record, so that trusting the destination later re-derives and checks the binding (MESH-DEST-006).

The hint surfaced to the human is two lines, where `who` is `A peer` or the display name in double quotes (double quotes inside the name replaced by `'`) and the intro suffix, when present, is `: "<intro>"`:

```
{who} (identity {identity}) knocks from instance {destination}{intro}
trust: .mesh trust {destination} | who: .mesh info {destination} | silence: .mesh block {identity}
```

### 8.5 Sender outcome

The knocker requests `/knock` with `KNOCK_REQUEST_TIMEOUT` = `15` seconds (src/mesh/node.rs).

The reference implements this path in `MeshRuntime` (`knock`, src/mesh/node.rs) but does not yet drive it from a REPL verb or a scheduler.

| Result of the direct request | Outcome |
|---|---|
| any reply value, or `Refused(NoAccess)` | **[MESH-KNOCK-016]** The knocker MUST treat the knock as landed directly (`Direct`) and MUST NOT fall back (`a_refused_knock_is_direct_and_leaves_the_propagation_node_alone`). |
| `Timeout`, `LinkFailed` or `LinkClosed` | **[MESH-KNOCK-017]** The knocker MUST fall back to store-and-forward (section 8.6) and only then (`an_unreachable_knock_falls_back_to_the_propagation_node`). |
| any other refusal code, or a version refusal | **[MESH-KNOCK-018]** The knocker MUST treat it as a direct failure, MUST NOT fall back and MUST NOT store anything (`a_refusal_other_than_no_access_is_a_direct_failure_and_never_stored`). |
| fallback wanted, no propagation node known | **[MESH-KNOCK-019]** The knocker MUST fail with `NoPropagationNode`. |

### 8.6 Knock over LXMF

A store-and-forward knock is an LXMF message (`knock_message`, `decode_knock_message`, src/mesh/knock.rs; `lxmf_knock_wire_shape_is_exactly_the_typed_two_field_layout`) posted per section 11.1. Title absent, content = the intro's UTF-8 bytes, fields as below.

LXMF fields map:

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `0xfb` (`FIELD_CUSTOM_TYPE`) | text | `KNOCK_TYPE` = `"coyote.knock/1"` | **[MESH-KNOCK-020]** When absent or not this tag, the receiver MUST NOT treat the message as a knock; it proceeds as an ordinary message (section 11.4, stage 9). |
| `0xfc` (`FIELD_CUSTOM_DATA`) | map | the custom data map below | **[MESH-KNOCK-021]** When missing or not a `map`, the receiver MUST drop the message as malformed. |
| any other key | any | nothing | **[MESH-KNOCK-022]** The receiver MUST ignore it. |

Custom data map:

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `name_hash` | bin(10) | the knocker's own name hash | **[MESH-KNOCK-023]** When missing or not a `bin` the receiver MUST drop the message as malformed (`name_hash is missing or not binary`); when not 10 bytes, likewise (`name_hash is not 10 bytes`). |
| any other key | any | nothing | **[MESH-KNOCK-024]** The receiver MUST ignore it. |

**[MESH-KNOCK-025]** A sender MUST leave the LXMF title absent.

**[MESH-KNOCK-026]** A sender MUST put the cleaned intro's UTF-8 bytes as the LXMF content, or empty content when there is no intro.

**[MESH-KNOCK-027]** The receiver MUST decode the content as lossy UTF-8 and clean it with `display_text` at 200 characters.

**[MESH-KNOCK-028]** The receiver MUST compute the knocking instance from the signed source identity and the carried `name_hash` per MESH-ENV-022 and MUST NOT use any destination the message supplies.

**[MESH-KNOCK-029]** A knock from a blocked identity arriving by store-and-forward MUST be discarded as `BlockedSource`, indistinguishable to the knocker from an unreachable receiver (section 11.4, stage 8).

## 9. /status

The status card is Coyote's brief as served to a trusted peer: a request on `STATUS_PATH` = `"/status"` whose reply value is the card map (`StatusCard::to_value`, `StatusCard::from_value`, src/mesh/card.rs).

### 9.1 Request and reply

**[MESH-STATUS-001]** A requester puts `nil` as the body, which the responder MUST ignore.

**[MESH-STATUS-002]** A requester MUST NOT fall back to store-and-forward for `/status` (`request_status_is_typed_and_never_store_and_forward`).

**[MESH-STATUS-003]** A responder MUST serve the card only to a requester whose destination is allowed (stage 9c of section 6.6); every other requester receives the outcome of section 6.6 for its standing (`status_is_never_served_to_unknown_or_untrusted_requesters`).

A card at every cap exceeds the link MDU and travels as a resource per section 6.3 (`oversize_status_card_round_trips_as_a_resource`).

### 9.2 Card

Emission order: `v`, `display_name`, `objective`, `state`, `repo`, `plan`, `todo`, `"snapshot_age_secs"`, `"served_at_secs"`.

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `v` | uint | `STATUS_CARD_VERSION` = `1` | **[MESH-STATUS-004]** Greater than 1: the receiver MUST reject the card as `UnsupportedVersion` with `found` and `supported` = `1` (`version_is_one_and_newer_or_missing_versions_are_refused_by_name`); missing or `nil`: `Malformed` (`v is missing`); `0`: `Malformed` (`v is 0`); not a `uint`: `Malformed`. |
| `display_name` | str | the node's display name, cleaned, at most `DISPLAY_NAME_MAX_CHARS` = `64` characters; omitted when none | **[MESH-STATUS-005]** Not a `str` (a `bin` included): the receiver MUST reject the card as `Malformed` (`Fields::text`, src/mesh/card.rs, reads with `as_str`); absent or `nil`: no name; longer than 64 characters: truncated, not refused (`decoding_sanitises_and_caps_peer_text_and_refuses_a_blank_required_string`). |
| `objective` | str | the current objective, at most `OBJECTIVE_MAX_CHARS` = `280` characters; omitted when none | **[MESH-STATUS-006]** Not a `str`: the receiver MUST reject the card as `Malformed`; absent or `nil`: none; longer than 280 characters: truncated. |
| `state` | map | the state map (section 9.3) | **[MESH-STATUS-007]** Missing, `nil` or not a `map`: the receiver MUST reject the card as `Malformed`. |
| `repo` | map | the repo map (section 9.4); omitted when none | **[MESH-STATUS-008]** Not a `map`: the receiver MUST reject the card as `Malformed`; absent or `nil`: none. |
| `plan` | map | the plan map (section 9.5); omitted when none | **[MESH-STATUS-009]** Not a `map`: the receiver MUST reject the card as `Malformed`; absent or `nil`: none. |
| `todo` | map | the todo map (section 9.6); omitted when none | **[MESH-STATUS-010]** Not a `map`: the receiver MUST reject the card as `Malformed`; absent or `nil`: none. |
| `"snapshot_age_secs"` | uint | seconds since the snapshot was taken; omitted when unknown | **[MESH-STATUS-011]** Not a non-negative integer: the receiver MUST reject the card as `Malformed`; absent or `nil`: none. |
| `"served_at_secs"` | uint | Unix seconds when the card was served | **[MESH-STATUS-012]** Missing, `nil` or not a non-negative integer: the receiver MUST reject the card as `Malformed`. |
| any other key | any | nothing | **[MESH-STATUS-013]** The receiver MUST ignore it (`unknown_keys_are_ignored_and_unknown_state_codes_are_kept`). |

**[MESH-STATUS-014]** A responder MUST emit the keys in the order given above, with sub-maps in the orders of sections 9.3 to 9.6.

**[MESH-STATUS-015]** A responder MUST omit an absent field rather than send `nil`.

**[MESH-STATUS-016]** A receiver MUST treat a `nil` value as a missing key (`Fields::get`).

### 9.3 State map

Emission order: `code`, `since_secs`.

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `code` | uint fitting u8 | `STATE_UNKNOWN` = `0`, `STATE_IDLE` = `1` or `STATE_WORKING` = `2` | **[MESH-STATUS-017]** Missing or not a byte: the receiver MUST reject the card as `Malformed`; any other byte value is kept (MESH-STATUS-029). |
| `since_secs` | uint | seconds in the current state; omitted when unknown | **[MESH-STATUS-018]** Not a non-negative integer: the receiver MUST reject the card as `Malformed`; absent or `nil`: none. |
| any other key | any | nothing | **[MESH-STATUS-019]** The receiver MUST ignore it. |

### 9.4 Repo map

Emission order: `name`, `branch`.

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `name` | str | the repository name, at most `REPO_NAME_MAX_CHARS` = `64` characters | **[MESH-STATUS-020]** Missing, `nil`, not a `str`, or blank after cleaning: the receiver MUST reject the card as `Malformed`; longer than 64 characters: truncated. |
| `branch` | str | the branch, at most `BRANCH_MAX_CHARS` = `64` characters; omitted when none | **[MESH-STATUS-021]** Not a `str`: the receiver MUST reject the card as `Malformed`; absent or `nil`: none; longer than 64 characters: truncated. |
| any other key | any | nothing | **[MESH-STATUS-022]** The receiver MUST ignore it. |

### 9.5 Plan map

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `title` | str | the plan title, at most `PLAN_TITLE_MAX_CHARS` = `120` characters | **[MESH-STATUS-023]** Missing, `nil`, not a `str`, or blank after cleaning: the receiver MUST reject the card as `Malformed`; longer than 120 characters: truncated (`human_rendering_shows_the_repo_plan_and_todo_when_present`). |
| any other key | any | nothing | **[MESH-STATUS-024]** The receiver MUST ignore it. |

### 9.6 Todo map

Emission order: `goal`, `done`, `total`.

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `goal` | str | the todo goal, at most `TODO_GOAL_MAX_CHARS` = `280` characters; omitted when none | **[MESH-STATUS-025]** Not a `str`: the receiver MUST reject the card as `Malformed`; absent or `nil`: none; longer than 280 characters: truncated. |
| `done` | uint fitting u32 | items done | **[MESH-STATUS-026]** Missing, `nil` or not a 32-bit count: the receiver MUST reject the card as `Malformed`. |
| `total` | uint fitting u32 | items in total | **[MESH-STATUS-027]** Missing, `nil` or not a 32-bit count: the receiver MUST reject the card as `Malformed`. |
| any other key | any | nothing | **[MESH-STATUS-028]** The receiver MUST ignore it. |

### 9.7 Reserved codes and client errors

| `state.code` | Meaning |
|---|---|
| `0` | unknown |
| `1` | idle |
| `2` | working |

**[MESH-STATUS-029]** A receiver MUST keep a `state.code` it does not know, render it as unknown, and MUST NOT reject the card for it.

**[MESH-STATUS-030]** A receiver MUST keep numeric fields as sent; consumers saturate when they compute with them.

The requester's typed errors are `NotServed` (a dispatch error map, `no_provider` or `unknown_path`, section 6.6), `Malformed`, `UnsupportedVersion` and `Transport`; the requester tries `DispatchError::from_value` before `StatusCard::from_value`.

## 10. /message

A peer message is a request on `MESSAGE_PATH` = `"/message"` (src/mesh/message.rs). The same body travels by store-and-forward as `"coyote.peer/1"` (section 10.8).

### 10.1 Body

Emission order: `v`, `kind`, `id`, `in_reply_to`, `title`, `content`, `fields`, `ts` (`to_r3_body`, `from_r3_body`, src/mesh/message.rs).

A wire id is 1 to `PEER_ID_MAX_CHARS` = `64` bytes, each in `[0-9A-Za-z_.:-]` (`is_wire_id`; `a_wire_id_is_our_uuid_or_another_short_ascii_token_and_nothing_else`).

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `v` | uint | `PEER_WIRE_VERSION` = `1` | **[MESH-MSG-001]** Missing or not equal to 1: the receiver MUST refuse with `InvalidData`. |
| `kind` | text | one of `message`, `ask`, `reply`, `bulletin` | **[MESH-MSG-002]** Missing or any other value: the receiver MUST refuse with `InvalidData`. |
| `id` | text, a wire id | a fresh id; Coyote mints 32 lowercase hex (UUIDv4 simple form) | **[MESH-MSG-003]** Missing or not a wire id: the receiver MUST refuse with `InvalidData`. |
| `in_reply_to` | text, a wire id | the `id` of the message answered; omitted otherwise | **[MESH-MSG-004]** Present and not a wire id: the receiver MUST refuse with `InvalidData`. |
| `title` | text | at most `PEER_TITLE_MAX_CHARS` = `120` characters; omitted when none | **[MESH-MSG-005]** Present and not text, or longer than 120 characters: the receiver MUST refuse with `InvalidData`. |
| `content` | text | at most `PEER_CONTENT_MAX_CHARS` = `4000` characters | **[MESH-MSG-006]** Missing, not text, or longer than 4000 characters: the receiver MUST refuse with `InvalidData`. |
| `fields` | map | a map of depth at most `PEER_FIELDS_MAX_DEPTH` = `8` and at most `PEER_FIELDS_MAX_BYTES` = `4096` bytes when re-serialised as JSON after cleaning (`sanitize_fields`, src/mesh/message.rs); omitted when none | **[MESH-MSG-007]** Present and not a `map`: the receiver MUST refuse with `InvalidData`. **[MESH-MSG-008]** Deeper than 8, or longer than 4096 bytes when re-serialised as JSON after cleaning (`sanitize_fields`, src/mesh/message.rs): the receiver MUST drop `fields` and keep the message. |
| `ts` | f64 | Unix seconds of sending, as an `f64` | **[MESH-MSG-009]** Missing, not a number (`uint`, `int`, `f32` or `f64`), or not finite once read as f64: the receiver MUST refuse with `InvalidData` (`r3_body_round_trips_and_rejects_malformed` accepts a `uint` `ts`). |
| any other key | any | nothing | **[MESH-MSG-010]** The receiver MUST ignore it (`r3_body_round_trips_and_rejects_malformed`). |

**[MESH-MSG-011]** A sender MUST emit the keys in the order given above.

**[MESH-MSG-012]** A receiver MUST validate in the order of the rows above, after first refusing a body that is not a `map` with `InvalidData`; the first failure decides.

**[MESH-MSG-013]** A receiver MUST treat a `nil` value as an absent key.

**[MESH-MSG-014]** A sender MUST mint an `id` that is a wire id and MUST clean and cap every text field before sending (`outbound_refuses_over_long_text_and_bad_fields_and_mints_an_id`, `peer_message_new_caps_and_sanitises_every_peer_string`).

### 10.2 Kinds

| `kind` | Meaning | Envoy |
|---|---|---|
| `message` | informational | answered when it carries no `in_reply_to` |
| `ask` | a question | answered when it carries no `in_reply_to` |
| `reply` | an answer to an `id` named by `in_reply_to` | never answered |
| `bulletin` | a broadcast | never answered |

### 10.3 Reply values

Acknowledgement (`received_reply`; `received_reply_is_recognised_only_for_its_id`):

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `received` | bool | `true` | **[MESH-MSG-015]** Absent or not `true`: the sender MUST NOT read the value as the acknowledgement. |
| `id` | str | the received `id`, echoed | **[MESH-MSG-016]** Absent, not a `str` (a `bin` included, `is_received_reply` reads with `as_str`), or not equal to the `id` sent: the sender MUST NOT read the value as the acknowledgement. |
| any other key | any | nothing | **[MESH-MSG-017]** The receiver MUST ignore it. |

**[MESH-MSG-018]** A receiver MUST answer `InvalidData` (`0xf4`) for every validation failure of section 10.1.

**[MESH-MSG-019]** A receiver MUST answer `Throttled` (`0xf6`) when the sending identity is over its limits (section 10.7), before the message is filed or acknowledged; nothing is delivered (`both_inbound_paths_ask_admission_before_delivering`, `the_message_provider_throttles_the_third_message_in_an_hour_per_identity`).

**[MESH-MSG-020]** A receiver MUST answer with silence when its inbox surface is gone or delivery fails.

**[MESH-MSG-021]** A receiver MUST send the acknowledgement as soon as the message is filed, before any envoy run.

### 10.4 Sender outcome

The sender requests `/message` with `PEER_REQUEST_TIMEOUT` = `15` seconds.

| Result of the direct request | Outcome |
|---|---|
| the acknowledgement of section 10.3 | **[MESH-MSG-022]** The sender MUST treat the message as delivered directly (`Direct`). |
| any other reply value | **[MESH-MSG-023]** The sender MUST report `NotAcknowledged` and MUST NOT fall back. |
| `Timeout`, `LinkFailed` or `LinkClosed` | **[MESH-MSG-024]** The sender MUST fall back to store-and-forward as `"coyote.peer/1"` (section 10.8). |
| any refusal code | **[MESH-MSG-025]** The sender MUST report `Refused` with the code and MUST NOT fall back. |
| a version refusal | **[MESH-MSG-026]** The sender MUST report `IncompatibleVersion` and MUST NOT fall back. |

### 10.5 Correlation

**[MESH-MSG-027]** The `id` of an `ask` or `message` MUST serve as its correlation id: a `reply` names it in `in_reply_to`.

**[MESH-MSG-028]** A receiver MUST close an open question when a `reply` arrives whose `in_reply_to` names that question and whose sending identity equals the asked identity, compared case-insensitively as hex (`Correlations::answer`, `answer_correlation`; `an_ask_is_answered_by_a_reply_that_resolves_the_correlation`).

**[MESH-MSG-029]** A receiver MUST deliver a `reply` that matches no open question as kind `message`, keeping its `in_reply_to` (`a_reply_that_answers_nothing_lands_in_the_inbox_as_a_message_keeping_in_reply_to`).

**[MESH-MSG-030]** A `message` or `ask` that carries `in_reply_to` is an interim notice: the receiver MUST NOT close the question for it and MUST NOT hand it to the envoy (`an_interim_message_naming_our_question_leaves_its_correlation_open`).

**[MESH-MSG-031]** A `reply` to an open question of the receiver MUST NOT count against the per-hour message limit (`a_reply_to_our_open_question_is_never_throttled`).

Open questions are kept for `PENDING_TTL` = `604800` seconds (7 days) in a store of at most `PENDING_MAX_ENTRIES` = `256` questions, answered ones evicted before open ones (src/mesh/pending.rs; `pending_store_survives_reopen_and_prunes_by_ttl_and_cap`).

### 10.6 Envoy contract

What a peer hears after an `ask` or `message` without `in_reply_to` (src/config/mesh_envoy.rs; `over_a_live_link_the_peer_hears_the_answer_the_handoff_and_the_late_reply`):

**[MESH-MSG-032]** The envoy's answer MUST be a `reply` whose `in_reply_to` is the question's `id`, with content of at most 4000 characters.

**[MESH-MSG-033]** When the envoy escalates to the human without an answer, the peer MUST receive a `message` whose `in_reply_to` is the question's `id` and whose content is exactly `escalated to the human; no answer yet (ref <id>)` with `<id>` the question's `id`.

**[MESH-MSG-034]** A later human answer MUST be a `reply` with the same `in_reply_to`.

**[MESH-MSG-035]** A failed run MUST be reported as a `reply` whose content is one of `no answer (timed out)`, `no answer (this node is shutting down)` or `this node cannot answer right now`.

**[MESH-MSG-036]** An envoy run MUST be bounded by `ENVOY_RUN_TIMEOUT_SECS` = `120` seconds; the hold before hand-off (`mesh.envoy_escalation_timeout`, `0` = hand off at once) is capped by that ceiling.

### 10.7 Peer limits

Limits are counted per sending identity over fixed windows of `PEER_WINDOW` = `3600` seconds anchored at the identity's first sighting, for at most `PEER_LIMITS_MAX_IDENTITIES` = `256` identities, the least recently seen idle identity evicted at the cap (src/mesh/limits.rs; `the_cap_evicts_the_least_recently_seen_idle_identity`). Defaults (src/config/mesh_config.rs; `config_maps_the_four_mesh_knobs_and_defaults_match`): `DEFAULT_PEER_MAX_MESSAGES_PER_HOUR` = `60`, `DEFAULT_PEER_MAX_CONCURRENT` = `1`, `DEFAULT_PEER_MAX_TOKENS_PER_HOUR` = `100000`, cost ceiling off (`0.0`). Reservation order is concurrency, token ceiling, then cost ceiling (only when above zero).

Refusal reasons (`RefusalReason`):

| `refusal` | Text | Sent |
|---|---|---|
| `rate_limited` | This node has taken as many messages from you as it accepts in an hour; try again when the hour is up. | yes |
| `envoy_busy` | This node's envoy is busy; try again in a couple of minutes. | yes |
| `envoy_stopping` | No answer: this node is shutting down. | yes |
| `peer_concurrency` | This node is not taking another message from you until the current one finishes. | yes |
| `token_ceiling` | This node has spent as many model tokens on you as it allows in an hour; try again when the hour is up. | yes |
| `cost_ceiling` | This node has spent as much on you as it allows in an hour; try again when the hour is up. | yes |
| `loop_guard` | This node does not answer replies. | never |

Typed refusal `fields` (`PeerRefusal::fields`; `refusal_fields_carry_the_reason_and_a_ceiled_retry_after`):

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `refusal` | str | a reason from the table above | **[MESH-MSG-037]** Absent or unknown: the receiver MUST treat the `reply` as an ordinary reply and MUST NOT reject it. |
| `"retry_after_secs"` | uint | `ceil(retry_after)`, at least `1`; `PEER_RETRY_AFTER_CAPACITY` = `120` for a capacity refusal (`capacity_retry_after_is_the_envoy_run_ceiling`) | **[MESH-MSG-038]** Absent or not a `uint`: the receiver MUST treat the retry hint as absent. |
| any other key | any | nothing | **[MESH-MSG-039]** The receiver MUST ignore it. |

**[MESH-MSG-040]** A typed refusal MUST be carried in a `reply` whose `in_reply_to` is the refused message's `id`, whose content is the reason's text, and whose `fields` is the map above.

The reference emits the typed refusal but does not yet read one: a receiving Coyote files it as an ordinary `reply`.

**[MESH-MSG-041]** A receiver MUST count messages against `mesh.peer_max_messages_per_hour` in fixed windows of `PEER_WINDOW` per sending identity; the `"retry_after_secs"` of `rate_limited` is the remainder of the window (`admit_message_refuses_the_sixty_first_in_an_hour_and_resets_after_rollover`).

**[MESH-MSG-042]** Over a live link, an over-limit message MUST be answered with the bare `Throttled` code and MUST NOT be filed.

**[MESH-MSG-043]** By store-and-forward, an over-limit message MUST be filed in the inbox without an envoy run, and the receiver MUST send at most one typed refusal `reply` per identity per reason per hour (`a_store_and_forward_refusal_is_answered_once_per_identity_per_reason_per_hour`).

**[MESH-MSG-044]** A receiver MUST NOT send a typed refusal in response to a message that itself carries `in_reply_to`.

**[MESH-MSG-045]** The `loop_guard` reason MUST NOT be sent to a peer; it only refuses an envoy job locally.

### 10.8 Peer message over LXMF

`peer_lxmf_message`, `decode_peer_lxmf` (src/mesh/message.rs; `peer_lxmf_round_trips_and_a_knock_is_not_a_peer`). LXMF title = the title's bytes (absent when none), LXMF content = the content's bytes.

LXMF fields map:

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `0xfb` (`FIELD_CUSTOM_TYPE`) | text | `PEER_MESSAGE_TYPE` = `"coyote.peer/1"` | **[MESH-MSG-046]** When absent or not this tag, the receiver MUST NOT treat the message as a peer message. |
| `0xfc` (`FIELD_CUSTOM_DATA`) | map | the custom data map below | **[MESH-MSG-047]** When missing or not a `map`, the receiver MUST drop the message as malformed. |
| any other key | any | nothing | **[MESH-MSG-048]** The receiver MUST ignore it. |

Custom data map (emission order `kind`, `id`, `in_reply_to`, `name_hash`, `fields`):

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `kind` | text | one of `message`, `ask`, `reply`, `bulletin` | **[MESH-MSG-049]** Missing or any other value: the receiver MUST drop the message as malformed. |
| `id` | text, a wire id | the message id | **[MESH-MSG-050]** Missing, blank or not a wire id: the receiver MUST drop the message as malformed. |
| `in_reply_to` | text, a wire id | the answered `id`; omitted otherwise | **[MESH-MSG-051]** Present and not a wire id: the receiver MUST drop the message as malformed. |
| `name_hash` | bin(10) | the sender's own name hash | **[MESH-MSG-052]** Missing, not a `bin` or not 10 bytes: the receiver MUST drop the message as malformed. |
| `fields` | map | the body `fields` map; omitted when none | **[MESH-MSG-053]** The receiver MUST read an absent or `nil` `fields` as none and MUST convert any other value to JSON as `json_from_rmpv` does (`nil` and `ext` become null, `bool` stays, integers and finite floats become numbers, `str` is decoded as lossy UTF-8, `bin` becomes lowercase hex, arrays and maps recurse, a non-`str` map key is rendered as msgpack prints it). **[MESH-MSG-054]** When the value nests deeper than `PEER_FIELDS_MAX_DEPTH` = `8`, the receiver MUST drop `fields` (absent) and keep the message; the sanitising seam (`PeerMessage::new`) then drops a `fields` over `PEER_FIELDS_MAX_BYTES` = `4096` bytes when re-serialised as JSON after cleaning (`sanitize_fields`, src/mesh/message.rs). |
| any other key | any | nothing | **[MESH-MSG-055]** The receiver MUST ignore it. |

**[MESH-MSG-056]** A sender MUST put the title's bytes as the LXMF title (absent when there is no title) and the content's bytes as the LXMF content.

**[MESH-MSG-057]** The receiver MUST decode title and content as lossy UTF-8, then clean and cap them at 120 and 4000 characters; an over-long text is truncated, not refused.

**[MESH-MSG-058]** The receiver MUST compute the sending instance as `trunc_16(H(name_hash || signer_identity_hash))` and authorize it as it would a link request (section 6.6, stage 8); an untrusted instance is dropped (`peer_routing_recomputes_the_source_destination_and_drops_untrusted`).

**[MESH-MSG-059]** The receiver MUST take `ts` from the LXMF timestamp.

## 11. Store-and-forward via LXMF propagation

When a direct request times out or the link fails (MESH-KNOCK-017, MESH-MSG-024), the message is posted to an LXMF propagation node and fetched by the recipient later.

### 11.1 Outbound

`build_signed_message`, `prepare_envelope` (src/mesh/propagation.rs); `post_to_node` (src/mesh/node.rs).

**[MESH-PROP-001]** The LXMF message MUST be addressed to the recipient's delivery hash with the sender's delivery hash as source (MESH-DEST-009).

**[MESH-PROP-002]** The payload MUST be `(timestamp, content, title or empty, fields map or empty map)`, with no stamp.

**[MESH-PROP-003]** A sender MUST refuse to encode `fields` that is not a `map`.

**[MESH-PROP-004]** The message MUST be signed by the sender's identity (object security).

**[MESH-PROP-005]** The transient MUST be `lxmf_data = destination(16) || encrypt_to_recipient(rest)` (upstream `WireMessage::pack_propagation_transient_with_rng`).

**[MESH-PROP-006]** The transient id MUST be `H(lxmf_data)`, 32 bytes.

Propagation envelope, msgpack array `[timestamp, [transient]]`:

| Element | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `[0]` timestamp | f64 | Unix seconds | **[MESH-PROP-007]** A sender MUST put an `f64`; the receiver is the propagation node, whose reading is LXMF's, not fixed here. |
| `[1]` | array of one bin | `[bin(lxmf_data || stamp(32))]` | **[MESH-PROP-008]** A sender MUST put exactly one `bin` holding the transient followed by its 32-byte stamp. |
| any other element | none | nothing | **[MESH-PROP-009]** A sender MUST NOT add elements; how a propagation node treats them is outside this document (the node is the receiver of this envelope). |

**[MESH-PROP-010]** For posting, a sender MUST select the propagation node that is enabled and whose announced stamp cost is at most `MAX_ACCEPTED_STAMP_COST` = `26`, with the fewest hops, ties broken by the most recent sighting (`select_for_posting_skips_disabled_and_expensive_nodes`).

**[MESH-PROP-011]** A sender MUST refuse a stamp cost above 26 before any mining (`StampCostAboveCeiling`; `costs_above_the_ceiling_are_refused_before_any_mining`).

**[MESH-PROP-012]** A sender MUST mine the stamp at the node's announced cost and MUST NOT default the cost (`mesh_module_never_names_the_default_stamp_cost`).

**[MESH-PROP-013]** A sender MUST refuse, before mining, an envelope longer than `per_transfer_limit_kb * 1000` bytes (`Oversize`).

**[MESH-PROP-014]** A sender MUST open the link to the propagation node without identifying.

**[MESH-PROP-015]** The envelope MUST travel as a single packet when it fits the link MDU and as a resource otherwise (`representation_is_a_packet_up_to_the_mdu_and_a_resource_above`).

### 11.2 Node verdict

**[MESH-PROP-016]** After the transfer, silence for `PROPAGATION_REJECT_WINDOW` = `2` seconds MUST be read as accepted.

**[MESH-PROP-017]** A signalling packet whose content is a msgpack array whose first element is one of the eight refusal codes (for example `91 cc f5`) MUST be read as rejected with that code (`refusal_is_read_from_signalling_packets_only`).

**[MESH-PROP-018]** The link closing after the transfer MUST be read as rejected (`ClosedAfterTransfer`).

**[MESH-PROP-019]** The whole transfer MUST be bounded by `PROPAGATION_TRANSFER_TIMEOUT` = `60` seconds.

### 11.3 Inbound fetch

A fetch is three requests on the propagation node's `/get` path over an identified link, each bounded by `FETCH_REQUEST_TIMEOUT` = `60` seconds (src/mesh/propagation_fetch.rs; `request_bodies_match_the_reference_bytes`, `three_rounds_identify_first_and_put_the_reference_bytes_on_the_wire`).

The reference implements this path in `MeshRuntime` (`fetch_propagated`, src/mesh/node.rs) but does not yet drive it from a REPL verb or a scheduler.

**[MESH-PROP-020]** A fetching node MUST identify on the link before the first request.

| Round | Request | Bytes | Node returns | Receiver action |
|---|---|---|---|---|
| 1 | `[nil, nil]` | `92 c0 c0` | an array of transient ids | **[MESH-PROP-021]** The receiver MUST treat anything but an `array` of `bin(32)` as `MalformedList`, and MUST consider only the first `MAX_LISTED_IDS` = `1024` ids (`listed_ids_beyond_the_cap_are_left_for_the_next_fetch`). |
| 2 | `[wants, haves, 240]` | `93 91 <bin32> 91 <bin32> cc f0` for one want and one have | an array of message bodies | **[MESH-PROP-022]** `wants` MUST hold at most `MAX_WANTS_PER_FETCH` = `64` unseen ids and `haves` the listed ids already seen; the third element is `FETCH_TRANSFER_LIMIT_KB` = `240`. **[MESH-PROP-023]** The receiver MUST treat anything but an `array` of `bin` as `MalformedBodies` and MUST consider only the first 64 bodies (`bodies_refuse_anything_but_binaries_and_cap_at_the_wants`). |
| 3 | `[nil, haves]` | `92 c0 91 <bin32>` for one have | nothing read | **[MESH-PROP-024]** `haves` MUST hold the id of every body recorded in this round (section 11.4); a body left unrecorded is not acknowledged. |

**[MESH-PROP-025]** A node refusal MUST be read as: `NoIdentity` is `NodeSawNoIdentity`, `NoAccess` is `NodeRefusedAccess`, any other code is `NodeRefused(code)`.

**[MESH-PROP-026]** A node MAY fetch at any time; the cadence is not fixed by this specification.

**[MESH-PROP-027]** A node MUST run at most one fetch at a time per identity (a runtime lock and a cross-process file lock in the reference).

### 11.4 Body pipeline

Each body of round 2 runs the stages below in order; the first that fails decides (`Discard`, src/mesh/propagation_fetch.rs). "Recorded" means the body is added to the dedup set and its id acknowledged in round 3.

| Stage | Check | On failure | Recorded |
|---|---|---|---|
| 1 | `MIN_FETCHED_MESSAGE_BYTES` = `112` <= length <= `MAX_FETCHED_MESSAGE_BYTES` = `131072` | **[MESH-PROP-028]** The receiver MUST discard it as `Undersize` or `Oversize` (`bounds_leave_room_under_the_transport_and_response_caps`). | no |
| 2 | `H(body)` is not in the transient dedup set | **[MESH-PROP-029]** The receiver MUST discard it as `Duplicate`. | no |
| 3 | the first 16 bytes equal the receiver's delivery hash | **[MESH-PROP-030]** The receiver MUST discard it as `Undecryptable` (`destination is not ours`). | yes |
| 4 | the remainder decrypts | **[MESH-PROP-031]** The receiver MUST discard it as `Undecryptable`. | yes |
| 5 | message id `H(destination || source || payload_without_stamp)` is not in the delivered dedup set | **[MESH-PROP-032]** The receiver MUST discard it as `Duplicate`. | no |
| 6 | the source's public key resolves (transport announce cache, then the peer table by delivery hash) | **[MESH-PROP-033]** The receiver MUST defer it as `UnknownSource`: not acknowledged, not recorded. **[MESH-PROP-034]** On the sighting after `MAX_UNKNOWN_SOURCE_DEFERRALS` = `3` deferrals the receiver MUST record, acknowledge and drop it (`UnknownSourceBudgetSpent`; `an_unknown_source_is_deferred_three_times_then_given_up_on`). | no, then yes |
| 7 | the signature verifies | **[MESH-PROP-035]** The receiver MUST discard it as `BadSignature`. | yes |
| 8 | the signer's standing is `Trusted` | **[MESH-PROP-036]** `Unknown` MUST be discarded as `UntrustedSource`. **[MESH-PROP-037]** `Blocked` MUST be discarded as `BlockedSource` (`a_blocked_signer_is_discarded_and_the_stamp_line_is_logged_for_a_trusted_one`). | yes |
| 9 | routing | **[MESH-PROP-038]** The receiver MUST route in this order: knock (`"coyote.knock/1"`, section 8.6), then peer message (`"coyote.peer/1"`, section 10.8), then the ordinary inbox. | yes |

**[MESH-PROP-039]** The deferral table MUST hold at most `MAX_DEFERRED_IDS` = `256` ids, the longest deferred evicted past the cap (`deferrals_evict_the_longest_deferred_past_capacity_and_log_it`).

**[MESH-PROP-040]** Each dedup set MUST hold at most `DEDUP_CAPACITY` = `4096` ids, the oldest evicted past capacity, over a horizon of `DEDUP_HORIZON` = `15552000` seconds (180 days, inclusive), and MUST be persisted (`dedup_evicts_the_oldest_past_capacity_and_logs_it`).

**[MESH-PROP-041]** The delivery stamp cost demanded of a fetched body MUST be `REQUIRED_DELIVERY_STAMP_COST` = `0`; fetched bodies carry no propagation stamp.

**[MESH-PROP-042]** Payload text MUST be treated as untrusted structure until the sanitising seam (`display_text`, section 3.2).

## 12. Extensibility

The rules below summarise behaviour the per-field tables carry; the tables govern.

**[MESH-EXT-001]** A receiver MUST ignore an unknown key in every map this document defines: the Envelope (MESH-ENV-017), the card and its sub-maps (MESH-STATUS-013, MESH-STATUS-019, MESH-STATUS-022, MESH-STATUS-024, MESH-STATUS-028), the message body (MESH-MSG-010), the knock body (MESH-KNOCK-002), the LXMF custom data (MESH-KNOCK-024, MESH-MSG-055).

**[MESH-EXT-002]** An unknown `kind` MUST be fatal for that message alone: `InvalidData` over a link (MESH-MSG-002), dropped as malformed by store-and-forward (MESH-MSG-049).

**[MESH-EXT-003]** An unknown `state.code` MUST be kept (MESH-STATUS-029).

**[MESH-EXT-004]** An unknown refusal `uint` MUST be read as a reply value (MESH-ENV-047).

**[MESH-EXT-005]** An unknown request path MUST yield the `unknown_path` map to an allowed requester and the section 6.6 outcome for its standing to any other (MESH-ENV-034).

**[MESH-EXT-006]** An unknown protocol version MUST be handled per section 7.

**[MESH-EXT-007]** A new field MUST be added as a new key and MUST NOT change the meaning of an existing key.

**[MESH-EXT-008]** A schema version (`STATUS_CARD_VERSION`, `PEER_WIRE_VERSION`, the LXMF type tags) MUST be bumped only for an incompatible change.

## 13. Code-point immutability

**[MESH-CODE-001]** The semantics of an allocated code point (a request path, a refusal code value, a map key, a `kind` name, a `refusal` reason string, a `state.code` value, an LXMF type tag, an `error` string, a version number) MUST NOT change once published.

**[MESH-CODE-002]** New semantics MUST use a new code point.

Registry of code points allocated by this document:

| Code point | Kind | Value | Section |
|---|---|---|---|
| `"/knock"` | request path | `KNOCK_PATH` | 8 |
| `"/status"` | request path | `STATUS_PATH` | 9 |
| `"/message"` | request path | `MESSAGE_PATH` | 10 |
| `"/get"` | request path, propagation node | | 11.3 |
| `NoIdentity` | refusal code | `0xf0` | 6.7 |
| `NoAccess` | refusal code | `0xf1` | 6.7 |
| `InvalidKey` | refusal code | `0xf3` | 6.7 |
| `InvalidData` | refusal code | `0xf4` | 6.7 |
| `InvalidStamp` | refusal code | `0xf5` | 6.7 |
| `Throttled` | refusal code | `0xf6` | 6.7 |
| `NotFound` | refusal code | `0xfd` | 6.7 |
| `Timeout` | refusal code | `0xfe` | 6.7 |
| `v`, `name_hash`, `body` | map key, Envelope | | 6.5 |
| `refusal`, `found`, `min`, `max` | map key, version refusal | | 7 |
| `"unsupported_version"` | `refusal` value | | 7 |
| `error`, `path_hash`, `path` | map key, dispatch error | | 6.6 |
| `"unknown_path"`, `"no_provider"` | `error` string | | 6.6 |
| `intro` | map key, knock body | | 8.1 |
| `name_hash` | map key, LXMF custom data | | 8.6, 10.8 |
| `v`, `display_name`, `objective`, `state`, `repo`, `plan`, `todo`, `"snapshot_age_secs"`, `"served_at_secs"` | map key, card | | 9.2 |
| `code`, `since_secs` | map key, state | | 9.3 |
| `name`, `branch` | map key, repo | | 9.4 |
| `title` | map key, plan | | 9.5 |
| `goal`, `done`, `total` | map key, todo | | 9.6 |
| `0`, `1`, `2` | `state.code` value | unknown, idle, working | 9.7 |
| `v`, `kind`, `id`, `in_reply_to`, `title`, `content`, `fields`, `ts` | map key, message body | | 10.1 |
| `message`, `ask`, `reply`, `bulletin` | `kind` name | | 10.2 |
| `received`, `id` | map key, acknowledgement | | 10.3 |
| `refusal`, `"retry_after_secs"` | map key, typed refusal | | 10.7 |
| `rate_limited`, `envoy_busy`, `envoy_stopping`, `peer_concurrency`, `token_ceiling`, `cost_ceiling`, `loop_guard` | `refusal` reason | | 10.7 |
| `"coyote.knock/1"` | LXMF type tag | `KNOCK_TYPE` | 8.6 |
| `"coyote.peer/1"` | LXMF type tag | `PEER_MESSAGE_TYPE` | 10.8 |
| `0xfb`, `0xfc` | LXMF field key | `FIELD_CUSTOM_TYPE`, `FIELD_CUSTOM_DATA` | 8.6, 10.8 |
| `"COYM"` | announce magic | `ANNOUNCE_MAGIC` | 5.1 |
| `1` | protocol version | `MESH_PROTOCOL_VERSION` | 7 |
| `1` | card schema version | `STATUS_CARD_VERSION` | 9.2 |
| `1` | message schema version | `PEER_WIRE_VERSION` | 10.1 |

## 14. Requirement-id stability

An id, once published, is never renumbered and never reused. A retired requirement keeps its id; its body text is replaced by `[RETIRED]` and its index entry is kept with the same marker. A new requirement takes the next unused number in its area, wherever it lands in the document. A reference to an id is written plain (`MESH-MSG-007`); the bold-bracket form appears only at the definition.

## 15. Constants

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `MESH_PROTOCOL_VERSION` | `1` | src/mesh/protocol.rs | protocol_version_constants_are_pinned |
| `MESH_PROTOCOL_MIN_SUPPORTED` | `1` | src/mesh/protocol.rs | protocol_version_constants_are_pinned |
| `NAME_HASH_LEN` | `10` | src/mesh/r3/frame.rs (re-export of rns_transport NAME_HASH_LENGTH) | envelope_round_trips_and_rejects_anything_that_names_no_origin |
| `ADDRESS_HASH_SIZE` | `16` | rns_transport hash.rs (upstream) | request_frame_layout_is_fixed_width_apart_from_the_body |
| `MAX_R3_PAYLOAD_BYTES` | `262144` | src/mesh/r3/frame.rs | oversized_request_resource_is_dropped_before_the_handler_runs |
| `KNOCK_PATH` | `"/knock"` | src/mesh/r3/dispatch.rs | spec_pins (this table) |
| `STATUS_PATH` | `"/status"` | src/mesh/r3/dispatch.rs | request_frame_matches_upstream_link_request_byte_for_byte |
| `MESSAGE_PATH` | `"/message"` | src/mesh/r3/dispatch.rs | spec_pins (this table) |
| `DEFAULT_REQUEST_TIMEOUT` | `30` | src/mesh/r3/client.rs | spec_pins (this table) |
| `DEFAULT_LINK_TIMEOUT` | `10` | src/mesh/r3/client.rs | spec_pins (this table) |
| `DEFAULT_RESPONSE_SEND_TIMEOUT` | `10` | src/mesh/r3/server.rs | spec_pins (this table) |
| `PEER_RESOLVE_TIMEOUT` | `2` | src/mesh/r3/server.rs | spec_pins (this table) |
| `HANDLER_TIMEOUT` | `20` | src/mesh/r3/server.rs | spec_pins (this table) |
| `MAX_CONCURRENT_INBOUND_REQUESTS` | `16` | src/mesh/r3/server.rs | requests_beyond_the_handler_slots_are_dropped_silently |
| `RefusalCode::NoIdentity` | `0xf0` | src/mesh/r3/error.rs | refusal_codes_round_trip_the_wire_and_reject_other_values |
| `RefusalCode::NoAccess` | `0xf1` | src/mesh/r3/error.rs | refusal_codes_round_trip_the_wire_and_reject_other_values |
| `RefusalCode::InvalidKey` | `0xf3` | src/mesh/r3/error.rs | refusal_codes_round_trip_the_wire_and_reject_other_values |
| `RefusalCode::InvalidData` | `0xf4` | src/mesh/r3/error.rs | refusal_codes_round_trip_the_wire_and_reject_other_values |
| `RefusalCode::InvalidStamp` | `0xf5` | src/mesh/r3/error.rs | refusal_codes_round_trip_the_wire_and_reject_other_values |
| `RefusalCode::Throttled` | `0xf6` | src/mesh/r3/error.rs | refusal_codes_round_trip_the_wire_and_reject_other_values |
| `RefusalCode::NotFound` | `0xfd` | src/mesh/r3/error.rs | refusal_codes_round_trip_the_wire_and_reject_other_values |
| `RefusalCode::Timeout` | `0xfe` | src/mesh/r3/error.rs | refusal_codes_round_trip_the_wire_and_reject_other_values |
| `ANNOUNCE_MAGIC` | `"COYM"` | src/mesh/announce.rs | announce_constants_are_pinned |
| `MAX_DISPLAY_NAME_BYTES` | `64` | src/mesh/announce.rs | announce_constants_are_pinned |
| `REANNOUNCE_FLOOR_SECS` | `300` | src/mesh/announce.rs | announce_constants_are_pinned |
| `HEARTBEAT_SECS` | `900` | src/mesh/announce.rs | announce_constants_are_pinned |
| `PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT` | `3` | src/mesh/announce.rs | announce_constants_are_pinned |
| `PEER_TTL` | `2700` | src/mesh/peers.rs | ttl_is_three_heartbeats |
| `PEER_STALE_AFTER` | `1800` | src/mesh/peers.rs | stale_is_two_heartbeats_and_never_for_a_future_sighting |
| `PEER_TABLE_MAX_ENTRIES` | `1024` | src/mesh/peers.rs | ttl_is_three_heartbeats |
| `KNOCK_TYPE` | `"coyote.knock/1"` | src/mesh/knock.rs | lxmf_knock_wire_shape_is_exactly_the_typed_two_field_layout |
| `KNOCK_INTRO_MAX_CHARS` | `200` | src/mesh/knocks.rs | intro_is_refused_over_the_cap_and_cleaned_under_it |
| `KNOCK_REQUEST_TIMEOUT` | `15` | src/mesh/knock.rs | spec_pins (this table) |
| `KNOCK_LINK_TIMEOUT` | `10` | src/mesh/knock.rs | spec_pins (this table) |
| `KNOCK_GATE_MAX_IDENTITIES` | `256` | src/mesh/knock.rs | the_gate_forgets_the_least_recently_seen_identity_past_its_cap |
| `KNOCK_BUCKET_BURST` | `3` | src/mesh/knock.rs | one_identity_is_rate_limited_per_identity_and_surfaced_once |
| `KNOCK_BUCKET_REFILL_INTERVAL` | `600` | src/mesh/knock.rs | the_bucket_refills_one_token_per_interval_and_never_past_the_burst |
| `KNOCK_CACHE_MAX_ENTRIES` | `256` | src/mesh/knocks.rs | append_caps_the_cache_at_max_entries_newest_wins |
| `KNOCK_CACHE_MAX_PER_IDENTITY` | `16` | src/mesh/knocks.rs | append_keeps_only_the_newest_per_identity_and_never_warns_for_it |
| `STATUS_CARD_VERSION` | `1` | src/mesh/card.rs | version_is_one_and_newer_or_missing_versions_are_refused_by_name |
| `STATE_UNKNOWN` | `0` | src/mesh/card.rs | unknown_keys_are_ignored_and_unknown_state_codes_are_kept |
| `STATE_IDLE` | `1` | src/mesh/card.rs | unknown_keys_are_ignored_and_unknown_state_codes_are_kept |
| `STATE_WORKING` | `2` | src/mesh/card.rs | unknown_keys_are_ignored_and_unknown_state_codes_are_kept |
| `DISPLAY_NAME_MAX_CHARS` | `64` | src/mesh/card.rs | decoding_sanitises_and_caps_peer_text_and_refuses_a_blank_required_string |
| `OBJECTIVE_MAX_CHARS` | `280` | src/mesh/card.rs | decoding_sanitises_and_caps_peer_text_and_refuses_a_blank_required_string |
| `REPO_NAME_MAX_CHARS` | `64` | src/mesh/card.rs | human_rendering_shows_the_repo_plan_and_todo_when_present |
| `BRANCH_MAX_CHARS` | `64` | src/mesh/card.rs | human_rendering_shows_the_repo_plan_and_todo_when_present |
| `PLAN_TITLE_MAX_CHARS` | `120` | src/mesh/card.rs | human_rendering_shows_the_repo_plan_and_todo_when_present |
| `TODO_GOAL_MAX_CHARS` | `280` | src/mesh/card.rs | human_rendering_shows_the_repo_plan_and_todo_when_present |
| `PEER_MESSAGE_TYPE` | `"coyote.peer/1"` | src/mesh/message.rs | peer_lxmf_round_trips_and_a_knock_is_not_a_peer |
| `PEER_WIRE_VERSION` | `1` | src/mesh/message.rs | r3_body_round_trips_and_rejects_malformed |
| `PEER_TITLE_MAX_CHARS` | `120` | src/mesh/message.rs | outbound_refuses_over_long_text_and_bad_fields_and_mints_an_id |
| `PEER_CONTENT_MAX_CHARS` | `4000` | src/mesh/message.rs | outbound_refuses_over_long_text_and_bad_fields_and_mints_an_id |
| `PEER_ID_MAX_CHARS` | `64` | src/mesh/message.rs | a_wire_id_is_our_uuid_or_another_short_ascii_token_and_nothing_else |
| `PEER_FIELDS_MAX_BYTES` | `4096` | src/mesh/message.rs | peer_message_new_caps_and_sanitises_every_peer_string |
| `PEER_FIELDS_MAX_DEPTH` | `8` | src/mesh/message.rs | r3_body_round_trips_and_rejects_malformed |
| `PEER_REQUEST_TIMEOUT` | `15` | src/mesh/message.rs | spec_pins (this table) |
| `PEER_LINK_TIMEOUT` | `10` | src/mesh/message.rs | spec_pins (this table) |
| `PENDING_TTL` | `604800` | src/mesh/pending.rs | pending_store_survives_reopen_and_prunes_by_ttl_and_cap |
| `PENDING_MAX_ENTRIES` | `256` | src/mesh/pending.rs | pending_store_survives_reopen_and_prunes_by_ttl_and_cap |
| `PEER_WINDOW` | `3600` | src/mesh/limits.rs | admit_message_refuses_the_sixty_first_in_an_hour_and_resets_after_rollover |
| `PEER_RETRY_AFTER_CAPACITY` | `120` | src/mesh/limits.rs | capacity_retry_after_is_the_envoy_run_ceiling |
| `PEER_LIMITS_MAX_IDENTITIES` | `256` | src/mesh/limits.rs | the_cap_evicts_the_least_recently_seen_idle_identity |
| `DEFAULT_PEER_MAX_CONCURRENT` | `1` | src/config/mesh_config.rs | config_maps_the_four_mesh_knobs_and_defaults_match |
| `DEFAULT_PEER_MAX_MESSAGES_PER_HOUR` | `60` | src/config/mesh_config.rs | config_maps_the_four_mesh_knobs_and_defaults_match |
| `DEFAULT_PEER_MAX_TOKENS_PER_HOUR` | `100000` | src/config/mesh_config.rs | config_maps_the_four_mesh_knobs_and_defaults_match |
| `ENVOY_RUN_TIMEOUT_SECS` | `120` | src/config/mesh_envoy.rs | capacity_retry_after_is_the_envoy_run_ceiling |
| `MAX_ACCEPTED_STAMP_COST` | `26` | src/mesh/propagation.rs | from_announce_refuses_negative_costs_and_files_any_other |
| `PROPAGATION_TRANSFER_TIMEOUT` | `60` | src/mesh/propagation.rs | spec_pins (this table) |
| `PROPAGATION_REJECT_WINDOW` | `2` | src/mesh/propagation.rs | spec_pins (this table) |
| `FETCH_REQUEST_TIMEOUT` | `60` | src/mesh/propagation_fetch.rs | spec_pins (this table) |
| `MAX_LISTED_IDS` | `1024` | src/mesh/propagation_fetch.rs | listed_ids_beyond_the_cap_are_left_for_the_next_fetch |
| `MAX_WANTS_PER_FETCH` | `64` | src/mesh/propagation_fetch.rs | bodies_refuse_anything_but_binaries_and_cap_at_the_wants |
| `FETCH_TRANSFER_LIMIT_KB` | `240` | src/mesh/propagation_fetch.rs | request_bodies_match_the_reference_bytes |
| `MAX_FETCHED_MESSAGE_BYTES` | `131072` | src/mesh/propagation_fetch.rs | bounds_leave_room_under_the_transport_and_response_caps |
| `MIN_FETCHED_MESSAGE_BYTES` | `112` | src/mesh/propagation_fetch.rs | bounds_leave_room_under_the_transport_and_response_caps |
| `DEDUP_CAPACITY` | `4096` | src/mesh/propagation_fetch.rs | dedup_evicts_the_oldest_past_capacity_and_logs_it |
| `DEDUP_HORIZON` | `15552000` | src/mesh/propagation_fetch.rs | bounds_leave_room_under_the_transport_and_response_caps |
| `MAX_UNKNOWN_SOURCE_DEFERRALS` | `3` | src/mesh/propagation_fetch.rs | an_unknown_source_is_deferred_three_times_then_given_up_on |
| `MAX_DEFERRED_IDS` | `256` | src/mesh/propagation_fetch.rs | deferrals_evict_the_longest_deferred_past_capacity_and_log_it |
| `REQUIRED_DELIVERY_STAMP_COST` | `0` | src/mesh/propagation_fetch.rs | a_blocked_signer_is_discarded_and_the_stamp_line_is_logged_for_a_trusted_one |
| `PROPAGATION_NODE_TABLE_MAX_ENTRIES` | `32` | src/mesh/propagation_nodes.rs | cap_evicts_the_least_recently_heard_and_logs_it |
| `FIELD_CUSTOM_TYPE` | `0xfb` | lxmf_core constants.rs (upstream) | lxmf_knock_wire_shape_is_exactly_the_typed_two_field_layout |
| `FIELD_CUSTOM_DATA` | `0xfc` | lxmf_core constants.rs (upstream) | lxmf_knock_wire_shape_is_exactly_the_typed_two_field_layout |

## 16. Requirements index

- [MESH-CANON-001](#31-hashes-as-text) -- hashes written as lowercase hex
- [MESH-CANON-002](#31-hashes-as-text) -- hash text accepted only as 32 hex digits, lowercased
- [MESH-CANON-003](#31-hashes-as-text) -- trust store keys are lowercase hex
- [MESH-CANON-004](#31-hashes-as-text) -- identity hashes compared byte-wise
- [MESH-CANON-005](#32-peer-text) -- peer text cleaned before display or comparison
- [MESH-CANON-006](#34-msgpack) -- shortest uint encoding on send
- [MESH-CANON-007](#34-msgpack) -- any uint width accepted on receipt
- [MESH-CANON-008](#34-msgpack) -- stated map key emission orders
- [MESH-CANON-009](#34-msgpack) -- map keys looked up by name
- [MESH-CANON-010](#34-msgpack) -- text fields accept str or bin
- [MESH-CANON-011](#34-msgpack) -- uint accepted in either msgpack integer family
- [MESH-CANON-012](#34-msgpack) -- duplicate keys, last wins in the Envelope and fields, first elsewhere
- [MESH-CANON-013](#34-msgpack) -- no key emitted twice
- [MESH-DEST-001](#4-destination-naming) -- identity hash derivation
- [MESH-DEST-002](#4-destination-naming) -- instance id form and lifetime
- [MESH-DEST-003](#4-destination-naming) -- destination name
- [MESH-DEST-004](#4-destination-naming) -- name hash derivation
- [MESH-DEST-005](#4-destination-naming) -- destination hash derivation
- [MESH-DEST-006](#4-destination-naming) -- binding rule for attributing a destination
- [MESH-DEST-007](#4-destination-naming) -- forged name hash trusts nothing
- [MESH-DEST-008](#4-destination-naming) -- a peer names only its own instances
- [MESH-DEST-009](#4-destination-naming) -- LXMF delivery hashes as source and destination
- [MESH-DEST-010](#4-destination-naming) -- propagation node recognised by name hash
- [MESH-ANN-001](#51-application-data) -- short or wrong-magic app data is not a Coyote announce
- [MESH-ANN-002](#51-application-data) -- every version recorded, out-of-window marked
- [MESH-ANN-003](#51-application-data) -- invalid display name ignores the announce
- [MESH-ANN-004](#51-application-data) -- empty name is no name
- [MESH-ANN-005](#51-application-data) -- everything after offset 6 is the name
- [MESH-ANN-006](#51-application-data) -- sender-side display name limits
- [MESH-ANN-007](#51-application-data) -- variation selectors pass the wire
- [MESH-ANN-008](#52-display-name-policy-and-receiver-record) -- display name only when configured
- [MESH-ANN-009](#52-display-name-policy-and-receiver-record) -- display name withheld on public interfaces
- [MESH-ANN-010](#52-display-name-policy-and-receiver-record) -- what a receiver records per announce
- [MESH-ANN-011](#52-display-name-policy-and-receiver-record) -- announce re-judges compatibility
- [MESH-TIME-001](#53-timing) -- first announce at start
- [MESH-TIME-002](#53-timing) -- heartbeat interval
- [MESH-TIME-003](#53-timing) -- re-announce floor
- [MESH-TIME-004](#53-timing) -- peer age-out
- [MESH-TIME-005](#53-timing) -- peer stale mark
- [MESH-TIME-006](#53-timing) -- sweep cadence
- [MESH-TIME-007](#53-timing) -- peer table cap
- [MESH-ANN-012](#54-propagation-node-announces) -- propagation node table takes only lxmf.propagation
- [MESH-ANN-013](#54-propagation-node-announces) -- undecodable or non-array app data refuses the announce
- [MESH-ANN-033](#54-propagation-node-announces) -- bytes after the array are ignored
- [MESH-ANN-014](#54-propagation-node-announces) -- fewer than 7 elements refuses the announce
- [MESH-ANN-015](#54-propagation-node-announces) -- slot 0 ignored
- [MESH-ANN-016](#54-propagation-node-announces) -- non-integer timebase refuses the announce
- [MESH-ANN-017](#54-propagation-node-announces) -- enabled slot
- [MESH-ANN-018](#54-propagation-node-announces) -- non-bool enabled slot refuses the announce
- [MESH-ANN-019](#54-propagation-node-announces) -- per-transfer limit slot
- [MESH-ANN-020](#54-propagation-node-announces) -- non-integer per-transfer limit refuses the announce
- [MESH-ANN-021](#54-propagation-node-announces) -- negative per-transfer limit refuses the announce
- [MESH-ANN-022](#54-propagation-node-announces) -- non-integer per-sync limit refuses the announce
- [MESH-ANN-023](#54-propagation-node-announces) -- stamp costs not three integers refuses the announce
- [MESH-ANN-024](#54-propagation-node-announces) -- stamp cost slot
- [MESH-ANN-025](#54-propagation-node-announces) -- negative stamp cost refused
- [MESH-ANN-026](#54-propagation-node-announces) -- stamp cost above u32 refused
- [MESH-ANN-027](#54-propagation-node-announces) -- any other cost filed
- [MESH-ANN-028](#54-propagation-node-announces) -- non-map metadata refuses the announce
- [MESH-ANN-029](#54-propagation-node-announces) -- elements beyond 6 ignored
- [MESH-ANN-030](#54-propagation-node-announces) -- propagation node table cap
- [MESH-ANN-031](#54-propagation-node-announces) -- propagation nodes never age out
- [MESH-ANN-032](#54-propagation-node-announces) -- fetch node selection
- [MESH-ENV-001](#61-request-frame) -- request time element type
- [MESH-ENV-002](#61-request-frame) -- request path hash element type
- [MESH-ENV-003](#61-request-frame) -- request frame arity
- [MESH-ENV-004](#61-request-frame) -- no trailing bytes
- [MESH-ENV-005](#62-response-frame) -- response request id
- [MESH-ENV-006](#62-response-frame) -- response value decoding order
- [MESH-ENV-007](#62-response-frame) -- malformed, unmatched or misrouted responses dropped
- [MESH-ENV-008](#63-size-branches) -- packet branch
- [MESH-ENV-009](#63-size-branches) -- resource branch
- [MESH-ENV-010](#63-size-branches) -- size branch in both directions
- [MESH-ENV-011](#63-size-branches) -- outbound payload cap
- [MESH-ENV-012](#64-identity) -- requests travel on identified links
- [MESH-ENV-013](#64-identity) -- anonymous requests hear silence
- [MESH-ENV-014](#65-the-envelope) -- Envelope version field
- [MESH-ENV-015](#65-the-envelope) -- Envelope name hash field
- [MESH-ENV-016](#65-the-envelope) -- Envelope body field
- [MESH-ENV-017](#65-the-envelope) -- Envelope unknown keys
- [MESH-ENV-018](#65-the-envelope) -- Envelope key order
- [MESH-ENV-019](#65-the-envelope) -- duplicate keys, last wins
- [MESH-ENV-020](#65-the-envelope) -- version judged first
- [MESH-ENV-021](#65-the-envelope) -- non-map data answered NoAccess
- [MESH-ENV-022](#65-the-envelope) -- requester instance recomputed
- [MESH-ENV-023](#65-the-envelope) -- no peer-supplied destination
- [MESH-ENV-024](#66-dispatch-order) -- stage 1, oversize frame dropped
- [MESH-ENV-025](#66-dispatch-order) -- stage 2, handler slots busy
- [MESH-ENV-026](#66-dispatch-order) -- stage 3, no identity
- [MESH-ENV-027](#66-dispatch-order) -- stage 4, unknown or blocked standing
- [MESH-ENV-028](#66-dispatch-order) -- stage 5, undecodable frame
- [MESH-ENV-029](#66-dispatch-order) -- stage 6, version refusal
- [MESH-ENV-030](#66-dispatch-order) -- stage 7, malformed Envelope
- [MESH-ENV-031](#66-dispatch-order) -- stage 8a, identity blocked
- [MESH-ENV-032](#66-dispatch-order) -- stage 8b, default closed files a knock
- [MESH-ENV-033](#66-dispatch-order) -- stage 8c, other refusing rule
- [MESH-ENV-034](#66-dispatch-order) -- stage 9a, unknown path
- [MESH-ENV-035](#66-dispatch-order) -- stage 9b, no provider
- [MESH-ENV-036](#66-dispatch-order) -- stage 9c, handler reply
- [MESH-ENV-037](#66-dispatch-order) -- stage 10, handler timeout
- [MESH-ENV-038](#66-dispatch-order) -- refusals byte-identical
- [MESH-ENV-039](#66-dispatch-order) -- trust rule precedence
- [MESH-ENV-040](#66-dispatch-order) -- dispatch error map recognition
- [MESH-ENV-041](#66-dispatch-order) -- unknown_path needs a 32-hex path_hash
- [MESH-ENV-042](#66-dispatch-order) -- no_provider needs a known path
- [MESH-ENV-043](#66-dispatch-order) -- dispatch error unknown keys
- [MESH-ENV-044](#67-refusal-codes-and-client-decoding) -- refusal is a bare uint
- [MESH-ENV-045](#67-refusal-codes-and-client-decoding) -- codes Coyote builds
- [MESH-ENV-046](#67-refusal-codes-and-client-decoding) -- client decode order
- [MESH-ENV-047](#67-refusal-codes-and-client-decoding) -- unknown uint is a reply value
- [MESH-TIME-008](#68-timeouts) -- request timeout outcome
- [MESH-TIME-009](#68-timeouts) -- link timeout outcome
- [MESH-TIME-010](#68-timeouts) -- handler abandoned at its timeout
- [MESH-VER-001](#7-version-negotiation) -- version is a 16-bit unsigned integer
- [MESH-VER-002](#7-version-negotiation) -- supported window
- [MESH-VER-003](#7-version-negotiation) -- sender puts its own version
- [MESH-VER-004](#7-version-negotiation) -- Envelope v is mandatory in version 1
- [MESH-VER-005](#7-version-negotiation) -- missing v is never read as 1
- [MESH-VER-006](#7-version-negotiation) -- refusal key recognition
- [MESH-VER-007](#7-version-negotiation) -- found key
- [MESH-VER-008](#7-version-negotiation) -- min key
- [MESH-VER-009](#7-version-negotiation) -- max key
- [MESH-VER-010](#7-version-negotiation) -- version refusal unknown keys
- [MESH-VER-011](#7-version-negotiation) -- version refusal key order
- [MESH-VER-012](#7-version-negotiation) -- when the version refusal is emitted
- [MESH-VER-013](#7-version-negotiation) -- when a refusal marks a peer incompatible
- [MESH-VER-014](#7-version-negotiation) -- outbound to an incompatible peer refused locally
- [MESH-KNOCK-001](#81-request-body) -- knock intro field
- [MESH-KNOCK-002](#81-request-body) -- knock body unknown keys
- [MESH-KNOCK-003](#81-request-body) -- sender intro cap
- [MESH-KNOCK-004](#81-request-body) -- sender cleans the intro
- [MESH-KNOCK-005](#82-reply) -- admitted knock answered NoAccess
- [MESH-KNOCK-006](#82-reply) -- rate-limited knock answered NoAccess, not surfaced
- [MESH-KNOCK-007](#82-reply) -- trusted caller answered nil
- [MESH-KNOCK-008](#82-reply) -- denied destination answered NoAccess
- [MESH-KNOCK-009](#82-reply) -- blocked or unknown identity hears silence
- [MESH-KNOCK-010](#83-gate) -- per-identity token bucket
- [MESH-KNOCK-011](#83-gate) -- gate identity cap
- [MESH-KNOCK-012](#83-gate) -- surfaced once per identity per process
- [MESH-KNOCK-013](#83-gate) -- knock cache cap
- [MESH-KNOCK-014](#83-gate) -- knock cache per-identity cap
- [MESH-KNOCK-015](#84-knock-record-and-hint) -- name hash filed with the record
- [MESH-KNOCK-016](#85-sender-outcome) -- reply or NoAccess lands directly
- [MESH-KNOCK-017](#85-sender-outcome) -- transport failure falls back
- [MESH-KNOCK-018](#85-sender-outcome) -- other refusal is a direct failure
- [MESH-KNOCK-019](#85-sender-outcome) -- no propagation node known
- [MESH-KNOCK-020](#86-knock-over-lxmf) -- LXMF knock type tag
- [MESH-KNOCK-021](#86-knock-over-lxmf) -- LXMF knock custom data
- [MESH-KNOCK-022](#86-knock-over-lxmf) -- LXMF knock other fields
- [MESH-KNOCK-023](#86-knock-over-lxmf) -- LXMF knock name hash
- [MESH-KNOCK-024](#86-knock-over-lxmf) -- LXMF knock custom data unknown keys
- [MESH-KNOCK-025](#86-knock-over-lxmf) -- LXMF knock title absent
- [MESH-KNOCK-026](#86-knock-over-lxmf) -- LXMF knock content is the intro
- [MESH-KNOCK-027](#86-knock-over-lxmf) -- LXMF knock content cleaned
- [MESH-KNOCK-028](#86-knock-over-lxmf) -- knocking instance from signer and name hash
- [MESH-KNOCK-029](#86-knock-over-lxmf) -- blocked knocker discarded by store-and-forward
- [MESH-STATUS-001](#91-request-and-reply) -- status request body ignored
- [MESH-STATUS-002](#91-request-and-reply) -- no store-and-forward for status
- [MESH-STATUS-003](#91-request-and-reply) -- card served only to allowed requesters
- [MESH-STATUS-004](#92-card) -- card version
- [MESH-STATUS-005](#92-card) -- card display name
- [MESH-STATUS-006](#92-card) -- card objective
- [MESH-STATUS-007](#92-card) -- card state
- [MESH-STATUS-008](#92-card) -- card repo
- [MESH-STATUS-009](#92-card) -- card plan
- [MESH-STATUS-010](#92-card) -- card todo
- [MESH-STATUS-011](#92-card) -- card snapshot age
- [MESH-STATUS-012](#92-card) -- card served-at time
- [MESH-STATUS-013](#92-card) -- card unknown keys
- [MESH-STATUS-014](#92-card) -- card key order
- [MESH-STATUS-015](#92-card) -- absent fields omitted
- [MESH-STATUS-016](#92-card) -- nil read as missing
- [MESH-STATUS-017](#93-state-map) -- state code
- [MESH-STATUS-018](#93-state-map) -- state since
- [MESH-STATUS-019](#93-state-map) -- state unknown keys
- [MESH-STATUS-020](#94-repo-map) -- repo name
- [MESH-STATUS-021](#94-repo-map) -- repo branch
- [MESH-STATUS-022](#94-repo-map) -- repo unknown keys
- [MESH-STATUS-023](#95-plan-map) -- plan title
- [MESH-STATUS-024](#95-plan-map) -- plan unknown keys
- [MESH-STATUS-025](#96-todo-map) -- todo goal
- [MESH-STATUS-026](#96-todo-map) -- todo done
- [MESH-STATUS-027](#96-todo-map) -- todo total
- [MESH-STATUS-028](#96-todo-map) -- todo unknown keys
- [MESH-STATUS-029](#97-reserved-codes-and-client-errors) -- unknown state codes kept
- [MESH-STATUS-030](#97-reserved-codes-and-client-errors) -- numeric fields kept as sent
- [MESH-MSG-001](#101-body) -- body version
- [MESH-MSG-002](#101-body) -- body kind
- [MESH-MSG-003](#101-body) -- body id
- [MESH-MSG-004](#101-body) -- body in_reply_to
- [MESH-MSG-005](#101-body) -- body title
- [MESH-MSG-006](#101-body) -- body content
- [MESH-MSG-007](#101-body) -- body fields
- [MESH-MSG-008](#101-body) -- over-deep or over-long fields dropped, message kept
- [MESH-MSG-009](#101-body) -- body ts
- [MESH-MSG-010](#101-body) -- body unknown keys
- [MESH-MSG-011](#101-body) -- body key order
- [MESH-MSG-012](#101-body) -- body validation order
- [MESH-MSG-013](#101-body) -- nil read as absent
- [MESH-MSG-014](#101-body) -- sender mints a wire id and caps text
- [MESH-MSG-015](#103-reply-values) -- acknowledgement received flag
- [MESH-MSG-016](#103-reply-values) -- acknowledgement id echo
- [MESH-MSG-017](#103-reply-values) -- acknowledgement unknown keys
- [MESH-MSG-018](#103-reply-values) -- InvalidData for validation failures
- [MESH-MSG-019](#103-reply-values) -- Throttled before filing
- [MESH-MSG-020](#103-reply-values) -- silence when delivery fails
- [MESH-MSG-021](#103-reply-values) -- acknowledgement precedes the envoy
- [MESH-MSG-022](#104-sender-outcome) -- acknowledgement is direct delivery
- [MESH-MSG-023](#104-sender-outcome) -- other reply value not acknowledged
- [MESH-MSG-024](#104-sender-outcome) -- transport failure falls back
- [MESH-MSG-025](#104-sender-outcome) -- refusal code never falls back
- [MESH-MSG-026](#104-sender-outcome) -- version refusal is incompatible version
- [MESH-MSG-027](#105-correlation) -- id is the correlation id
- [MESH-MSG-028](#105-correlation) -- reply closes an open question
- [MESH-MSG-029](#105-correlation) -- unmatched reply delivered as message
- [MESH-MSG-030](#105-correlation) -- interim notices
- [MESH-MSG-031](#105-correlation) -- replies to open questions uncounted
- [MESH-MSG-032](#106-envoy-contract) -- envoy answer shape
- [MESH-MSG-033](#106-envoy-contract) -- escalation notice
- [MESH-MSG-034](#106-envoy-contract) -- late human answer
- [MESH-MSG-035](#106-envoy-contract) -- failure texts
- [MESH-MSG-036](#106-envoy-contract) -- envoy run ceiling
- [MESH-MSG-037](#107-peer-limits) -- refusal reason key
- [MESH-MSG-038](#107-peer-limits) -- retry_after_secs key
- [MESH-MSG-039](#107-peer-limits) -- typed refusal unknown keys
- [MESH-MSG-040](#107-peer-limits) -- typed refusal carried in a reply
- [MESH-MSG-041](#107-peer-limits) -- fixed hourly windows per identity
- [MESH-MSG-042](#107-peer-limits) -- live link over-limit is bare Throttled
- [MESH-MSG-043](#107-peer-limits) -- store-and-forward over-limit filed and refused once
- [MESH-MSG-044](#107-peer-limits) -- no typed refusal for a message with in_reply_to
- [MESH-MSG-045](#107-peer-limits) -- loop_guard never sent
- [MESH-MSG-046](#108-peer-message-over-lxmf) -- LXMF peer type tag
- [MESH-MSG-047](#108-peer-message-over-lxmf) -- LXMF peer custom data
- [MESH-MSG-048](#108-peer-message-over-lxmf) -- LXMF peer other fields
- [MESH-MSG-049](#108-peer-message-over-lxmf) -- LXMF peer kind
- [MESH-MSG-050](#108-peer-message-over-lxmf) -- LXMF peer id
- [MESH-MSG-051](#108-peer-message-over-lxmf) -- LXMF peer in_reply_to
- [MESH-MSG-052](#108-peer-message-over-lxmf) -- LXMF peer name hash
- [MESH-MSG-053](#108-peer-message-over-lxmf) -- LXMF peer fields
- [MESH-MSG-054](#108-peer-message-over-lxmf) -- LXMF peer fields depth cap
- [MESH-MSG-055](#108-peer-message-over-lxmf) -- LXMF peer custom data unknown keys
- [MESH-MSG-056](#108-peer-message-over-lxmf) -- LXMF title and content bytes
- [MESH-MSG-057](#108-peer-message-over-lxmf) -- LXMF text decoded, cleaned, capped
- [MESH-MSG-058](#108-peer-message-over-lxmf) -- sending instance recomputed and authorized
- [MESH-MSG-059](#108-peer-message-over-lxmf) -- ts from the LXMF timestamp
- [MESH-PROP-001](#111-outbound) -- LXMF addressing
- [MESH-PROP-002](#111-outbound) -- LXMF payload
- [MESH-PROP-003](#111-outbound) -- non-map fields refused
- [MESH-PROP-004](#111-outbound) -- message signed by the sender
- [MESH-PROP-005](#111-outbound) -- transient packing
- [MESH-PROP-006](#111-outbound) -- transient id
- [MESH-PROP-007](#111-outbound) -- envelope timestamp
- [MESH-PROP-008](#111-outbound) -- envelope transient array
- [MESH-PROP-009](#111-outbound) -- envelope has no other elements
- [MESH-PROP-010](#111-outbound) -- posting node selection
- [MESH-PROP-011](#111-outbound) -- stamp cost ceiling
- [MESH-PROP-012](#111-outbound) -- stamp mined at the announced cost
- [MESH-PROP-013](#111-outbound) -- per-transfer size limit
- [MESH-PROP-014](#111-outbound) -- posting link is anonymous
- [MESH-PROP-015](#111-outbound) -- envelope packet or resource
- [MESH-PROP-016](#112-node-verdict) -- silence is acceptance
- [MESH-PROP-017](#112-node-verdict) -- signalling packet is rejection
- [MESH-PROP-018](#112-node-verdict) -- closed link is rejection
- [MESH-PROP-019](#112-node-verdict) -- transfer bound
- [MESH-PROP-020](#113-inbound-fetch) -- fetch link identifies first
- [MESH-PROP-021](#113-inbound-fetch) -- round 1 list parsing
- [MESH-PROP-022](#113-inbound-fetch) -- round 2 request shape
- [MESH-PROP-023](#113-inbound-fetch) -- round 2 bodies parsing
- [MESH-PROP-024](#113-inbound-fetch) -- round 3 acknowledgement
- [MESH-PROP-025](#113-inbound-fetch) -- node refusal reading
- [MESH-PROP-026](#113-inbound-fetch) -- fetch cadence unfixed
- [MESH-PROP-027](#113-inbound-fetch) -- one fetch at a time
- [MESH-PROP-028](#114-body-pipeline) -- stage 1, size bounds
- [MESH-PROP-029](#114-body-pipeline) -- stage 2, transient dedup
- [MESH-PROP-030](#114-body-pipeline) -- stage 3, destination is ours
- [MESH-PROP-031](#114-body-pipeline) -- stage 4, decrypt
- [MESH-PROP-032](#114-body-pipeline) -- stage 5, message id dedup
- [MESH-PROP-033](#114-body-pipeline) -- stage 6, unknown source deferral
- [MESH-PROP-034](#114-body-pipeline) -- stage 6, unknown source budget spent
- [MESH-PROP-035](#114-body-pipeline) -- stage 7, signature
- [MESH-PROP-036](#114-body-pipeline) -- stage 8, unknown signer discarded
- [MESH-PROP-037](#114-body-pipeline) -- stage 8, blocked signer discarded
- [MESH-PROP-038](#114-body-pipeline) -- stage 9, routing order
- [MESH-PROP-039](#114-body-pipeline) -- deferral table cap
- [MESH-PROP-040](#114-body-pipeline) -- dedup capacity and horizon
- [MESH-PROP-041](#114-body-pipeline) -- delivery stamp cost
- [MESH-PROP-042](#114-body-pipeline) -- payload text untrusted until cleaned
- [MESH-EXT-001](#12-extensibility) -- unknown map keys ignored
- [MESH-EXT-002](#12-extensibility) -- unknown kind fatal per message
- [MESH-EXT-003](#12-extensibility) -- unknown state code kept
- [MESH-EXT-004](#12-extensibility) -- unknown refusal uint is a reply value
- [MESH-EXT-005](#12-extensibility) -- unknown request path
- [MESH-EXT-006](#12-extensibility) -- unknown protocol version
- [MESH-EXT-007](#12-extensibility) -- new fields as new keys
- [MESH-EXT-008](#12-extensibility) -- schema versions bump for incompatible changes
- [MESH-CODE-001](#13-code-point-immutability) -- code point semantics never change
- [MESH-CODE-002](#13-code-point-immutability) -- new semantics take a new code point
