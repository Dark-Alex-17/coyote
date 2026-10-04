//! An `arbitrary`-based fuzz harness for the mesh protocol parsers and state machines,
//! run as part of the ordinary test suite. The crate is bin-only (no `[lib]` target), so
//! `cargo-fuzz` cannot link against it; do not re-attempt that route. Instead every target
//! here derives structured inputs with `arbitrary::Arbitrary` from a seeded byte stream and
//! checks them against a semantic oracle in `oracles.rs`: a spec-derived predicate, a
//! reference model, or a round-trip law, never merely "did not panic".
//!
//! Budget. Each target replays its committed corpus first (`corpus/<target>/*.bin`, sorted
//! by file name), then runs its random budget and prints one
//! `mesh-fuzz target=... seed=... corpus=... iterations=... elapsed_ms=...` line. By
//! default the budget is `DEFAULT_ITERS` iterations or `TARGET_WALL_CAP`, whichever comes
//! first, and a cap that stops the loop early prints a `WALL CAP HIT` line as well. The
//! cap guards that CI default only: setting `COYOTE_MESH_FUZZ_ITERS` to a count is a
//! deliberate soak, so exactly that many iterations run and the cap is lifted.
//! `COYOTE_MESH_FUZZ_SEED` (decimal or `0x` hex) replaces `DEFAULT_SEED`; the same seed
//! yields the same inputs, so a run is reproducible on every platform.
//! `COYOTE_MESH_FUZZ_WRITE_CORPUS=1` rewrites the codec corpus files that carry a wire
//! identifier from the live constants instead of checking them.
//!
//! Reproducing a failure. A violation prints the target, the seed, the iteration (or corpus
//! file), the oracle's text and the input as hex, and writes the input to
//! `target/mesh-fuzz-failures/<target>-<seed>-<iteration>.bin` (under `CARGO_TARGET_DIR`
//! when set). Iteration `i` under seed `s` is generated from
//! `SplitMix::new(s ^ i * 0x9E3779B97F4A7C15)`, so every input is reproducible from the
//! seed; the receipt target's ephemeral sealing keys also depend on the iteration order,
//! which the same seed reproduces, and its sealed bytes on how many kind-`0x01` corpus
//! files were replayed first, since each advances the shared seal stream. Adding one such
//! file changes the ciphertext of later iterations but not the sequences they shape.
//!
//! Promoting a failure. Copy the written `.bin` into `corpus/<target>/` under a name of the
//! form `<spec-id-or-bug>-<short-description>.bin` (see `corpus/README.md`); every later run
//! replays it before any random iteration. Three targets write the wire bytes the oracle
//! judged. The receipt target judges a whole event sequence whose sealed bodies depend on
//! the bench they were built against, so it writes `0x01` and the raw `Unstructured` buffer
//! the sequence was shaped from; replay shapes the same sequence again. A `receipt/` file
//! starting `0x00` is instead one body, replayed as a single `Garbage` event.

mod oracles;

use arbitrary::{Arbitrary, Unstructured};
use rand_core::{CryptoRng, RngCore};
use std::cell::RefCell;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(super) const DEFAULT_ITERS: u64 = 2000;
pub(super) const TARGET_WALL_CAP: Duration = Duration::from_secs(10);
const ITERS_ENV: &str = "COYOTE_MESH_FUZZ_ITERS";
const SEED_ENV: &str = "COYOTE_MESH_FUZZ_SEED";
const WRITE_CORPUS_ENV: &str = "COYOTE_MESH_FUZZ_WRITE_CORPUS";
const DEFAULT_SEED: u64 = 0xC07E_5EED_2026_0001;
/// The splitmix64 increment, also the per-iteration seed spreader.
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
const MIN_INPUT_BYTES: u64 = 64;
const MAX_INPUT_BYTES: u64 = 4096;
/// Bytes of a failing input printed as hex before the dump is cut short.
const HEX_PRINT_CAP: usize = 4096;

/// splitmix64. `Copy` on purpose: a copy replays the same stream, which is what the LXMF
/// sealing call wants from its `CryptoRngCore + Copy` bound; callers that need independent
/// streams take a `fork`.
#[derive(Clone, Copy)]
struct SplitMix {
    state: u64,
}

impl SplitMix {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GOLDEN);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A generator seeded from this one's next output, so its stream is independent.
    fn fork(&mut self) -> Self {
        Self::new(self.next())
    }
}

impl RngCore for SplitMix {
    fn next_u32(&mut self) -> u32 {
        (self.next() >> 32) as u32
    }

    fn next_u64(&mut self) -> u64 {
        self.next()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        rand_core::impls::fill_bytes_via_next(self, dest)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

// Test-only: the identities it mints are fixtures, not keys anyone relies on.
impl CryptoRng for SplitMix {}

/// `Ok(None)` when unset or blank, `Ok(Some(n))` for a decimal count, `Err(text)` for
/// anything else.
fn parse_iters(value: Option<&OsStr>) -> Result<Option<u64>, String> {
    let Some(value) = value else { return Ok(None) };
    let text = value.to_string_lossy();
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse().map(Some).map_err(|_| text.to_string())
}

/// As `parse_iters`, also accepting `0x`-prefixed hex.
fn parse_seed(value: Option<&OsStr>) -> Result<Option<u64>, String> {
    let Some(value) = value else { return Ok(None) };
    let text = value.to_string_lossy();
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map(Some).map_err(|_| text.to_string())
}

/// The parsed setting, or `default` with a note when the text was not a number.
fn or_default(name: &str, parsed: Result<Option<u64>, String>, default: u64) -> u64 {
    match parsed {
        Ok(Some(value)) => value,
        Ok(None) => default,
        Err(text) => {
            eprintln!("mesh-fuzz: {name}={text:?} is not a number; using the default {default}");
            default
        }
    }
}

/// The random budget of one target. An explicit iteration count is a deliberate soak and
/// runs uncapped; the default (and an unparseable setting) runs under `TARGET_WALL_CAP`.
#[derive(Debug, PartialEq)]
struct Budget {
    requested: u64,
    wall_cap: Option<Duration>,
}

fn budget(iters: Result<Option<u64>, String>) -> Budget {
    match iters {
        Ok(Some(requested)) => Budget {
            requested,
            wall_cap: None,
        },
        parsed => Budget {
            requested: or_default(ITERS_ENV, parsed, DEFAULT_ITERS),
            wall_cap: Some(TARGET_WALL_CAP),
        },
    }
}

fn corpus_dir(target: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("mesh")
        .join("fuzz")
        .join("corpus")
        .join(target)
}

/// Every `.bin` under the target's corpus directory, sorted by path. The corpus is
/// committed, so a missing directory is a bug, not an empty corpus.
fn corpus_files(target: &str) -> Vec<PathBuf> {
    let dir = corpus_dir(target);
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("no fuzz corpus at {}: {err}", dir.display()));
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
        .collect();
    files.sort();
    files
}

fn hex_dump(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(HEX_PRINT_CAP)];
    let mut text: String = shown.iter().map(|byte| format!("{byte:02x}")).collect();
    if bytes.len() > HEX_PRINT_CAP {
        text.push_str(&format!(
            " ... ({} more bytes not shown)",
            bytes.len() - HEX_PRINT_CAP
        ));
    }
    text
}

