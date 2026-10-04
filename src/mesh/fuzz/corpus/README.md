# Mesh fuzz corpus

Wire-byte inputs the fuzz targets in `src/mesh/fuzz/mod.rs` replay before their random
iterations. One directory per target: `dispatch/`, `envelope/`, `receipt/`, `codecs/`.
Every file is fed to the same oracle as a random iteration, so a file that no longer
matches the spec predicate fails the test exactly as a random input would.

## Naming

`<spec-id-or-bug>-<short-description>.bin`

The spec id is the requirement in `docs/mesh/PROTOCOL.md` the input exercises
(`MESH-ENV-014-negative-v.bin`). An input promoted from a failure that had no single
requirement behind it is named after the bug instead.

## What each directory holds

| Directory | Bytes | Oracle |
|---|---|---|
| `dispatch/` | one request frame: msgpack `[f64 time, bin(16) path_hash, data]` | section 6.1 frame predicate, then the section 6.6 pipeline against every trust tier |
| `envelope/` | one Envelope map: `v`, `name_hash`, `body` | section 6.5 predicate and the canonical re-encoding |
| `receipt/` | a kind byte, then a body or a sequence buffer (below) | the section 11.4 stage order and the dedup model on a fresh store |
| `codecs/` | a tag byte, then that decoder's wire bytes | per-decoder predicate and round trip |

### Receipt kind bytes

| Kind | Payload | Replay |
|---|---|---|
| `0x00` | one propagation body as a node serves it | a single `Garbage` event on a fresh bench |
| `0x01` | the raw `arbitrary::Unstructured` buffer a `Sequence` was shaped from | `Sequence::arbitrary_take_rest`, then the whole sequence on a fresh bench; a buffer that shapes no sequence is a violation |

The five boundary files (`MESH-PROP-028-*`, `MESH-PROP-030-*`, `MESH-INV-005-*`) are kind
`0x00`; their names give the body length, one less than the file's.

### Codec tag bytes

The first byte of a `codecs/` file selects the decoder. The table is `check_codec_bytes`
in `oracles.rs`; keep the two in step. A msgpack payload that is not exactly one value
within the frame decoder's nesting budget is a violation, not a skip.

| Tag | Decoder | Payload |
|---|---|---|
| `0x01` | `StatusCard::from_value` (section 9.2) | msgpack card map |
| `0x02` | `from_r3_body`, the `/message` body (section 10.1) | msgpack body map |
| `0x03` | `intro_from_r3_body`, the `/knock` body (section 8) | msgpack map |
| `0x04` | `AnnounceAppData::decode` (section 5.1) | raw bytes: `SCOPE`, u16 BE version, name |
| `0x05` | `decode_peer_lxmf`, LXMF peer custom fields (section 10.8) | msgpack fields map keyed `0xFB`, `0xFC` |
| `0x06` | `decode_knock_message`, LXMF knock custom fields (section 8.6) | msgpack fields map keyed `0xFB`, `0xFC` |
| `0x07` | `RefusalCode::from_wire` (section 6.7) | msgpack value |
| `0x08` | `VersionRefusal::from_value` (section 7) | msgpack map |
| `0x09` | `DispatchError::from_value` (section 6.7) | msgpack map |
| `0x0a` | `PendingStore::load_pending` (section 10.5) | jsonl text |
| `0x0b` | `WirePath::parse`, the file part name grammar (`wire_path.rs`) | UTF-8 text |

## Replay

Every run of a target reads its directory, sorts the `.bin` files by name and checks each
one before the first random iteration. The `corpus=<n>` field of the summary line counts
them. Replay is unconditional: neither `COYOTE_MESH_FUZZ_ITERS` nor `COYOTE_MESH_FUZZ_SEED`
affects it.

