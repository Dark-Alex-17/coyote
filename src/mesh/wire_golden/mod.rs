//! Golden wire bytes: the mesh protocol's bytes recorded as committed hex fixtures, so a
//! change to any codec is judged against what the wire carried when the fixture was
//! written rather than against the encoder compared with itself.
//!
//! `inputs.hex` holds the fixed-data input of every typed conformance row, keyed by table
//! and row name. `encoders.hex` holds the msgpack bytes each production encoder emits over
//! a fixed sample: fixed identities, literal timestamps and literal message ids. A message
//! built for the propagation route is recorded as the array of its title, content and
//! fields.
//!
//! Each line is `<record name> <lowercase hex>`, sorted by name; an empty record is the
//! name alone. A record that no longer matches means the wire changed. When that is
//! intended, regenerate with `cargo test --all -- --ignored mesh::wire_golden` and commit
//! the new fixtures with the change.

use super::access::{
    AccessOutcome, AccessRefusal, access_body, access_message, access_reply, decision_reply,
    validate_access,
};
use super::card::build_card;
use super::events::{AccessDecision, MeshHooks};
use super::fetch::{FetchHandler, FetchServing, ListHandler, ShareSource};
use super::grants::GrantStore;
use super::knock::{KnockIntro, knock_message};
use super::message::{
    Disposition, OutboundPeer, PEER_WIRE_VERSION, PartLimits, PeerKind, RawPart, peer_lxmf_message,
    to_request_body,
};
use super::propagation::OutboundMessage;
use super::protocol::VersionRefusal;
use super::request::{
    AdmittedRequest, Envelope, FETCH_PATH, Handler, LIST_PATH, NAME_HASH_LEN, OriginName, PathHash,
    RefusalCode, Reply, RequestId,
};
use super::shares::SHARES_FILE_VERSION;
use super::snapshot::{BriefState, MeshSnapshot, RepoInfo, TurnState};
use super::test_support::TempDir;
use super::{decode_hex, hex_lower, mesh_config_dir};
use crate::config::mesh_config::MeshBrief;
use crate::config::todo::{TodoItem, TodoList};

use rmpv::Value;
use rns_transport::hash::AddressHash;
use rns_transport::identity::PrivateIdentity;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REGENERATE: &str = "cargo test --all -- --ignored mesh::wire_golden";

/// Conformance rows whose input is left out of `inputs.hex`, with the reason.
const UNRECORDED_INPUTS: &[(&str, &str)] = &[
    (
        "share_vectors/ok-reply-with-bytes-at-the-file-bound-is-read",
        "4 MiB of filler bytes at the fetch file bound",
    ),
    (
        "share_vectors/ok-reply-with-bytes-over-the-file-bound-is-oversize",
        "4 MiB of filler bytes past the fetch file bound",
    ),
    (
        "share_vectors/shares-page-over-the-page-size-is-cut-to-the-page-size",
        "77 KB of generated entries past the list page size",
    ),
];

const ORIGIN: OriginName = OriginName([7u8; NAME_HASH_LEN]);

/// `2026-09-21T14:13:20Z`, the fixed clock every sample is built against.
fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_790_000_000)
}

struct Fixture {
    file: &'static str,
    records: Vec<(String, Vec<u8>)>,
}

/// Both fixture files with their records sorted by name, as the generator writes them and
/// the checker reads them back.
fn fixtures() -> [Fixture; 2] {
    let mut fixtures = [
        Fixture {
            file: "inputs.hex",
            records: input_records(),
        },
        Fixture {
            file: "encoders.hex",
            records: encoder_records(),
        },
    ];
    for fixture in &mut fixtures {
        fixture.records.sort_by(|a, b| a.0.cmp(&b.0));
    }
    fixtures
}

fn render(records: &[(String, Vec<u8>)]) -> String {
    records
        .iter()
        .map(|(name, bytes)| match bytes.as_slice() {
            [] => format!("{name}\n"),
            bytes => format!("{name} {}\n", hex_lower(bytes)),
        })
        .collect()
}

fn fixture_path(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("mesh")
        .join("wire_golden")
        .join(file)
}