/// Prints everything needed to reproduce the violation, writes the input under the target
/// directory, then fails the test.
fn fail(target: &str, seed: u64, case: &str, violation: &str, input: &[u8]) -> ! {
    eprintln!("mesh-fuzz FAILURE target={target} seed={seed:#018x} case={case}");
    eprintln!("mesh-fuzz violation: {violation}");
    eprintln!(
        "mesh-fuzz input ({} bytes): {}",
        input.len(),
        hex_dump(input)
    );
    let dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
        .join("mesh-fuzz-failures");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{target}-{seed:016x}-{case}.bin"));
    fs::write(&path, input).unwrap();
    eprintln!("mesh-fuzz input written to {}", path.display());
    panic!("mesh-fuzz target {target} violated its oracle at {case}: {violation}");
}

/// An oracle violation on a random iteration, with the bytes that reproduce it: wire bytes,
/// or for the receipt target the tagged `Unstructured` buffer.
struct Violation {
    what: String,
    input: Vec<u8>,
}

impl Violation {
    fn new(what: String, input: Vec<u8>) -> Self {
        Self { what, input }
    }
}

/// Replays the corpus, then runs the random budget. `replay` checks one corpus file;
/// `random` checks one generated value, given also the buffer it was shaped from, and on a
/// violation hands back the bytes that reproduce it.
fn run_target<T: for<'a> Arbitrary<'a>>(
    target: &str,
    seed: u64,
    replay: impl Fn(&[u8]) -> Result<(), String>,
    random: impl Fn(&[u8], T) -> Result<(), Violation>,
) {
    let Budget {
        requested,
        wall_cap,
    } = budget(parse_iters(std::env::var_os(ITERS_ENV).as_deref()));
    let files = corpus_files(target);
    for path in &files {
        let bytes = fs::read(path).unwrap();
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        if let Err(violation) = replay(&bytes) {
            fail(target, seed, &format!("corpus-{stem}"), &violation, &bytes);
        }
    }
    let start = Instant::now();
    let mut completed = 0;
    let mut skipped = 0;
    while completed < requested && wall_cap.is_none_or(|cap| start.elapsed() < cap) {
        let iteration = completed;
        let mut rng = SplitMix::new(seed ^ iteration.wrapping_mul(GOLDEN));
        let len = MIN_INPUT_BYTES + rng.next() % (MAX_INPUT_BYTES - MIN_INPUT_BYTES + 1);
        let mut buf = vec![0u8; usize::try_from(len).unwrap()];
        rng.fill_bytes(&mut buf);
        match T::arbitrary_take_rest(Unstructured::new(&buf)) {
            Ok(value) => {
                if let Err(violation) = random(&buf, value) {
                    fail(
                        target,
                        seed,
                        &iteration.to_string(),
                        &violation.what,
                        &violation.input,
                    );
                }
            }
            Err(_) => skipped += 1,
        }
        completed += 1;
    }
    let elapsed = start.elapsed();
    match wall_cap {
        Some(cap) => {
            assert!(
                completed == requested || elapsed >= cap,
                "{target}: stopped after {completed} of {requested} iterations at {elapsed:?} without reaching the wall cap"
            );
            assert!(
                elapsed < cap + Duration::from_secs(5),
                "{target}: one iteration overran the wall cap by more than 5 s ({elapsed:?})"
            );
        }
        None => assert_eq!(
            completed, requested,
            "{target}: a soak runs every requested iteration"
        ),
    }
    eprintln!(
        "mesh-fuzz target={target} seed={seed:#018x} corpus={} iterations={completed} elapsed_ms={}",
        files.len(),
        elapsed.as_millis()
    );
    if completed < requested {
        eprintln!(
            "mesh-fuzz target={target} WALL CAP HIT after {completed} of {requested} iterations"
        );
    }
    if skipped > 0 {
        eprintln!(
            "mesh-fuzz target={target} skipped={skipped} (inputs `arbitrary` could not shape)"
        );
    }
}

fn seed() -> u64 {
    or_default(
        SEED_ENV,
        parse_seed(std::env::var_os(SEED_ENV).as_deref()),
        DEFAULT_SEED,
    )
}

#[test]
fn fuzz_dispatch_authorizes_every_frame_exactly_as_the_trust_model_predicts() {
    let seed = seed();
    let fixture = oracles::DispatchFixture::new(&mut SplitMix::new(seed));
    run_target(
        "dispatch",
        seed,
        |bytes| oracles::check_dispatch_bytes(&fixture, bytes),
        |_, frame: oracles::FrameGen| {
            let bytes = frame.into_bytes();
            oracles::check_dispatch_bytes(&fixture, &bytes)
                .map_err(|what| Violation::new(what, bytes))
        },
    );
}