The random budget that follows is `DEFAULT_ITERS` iterations under the `TARGET_WALL_CAP`
wall cap by default; a cap that stops the loop early prints a `WALL CAP HIT` line. The cap
guards that CI default only. Setting `COYOTE_MESH_FUZZ_ITERS=<n>` is a deliberate soak:
exactly `n` iterations run with the cap lifted, and the summary line reports
`iterations=<n>`.

## Promoting a failure

A violation writes its input to `target/mesh-fuzz-failures/<target>-<seed>-<iteration>.bin`
(under `CARGO_TARGET_DIR` when set) and prints the same bytes as hex. Copy that file into
`<target>/` under the naming rule above. For `codecs/` the written file already starts
with its tag byte. For `receipt/` it already starts with kind `0x01` and holds the whole
`Unstructured` buffer, because the sealed bodies of a sequence depend on the bench they
were built against and only the buffer reproduces them; a hand-made single body goes in
under kind `0x00`.

The four `codecs/` files that spell the announce magic or an LXMF type tag are pinned to
the live constants by `fuzz_codec_corpus_files_carrying_wire_identifiers_are_built_from_the_live_constants`;
after a rename, `COYOTE_MESH_FUZZ_WRITE_CORPUS=1 cargo test --all fuzz_codec_corpus_files_carrying` rewrites them.
The `codecs/MESH-FETCH-004-<rule>.bin` files, one per wire-path rule, are pinned to the live
path limits by `fuzz_codec_corpus_wire_path_seeds_break_exactly_the_rule_their_name_claims`;
after a limit or rule change, `COYOTE_MESH_FUZZ_WRITE_CORPUS=1 cargo test --all fuzz_codec_corpus_wire_path`
rewrites them.

`.gitattributes` marks `src/mesh/fuzz/corpus/**/*.bin` as `binary`. Without it Git's text
heuristic can take a small msgpack file for text and rewrite a `0x0a` byte (`c4 0a`, the
bin-8 header of a ten-byte `name_hash`) as CRLF on a checkout with `autocrlf`, as the
Windows CI leg does.

## Parser bugs found

- `MESH-ENV-028-nested-400-arrays.bin` (`dispatch/`): `RequestFrame::decode` read nested
  containers with `rmpv`'s default depth limit of 1024, deep enough for about 400 nested
  one-element arrays (a 428-byte frame) to overflow a 2 MiB thread stack in a debug build
  and abort the process before the limit was reached. A release build returned
  `depth limit exceeded` cleanly. `decode_whole` in `src/mesh/r3/frame.rs` now reads with
  `MAX_R3_NESTING_DEPTH` = 128. The deepest frame the spec delivers intact (envelope, body
  and `fields` nested to the sanitiser's `PEER_FIELDS_MAX_DEPTH` limit) spends about 25 of
  the 128; 62 nested one-element arrays under `data` fit and a 63rd overruns it, so both
  request and response frames past it fail to decode (MESH-ENV-048, MESH-ENV-049) and a
  request earns silence as MESH-ENV-028 requires. The `unpack_whole` helper in
  `oracles.rs` reads with the same budget so the predicate agrees.

`docs/mesh/PROTOCOL.md` defines no reserved or must-be-zero field in any structure
(envelope, card, body, knock, announce, LXMF custom data), so the "nonzero reserved
fields" corpus class has no member. The nearest analogues seeded are the unknown
`state.code` (`MESH-STATUS-029-unknown-state-code.bin`) and the out-of-window announce
version (`MESH-ANN-002-announce-version-ffff.bin`).

Sibling decoders not bounded by this fix: `PropagationNode::from_announce`
(`src/mesh/propagation.rs:168`, the propagation-node announce `app_data`, which lxmf-core
decodes upstream first) and `refusal_in` (`src/mesh/propagation.rs:756`, a link packet
payload) still use the default-depth `rmpv::decode::read_value`. Both inputs are
MTU-bounded (about 500 bytes) and run on 16 MiB production threads, so they are
parity-hardening follow-ups, deliberately not changed here.