/// The committed text with line endings normalised, since a Windows checkout may hand the
/// fixtures over with CRLF.
fn committed_text(file: &str) -> String {
    let path = fixture_path(file);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| {
            panic!(
                "{} could not be read ({err}); generate it with `{REGENERATE}`",
                path.display()
            )
        })
        .replace("\r\n", "\n")
}

fn committed_records(file: &str) -> Vec<(String, Vec<u8>)> {
    committed_text(file)
        .lines()
        .map(|line| {
            let (name, hex) = line.split_once(' ').unwrap_or((line, ""));
            let bytes = decode_hex(hex)
                .unwrap_or_else(|| panic!("{file}: the record {name} is not lowercase hex"));
            (name.to_string(), bytes)
        })
        .collect()
}

fn duplicates<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = BTreeSet::new();
    names
        .into_iter()
        .filter(|name| !seen.insert(*name))
        .collect()
}

fn packed(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, value).unwrap();
    bytes
}

fn packed_message(message: OutboundMessage) -> Vec<u8> {
    packed(&Value::Array(vec![
        message.title.map_or(Value::Nil, Value::Binary),
        Value::Binary(message.content),
        message.fields.unwrap_or(Value::Nil),
    ]))
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn input_records() -> Vec<(String, Vec<u8>)> {
    let unrecorded: BTreeSet<&str> = UNRECORDED_INPUTS.iter().map(|(name, _)| *name).collect();
    crate::mesh::conformance::golden_inputs()
        .into_iter()
        .filter(|(name, _)| !unrecorded.contains(name.as_str()))
        .collect()
}

fn encoder_records() -> Vec<(String, Vec<u8>)> {
    [
        message_records(),
        card_records(),
        access_records(),
        knock_records(),
        envelope_records(),
        refusal_records(),
        share_records(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `peer` with the id its constructor minted replaced by `id`.
fn with_id(mut peer: OutboundPeer, id: &str) -> OutboundPeer {
    peer.id = id.to_string();
    peer
}

fn peer_samples() -> Vec<(&'static str, OutboundPeer)> {
    let minimal = OutboundPeer::new(PeerKind::Message, "hello", None, None, None).unwrap();
    let every_optional_key = OutboundPeer::with_parts(
        PeerKind::Reply,
        "the answer",
        Some("Re: question"),
        Some("q-1"),
        Some(serde_json::json!({ "n": 1 })),
        vec![
            RawPart::Text {
                text: "aside".to_string(),
            },
            RawPart::Data {
                data: serde_json::json!({ "k": "v" }),
            },
        ],
        &PartLimits::default(),
    )
    .unwrap()
    .with_thread(Some("t-1".into()))
    .unwrap()
    .with_disposition(Disposition::Refused, Some(60));
    let ask = OutboundPeer::new(PeerKind::Ask, "a question", Some("Question"), None, None)
        .unwrap()
        .with_thread(Some("t-2".into()))
        .unwrap();
    let bulletin = OutboundPeer::new(PeerKind::Bulletin, "news", None, None, None).unwrap();
    let decided = |decision, expires, standing| {
        decision_reply(
            "r-1",
            "t-3",
            2,
            decision,
            expires,
            standing,
            &PartLimits::default(),
        )
        .unwrap()
    };
    vec![
        ("minimal", with_id(minimal, "m-1")),
        ("every-optional-key", with_id(every_optional_key, "m-2")),
        ("ask", with_id(ask, "m-3")),
        ("bulletin", with_id(bulletin, "m-4")),
        (
            "decision-granted-until",
            with_id(
                decided(
                    AccessDecision::Granted,
                    Some(now() + Duration::from_secs(3600)),
                    false,
                ),
                "m-5",
            ),
        ),
        (
            "decision-granted-standing",
            with_id(decided(AccessDecision::Granted, None, true), "m-6"),
        ),
        (
            "decision-denied",
            with_id(decided(AccessDecision::Denied, None, false), "m-7"),
        ),
    ]
}

fn message_records() -> Vec<(String, Vec<u8>)> {
    peer_samples()
        .into_iter()
        .flat_map(|(sample, peer)| {
            [
                (
                    format!("to_request_body/{sample}"),
                    packed(&to_request_body(&peer, 1_790_000_000.5)),
                ),
                (
                    format!("peer_lxmf_message/{sample}"),
                    packed_message(peer_lxmf_message(&peer, &ORIGIN)),
                ),
            ]
        })
        .collect()
}

fn snapshot() -> MeshSnapshot {
    MeshSnapshot {
        objective: Some("ship the release".into()),
        state: TurnState::Working {
            since: now() - Duration::from_secs(90),
        },
        repo: Some(RepoInfo {
            root: PathBuf::from("/home/u/proj"),
            branch: Some("feat/cards".into()),
        }),
        todo: TodoList {
            goal: "land the cards".into(),
            todos: vec![
                TodoItem {
                    id: 1,
                    desc: "encode".into(),
                    done: true,
                },
                TodoItem {
                    id: 2,
                    desc: "decode".into(),
                    done: false,
                },
            ],
        },
        brief: BriefState {
            mode: MeshBrief::Auto,
            text: None,
            digest_generated_at: None,
        },
        session: Some("review".into()),
        cwd: PathBuf::from("/home/u/proj"),
        captured_at: now() - Duration::from_secs(7),
    }
}

fn card_records() -> Vec<(String, Vec<u8>)> {
    let idle = MeshSnapshot {
        state: TurnState::Idle {
            since: now() - Duration::from_secs(30),
        },
        repo: None,
        todo: TodoList::default(),
        session: None,
        ..snapshot()
    };
    let caps = ["list".to_string(), "fetch".to_string()];
    let cards = [
        (
            "no-snapshot",
            build_card(None, None, None, None, None, &[], now()),
        ),
        (
            "idle",
            build_card(Some(&idle), None, None, Some("Alex"), None, &[], now()),
        ),
        (
            "working-every-key",
            build_card(
                Some(&snapshot()),
                None,
                None,
                Some("Alex"),
                Some("on call this week"),
                &caps,
                now(),
            )
            .with_configured_names(Some("Alex Smith"), None, true),
        ),
        (
            "objective-override",
            build_card(
                Some(&snapshot()),
                Some("the override"),
                Some("the digest"),
                None,
                None,
                &[],
                now(),
            ),
        ),
    ];
    cards
        .into_iter()
        .map(|(sample, card)| (format!("status_card/{sample}"), packed(&card.to_value())))
        .collect()
}

fn access_records() -> Vec<(String, Vec<u8>)> {
    let one = validate_access("a-1", vec!["src/x.rs".to_string()], "need the struct").unwrap();
    let several = validate_access(
        "a-2",
        vec![
            "docs/guide.md".to_string(),
            "README.md".to_string(),
            "docs/guide.md".to_string(),
        ],
        "reading up",
    )
    .unwrap();
    let mut records = Vec::new();
    for (sample, request) in [("one-path", &one), ("several-paths", &several)] {
        records.push((
            format!("access_body/{sample}"),
            packed(&access_body(request)),
        ));
        records.push((
            format!("access_message/{sample}"),
            packed_message(access_message(request, &ORIGIN)),
        ));
    }
    for (sample, outcome) in [
        ("pending", AccessOutcome::Pending),
        (
            "granted",
            AccessOutcome::Granted {
                expires: 1_790_003_600.0,
            },
        ),
        (
            "refused-duplicate",
            AccessOutcome::Refused(AccessRefusal::Duplicate),
        ),
        (
            "refused-too-many-pending",
            AccessOutcome::Refused(AccessRefusal::TooManyPending),
        ),
    ] {
        records.push((
            format!("access_reply/{sample}"),
            packed(&access_reply("a-1", &outcome)),
        ));
    }
    records
}

fn knock_records() -> Vec<(String, Vec<u8>)> {
    [
        ("plain", "hello, it's Alex from the infra team"),
        ("empty", ""),
    ]
    .into_iter()
    .flat_map(|(sample, text)| {
        let intro = KnockIntro::new(text).unwrap();
        [
            (
                format!("knock_body/{sample}"),
                packed(&intro.to_request_body()),
            ),
            (
                format!("knock_message/{sample}"),
                packed_message(knock_message(&intro, &ORIGIN)),
            ),
        ]
    })
    .collect()
}

fn envelope_records() -> Vec<(String, Vec<u8>)> {
    let request = validate_access("a-1", vec!["src/x.rs".to_string()], "need the struct").unwrap();
    [
        ("nil-body", Value::Nil),
        ("access-body", access_body(&request)),
        (
            "knock-body",
            KnockIntro::new("hello").unwrap().to_request_body(),
        ),
    ]
    .into_iter()
    .map(|(sample, body)| {
        (
            format!("envelope/{sample}"),
            packed(&Envelope::new(ORIGIN, body).into_value()),
        )
    })
    .collect()
}

fn refusal_records() -> Vec<(String, Vec<u8>)> {
    let versions = [("none", None), ("zero", Some(0)), ("future", Some(99))]
        .into_iter()
        .map(|(sample, found)| {
            (
                format!("version_refusal/{sample}"),
                packed(&VersionRefusal::current(found).to_value()),
            )
        });
    let codes = [
        ("no-identity", RefusalCode::NoIdentity),
        ("no-access", RefusalCode::NoAccess),
        ("invalid-key", RefusalCode::InvalidKey),
        ("invalid-data", RefusalCode::InvalidData),
        ("invalid-stamp", RefusalCode::InvalidStamp),
        ("throttled", RefusalCode::Throttled),
        ("not-found", RefusalCode::NotFound),
        ("timeout", RefusalCode::Timeout),
    ]
    .into_iter()
    .map(|(sample, code)| (format!("refusal_code/{sample}"), packed(&code.to_wire())));
    versions.chain(codes).collect()
}

struct Root {
    root: PathBuf,
    serving: Arc<FetchServing>,
}

impl ShareSource for Root {
    fn share_root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }

    fn serving(&self) -> Option<Arc<FetchServing>> {
        Some(Arc::clone(&self.serving))
    }
}

/// Larger than `SERVING_LIMIT`, so fetching it is refused `too_large`.
const BIG_FILE: &str = "docs/big.md";
const SERVING_LIMIT: u64 = 64;
const README: &[u8] = b"# Golden\n\nFixed bytes for the fetch reply.\n";

/// The `/list` and `/fetch` replies of a share root holding fixed files with a fixed
/// mtime, served to a peer with a fixed identity. The handlers are reached the way the
/// dispatcher reaches them, since the reply encoders behind them are private.
fn share_records() -> Vec<(String, Vec<u8>)> {
    let tmp = TempDir::new("wire-golden-share");
    let root = tmp.path.join("workspace");
    for (relative, bytes) in [
        ("README.md", README.to_vec()),
        ("docs/guide.md", b"guide\n".to_vec()),
        (BIG_FILE, vec![b'x'; SERVING_LIMIT as usize + 1]),
    ] {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(now())
            .unwrap();
    }
    let config_dir = tmp.path.join("config");
    let shares = mesh_config_dir(&config_dir).join("shares.yaml");
    fs::create_dir_all(shares.parent().unwrap()).unwrap();
    fs::write(
        &shares,
        format!(
            "version: {SHARES_FILE_VERSION}\nallow:\n- pattern: 'README.md'\n- pattern: 'docs/*.md'\n"
        ),
    )
    .unwrap();
    let cache_dir = tmp.path.join("cache");
    let source: Arc<dyn ShareSource> = Arc::new(Root {
        root,
        serving: Arc::new(FetchServing::new(
            config_dir,
            cache_dir.clone(),
            None,
            SERVING_LIMIT,
            GrantStore::new(&cache_dir, "inst"),
            MeshHooks::default(),
        )),
    });
    let identity = PrivateIdentity::from_private_key_bytes(&[11u8; 64]).unwrap();
    let request = |path: &str, body: Vec<(&str, Value)>| {
        let mut entries = vec![(Value::from("v"), Value::from(PEER_WIRE_VERSION))];
        entries.extend(
            body.into_iter()
                .map(|(key, value)| (Value::from(key), value)),
        );
        AdmittedRequest {
            link_id: AddressHash::new([0x11; 16]),
            identity: *identity.as_identity(),
            destination_hash: AddressHash::new([0x22; 16]),
            request_id: RequestId::from([1u8; 16]),
            path_hash: PathHash::of(path),
            body: Value::Map(entries),
        }
    };
    let weak: Weak<dyn ShareSource> = Arc::downgrade(&source);
    let list = ListHandler::new(weak.clone());
    let fetch = FetchHandler::new(weak);
    let readme_sha256: [u8; 32] = Sha256::digest(README).into();
    let lists = [
        ("everything", vec![]),
        ("prefix", vec![("prefix", Value::from("docs/"))]),
    ]
    .into_iter()
    .map(|(sample, body)| {
        let reply = block_on(list.handle(request(LIST_PATH, body)));
        (format!("list_reply/{sample}"), packed(&reply_value(reply)))
    });
    let fetches = [
        ("ok", vec![("path", Value::from("README.md"))]),
        (
            "not-modified",
            vec![
                ("path", Value::from("README.md")),
                ("if_sha256", Value::Binary(readme_sha256.to_vec())),
            ],
        ),
        ("not-shared", vec![("path", Value::from("secret.txt"))]),
        ("too-large", vec![("path", Value::from(BIG_FILE))]),
        ("invalid-path", vec![("path", Value::from("../outside.md"))]),
    ]
    .into_iter()
    .map(|(sample, body)| {
        let reply = block_on(fetch.handle(request(FETCH_PATH, body)));
        (format!("fetch_reply/{sample}"), packed(&reply_value(reply)))
    });
    lists.chain(fetches).collect()
}

fn reply_value(reply: Reply) -> Value {
    match reply {
        Reply::Value(value) | Reply::Settled { value, .. } => value,
        Reply::Code(code) => panic!("the handler refused the sample with {code:?}"),
        Reply::Silent => panic!("the handler stayed silent on the sample"),
    }
}

#[test]
fn every_golden_record_matches_the_committed_bytes() {
    for fixture in fixtures() {
        let names = fixture.records.iter().map(|(name, _)| name.as_str());
        assert_eq!(
            duplicates(names),
            Vec::<&str>::new(),
            "{} would hold these record names twice",
            fixture.file
        );
        let committed = committed_records(fixture.file);
        assert_eq!(
            duplicates(committed.iter().map(|(name, _)| name.as_str())),
            Vec::<&str>::new(),
            "{} holds these record names twice",
            fixture.file
        );
        for (name, bytes) in &fixture.records {
            let Some((_, recorded)) = committed.iter().find(|(committed, _)| committed == name)
            else {
                panic!(
                    "{}: the record {name} is not committed; if it is new, regenerate with `{REGENERATE}`",
                    fixture.file
                );
            };
            assert_eq!(
                hex_lower(bytes),
                hex_lower(recorded),
                "{}: the record {name} no longer matches its committed wire bytes; if the wire change is intended, regenerate with `{REGENERATE}`",
                fixture.file
            );
        }
        let produced: BTreeSet<&str> = fixture
            .records
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        let orphaned: Vec<&str> = committed
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| !produced.contains(name))
            .collect();
        assert_eq!(
            orphaned,
            Vec::<&str>::new(),
            "{}: these committed records are no longer produced; regenerate with `{REGENERATE}`",
            fixture.file
        );
    }
}

#[test]
fn the_committed_fixtures_are_exactly_what_the_generator_renders() {
    for fixture in fixtures() {
        assert!(
            committed_text(fixture.file) == render(&fixture.records),
            "{} differs from what the generator renders; regenerate with `{REGENERATE}`",
            fixture.file
        );
    }
}

#[test]
fn every_unrecorded_input_names_a_conformance_row() {
    let rows: BTreeSet<String> = crate::mesh::conformance::golden_inputs()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    for (name, reason) in UNRECORDED_INPUTS {
        assert!(
            rows.contains(*name),
            "{name} is left unrecorded ({reason}) but no table has that row"
        );
    }
}

#[test]
#[ignore] // writes the fixtures; run with --ignored to regenerate
fn regenerate_the_golden_fixtures() {
    for fixture in fixtures() {
        fs::write(fixture_path(fixture.file), render(&fixture.records)).unwrap();
    }
}