#[test]
fn fuzz_envelope_parser_matches_the_spec_predicate_and_round_trips() {
    let seed = seed();
    run_target(
        "envelope",
        seed,
        oracles::check_envelope_bytes,
        |_, envelope: oracles::EnvelopeGen| {
            let value = envelope.into_value();
            let bytes = oracles::packed(&value);
            oracles::check_envelope_wire_and_value(&bytes, &value)
                .map_err(|what| Violation::new(what, bytes))
        },
    );
}

#[test]
fn fuzz_receipt_pipeline_follows_the_stage_order_and_the_dedup_model() {
    let seed = seed();
    let fixture = oracles::ReceiptFixture::new(&mut SplitMix::new(seed));
    run_target(
        "receipt",
        seed,
        |bytes| oracles::check_receipt_file(&fixture, bytes),
        |buf, sequence: oracles::Sequence| {
            oracles::check_receipt_sequence(&fixture, sequence).map_err(|what| {
                let mut input = Vec::with_capacity(buf.len() + 1);
                input.push(oracles::RECEIPT_SEQUENCE);
                input.extend_from_slice(buf);
                Violation::new(what, input)
            })
        },
    );
}

#[test]
fn fuzz_receipt_oracle_spends_the_deferral_budget_a_heartbeat_after_three_sightings() {
    let fixture = oracles::ReceiptFixture::new(&mut SplitMix::new(seed()));
    oracles::check_receipt_deferral_budget(&fixture).unwrap_or_else(|what| panic!("{what}"));
}

#[test]
fn fuzz_codecs_refuse_only_what_the_spec_names_and_re_encode_stably() {
    let seed = seed();
    let fixture = oracles::CodecFixture::new();
    run_target(
        "codecs",
        seed,
        |bytes| oracles::check_codec_bytes(&fixture, bytes),
        |_, bundle: oracles::CodecBundle| {
            for bytes in bundle.into_tagged_inputs() {
                oracles::check_codec_bytes(&fixture, &bytes)
                    .map_err(|what| Violation::new(what, bytes))?;
            }
            Ok(())
        },
    );
}

#[test]
fn fuzz_budget_constants_are_pinned() {
    assert_eq!(DEFAULT_ITERS, 2000);
    assert_eq!(TARGET_WALL_CAP, Duration::from_secs(10));
    assert_eq!(ITERS_ENV, "COYOTE_MESH_FUZZ_ITERS");
    assert_eq!(SEED_ENV, "COYOTE_MESH_FUZZ_SEED");
    assert_eq!(WRITE_CORPUS_ENV, "COYOTE_MESH_FUZZ_WRITE_CORPUS");
    let ci = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/.github/workflows/ci.yaml"
    ));
    let lines: Vec<&str> = ci.lines().collect();
    let all = lines
        .iter()
        .position(|line| line.trim_end() == "  all:")
        .expect("the `all` job is at the two-space indent");
    let next_job = lines[all + 1..]
        .iter()
        .position(|line| {
            line.starts_with("  ")
                && !line.starts_with("   ")
                && !line.starts_with("  #")
                && line.trim_end().ends_with(':')
        })
        .map_or(lines.len(), |offset| all + 1 + offset);
    let job = lines[all..next_job].join("\n");
    for needle in [
        ITERS_ENV.to_string(),
        format!("{DEFAULT_ITERS} iterations"),
        format!("{} s wall cap", TARGET_WALL_CAP.as_secs()),
    ] {
        assert!(
            job.contains(&needle),
            "the `all` job in ci.yaml must say that `cargo test --all` runs this harness with its budget; missing {needle:?}"
        );
    }
}

#[test]
fn fuzz_budget_lifts_the_wall_cap_only_for_an_explicit_iteration_count() {
    let capped_default = Budget {
        requested: DEFAULT_ITERS,
        wall_cap: Some(TARGET_WALL_CAP),
    };
    assert_eq!(
        budget(Ok(Some(50))),
        Budget {
            requested: 50,
            wall_cap: None,
        }
    );
    assert_eq!(budget(Ok(None)), capped_default);
    assert_eq!(budget(Err("lots".to_string())), capped_default);
}

#[test]
fn fuzz_run_target_replays_every_corpus_file_before_the_first_random_iteration() {
    let requested = budget(parse_iters(std::env::var_os(ITERS_ENV).as_deref())).requested;
    let files = corpus_files("envelope");
    let order: RefCell<Vec<&'static str>> = RefCell::new(Vec::new());
    let replayed: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
    run_target::<u8>(
        "envelope",
        DEFAULT_SEED,
        |bytes| {
            order.borrow_mut().push("replay");
            replayed.borrow_mut().push(bytes.to_vec());
            Ok(())
        },
        |_, _| {
            order.borrow_mut().push("random");
            Ok(())
        },
    );
    let expected: Vec<Vec<u8>> = files.iter().map(|path| fs::read(path).unwrap()).collect();
    assert_eq!(*replayed.borrow(), expected);
    let order = order.borrow();
    assert!(order[..files.len()].iter().all(|step| *step == "replay"));
    assert!(order[files.len()..].iter().all(|step| *step == "random"));
    if requested > 0 {
        assert!(
            order.len() > files.len(),
            "the random budget must run at least once"
        );
    }
}

#[test]
fn fuzz_receipt_boundary_corpus_lengths_are_pinned() {
    use super::propagation_fetch::{MAX_FETCHED_MESSAGE_BYTES, MIN_FETCHED_MESSAGE_BYTES};
    let body_len = |name: &str| {
        let on_disk = fs::metadata(corpus_dir("receipt").join(name))
            .unwrap()
            .len();
        usize::try_from(on_disk).unwrap() - 1
    };
    assert_eq!(
        body_len("MESH-PROP-028-undersize-111.bin"),
        MIN_FETCHED_MESSAGE_BYTES - 1
    );
    assert_eq!(
        body_len("MESH-PROP-028-exact-min-112-zeros.bin"),
        MIN_FETCHED_MESSAGE_BYTES
    );
    assert_eq!(
        body_len("MESH-PROP-028-oversize-131073.bin"),
        MAX_FETCHED_MESSAGE_BYTES + 1
    );
}

