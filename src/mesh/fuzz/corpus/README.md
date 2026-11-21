# Mesh fuzz corpus

Wire-byte inputs the fuzz targets in `src/mesh/fuzz/mod.rs` replay before their random
iterations. One directory per target with wire bytes of its own: `envelope/`, `receipt/`,
`codecs/`. The dispatch target has none: the library decodes the request frame, so it
replays `envelope/` as the request data on every known path and on one unregistered path.
Every file is fed to the same oracle as a random iteration, so a file that no longer
matches the spec predicate fails the test exactly as a random input would.

## Naming

`<area>-<short-description>.bin`

The area is the lowercase name of the protocol surface the input exercises
(`env-negative-v.bin` is an Envelope input, `fetch-colon.bin` a wire-path one). An input
promoted from a failure is named after the bug.

## What each directory holds

| Directory | Bytes | Oracle |
|---|---|---|
| `envelope/` | one Envelope map: `v`, `name_hash`, `body` | the Envelope predicate and the canonical re-encoding |
| `receipt/` | a kind byte, then a body or a sequence buffer (below) | the receipt stage order and the dedup model on a fresh store |
| `codecs/` | a tag byte, then that decoder's wire bytes | per-decoder predicate and round trip |

### Receipt kind bytes

| Kind | Payload | Replay |
|---|---|---|
| `0x00` | one propagation body as a node serves it | a single `Garbage` event on a fresh bench |
| `0x01` | the raw `arbitrary::Unstructured` buffer a `Sequence` was shaped from | `Sequence::arbitrary_take_rest`, then the whole sequence on a fresh bench; a buffer that shapes no sequence is a violation |

The five boundary files (`prop-*`, `inv-*`) are kind `0x00`; their names give the body
length, one less than the file's.

### Codec tag bytes

The first byte of a `codecs/` file selects the decoder. The table is `check_codec_bytes`
in `oracles.rs`; keep the two in step. A msgpack payload that is not exactly one value
within the library's frame nesting budget is a violation, not a skip.

| Tag | Decoder | Payload |
|---|---|---|
| `0x01` | `StatusCard::from_value` | msgpack card map |
| `0x02` | `from_request_body`, the `/message` body | msgpack body map |
| `0x03` | `intro_from_request_body`, the `/knock` body | msgpack map |
| `0x04` | `AnnounceAppData::decode` | raw bytes: `SCOPE`, u16 BE version, name |
| `0x05` | `decode_peer_lxmf`, LXMF peer custom fields | msgpack fields map keyed `0xFB`, `0xFC` |
| `0x06` | `decode_knock_message`, LXMF knock custom fields | msgpack fields map keyed `0xFB`, `0xFC` |
| `0x07` | `RefusalCode::from_wire` | msgpack value |
| `0x08` | `VersionRefusal::from_value` | msgpack map |
| `0x09` | `DispatchError::from_value` | msgpack map |
| `0x0a` | `PendingStore::load_pending` | jsonl text |
| `0x0b` | `WirePath::parse`, the file part name grammar (`wire_path.rs`) | UTF-8 text |
| `0x0c` | `SharesPage::from_value`, the `/list` reply | msgpack page map |
| `0x0d` | `read_fetch_reply`, the `/fetch` reply | msgpack reply map |
| `0x0e` | `read_access_reply`, the `/access` reply | msgpack reply map |
| `0x0f` | `decode_access_message`, LXMF access custom fields | msgpack fields map keyed `0xFB`, `0xFC` |
| `0x10` | `decode_access`, the `/access` request body | msgpack body map |

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
under kind `0x00`. A dispatch violation also writes the `Unstructured` buffer; to promote
it, re-encode the envelope it shaped and seed `envelope/` with that.

The four `codecs/` files that spell the announce magic or an LXMF type tag are checked
against the live constants by `fuzz_codec_corpus_files_carrying_wire_identifiers_are_built_from_the_live_constants`;
after a rename, `COYOTE_MESH_FUZZ_WRITE_CORPUS=1 cargo test --all fuzz_codec_corpus_files_carrying` rewrites them.
The `codecs/fetch-<rule>.bin` files, one per wire-path rule, are checked against the live
path limits by `fuzz_codec_corpus_wire_path_seeds_break_exactly_the_rule_their_name_claims`;
after a limit or rule change, `COYOTE_MESH_FUZZ_WRITE_CORPUS=1 cargo test --all fuzz_codec_corpus_wire_path`
rewrites them.
The `codecs/list-*`, `fetch-unknown-status`, `fetch-corrupt-digest`, `fetch-unknown-rule` and
`access-*` files are checked against the live page, cursor, path and reply constants by
`fuzz_codec_corpus_share_and_access_seeds_reach_the_outcome_their_name_claims`;
`COYOTE_MESH_FUZZ_WRITE_CORPUS=1 cargo test --all fuzz_codec_corpus_share_and_access` rewrites them.

`.gitattributes` marks `src/mesh/fuzz/corpus/**/*.bin` as `binary`. Without it Git's text
heuristic can take a small msgpack file for text and rewrite a `0x0a` byte (`c4 0a`, the
bin-8 header of a ten-byte `name_hash`) as CRLF on a checkout with `autocrlf`, as the
Windows CI leg does.

## Nesting depth

`unpack_whole` in `oracles.rs` reads with the library's frame bound, `MAX_DEPTH` = 128, as
an `rmpv` depth limit; `rmpv` spends two units per container, so the oracle stops a little
shallower than the library, which counts containers. For the envelope and dispatch targets
bytes the oracle cannot read are the library's to refuse and are skipped; for `codecs/`
they are a violation. The cap exists because under `rmpv`'s default limit of 1024 a few
hundred nested one-element arrays overflow a 2 MiB thread stack in a debug build and abort
the process before the limit is reached, while a release build returns `depth limit
exceeded` cleanly. The live tests in `src/mesh/request/tests.rs` pin the library's bound.

The protocol defines no reserved or must-be-zero field in any structure
(envelope, card, body, knock, announce, LXMF custom data), so the "nonzero reserved
fields" corpus class has no member. The nearest analogues seeded are the unknown
`state.code` (`status-unknown-state-code.bin`) and the out-of-window announce
version (`ann-announce-version-ffff.bin`).

Two sibling decoders in `src/mesh/propagation.rs` read with the default-depth
`rmpv::decode::read_value` and no such budget: `PropagationNode::from_announce` (the
propagation-node announce `app_data`, which lxmf-core decodes upstream first) and
`refusal_in` (a link packet payload). Both inputs are MTU-bounded (about 500 bytes) and
run on 16 MiB production threads.