/// The codec corpus files that spell a wire identifier or a store version are derived
/// from the live constants, so a rename of the magic or a type tag, or a store bump,
/// regenerates them instead of leaving the corpus replaying the old bytes.
/// `WRITE_CORPUS_ENV=1` writes the derived bytes in place of checking them. Either way
/// each file must still reach the decoder outcome its name describes, so a regenerated
/// file cannot silently stop exercising it.
#[test]
fn fuzz_codec_corpus_files_carrying_wire_identifiers_are_built_from_the_live_constants() {
    use super::announce::{ANNOUNCE_MAGIC, AnnounceAppData};
    use super::knock::{KNOCK_TYPE, KnockMessage, decode_knock_message};
    use super::message::{PEER_MESSAGE_TYPE, PeerLxmf, decode_peer_lxmf};
    use super::pending::{PENDING_RECORD_VERSION, PendingRecord, PendingState};
    use lxmf_core::constants::{FIELD_CUSTOM_DATA, FIELD_CUSTOM_TYPE};
    use oracles::{
        TAG_ANNOUNCE, TAG_KNOCK_FIELDS, TAG_PEER_FIELDS, TAG_PENDING, inbound_with, unpack_whole,
    };
    use rmpv::Value;

    fn announce(version: [u8; 2], name: &[u8]) -> Vec<u8> {
        [&[TAG_ANNOUNCE][..], &ANNOUNCE_MAGIC, &version, name].concat()
    }
    fn lxmf_fields(tag: u8, custom_type: &str, data: Vec<(Value, Value)>) -> Vec<u8> {
        let fields = Value::Map(vec![
            (Value::from(FIELD_CUSTOM_TYPE), Value::from(custom_type)),
            (Value::from(FIELD_CUSTOM_DATA), Value::Map(data)),
        ]);
        let mut bytes = vec![tag];
        rmpv::encode::write_value(&mut bytes, &fields).unwrap();
        bytes
    }
    fn pending_line_with_unknown_field() -> Vec<u8> {
        let record = PendingRecord {
            version: PENDING_RECORD_VERSION,
            id: "q1".to_string(),
            peer_destination: "0b".repeat(16),
            peer_identity: "0a".repeat(16),
            thread: "q1".to_string(),
            question: "what time is it".to_string(),
            sent_at: "2027-01-15T05:13:20Z".to_string(),
            timeout_at: "2027-01-15T05:23:20Z".to_string(),
            state: PendingState::Open,
            reply: None,
        };
        let mut object = serde_json::to_value(&record).unwrap();
        object["later_field"] = serde_json::Value::from("refused");
        let mut bytes = vec![TAG_PENDING];
        bytes.extend(serde_json::to_string(&object).unwrap().into_bytes());
        bytes.push(b'\n');
        bytes
    }

    enum Outcome {
        AnnounceVersion(u16),
        NotAnAnnounce,
        PeerNameHashLength,
        KnockNameHashLength,
        PendingUnknownField,
    }
    const NAME_HASH_LENGTH_REASON: &str = "name_hash is not 10 bytes";

    let expected = [
        (
            "MESH-ANN-002-announce-version-ffff.bin",
            announce([0xff, 0xff], b"Alex"),
            Outcome::AnnounceVersion(0xffff),
        ),
        (
            "MESH-ANN-003-announce-name-65-bytes.bin",
            announce([0x00, 0x01], &[b'a'; 65]),
            Outcome::NotAnAnnounce,
        ),
        (
            "MESH-MSG-052-peer-name-hash-nine-bytes.bin",
            lxmf_fields(
                TAG_PEER_FIELDS,
                PEER_MESSAGE_TYPE,
                vec![
                    (Value::from("name_hash"), Value::Binary(vec![0x09; 9])),
                    (Value::from("kind"), Value::from("message")),
                    (Value::from("id"), Value::from("m1")),
                ],
            ),
            Outcome::PeerNameHashLength,
        ),
        (
            "MESH-KNOCK-023-name-hash-nine-bytes.bin",
            lxmf_fields(
                TAG_KNOCK_FIELDS,
                KNOCK_TYPE,
                vec![(Value::from("name_hash"), Value::Binary(vec![0x07; 9]))],
            ),
            Outcome::KnockNameHashLength,
        ),
        (
            "MESH-CODE-005-pending-unknown-field.bin",
            pending_line_with_unknown_field(),
            Outcome::PendingUnknownField,
        ),
    ];
    let write = std::env::var_os(WRITE_CORPUS_ENV).is_some_and(|v| v == "1");
    for (name, bytes, outcome) in expected {
        let path = corpus_dir("codecs").join(name);
        if write {
            fs::write(&path, &bytes).unwrap();
        }
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "codecs/{name} must be the bytes built from the live wire constants; regenerate with {WRITE_CORPUS_ENV}=1"
        );
        let payload = &bytes[1..];
        match outcome {
            Outcome::AnnounceVersion(version) => {
                let decoded = AnnounceAppData::decode(payload)
                    .unwrap_or_else(|| panic!("codecs/{name} must decode as an announce"));
                assert_eq!(decoded.version, version, "codecs/{name}");
            }
            Outcome::NotAnAnnounce => {
                assert!(
                    AnnounceAppData::decode(payload).is_none(),
                    "codecs/{name} must be refused by the announce decoder"
                );
            }
            Outcome::PeerNameHashLength => {
                let fields = unpack_whole(payload).unwrap();
                let observed = decode_peer_lxmf(&inbound_with(fields));
                assert!(
                    matches!(observed, PeerLxmf::Malformed(NAME_HASH_LENGTH_REASON)),
                    "codecs/{name}: decoded as {observed:?}"
                );
            }
            Outcome::KnockNameHashLength => {
                let fields = unpack_whole(payload).unwrap();
                let observed = decode_knock_message(&inbound_with(fields));
                assert!(
                    matches!(observed, KnockMessage::Malformed(NAME_HASH_LENGTH_REASON)),
                    "codecs/{name}: decoded as {observed:?}"
                );
            }
            Outcome::PendingUnknownField => {
                let line = std::str::from_utf8(payload).unwrap().trim_end();
                let refusal = serde_json::from_str::<PendingRecord>(line)
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| panic!("codecs/{name} must be refused by PendingRecord"));
                assert!(
                    refusal.contains("later_field"),
                    "codecs/{name}: refusal must name the unknown key, got {refusal}"
                );
            }
        }
    }
}

/// One codec seed per wire-path rule, each a path that breaks exactly the rule its name
/// carries and none of the rules checked before it, so the replay exercises every
/// `invalid_path` id the grammar can answer with. The texts are built from the live
/// limits and `WRITE_CORPUS_ENV=1` rewrites the files, as for the wire-identifier seeds.
#[test]
fn fuzz_codec_corpus_wire_path_seeds_break_exactly_the_rule_their_name_claims() {
    use super::wire_path::{RULES, WIRE_PATH_MAX_BYTES, WIRE_PATH_MAX_SEGMENTS, WirePath};
    use oracles::TAG_WIRE_PATH;

    fn text_for(rule: &str) -> String {
        match rule {
            "empty" => String::new(),
            "length" => "a".repeat(WIRE_PATH_MAX_BYTES + 1),
            "control" => "a\tb".to_string(),
            "invisible" => "a\u{200b}b".to_string(),
            "backslash" => "docs\\a.md".to_string(),
            "leading_slash" => "/a".to_string(),
            "drive_letter" => "C:x".to_string(),
            "colon" => "ab:c".to_string(),
            "nfc" => "e\u{301}".to_string(),
            "segments" => "a/".repeat(WIRE_PATH_MAX_SEGMENTS) + "a",
            "segment" => "docs//a.md".to_string(),
            "trailing_dot" => "a.".to_string(),
            "trailing_space" => "a ".to_string(),
            "reserved_name" => "CON".to_string(),
            other => panic!("no seed text for wire-path rule `{other}`"),
        }
    }

    let fixture = oracles::CodecFixture::new();
    let write = std::env::var_os(WRITE_CORPUS_ENV).is_some_and(|v| v == "1");
    for (rule, _) in RULES {
        let name = format!("MESH-FETCH-004-{rule}.bin");
        let text = text_for(rule);
        let bytes = [&[TAG_WIRE_PATH][..], text.as_bytes()].concat();
        let path = corpus_dir("codecs").join(&name);
        if write {
            fs::write(&path, &bytes).unwrap();
        }
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "codecs/{name} must be the bytes built from the live wire-path limits; regenerate with {WRITE_CORPUS_ENV}=1"
        );
        let Err(invalid) = WirePath::parse(&text) else {
            panic!("codecs/{name}: {text:?} must be refused");
        };
        assert_eq!(
            invalid.rule, rule,
            "codecs/{name}: {text:?} must break `{rule}` first"
        );
        oracles::check_codec_bytes(&fixture, &bytes).unwrap_or_else(|what| {
            panic!("codecs/{name} must satisfy the wire-path oracle: {what}")
        });
    }
}

#[test]
fn fuzz_wire_path_oracle_names_the_grammar_rules_in_the_order_they_are_checked() {
    assert_eq!(
        oracles::WIRE_PATH_RULES,
        super::wire_path::RULES.map(|(id, _)| id)
    );
}

/// Usage probe: the decoder and the fuzz oracle both read the version at a
/// magic-length-relative offset. Every prefix of a valid announce, the exact one-short
/// header (magic + 1 byte) and the exact header (magic + 2 bytes) must agree between the
/// two, and the header-only announce must decode as its big-endian version with no name;
/// an announce in the old four-byte-magic layout is another application for both.
#[test]
fn usage_probe_announce_decoder_and_oracle_agree_at_the_magic_relative_header_boundary() {
    use super::announce::{ANNOUNCE_MAGIC, AnnounceAppData};
    use oracles::TAG_ANNOUNCE;

    let fixture = oracles::CodecFixture::new();
    let agree = |bytes: &[u8]| {
        let tagged = [&[TAG_ANNOUNCE][..], bytes].concat();
        oracles::check_codec_bytes(&fixture, &tagged)
            .unwrap_or_else(|what| panic!("decoder and oracle disagree on {bytes:?}: {what}"));
    };

    let full = [&ANNOUNCE_MAGIC[..], &[0x01, 0x02], b"Alex"].concat();
    for len in 0..=full.len() {
        agree(&full[..len]);
    }

    let one_short = &full[..ANNOUNCE_MAGIC.len() + 1];
    assert_eq!(
        AnnounceAppData::decode(one_short),
        None,
        "magic plus one byte is shorter than the header"
    );
    let header_only = &full[..ANNOUNCE_MAGIC.len() + 2];
    let decoded = AnnounceAppData::decode(header_only).expect("magic plus two bytes is a header");
    assert_eq!(
        decoded.version, 0x0102,
        "the version is the two bytes right after the magic, not bytes 4..6"
    );
    assert_eq!(decoded.display_name, None);
    let decoded = AnnounceAppData::decode(&full).unwrap();
    assert_eq!(decoded.version, 0x0102);
    assert_eq!(
        decoded.display_name.as_deref(),
        Some("Alex"),
        "the name starts right after the version, not at offset 6"
    );

    // A same-width magic one bit off is a foreign announce: neither the decoder nor the
    // oracle reads it, and they agree on that at every prefix.
    let mut foreign = ANNOUNCE_MAGIC;
    foreign[ANNOUNCE_MAGIC.len() - 1] ^= 0x01;
    let foreign_layout = [&foreign[..], &[0x00, 0x01], b"Alex"].concat();
    assert_eq!(AnnounceAppData::decode(&foreign_layout), None);
    agree(&foreign_layout);
    for len in 0..=foreign_layout.len() {
        agree(&foreign_layout[..len]);
    }
    // A SCOPE-prefixed announce padded to the old total header width still needs the full
    // seven header bytes.
    let mut padded = ANNOUNCE_MAGIC.to_vec();
    padded.push(0x00);
    assert_eq!(padded.len(), 6);
    assert_eq!(AnnounceAppData::decode(&padded), None);
    agree(&padded);
}

#[test]
fn fuzz_corpus_files_belong_to_the_classes_their_names_claim() {
    use super::card::StatusCard;
    use super::message::PEER_FIELDS_MAX_DEPTH;
    use super::pending::{PENDING_RECORD_VERSION, PendingRecord};
    use super::propagation_fetch::MAX_FETCHED_MESSAGE_BYTES;
    use super::r3::{MAX_R3_PAYLOAD_BYTES, R3Error, RefusalCode, RequestFrame};
    use oracles::{
        Expect, TAG_BODY, TAG_CARD, TAG_PENDING, TAG_REFUSAL_CODE, expect_envelope, unpack_whole,
    };
    use rmpv::Value;

    fn str_keys(value: &Value) -> Vec<&str> {
        value.as_map().map_or_else(Vec::new, |entries| {
            entries.iter().filter_map(|(key, _)| key.as_str()).collect()
        })
    }

    /// Containers along the deepest path; a scalar is 0.
    fn nesting(value: &Value) -> usize {
        match value {
            Value::Array(items) => 1 + items.iter().map(nesting).max().unwrap_or(0),
            Value::Map(entries) => {
                1 + entries
                    .iter()
                    .map(|(key, value)| nesting(key).max(nesting(value)))
                    .max()
                    .unwrap_or(0)
            }
            _ => 0,
        }
    }

    let read = |target: &str, name: &str| fs::read(corpus_dir(target).join(name)).unwrap();
    let envelope = |name: &str| {
        unpack_whole(&read("envelope", name)).unwrap_or_else(|| {
            panic!("envelope/{name} must be one msgpack value within the nesting budget")
        })
    };
    let frame = |name: &str| RequestFrame::decode(&read("dispatch", name));
    let frame_data = |name: &str| {
        frame(name)
            .unwrap_or_else(|err| panic!("dispatch/{name} must decode as a frame: {err:?}"))
            .data
    };
    let frame_refusal = |name: &str| match frame(name) {
        Err(R3Error::Decode(reason)) => reason,
        other => panic!("dispatch/{name} must fail to decode as a frame, got {other:?}"),
    };
    let codec = |name: &str| {
        let bytes = read("codecs", name);
        let (tag, payload) = bytes
            .split_first()
            .unwrap_or_else(|| panic!("codecs/{name} must start with a tag byte"));
        (*tag, payload.to_vec())
    };
    let codec_value = |name: &str, expected_tag: u8| {
        let (tag, payload) = codec(name);
        assert_eq!(
            tag, expected_tag,
            "codecs/{name} must carry tag {expected_tag:#04x}"
        );
        unpack_whole(&payload).unwrap_or_else(|| {
            panic!("codecs/{name} payload must be one msgpack value within the nesting budget")
        })
    };

    // MESH-ENV-014: `v` outside u16 or outside the supported window.
    for name in [
        "MESH-ENV-014-negative-v.bin",
        "MESH-ENV-014-oversize-v-65536.bin",
    ] {
        assert!(
            matches!(expect_envelope(&envelope(name)), Expect::Version(None)),
            "envelope/{name} claims MESH-ENV-014: a version refusal with no representable `v`"
        );
    }
    assert!(
        matches!(
            expect_envelope(&frame_data("MESH-ENV-014-v-two.bin")),
            Expect::Version(Some(2))
        ),
        "dispatch/MESH-ENV-014-v-two.bin claims MESH-ENV-014: a version refusal finding v=2"
    );
    assert!(
        matches!(
            expect_envelope(&frame_data("MESH-ENV-014-v-u64-max.bin")),
            Expect::Version(None)
        ),
        "dispatch/MESH-ENV-014-v-u64-max.bin claims MESH-ENV-014: a version refusal with no representable `v`"
    );

    // MESH-ENV-028: bytes the frame decoder refuses outright.
    for name in [
        "MESH-ENV-028-truncated-msgpack.bin",
        "MESH-ENV-028-nested-past-depth.bin",
    ] {
        assert!(
            unpack_whole(&read("envelope", name)).is_none(),
            "envelope/{name} claims MESH-ENV-028: not one msgpack value within the nesting budget"
        );
    }
    let nested = read("envelope", "MESH-ENV-028-nested-past-depth.bin");
    assert!(
        rmpv::decode::read_value(&mut nested.as_slice()).is_ok(),
        "envelope/MESH-ENV-028-nested-past-depth.bin claims MESH-ENV-028: well-formed msgpack that only the depth budget refuses"
    );

    // MESH-ENV-019 and MESH-ENV-017: key handling inside an otherwise valid map.
    let duplicate = envelope("MESH-ENV-019-duplicate-keys-last-wins.bin");
    assert!(
        str_keys(&duplicate)
            .iter()
            .filter(|key| **key == "v")
            .count()
            >= 2,
        "envelope/MESH-ENV-019-duplicate-keys-last-wins.bin claims MESH-ENV-019: the str key `v` at least twice"
    );
    let extension = envelope("MESH-ENV-017-unknown-extension-keys.bin");
    assert!(
        str_keys(&extension)
            .iter()
            .any(|key| !["v", "name_hash", "body"].contains(key)),
        "envelope/MESH-ENV-017-unknown-extension-keys.bin claims MESH-ENV-017: a str key the spec does not name"
    );
    assert!(
        matches!(expect_envelope(&extension), Expect::Ok { .. }),
        "envelope/MESH-ENV-017-unknown-extension-keys.bin claims MESH-ENV-017: still a valid envelope"
    );
    let non_str = envelope("MESH-ENV-017-non-str-keys-ignored.bin");
    assert!(
        non_str
            .as_map()
            .is_some_and(|entries| entries.iter().any(|(key, _)| key.as_str().is_none())),
        "envelope/MESH-ENV-017-non-str-keys-ignored.bin claims MESH-ENV-017: a key that is not a str"
    );
    assert!(
        matches!(expect_envelope(&non_str), Expect::Ok { .. }),
        "envelope/MESH-ENV-017-non-str-keys-ignored.bin claims MESH-ENV-017: still a valid envelope"
    );

    // Frame-level bounds and refusals.
    assert_eq!(
        read("dispatch", "MESH-ENV-011-max-size-frame.bin").len(),
        MAX_R3_PAYLOAD_BYTES,
        "dispatch/MESH-ENV-011-max-size-frame.bin claims MESH-ENV-011: exactly the payload limit"
    );
    assert!(
        frame("MESH-ENV-011-max-size-frame.bin").is_ok(),
        "dispatch/MESH-ENV-011-max-size-frame.bin claims MESH-ENV-011: a frame that decodes"
    );
    let Expect::Ok { body, .. } =
        expect_envelope(&frame_data("MESH-MSG-008-deeply-nested-body-fields.bin"))
    else {
        panic!(
            "dispatch/MESH-MSG-008-deeply-nested-body-fields.bin claims MESH-MSG-008: a valid envelope"
        )
    };
    let fields = body
        .as_map()
        .and_then(|entries| {
            entries
                .iter()
                .find(|(key, _)| key.as_str() == Some("fields"))
                .map(|(_, fields)| fields)
        })
        .expect("dispatch/MESH-MSG-008-deeply-nested-body-fields.bin claims MESH-MSG-008: a body with `fields`");
    assert!(
        nesting(fields) > PEER_FIELDS_MAX_DEPTH,
        "dispatch/MESH-MSG-008-deeply-nested-body-fields.bin claims MESH-MSG-008: `fields` nested past {PEER_FIELDS_MAX_DEPTH}, got {}",
        nesting(fields)
    );
    for name in [
        "MESH-ENV-028-nested-400-arrays.bin",
        "MESH-ENV-028-nested-past-rmpv-depth.bin",
    ] {
        let reason = frame_refusal(name);
        assert!(
            reason.contains("depth limit exceeded"),
            "dispatch/{name} claims MESH-ENV-028: refused by the depth budget, got {reason:?}"
        );
    }
    let reason = frame_refusal("MESH-ENV-004-trailing-bytes.bin");
    assert!(
        reason.contains("trailing"),
        "dispatch/MESH-ENV-004-trailing-bytes.bin claims MESH-ENV-004: refused for trailing bytes, got {reason:?}"
    );
    let reason = frame_refusal("MESH-ENV-003-two-element-frame.bin");
    assert!(
        reason.contains("elements"),
        "dispatch/MESH-ENV-003-two-element-frame.bin claims MESH-ENV-003: refused for its arity, got {reason:?}"
    );

    // Codec sub-targets.
    for name in [
        "MESH-EXT-004-unknown-refusal-code-0xf2.bin",
        "MESH-EXT-004-unknown-refusal-code-0xff.bin",
    ] {
        let value = codec_value(name, TAG_REFUSAL_CODE);
        assert!(
            value.as_u64().is_some() && RefusalCode::from_wire(&value).is_none(),
            "codecs/{name} claims MESH-EXT-004: a uint no refusal code is assigned to, got {value}"
        );
    }
    let card = StatusCard::from_value(&codec_value(
        "MESH-STATUS-029-unknown-state-code.bin",
        TAG_CARD,
    ))
    .expect(
        "codecs/MESH-STATUS-029-unknown-state-code.bin claims MESH-STATUS-029: a card that decodes",
    );
    assert!(
        !(0..=2).contains(&card.state.code),
        "codecs/MESH-STATUS-029-unknown-state-code.bin claims MESH-STATUS-029: a state code outside the three named, got {}",
        card.state.code
    );
    let card = codec_value("MESH-CANON-012-card-duplicate-v-first-wins.bin", TAG_CARD);
    let versions: Vec<&Value> = card.as_map().map_or_else(Vec::new, |entries| {
        entries
            .iter()
            .filter(|(key, _)| key.as_str() == Some("v"))
            .map(|(_, value)| value)
            .collect()
    });
    assert!(
        versions.len() >= 2 && versions[0].as_u64() == Some(1),
        "codecs/MESH-CANON-012-card-duplicate-v-first-wins.bin claims MESH-CANON-012: `v` twice with 1 first, got {versions:?}"
    );
    let body = codec_value("MESH-MSG-012-body-not-a-map.bin", TAG_BODY);
    assert!(
        body.as_map().is_none(),
        "codecs/MESH-MSG-012-body-not-a-map.bin claims MESH-MSG-012: a body that is not a map, got {body}"
    );
    let (tag, payload) = codec("MESH-CODE-005-pending-unknown-field.bin");
    assert_eq!(
        tag, TAG_PENDING,
        "codecs/MESH-CODE-005-pending-unknown-field.bin must carry the pending tag"
    );
    let text = String::from_utf8(payload)
        .expect("codecs/MESH-CODE-005-pending-unknown-field.bin claims MESH-CODE-005: UTF-8 jsonl");
    let line = text.lines().find(|line| !line.trim().is_empty()).expect(
        "codecs/MESH-CODE-005-pending-unknown-field.bin claims MESH-CODE-005: a record line",
    );
    let object: serde_json::Map<String, serde_json::Value> = serde_json::from_str(line).expect(
        "codecs/MESH-CODE-005-pending-unknown-field.bin claims MESH-CODE-005: a JSON object",
    );
    assert_eq!(
        object.get("version").and_then(serde_json::Value::as_u64),
        Some(PENDING_RECORD_VERSION),
        "codecs/MESH-CODE-005-pending-unknown-field.bin claims MESH-CODE-005: a current-version record"
    );
    assert!(
        object.contains_key("later_field"),
        "codecs/MESH-CODE-005-pending-unknown-field.bin claims MESH-CODE-005: a key the PendingRecord layout does not know, got {:?}",
        object.keys().collect::<Vec<_>>()
    );
    let refusal = serde_json::from_str::<PendingRecord>(line)
        .err()
        .map(|error| error.to_string())
        .expect("codecs/MESH-CODE-005-pending-unknown-field.bin claims MESH-CODE-005: a current-version record the `PendingRecord` layout refuses");
    assert!(
        refusal.contains("later_field"),
        "codecs/MESH-CODE-005-pending-unknown-field.bin claims MESH-CODE-005: a refusal naming the unknown key, got {refusal}"
    );

    // Receipt bodies (kind 0x00).
    let oversize = read("receipt", "MESH-PROP-028-oversize-131073.bin");
    assert!(
        oversize.first() == Some(&oracles::RECEIPT_BODY)
            && oversize.len() - 1 == MAX_FETCHED_MESSAGE_BYTES + 1,
        "receipt/MESH-PROP-028-oversize-131073.bin claims MESH-PROP-028: one body a byte past the limit, got kind {:?} and {} bytes",
        oversize.first(),
        oversize.len().saturating_sub(1)
    );
    let wrong_prefix = read("receipt", "MESH-PROP-030-wrong-destination-prefix.bin");
    assert!(
        wrong_prefix.first() == Some(&oracles::RECEIPT_BODY)
            && wrong_prefix.len() > 17
            && wrong_prefix[1..17].iter().any(|byte| *byte != 0),
        "receipt/MESH-PROP-030-wrong-destination-prefix.bin claims MESH-PROP-030: one body whose 16-byte destination prefix is not all zero"
    );
}

#[test]
fn fuzz_source_never_gates_on_platform_or_ignores() {
    let needles = [
        ["cfg(", "unix)"].concat(),
        ["cfg(", "windows)"].concat(),
        ["target", "_os"].concat(),
        ["std::", "os::"].concat(),
        ["#[", "ignore"].concat(),
    ];
    let fuzz = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("mesh")
        .join("fuzz");
    let sources: Vec<PathBuf> = crate::mesh::test_support::rust_sources()
        .into_iter()
        .filter(|path| path.starts_with(&fuzz))
        .collect();
    assert!(
        sources.len() >= 2,
        "expected mod.rs and oracles.rs under {}",
        fuzz.display()
    );
    for path in sources {
        let text = fs::read_to_string(&path).unwrap();
        for needle in &needles {
            assert!(
                !text.contains(needle.as_str()),
                "{} must run identically on every platform; found {needle:?}",
                path.display()
            );
        }
    }
}

#[test]
fn fuzz_codec_tag_table_in_the_readme_matches_the_code() {
    use super::announce::ANNOUNCE_MAGIC;

    let readme = include_str!("corpus/README.md");
    let table = &readme[readme.find("### Codec tag bytes").unwrap()..];
    let table = &table[..table.find("\n## ").unwrap_or(table.len())];
    for tag in oracles::CODEC_TAGS {
        let row = format!("| `{tag:#04x}` |");
        assert!(table.contains(&row), "the README codec table lacks {row}");
    }
    assert_eq!(table.matches("\n| `0x").count(), oracles::CODEC_TAGS.len());
    let announce_prefix = format!("| `{:#04x}` |", oracles::TAG_ANNOUNCE);
    let announce_row = table
        .lines()
        .find(|line| line.starts_with(&announce_prefix))
        .unwrap();
    let magic = format!("`{}`", std::str::from_utf8(&ANNOUNCE_MAGIC).unwrap());
    assert!(
        announce_row.contains(&magic),
        "the README announce row does not spell {magic}: {announce_row}"
    );
    for pin in [
        WRITE_CORPUS_ENV,
        "fuzz_codec_corpus_files_carrying_wire_identifiers_are_built_from_the_live_constants",
        "fuzz_codec_corpus_wire_path_seeds_break_exactly_the_rule_their_name_claims",
    ] {
        assert!(readme.contains(pin), "the README does not name {pin}");
    }
}

#[test]
fn fuzz_iteration_setting_reads_a_count_and_falls_back_on_anything_else() {
    assert_eq!(parse_iters(None), Ok(None));
    assert_eq!(parse_iters(Some(OsStr::new(""))), Ok(None));
    assert_eq!(parse_iters(Some(OsStr::new("  \t"))), Ok(None));
    assert_eq!(parse_iters(Some(OsStr::new(" 250 "))), Ok(Some(250)));
    assert_eq!(parse_iters(Some(OsStr::new("0"))), Ok(Some(0)));
    assert_eq!(
        parse_iters(Some(OsStr::new("lots"))),
        Err("lots".to_string())
    );
    assert_eq!(parse_iters(Some(OsStr::new("-1"))), Err("-1".to_string()));
}

#[test]
fn fuzz_seed_setting_reads_decimal_or_hex_and_falls_back_on_anything_else() {
    assert_eq!(parse_seed(None), Ok(None));
    assert_eq!(parse_seed(Some(OsStr::new(""))), Ok(None));
    assert_eq!(parse_seed(Some(OsStr::new("42"))), Ok(Some(42)));
    assert_eq!(parse_seed(Some(OsStr::new("0x2A"))), Ok(Some(42)));
    assert_eq!(parse_seed(Some(OsStr::new(" 0Xff "))), Ok(Some(255)));
    assert_eq!(
        parse_seed(Some(OsStr::new("0xzz"))),
        Err("0xzz".to_string())
    );
    assert_eq!(
        parse_seed(Some(OsStr::new("seed"))),
        Err("seed".to_string())
    );
}

#[test]
fn fuzz_splitmix_is_deterministic_and_matches_the_reference_stream() {
    // The published splitmix64 output for seed 0 is 0xE220A8397B1DCDAF, then 0x6E789E6AA1B965F4.
    let mut rng = SplitMix::new(0);
    assert_eq!(rng.next(), 0xE220_A839_7B1D_CDAF);
    assert_eq!(rng.next(), 0x6E78_9E6A_A1B9_65F4);
    let mut a = SplitMix::new(DEFAULT_SEED);
    let mut b = SplitMix::new(DEFAULT_SEED);
    let mut bytes_a = [0u8; 40];
    let mut bytes_b = [0u8; 40];
    a.fill_bytes(&mut bytes_a);
    b.fill_bytes(&mut bytes_b);
    assert_eq!(bytes_a, bytes_b);
    assert_ne!(a.fork().next(), a.fork().next());
}
