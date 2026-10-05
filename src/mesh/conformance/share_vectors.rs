//! Requirement-id keyed vectors for the share side of the protocol: the wire-path grammar
//! of section 10.13 and the three places that reuse it, the share set and the grant store
//! of section 10.17, the `/list` and `/fetch` handlers and requesters of sections 10.14
//! and 10.15, and the on-disk versioning of section 14.1 for the stores they read. Every
//! row runs in-process against this crate's own `WirePath`, `ShareSet`, `GrantStore`,
//! `ListHandler`, `FetchHandler`, `SharesPage`, `R3Client` and record types, on a share
//! root the row builds in a temporary directory.
//!
//! Every row names the id it exercises and the receiver action the spec mandates for it. A
//! row written faithfully from the spec that the code does not honour is kept as written
//! and flagged with `known_divergence`; the executor prints such a row instead of asserting
//! it, and fails when the flag goes stale.

use super::{Kind, Listed};
use crate::config::WORKSPACE_COYOTE_DIR_NAME;
use crate::config::mesh_config::MAX_FETCH_FILE_BYTES;
use crate::hooks::HookEvent;
use crate::mesh::access::validate_access;
use crate::mesh::card::{STATE_IDLE, STATUS_CARD_VERSION, StatusCard};
use crate::mesh::events::{MeshHooks, RecordingHookSink, env_value};
use crate::mesh::fetch::{
    CURSOR_MAX_BYTES, FILE_FETCH_REQUEST_TIMEOUT, FetchError, FetchHandler, FetchReply,
    FetchServing, Fetched, FileReader, LIST_PAGE_HEADROOM, ListHandler,
    SINGLE_SEGMENT_FETCH_CEILING, ShareSource, SharesPage, field, read_fetch_reply, rule_of,
};
use crate::mesh::grants::{
    DEFAULT_GRANT_TTL, DEFAULT_GRANT_USES, GRANT_MAX_PATHS, GRANT_RECORD_VERSION, GrantStore,
};
use crate::mesh::inbox::{InboxStaging, StageError, inbox_root};
use crate::mesh::message::{
    PEER_REQUEST_TIMEOUT, PEER_WIRE_VERSION, PartLimits, RawPart, admit_parts,
};
use crate::mesh::pending::{INBOUND_RECORD_VERSION, InboundKind, InboundRecord};
use crate::mesh::r3::{
    AdmittedRequest, DispatchError, Dispatcher, Envelope, FETCH_PATH, Handler, InboundRequest,
    KnockEvent, KnockSink, LIST_PATH, MAX_FETCH_RESPONSE_BYTES, MAX_R3_PAYLOAD_BYTES,
    NAME_HASH_LEN, OriginName, PathHash, R3Client, R3Error, RESPONSE_FRAME_PREFIX, RefusalCode,
    Reply, RequestHandler, RequestId, ResponseFrame, STATUS_PATH, SizeBranch,
};
use crate::mesh::schema::{Remedy, version_refusal};
use crate::mesh::shares::{
    DEFAULT_LIST_WALK_BOUND, LIST_PAGE_SIZE, Layer, Mutation, PeerRef, SHARES_FILE_VERSION, Served,
    ServedFile, ShareLocations, ShareSet, Verdict, Via, WriteScope, list_cursor,
    probe_case_insensitive, validate_override, validate_pattern, write_target,
};
use crate::mesh::test_support::{TempDir, TrustList, siblings_of};
use crate::mesh::wire_path::{
    RULES, WIRE_PATH_MAX_BYTES, WIRE_PATH_MAX_SEGMENTS, WirePath, is_rule_id,
};
use crate::mesh::{hex_lower, mesh_config_dir, rfc3339_utc};
use crate::testing::{debug_snapshot, install_log_collector, warn_snapshot};

use rand_core::OsRng;
use rmpv::Value;
use rns_transport::destination::link::LinkId;
use rns_transport::hash::AddressHash;
use rns_transport::identity::PrivateIdentity;
use sha2::{Digest, Sha256};
use std::fmt::Debug;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};
use tokio::sync::oneshot::error::TryRecvError;

/// One requirement id, one input, one mandated receiver action.
struct Vector {
    id: &'static str,
    kind: Kind,
    case: Case,
    /// Set when the row is faithful to the spec and the code does not honour it; the
    /// executor prints the observed behaviour instead of asserting. Never set to make a
    /// row pass.
    known_divergence: Option<&'static str>,
}

/// A scenario too stateful for a data row: it builds its own fixture and reports the
/// first expectation it misses.
type Check = fn() -> Result<(), String>;

/// The input and expectation of a row, one variant per surface under test. The variant
/// name is the family the coverage report groups rows by.
enum Case {
    /// `WirePath::parse` and the decoders that reuse it, section 10.13.
    WirePath(WireProbe),
    /// `ShareSet` on a share root built for the row, section 10.17.
    ShareSet(ShareProbe),
    /// `GrantStore` in a cache directory built for the row, section 10.17.
    GrantStore(Check),
    /// The version discipline of the share files, the grant store and the inbound
    /// record, section 14.1.
    StoreSchema(SchemaProbe),
    /// `ListHandler` over a share root built for the row, section 10.14.
    ListServe(ListProbe),
    /// `FetchHandler` over a share root built for the row, section 10.15.
    FetchServe(FetchProbe),
    /// The requester's side of both paths: `SharesPage`, the inbox staging, the response
    /// bound of `R3Client` and the card keys of section 9 that announce the paths.
    FetchClient(ClientProbe),
}

enum WireProbe {
    /// The first rule the text breaks, or none.
    Parse {
        text: String,
        expect: Result<(), &'static str>,
    },
    /// Whether `is_rule_id` knows the name.
    RuleId { rule: &'static str, known: bool },
    /// What a requester keeps of a reply's `rule`.
    RuleOf {
        value: Value,
        expect: Option<&'static str>,
    },
    /// Whether `validate_access` admits `paths`.
    AccessPaths {
        paths: &'static [&'static str],
        accepted: bool,
    },
    /// Whether a `/list` entry naming `path` is kept by the requester.
    ListEntry { path: &'static str, kept: bool },
    /// Whether a `file` part with this `name` and `ref.path` is admitted.
    FilePart {
        name: &'static str,
        reference: Option<&'static str>,
        kept: bool,
    },
}

/// Which peer a share is judged for.
#[derive(Clone, Copy)]
enum Who {
    Named,
    Other,
}

impl Who {
    fn hashes(self) -> (String, String) {
        match self {
            Self::Named => (fake_hash(0x1a), fake_hash(0x2b)),
            Self::Other => (fake_hash(0x3c), fake_hash(0x4d)),
        }
    }
}

enum ShareProbe {
    /// Both files written as given (empty is no file), the files created under the root,
    /// then `verdict_for` on `path` for `who`.
    Verdict {
        global: String,
        workspace: String,
        files: Vec<String>,
        who: Who,
        path: String,
        folded: bool,
        expect: Option<Verdict>,
    },
    /// A file that loads without a refusal.
    Loads {
        layer: Layer,
        text: String,
    },
    /// A file whose refusal names it.
    Poisoned {
        layer: Layer,
        text: String,
    },
    /// `validate_pattern` on the text.
    Pattern {
        text: &'static str,
        valid: bool,
    },
    /// `validate_override` on the text.
    Override {
        text: &'static str,
        valid: bool,
    },
    /// The layer a mutation lands in.
    WriteTarget {
        workspace_exists: bool,
        scope: WriteScope,
        layer: Layer,
    },
    Check(Check),
}

enum SchemaProbe {
    /// A share file whose refusal carries every phrase; none means it loads.
    SharesFile {
        text: String,
        phrases: &'static [&'static str],
    },
    /// A written grant line edited as a hand or another build would leave it; the
    /// refusal carries every phrase, none meaning the store still reads.
    GrantLine {
        edit: fn(&mut serde_json::Value),
        phrases: &'static [&'static str],
    },
    /// An inbound line and what it reads as, `None` when it is refused.
    InboundLine {
        json: String,
        expect: Option<(InboundKind, &'static [&'static str], &'static str)>,
    },
    Check(Check),
}

enum ListProbe {
    /// The handler's answer to `body` on a root holding `files`, every one allowed.
    Request {
        files: &'static [&'static str],
        body: Value,
        expect: ListAnswer,
    },
    Check(Check),
}

#[derive(Debug, PartialEq)]
enum ListAnswer {
    /// `Reply::Code(InvalidData)`.
    Refused,
    Page {
        paths: Vec<&'static str>,
        next: Option<String>,
    },
}

enum FetchProbe {
    /// The handler's answer to `body` on a root holding `files`, every one allowed.
    Request {
        files: &'static [&'static str],
        body: Value,
        expect: FetchAnswer,
    },
    Check(Check),
}

#[derive(Debug, PartialEq)]
enum FetchAnswer {
    /// `Reply::Code(InvalidData)`.
    Refused,
    /// A typed status reply; `invalid_path` names the rule.
    Status {
        status: &'static str,
        rule: Option<&'static str>,
    },
}

enum ClientProbe {
    /// `SharesPage::from_value` on a peer's reply: how many entries are kept and the
    /// cursor, or the key that made the reply malformed.
    Page {
        value: Value,
        expect: Result<(usize, Option<String>), &'static str>,
    },
    /// Whether `SharesPage::entry` keeps the entry.
    Entry {
        value: Value,
        kept: bool,
    },
    /// What `read_fetch_reply` makes of a peer's `/fetch` reply.
    Reply {
        value: Value,
        expect: Result<FetchReply, ReplyError>,
    },
    Check(Check),
}

/// The `FetchError` a reply reader can end in, comparable; `Transport` and `Stage` never
/// come out of a pure read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyError {
    NotServed,
    Malformed(&'static str),
    Oversize(usize),
    Corrupt,
    UnknownStatus,
}

fn reply_error(err: &FetchError) -> Result<ReplyError, String> {
    Ok(match err {
        FetchError::NotServed => ReplyError::NotServed,
        FetchError::Malformed(key) => ReplyError::Malformed(key),
        FetchError::Oversize { len } => ReplyError::Oversize(*len),
        FetchError::Corrupt => ReplyError::Corrupt,
        FetchError::UnknownStatus => ReplyError::UnknownStatus,
        FetchError::Transport(_) | FetchError::Stage(_) => {
            return Err(format!("a pure read ended in {err:?}"));
        }
    })
}

impl Case {
    fn family(&self) -> &'static str {
        match self {
            Self::WirePath(_) => "WirePath",
            Self::ShareSet(_) => "ShareSet",
            Self::GrantStore(_) => "GrantStore",
            Self::StoreSchema(_) => "StoreSchema",
            Self::ListServe(_) => "ListServe",
            Self::FetchServe(_) => "FetchServe",
            Self::FetchClient(_) => "FetchClient",
        }
    }
}

pub(super) fn listed() -> Vec<Listed> {
    vectors()
        .iter()
        .map(|vector| Listed {
            id: vector.id,
            kind: vector.kind,
            family: vector.case.family(),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------------------

fn same<T: Debug + PartialEq>(what: &str, observed: T, expected: T) -> Result<(), String> {
    if observed == expected {
        Ok(())
    } else {
        Err(format!(
            "{what}: expected {expected:?}, observed {observed:?}"
        ))
    }
}

fn ensure(condition: bool, what: impl Into<String>) -> Result<(), String> {
    if condition { Ok(()) } else { Err(what.into()) }
}

fn run(case: &Case) -> Result<(), String> {
    match case {
        Case::WirePath(probe) => run_wire(probe),
        Case::ShareSet(probe) => run_share(probe),
        Case::GrantStore(check) => check(),
        Case::StoreSchema(probe) => run_schema(probe),
        Case::ListServe(probe) => run_list(probe),
        Case::FetchServe(probe) => run_fetch(probe),
        Case::FetchClient(probe) => run_client(probe),
    }
}

fn run_wire(probe: &WireProbe) -> Result<(), String> {
    match probe {
        WireProbe::Parse { text, expect } => same(
            "first rule broken",
            WirePath::parse(text)
                .map(|_| ())
                .map_err(|invalid| invalid.rule),
            *expect,
        ),
        WireProbe::RuleId { rule, known } => same("is_rule_id", is_rule_id(rule), *known),
        WireProbe::RuleOf { value, expect } => {
            same("rule_of", rule_of(value), expect.map(str::to_string))
        }
        WireProbe::AccessPaths { paths, accepted } => same(
            "validate_access",
            validate_access(
                "req-1",
                paths.iter().map(|path| (*path).to_string()).collect(),
                "",
            )
            .is_ok(),
            *accepted,
        ),
        WireProbe::ListEntry { path, kept } => {
            let entry = Value::Map(vec![
                (Value::from("path"), Value::from(*path)),
                (Value::from("size"), Value::from(2u64)),
                (Value::from("sha256"), Value::Binary(vec![0; 32])),
                (Value::from("mtime"), Value::from(1.0f64)),
            ]);
            same(
                "list entry kept",
                SharesPage::entry(&entry).is_some(),
                *kept,
            )
        }
        WireProbe::FilePart {
            name,
            reference,
            kept,
        } => {
            let bytes = b"hi".to_vec();
            let part = RawPart::File {
                name: (*name).to_string(),
                size: 2,
                sha256: Sha256::digest(&bytes).into(),
                bytes: reference.is_none().then_some(bytes),
                reference: reference.map(str::to_string),
            };
            let (admitted, dropped) = admit_parts(vec![part], &PartLimits::default(), "peer");
            same("file part admitted", admitted.len() == 1, *kept)?;
            same("file parts dropped", dropped, u32::from(!*kept))
        }
    }
}

fn run_share(probe: &ShareProbe) -> Result<(), String> {
    match probe {
        ShareProbe::Verdict {
            global,
            workspace,
            files,
            who,
            path,
            folded,
            expect,
        } => {
            let fx = Fixture::new("share-verdict");
            if !global.is_empty() {
                fx.write(Layer::Global, global);
            }
            if !workspace.is_empty() {
                fx.write(Layer::Workspace, workspace);
            }
            for file in files {
                fx.file(file);
            }
            let set = fx.load();
            let (identity, destination) = who.hashes();
            let peer = PeerRef {
                identity: &identity,
                destination: &destination,
            };
            let observed = set
                .verdict_for(&peer, path, *folded)
                .map_err(|err| format!("rules: {err}"))?
                .map(|(verdict, _)| verdict);
            same("verdict", observed, *expect)
        }
        ShareProbe::Loads { layer, text } => {
            let fx = Fixture::new("share-loads");
            fx.write(*layer, text);
            same("refusal", fx.load().refusal(), None)
        }
        ShareProbe::Poisoned { layer, text } => {
            let fx = Fixture::new("share-poisoned");
            let path = fx.write(*layer, text);
            let set = fx.load();
            let refusal = set.refusal().ok_or("the file loaded")?;
            ensure(
                refusal.contains(&path.display().to_string()),
                format!("the refusal does not name the file: {refusal}"),
            )
        }
        ShareProbe::Pattern { text, valid } => {
            same("validate_pattern", validate_pattern(text).is_ok(), *valid)
        }
        ShareProbe::Override { text, valid } => {
            same("validate_override", validate_override(text).is_ok(), *valid)
        }
        ShareProbe::WriteTarget {
            workspace_exists,
            scope,
            layer,
        } => same(
            "write_target",
            write_target(*workspace_exists, *scope),
            *layer,
        ),
        ShareProbe::Check(check) => check(),
    }
}

fn run_schema(probe: &SchemaProbe) -> Result<(), String> {
    match probe {
        SchemaProbe::SharesFile { text, phrases } => {
            let fx = Fixture::new("schema-shares");
            let path = fx.write(Layer::Global, text);
            let set = fx.load();
            match (set.refusal(), phrases.is_empty()) {
                (None, true) => Ok(()),
                (None, false) => Err("the file loaded".into()),
                (Some(refusal), _) => {
                    ensure(
                        !phrases.is_empty(),
                        format!("the file was refused: {refusal}"),
                    )?;
                    ensure(
                        refusal.contains(&path.display().to_string()),
                        format!("the refusal does not name the file: {refusal}"),
                    )?;
                    missing_phrases(refusal, phrases)
                }
            }
        }
        SchemaProbe::GrantLine { edit, phrases } => {
            let (store, _tmp) = grant_store("schema-grant-line");
            let destination = fake_hash(0x2b);
            let record = store
                .grant("req", &destination, &paths(&["docs/a.md"]), None, t(1_000))
                .map_err(|err| err.to_string())?;
            let mut line = serde_json::to_value(&record).unwrap();
            edit(&mut line);
            write_line(&store, &line);
            let identity = fake_hash(0x1a);
            let peer = PeerRef {
                identity: &identity,
                destination: &destination,
            };
            match (store.list(), phrases.is_empty()) {
                (Ok(records), true) => same("records read", records.len(), 1),
                (Ok(_), false) => Err("the store loaded".into()),
                (Err(err), _) => {
                    let err = format!("{err:#}");
                    ensure(!phrases.is_empty(), format!("the store was refused: {err}"))?;
                    ensure(
                        err.contains(&store.path().display().to_string()),
                        format!("the refusal does not name the store: {err}"),
                    )?;
                    missing_phrases(&err, phrases)?;
                    ensure(
                        store.is_granted(&peer, "docs/a.md", t(1_000)).is_err(),
                        "a check read the refused store",
                    )
                }
            }
        }
        SchemaProbe::InboundLine { json, expect } => {
            let observed = serde_json::from_str::<InboundRecord>(json)
                .ok()
                .map(|record| (record.kind, record.paths, record.reason));
            let expected = expect.map(|(kind, paths, reason)| {
                (
                    kind,
                    paths.iter().map(|path| (*path).to_string()).collect(),
                    reason.to_string(),
                )
            });
            same("inbound record", observed, expected)
        }
        SchemaProbe::Check(check) => check(),
    }
}

fn run_list(probe: &ListProbe) -> Result<(), String> {
    match probe {
        ListProbe::Request {
            files,
            body,
            expect,
        } => {
            let serve = Serve::new("list-row", MAX_FETCH_FILE_BYTES, &["**"]);
            for file in *files {
                serve.file(file, file.as_bytes());
            }
            let reply = serve.list(body.clone());
            match expect {
                ListAnswer::Refused => refused(&reply),
                ListAnswer::Page { paths, next } => {
                    let page = answered(reply)?;
                    same("paths", entry_paths(&page), strings(paths))?;
                    same("next", next_of(&page), next.clone())
                }
            }
        }
        ListProbe::Check(check) => check(),
    }
}

fn run_fetch(probe: &FetchProbe) -> Result<(), String> {
    match probe {
        FetchProbe::Request {
            files,
            body,
            expect,
        } => {
            let serve = Serve::new("fetch-row", MAX_FETCH_FILE_BYTES, &["**"]);
            for file in *files {
                serve.file(file, file.as_bytes());
            }
            let reply = serve.fetch(body.clone());
            match expect {
                FetchAnswer::Refused => refused(&reply),
                FetchAnswer::Status { status, rule } => {
                    let value = answered(reply)?;
                    same("status", status_of(&value), Some(*status))?;
                    same(
                        "rule",
                        key_of(&value, "rule").and_then(Value::as_str),
                        *rule,
                    )
                }
            }
        }
        FetchProbe::Check(check) => check(),
    }
}

fn run_client(probe: &ClientProbe) -> Result<(), String> {
    match probe {
        ClientProbe::Page { value, expect } => {
            match (SharesPage::from_value(value, "peer"), expect) {
                (Ok(page), Ok((kept, next))) => {
                    same("entries kept", page.entries.len(), *kept)?;
                    same("next", page.next.as_deref(), next.as_deref())
                }
                (Err(FetchError::Malformed(key)), Err(expected)) => {
                    same("malformed key", key, *expected)
                }
                (Ok(page), Err(expected)) => Err(format!(
                    "expected malformed `{expected}`, observed {page:?}"
                )),
                (Err(err), _) => Err(format!("expected {expect:?}, observed {err:?}")),
            }
        }
        ClientProbe::Entry { value, kept } => {
            same("kept", SharesPage::entry(value).is_some(), *kept)
        }
        ClientProbe::Reply { value, expect } => {
            let observed = match read_fetch_reply(value) {
                Ok(reply) => Ok(reply),
                Err(err) => Err(reply_error(&err)?),
            };
            same("fetch reply", &observed, expect)
        }
        ClientProbe::Check(check) => check(),
    }
}

fn missing_phrases(text: &str, phrases: &[&str]) -> Result<(), String> {
    let missing: Vec<&str> = phrases
        .iter()
        .copied()
        .filter(|phrase| !text.contains(phrase))
        .collect();
    ensure(
        missing.is_empty(),
        format!("refusal lacks {missing:?}: {text}"),
    )
}

// ---------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------

/// A share root, a config directory and a cache directory in one temporary directory.
struct Fixture {
    _tmp: TempDir,
    config_dir: PathBuf,
    cache_dir: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let tmp = TempDir::new(tag);
        let config_dir = tmp.path.join("config");
        let root = tmp.path.join("workspace");
        Self::at(tmp, config_dir, root)
    }

    /// The global config dir inside the share root, as a REPL started from `$HOME` has it.
    fn enclosing(tag: &str) -> Self {
        let tmp = TempDir::new(tag);
        let root = tmp.path.join("workspace");
        Self::at(tmp, root.join("config"), root)
    }

    fn at(tmp: TempDir, config_dir: PathBuf, root: PathBuf) -> Self {
        fs::create_dir_all(&root).unwrap();
        let cache_dir = tmp.path.join("cache");
        Self {
            _tmp: tmp,
            config_dir,
            cache_dir,
            root,
        }
    }

    /// Pins the default directory name so the rows neither read nor depend on the env
    /// override another test may be holding.
    fn locations(&self) -> ShareLocations {
        ShareLocations::with_dir_names(
            &self.config_dir,
            &self.root,
            vec![WORKSPACE_COYOTE_DIR_NAME.to_string()],
        )
    }

    fn load(&self) -> ShareSet {
        ShareSet::load(self.locations())
    }

    fn write(&self, layer: Layer, text: &str) -> PathBuf {
        let path = match layer {
            Layer::Global => self.locations().global,
            Layer::Workspace => self.locations().workspace,
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    /// Creates `relative` under the share root and returns where it resolved to.
    fn file(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, relative).unwrap();
        dunce::canonicalize(path).unwrap()
    }

    fn grants(&self) -> GrantStore {
        GrantStore::new(&self.cache_dir, "inst")
    }
}

fn fake_hash(fill: u8) -> String {
    hex_lower(&[fill; 16])
}

fn t(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

fn paths(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| (*text).to_string()).collect()
}

fn strings(texts: &[&str]) -> Vec<String> {
    paths(texts)
}

fn grant_store(tag: &str) -> (GrantStore, TempDir) {
    let tmp = TempDir::new(tag);
    let store = GrantStore::new(&tmp.path, "inst");
    (store, tmp)
}

/// Rewrites the store as one record, as a hand edit or another build would leave it.
fn write_line(store: &GrantStore, line: &serde_json::Value) {
    fs::create_dir_all(store.path().parent().unwrap()).unwrap();
    fs::write(store.path(), format!("{line}\n")).unwrap();
}

/// A share file of this build's version with the given lists; an allow may name a peer.
fn shares_yaml(allow: &[(&str, Option<&str>)], deny: &[&str], overrides: &[&str]) -> String {
    let mut text = format!("version: {SHARES_FILE_VERSION}\n");
    if !allow.is_empty() {
        text.push_str("allow:\n");
        for (pattern, peer) in allow {
            text.push_str(&format!("- pattern: '{pattern}'\n"));
            if let Some(peer) = peer {
                text.push_str(&format!("  peer: {peer}\n"));
            }
        }
    }
    if !deny.is_empty() {
        text.push_str("deny:\n");
        for pattern in deny {
            text.push_str(&format!("- pattern: '{pattern}'\n"));
        }
    }
    if !overrides.is_empty() {
        text.push_str("override:\n");
        for path in overrides {
            text.push_str(&format!("- path: '{path}'\n"));
        }
    }
    text
}

fn allow_all() -> String {
    shares_yaml(&[("**", None)], &[], &[])
}

fn inbound_json(extra: &str) -> String {
    format!(
        r#"{{"version":{INBOUND_RECORD_VERSION},"id":"q","peer_destination":"{}","peer_identity":"{}","thread":"t","question":"why","envoy_question":"","received_at":"2024-01-01T00:00:00Z"{extra}}}"#,
        fake_hash(0x2b),
        fake_hash(0x1a)
    )
}

fn row(id: &'static str, kind: Kind, case: Case) -> Vector {
    Vector {
        id,
        kind,
        case,
        known_divergence: None,
    }
}

fn parse(
    id: &'static str,
    kind: Kind,
    text: impl Into<String>,
    expect: Result<(), &'static str>,
) -> Vector {
    row(
        id,
        kind,
        Case::WirePath(WireProbe::Parse {
            text: text.into(),
            expect,
        }),
    )
}

/// A verdict row on a root holding `files`, judged for the named peer with no case fold.
fn verdict(
    id: &'static str,
    kind: Kind,
    global: String,
    workspace: String,
    files: &[&str],
    path: &str,
    expect: Option<Verdict>,
) -> Vector {
    verdict_as(
        id,
        kind,
        VerdictRow {
            global,
            workspace,
            files,
            who: Who::Named,
            path,
            folded: false,
            expect,
        },
    )
}

struct VerdictRow<'a> {
    global: String,
    workspace: String,
    files: &'a [&'a str],
    who: Who,
    path: &'a str,
    folded: bool,
    expect: Option<Verdict>,
}

fn verdict_as(id: &'static str, kind: Kind, probe: VerdictRow<'_>) -> Vector {
    let VerdictRow {
        global,
        workspace,
        files,
        who,
        path,
        folded,
        expect,
    } = probe;
    row(
        id,
        kind,
        Case::ShareSet(ShareProbe::Verdict {
            global,
            workspace,
            files: strings(files),
            who,
            path: path.to_string(),
            folded,
            expect,
        }),
    )
}

fn share_check(id: &'static str, kind: Kind, check: Check) -> Vector {
    row(id, kind, Case::ShareSet(ShareProbe::Check(check)))
}

fn grant_check(id: &'static str, kind: Kind, check: Check) -> Vector {
    row(id, kind, Case::GrantStore(check))
}

fn schema_check(id: &'static str, kind: Kind, check: Check) -> Vector {
    row(id, kind, Case::StoreSchema(SchemaProbe::Check(check)))
}

// ---------------------------------------------------------------------------------------
// Checks: share set
// ---------------------------------------------------------------------------------------

fn named_peer() -> (String, String) {
    Who::Named.hashes()
}

fn is_file_via(served: &Served, via: Via) -> bool {
    matches!(served, Served::File(ServedFile { via: observed, .. }) if *observed == via)
}

fn serve(
    set: &ShareSet,
    path: &str,
    folded: bool,
    grants: Option<(&GrantStore, SystemTime)>,
) -> Served {
    let (identity, destination) = named_peer();
    let peer = PeerRef {
        identity: &identity,
        destination: &destination,
    };
    set.is_served(&peer, path, folded, MAX_FETCH_FILE_BYTES, grants)
}

fn a_noncanonical_peer_is_refused_at_write_and_matches_nobody_on_disk() -> Result<(), String> {
    let fx = Fixture::new("share-peer");
    let (identity, destination) = named_peer();
    let mut set = fx.load();
    let refused = set.apply(
        Mutation::Allow {
            pattern: "docs/**".into(),
            peer: Some("bob".into()),
        },
        WriteScope::Auto,
    );
    ensure(refused.is_err(), "a non-hash peer was written")?;
    ensure(
        !fx.locations().global.exists(),
        "the refused mutation wrote the file",
    )?;
    let upper = identity.to_ascii_uppercase();
    fx.write(
        Layer::Global,
        &shares_yaml(
            &[
                ("upper/**", Some(&upper)),
                ("named/**", Some("bob")),
                ("mine/**", Some(&identity)),
                ("open/**", None),
            ],
            &[],
            &[],
        ),
    );
    let set = fx.load();
    same("refusal", set.refusal(), None)?;
    let peer = PeerRef {
        identity: &identity,
        destination: &destination,
    };
    let patterns: Vec<String> = set
        .effective(&peer)
        .into_iter()
        .map(|entry| entry.pattern)
        .collect();
    same(
        "effective allows",
        patterns,
        strings(&["mine/**", "open/**"]),
    )
}

fn a_refused_file_serves_nothing_from_either_layer_and_takes_no_mutation() -> Result<(), String> {
    let fx = Fixture::new("share-poisoned-set");
    let corrupt = "version: 1\nallow: []\nfuture_section: {}\n";
    let path = fx.write(Layer::Global, corrupt);
    fx.write(Layer::Workspace, &allow_all());
    fx.file("README.md");
    let mut set = fx.load();
    let refusal = set.refusal().ok_or("the corrupt file loaded")?.to_string();
    ensure(
        refusal.contains(&path.display().to_string()),
        format!("the refusal does not name the file: {refusal}"),
    )?;
    ensure(
        matches!(serve(&set, "README.md", false, None), Served::NotShared),
        "the other layer served alone",
    )?;
    let (identity, destination) = named_peer();
    let store = fx.grants();
    store
        .grant("req", &destination, &paths(&["README.md"]), None, t(1_000))
        .map_err(|err| err.to_string())?;
    ensure(
        matches!(
            serve(&set, "README.md", false, Some((&store, t(1_000)))),
            Served::NotShared
        ),
        "a grant served past the refused set",
    )?;
    let peer = PeerRef {
        identity: &identity,
        destination: &destination,
    };
    ensure(
        set.verdict_for(&peer, "README.md", false).is_err(),
        "rules were built over a refused file",
    )?;
    ensure(
        set.effective(&peer).is_empty(),
        "effective allows survived the refusal",
    )?;
    let applied = set.apply(
        Mutation::Allow {
            pattern: "docs/**".into(),
            peer: None,
        },
        WriteScope::Global,
    );
    ensure(
        applied.is_err(),
        "a mutation was applied over a refused file",
    )?;
    same(
        "file bytes",
        fs::read_to_string(&path).unwrap(),
        corrupt.to_string(),
    )
}

fn the_global_config_dir_inside_the_root_is_protected() -> Result<(), String> {
    let fx = Fixture::enclosing("share-enclosing");
    fx.write(Layer::Global, &allow_all());
    fx.file("config/notes.md");
    fx.file("docs/a.md");
    let set = fx.load();
    let (identity, destination) = named_peer();
    let peer = PeerRef {
        identity: &identity,
        destination: &destination,
    };
    let verdict = |path: &str| {
        set.verdict_for(&peer, path, false)
            .map_err(|err| err.to_string())
            .map(|found| found.map(|(verdict, _)| verdict))
    };
    same(
        "the config dir",
        verdict("config/notes.md")?,
        Some(Verdict::Protected),
    )?;
    same(
        "the share file itself",
        verdict("config/mesh/shares.yaml")?,
        Some(Verdict::Protected),
    )?;
    same(
        "a doc beside it",
        verdict("docs/a.md")?,
        Some(Verdict::Shared),
    )
}

fn the_cache_dir_and_a_configured_inbox_are_protected() -> Result<(), String> {
    let fx = Fixture::new("share-cache-inbox");
    fx.write(Layer::Global, &allow_all());
    fx.file("cache/mesh/grants-inst.jsonl");
    fx.file("inbox/peer/a.md");
    fx.file("docs/a.md");
    let locations = fx
        .locations()
        .with_cache_dir(&fx.root.join("cache"))
        .with_protected(&fx.root.join("inbox"));
    let set = ShareSet::load(locations);
    let (identity, destination) = named_peer();
    let peer = PeerRef {
        identity: &identity,
        destination: &destination,
    };
    let verdict = |path: &str| {
        set.verdict_for(&peer, path, false)
            .map_err(|err| err.to_string())
            .map(|found| found.map(|(verdict, _)| verdict))
    };
    same(
        "the cache dir",
        verdict("cache/mesh/grants-inst.jsonl")?,
        Some(Verdict::Protected),
    )?;
    same(
        "the inbox",
        verdict("inbox/peer/a.md")?,
        Some(Verdict::Protected),
    )?;
    same("a doc", verdict("docs/a.md")?, Some(Verdict::Shared))
}

fn a_workspace_override_is_read_and_shown_but_never_applied() -> Result<(), String> {
    let fx = Fixture::new("share-inert-override");
    fx.write(Layer::Global, &allow_all());
    fx.write(Layer::Workspace, &shares_yaml(&[], &[], &[".env.example"]));
    fx.file(".env.example");
    let set = fx.load();
    let shown: Vec<&str> = set
        .inert_overrides()
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    same("inert overrides", shown, vec![".env.example"])?;
    ensure(
        matches!(serve(&set, ".env.example", false, None), Served::NotShared),
        "a workspace override lifted the built-in deny",
    )
}

fn the_case_probe_answers_and_leaves_no_file_behind() -> Result<(), String> {
    let fx = Fixture::new("share-case-probe");
    probe_case_insensitive(&fx.root).map_err(|err| err.to_string())?;
    let left: Vec<PathBuf> = fs::read_dir(&fx.root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    same("files left by the probe", left, Vec::new())
}

fn the_module_never_reads_the_current_directory() -> Result<(), String> {
    let source = include_str!("../shares.rs").replace("\r\n", "\n");
    let tests_start = source
        .find("\n#[cfg(test)]\nmod tests")
        .ok_or("shares.rs has no test module to anchor the production slice on")?;
    let production = &source[..tests_start];
    ensure(
        production.contains("impl ShareSet"),
        "the production slice stops before the ShareSet implementation",
    )?;
    for needle in [
        ["current", "_dir"].concat(),
        ["workspace_config", "_dir()"].concat(),
    ] {
        ensure(
            !production.contains(&needle),
            format!("production code uses {needle}"),
        )?;
    }
    Ok(())
}

fn apply_validates_first_and_writes_the_target_atomically() -> Result<(), String> {
    let fx = Fixture::new("share-apply");
    let mut set = fx.load();
    let refused = set.apply(
        Mutation::Allow {
            pattern: "docs/../**".into(),
            peer: None,
        },
        WriteScope::Auto,
    );
    ensure(refused.is_err(), "a dotted pattern was accepted")?;
    ensure(
        !fx.locations().global.exists(),
        "a refused mutation wrote the file",
    )?;
    let applied = set
        .apply(
            Mutation::Allow {
                pattern: "docs/**".into(),
                peer: None,
            },
            WriteScope::Auto,
        )
        .map_err(|err| err.to_string())?;
    same(
        "target with no workspace file",
        applied.path.clone(),
        fx.locations().global,
    )?;
    same("changed", applied.changed, true)?;
    same(
        "siblings after the write",
        siblings_of(&applied.path),
        vec!["shares.yaml".to_string()],
    )?;
    let again = set
        .apply(
            Mutation::Allow {
                pattern: "docs/**".into(),
                peer: None,
            },
            WriteScope::Auto,
        )
        .map_err(|err| err.to_string())?;
    same("a repeat changes nothing", again.changed, false)?;
    let workspace = set
        .apply(
            Mutation::Deny {
                pattern: "src/**".into(),
            },
            WriteScope::Workspace,
        )
        .map_err(|err| err.to_string())?;
    same(
        "a named layer",
        workspace.path.clone(),
        fx.locations().workspace,
    )?;
    let auto = set
        .apply(
            Mutation::Deny {
                pattern: "tmp/**".into(),
            },
            WriteScope::Auto,
        )
        .map_err(|err| err.to_string())?;
    same(
        "auto once the workspace file exists",
        auto.path,
        fx.locations().workspace,
    )?;
    same(
        "workspace siblings",
        siblings_of(&workspace.path),
        vec!["mesh-shares.yaml".to_string()],
    )
}

fn mutation_logs_name_the_file_and_never_a_pattern() -> Result<(), String> {
    install_log_collector();
    let fx = Fixture::new("share-mutation-logs");
    let mut set = fx.load();
    for mutation in [
        Mutation::Allow {
            pattern: "docs/**".into(),
            peer: None,
        },
        Mutation::Deny {
            pattern: "src/vault/*".into(),
        },
        Mutation::Override {
            path: ".env.example".into(),
        },
    ] {
        set.apply(mutation, WriteScope::Auto)
            .map_err(|err| err.to_string())?;
    }
    let path = fx.locations().global.display().to_string();
    let logged: Vec<String> = debug_snapshot()
        .into_iter()
        .chain(warn_snapshot())
        .filter(|line| line.contains(&path))
        .collect();
    same("lines naming the file", logged.len(), 3)?;
    for line in &logged {
        for text in ["docs/**", "src/vault/*", ".env.example"] {
            ensure(
                !line.contains(text),
                format!("a log line carries {text:?}: {line}"),
            )?;
        }
    }
    Ok(())
}

fn a_grant_serves_a_not_allowed_file_once_and_never_a_denied_one() -> Result<(), String> {
    let fx = Fixture::new("share-grant");
    fx.write(
        Layer::Global,
        &shares_yaml(&[("docs/**", None)], &["src/secret/**"], &[]),
    );
    fx.file("src/a.rs");
    fx.file(".env");
    fx.file("src/secret/k.txt");
    fx.file(".git/config");
    fx.file("docs/d.md");
    let set = fx.load();
    let (identity, destination) = named_peer();
    let peer = PeerRef {
        identity: &identity,
        destination: &destination,
    };
    let store = fx.grants();
    store
        .grant(
            "req",
            &destination,
            &paths(&["src/a.rs", ".env", "src/secret/k.txt", ".git/config"]),
            None,
            t(1_000),
        )
        .map_err(|err| err.to_string())?;
    let now = t(1_001);
    let grants = Some((&store, now));
    ensure(
        is_file_via(&serve(&set, "src/a.rs", false, grants), Via::Grant),
        "a granted, not allowed file was not served",
    )?;
    ensure(
        matches!(serve(&set, "src/a.rs", false, grants), Served::NotShared),
        "the one use served twice",
    )?;
    for denied in [".env", "src/secret/k.txt", ".git/config"] {
        ensure(
            matches!(serve(&set, denied, false, grants), Served::NotShared),
            format!("a grant served {denied}, which the set refuses"),
        )?;
        same(
            &format!("the use on {denied} stays unspent"),
            store
                .is_granted(&peer, denied, now)
                .map_err(|err| err.to_string())?,
            true,
        )?;
    }
    ensure(
        is_file_via(&serve(&set, "docs/d.md", false, grants), Via::Allow),
        "an allowed file was not served by the allow",
    )
}

fn a_grant_matches_byte_for_byte_without_glob_or_case_fold() -> Result<(), String> {
    let fx = Fixture::new("share-grant-exact");
    fx.write(Layer::Global, &shares_yaml(&[("docs/**", None)], &[], &[]));
    fx.file("src/a.rs");
    let set = fx.load();
    let (_, destination) = named_peer();
    let store = fx.grants();
    store
        .grant(
            "req",
            &destination,
            &paths(&["src/*", "SRC/A.RS"]),
            None,
            t(1_000),
        )
        .map_err(|err| err.to_string())?;
    let grants = Some((&store, t(1_001)));
    ensure(
        matches!(serve(&set, "src/a.rs", false, grants), Served::NotShared),
        "a glob or a case variant in a grant matched",
    )?;
    ensure(
        matches!(serve(&set, "src/a.rs", true, grants), Served::NotShared),
        "a case variant in a grant matched under the fold",
    )?;
    store
        .grant("exact", &destination, &paths(&["src/a.rs"]), None, t(1_000))
        .map_err(|err| err.to_string())?;
    ensure(
        is_file_via(&serve(&set, "src/a.rs", true, grants), Via::Grant),
        "the byte-exact grant did not serve",
    )
}

// ---------------------------------------------------------------------------------------
// Checks: grant store
// ---------------------------------------------------------------------------------------

fn grant_err(id: &str, peer: &str, texts: &[&str]) -> Result<(), String> {
    let (store, _tmp) = grant_store("grant-refused");
    ensure(
        store
            .grant(id, peer, &paths(texts), None, t(1_000))
            .is_err(),
        "the grant was written",
    )?;
    ensure(!store.path().exists(), "a refused grant created the store")
}

fn a_grant_with_an_id_that_is_not_a_wire_id_is_refused() -> Result<(), String> {
    grant_err("bad id!", &fake_hash(0x2b), &["docs/a.md"])
}

fn a_grant_for_a_peer_that_is_not_a_canonical_hash_is_refused() -> Result<(), String> {
    grant_err("req", "bob", &["docs/a.md"])?;
    grant_err("req", &fake_hash(0x2b)[..31], &["docs/a.md"])
}

fn a_grant_with_no_paths_is_refused() -> Result<(), String> {
    grant_err("req", &fake_hash(0x2b), &[])
}

fn a_grant_with_a_path_that_is_not_a_wire_path_is_refused() -> Result<(), String> {
    grant_err("req", &fake_hash(0x2b), &["docs/a.md", "../etc/passwd"])
}

fn a_grant_over_the_path_cap_is_refused_and_one_at_it_is_written() -> Result<(), String> {
    let over: Vec<String> = (0..=GRANT_MAX_PATHS).map(|n| format!("p{n}.md")).collect();
    let over: Vec<&str> = over.iter().map(String::as_str).collect();
    grant_err("req", &fake_hash(0x2b), &over)?;
    let (store, _tmp) = grant_store("grant-cap");
    let at: Vec<String> = (0..GRANT_MAX_PATHS).map(|n| format!("p{n}.md")).collect();
    let record = store
        .grant("req", &fake_hash(0x2b), &at, None, t(1_000))
        .map_err(|err| err.to_string())?;
    same("paths kept", record.paths.len(), GRANT_MAX_PATHS)
}

fn repeats_are_dropped_and_each_path_lends_one_use_until_the_ttl() -> Result<(), String> {
    let (store, _tmp) = grant_store("grant-defaults");
    let record = store
        .grant(
            "req",
            &fake_hash(0x2b),
            &paths(&["docs/a.md", "docs/b.md", "docs/a.md"]),
            None,
            t(1_000),
        )
        .map_err(|err| err.to_string())?;
    let kept: Vec<&str> = record.paths.iter().map(|path| path.path.as_str()).collect();
    same("paths", kept, vec!["docs/a.md", "docs/b.md"])?;
    for path in &record.paths {
        same(
            "uses lent",
            (path.uses, path.uses_left),
            (DEFAULT_GRANT_USES, DEFAULT_GRANT_USES),
        )?;
    }
    same("default uses", DEFAULT_GRANT_USES, 1)?;
    same("default ttl", DEFAULT_GRANT_TTL, Duration::from_secs(900))?;
    same(
        "expires",
        record.expires.clone(),
        rfc3339_utc(t(1_000) + DEFAULT_GRANT_TTL),
    )?;
    let named = store
        .grant(
            "other",
            &fake_hash(0x2b),
            &paths(&["docs/c.md"]),
            Some(Duration::from_secs(60)),
            t(1_000),
        )
        .map_err(|err| err.to_string())?;
    same(
        "expires with a named ttl",
        named.expires,
        rfc3339_utc(t(1_060)),
    )
}

fn a_grant_under_the_same_id_replaces_the_earlier_one_for_that_peer_alone() -> Result<(), String> {
    let (store, _tmp) = grant_store("grant-replace");
    let (alice, bob) = (fake_hash(0x2b), fake_hash(0x4d));
    store
        .grant("req", &alice, &paths(&["docs/a.md"]), None, t(1_000))
        .map_err(|err| err.to_string())?;
    store
        .grant("req", &bob, &paths(&["docs/b.md"]), None, t(1_000))
        .map_err(|err| err.to_string())?;
    store
        .grant("req", &alice, &paths(&["docs/c.md"]), None, t(1_001))
        .map_err(|err| err.to_string())?;
    let records = store.list().map_err(|err| err.to_string())?;
    let mut held: Vec<(String, Vec<String>)> = records
        .iter()
        .map(|record| {
            (
                record.peer.clone(),
                record.paths.iter().map(|path| path.path.clone()).collect(),
            )
        })
        .collect();
    held.sort();
    let mut expected = vec![
        (alice, strings(&["docs/c.md"])),
        (bob, strings(&["docs/b.md"])),
    ];
    expected.sort();
    same("grants on file", held, expected)
}

fn peer_for<'a>(identity: &'a str, destination: &'a str) -> PeerRef<'a> {
    PeerRef {
        identity,
        destination,
    }
}

fn a_refund_restores_the_use_and_never_above_what_was_lent() -> Result<(), String> {
    let (store, _tmp) = grant_store("grant-refund");
    let (identity, destination) = named_peer();
    let peer = peer_for(&identity, &destination);
    store
        .grant("req", &destination, &paths(&["docs/a.md"]), None, t(1_000))
        .map_err(|err| err.to_string())?;
    let step = |what: &str, outcome: anyhow::Result<bool>, expected: bool| {
        same(what, outcome.map_err(|err| err.to_string())?, expected)
    };
    step(
        "first consume",
        store.consume(&peer, "docs/a.md", t(1_001)),
        true,
    )?;
    step(
        "second consume",
        store.consume(&peer, "docs/a.md", t(1_001)),
        false,
    )?;
    step("refund", store.refund(&peer, "docs/a.md", t(1_001)), true)?;
    step(
        "refund above the lent uses",
        store.refund(&peer, "docs/a.md", t(1_001)),
        false,
    )?;
    step(
        "granted again",
        store.is_granted(&peer, "docs/a.md", t(1_001)),
        true,
    )?;
    step(
        "consume after the refund",
        store.consume(&peer, "docs/a.md", t(1_001)),
        true,
    )?;
    step(
        "refund of an expired grant",
        store.refund(
            &peer,
            "docs/a.md",
            t(1_000) + DEFAULT_GRANT_TTL + Duration::from_secs(1),
        ),
        false,
    )
}

fn an_exhausted_grant_stays_on_file_until_it_expires() -> Result<(), String> {
    let (store, _tmp) = grant_store("grant-exhausted");
    let (identity, destination) = named_peer();
    let peer = peer_for(&identity, &destination);
    store
        .grant(
            "req",
            &destination,
            &paths(&["docs/a.md"]),
            Some(Duration::from_secs(10)),
            t(1_000),
        )
        .map_err(|err| err.to_string())?;
    same(
        "consume",
        store
            .consume(&peer, "docs/a.md", t(1_001))
            .map_err(|err| err.to_string())?,
        true,
    )?;
    same(
        "granted once spent",
        store
            .is_granted(&peer, "docs/a.md", t(1_005))
            .map_err(|err| err.to_string())?,
        false,
    )?;
    same(
        "records before expiry",
        store.list().map_err(|err| err.to_string())?.len(),
        1,
    )?;
    same(
        "swept at expiry",
        store.prune(t(1_011)).map_err(|err| err.to_string())?,
        1,
    )?;
    same(
        "records after expiry",
        store.list().map_err(|err| err.to_string())?.len(),
        0,
    )
}

fn revoke_takes_one_peers_grant_under_the_id_and_leaves_the_rest() -> Result<(), String> {
    let (store, _tmp) = grant_store("grant-revoke");
    let (alice, bob) = (fake_hash(0x2b), fake_hash(0x4d));
    for (id, peer) in [("req", &alice), ("req", &bob), ("other", &alice)] {
        store
            .grant(id, peer, &paths(&["docs/a.md"]), None, t(1_000))
            .map_err(|err| err.to_string())?;
    }
    same(
        "revoke of nothing",
        store
            .revoke("missing", &alice)
            .map_err(|err| err.to_string())?,
        false,
    )?;
    same(
        "revoke",
        store.revoke("req", &alice).map_err(|err| err.to_string())?,
        true,
    )?;
    let mut left: Vec<(String, String)> = store
        .list()
        .map_err(|err| err.to_string())?
        .iter()
        .map(|record| (record.id.clone(), record.peer.clone()))
        .collect();
    left.sort();
    same(
        "grants left",
        left,
        vec![("other".to_string(), alice), ("req".to_string(), bob)],
    )
}

fn expired_grants_are_swept_on_open_and_on_every_check() -> Result<(), String> {
    let (store, tmp) = grant_store("grant-sweep");
    let (identity, destination) = named_peer();
    let peer = peer_for(&identity, &destination);
    store
        .grant(
            "short",
            &destination,
            &paths(&["docs/a.md"]),
            Some(Duration::from_secs(10)),
            t(1_000),
        )
        .map_err(|err| err.to_string())?;
    store
        .grant("long", &destination, &paths(&["docs/b.md"]), None, t(1_000))
        .map_err(|err| err.to_string())?;
    same(
        "granted at the last second",
        store
            .is_granted(&peer, "docs/a.md", t(1_009))
            .map_err(|err| err.to_string())?,
        true,
    )?;
    same(
        "granted past expiry",
        store
            .is_granted(&peer, "docs/a.md", t(1_011))
            .map_err(|err| err.to_string())?,
        false,
    )?;
    let ids = |store: &GrantStore| -> Result<Vec<String>, String> {
        store
            .list()
            .map(|records| records.iter().map(|record| record.id.clone()).collect())
            .map_err(|err| err.to_string())
    };
    same("swept by the check", ids(&store)?, strings(&["long"]))?;
    let reopened = GrantStore::open(
        &tmp.path,
        "inst",
        t(1_000) + DEFAULT_GRANT_TTL + Duration::from_secs(1),
    )
    .map_err(|err| err.to_string())?;
    same("swept on open", ids(&reopened)?, Vec::new())
}

fn a_line_whose_expires_does_not_parse_refuses_the_store() -> Result<(), String> {
    let (store, _tmp) = grant_store("grant-bad-expires");
    let (identity, destination) = named_peer();
    let peer = peer_for(&identity, &destination);
    let record = store
        .grant("req", &destination, &paths(&["docs/a.md"]), None, t(1_000))
        .map_err(|err| err.to_string())?;
    let mut edited = serde_json::to_value(&record).unwrap();
    edited["expires"] = serde_json::json!("yesterday");
    write_line(&store, &edited);
    ensure(
        store.list().is_err(),
        "a grant with an unparsable expiry was read",
    )?;
    ensure(
        store.is_granted(&peer, "docs/a.md", t(1_000)).is_err(),
        "a check read past the unparsable line",
    )
}

fn a_line_with_more_uses_left_than_lent_refuses_the_store() -> Result<(), String> {
    let (store, _tmp) = grant_store("grant-surplus");
    let (identity, destination) = named_peer();
    let peer = peer_for(&identity, &destination);
    let record = store
        .grant("req", &destination, &paths(&["docs/a.md"]), None, t(1_000))
        .map_err(|err| err.to_string())?;
    let mut edited = serde_json::to_value(&record).unwrap();
    edited["paths"][0]["uses_left"] = serde_json::json!(2);
    write_line(&store, &edited);
    let err = store.list().err().ok_or("the surplus line was read")?;
    missing_phrases(
        &format!("{err:#}"),
        &["line 1", "more uses left than were granted"],
    )?;
    ensure(
        store.consume(&peer, "docs/a.md", t(1_000)).is_err(),
        "a consume read past the surplus line",
    )
}

// ---------------------------------------------------------------------------------------
// Checks: schema
// ---------------------------------------------------------------------------------------

fn a_refused_share_file_keeps_the_other_layer_from_loading_alone() -> Result<(), String> {
    let fx = Fixture::new("schema-both-layers");
    fx.write(
        Layer::Global,
        &format!("version: {}\nallow: []\n", SHARES_FILE_VERSION + 1),
    );
    fx.write(Layer::Workspace, &allow_all());
    fx.file("README.md");
    let set = fx.load();
    ensure(set.refusal().is_some(), "the newer file loaded")?;
    ensure(
        matches!(serve(&set, "README.md", false, None), Served::NotShared),
        "the workspace layer served alone",
    )
}

fn the_share_and_grant_versions_are_the_baseline() -> Result<(), String> {
    same("SHARES_FILE_VERSION", SHARES_FILE_VERSION, 1)?;
    same("GRANT_RECORD_VERSION", GRANT_RECORD_VERSION, 1)?;
    same("INBOUND_RECORD_VERSION", INBOUND_RECORD_VERSION, 2)
}

fn the_share_list_and_grant_store_refuse_in_the_common_wording() -> Result<(), String> {
    let fx = Fixture::new("schema-wording");
    let newer = SHARES_FILE_VERSION + 1;
    let path = fx.write(Layer::Global, &format!("version: {newer}\nallow: []\n"));
    let refusal = fx
        .load()
        .refusal()
        .map(str::to_string)
        .ok_or("the newer share file loaded")?;
    let expected = version_refusal(
        "share list",
        &path,
        None,
        newer,
        SHARES_FILE_VERSION,
        Remedy::UserFile("is the share list, and a fresh one shares nothing"),
    );
    ensure(
        refusal.contains(&expected),
        format!("share refusal {refusal:?} does not carry {expected:?}"),
    )?;
    let (store, _tmp) = grant_store("schema-wording-grant");
    let record = store
        .grant(
            "req",
            &fake_hash(0x2b),
            &paths(&["docs/a.md"]),
            None,
            t(1_000),
        )
        .map_err(|err| err.to_string())?;
    let mut edited = serde_json::to_value(&record).unwrap();
    edited["version"] = serde_json::json!(GRANT_RECORD_VERSION + 1);
    write_line(&store, &edited);
    let err = format!(
        "{:#}",
        store.list().err().ok_or("the newer grant line loaded")?
    );
    let expected = version_refusal(
        "grant store",
        store.path(),
        Some(1),
        GRANT_RECORD_VERSION + 1,
        GRANT_RECORD_VERSION,
        Remedy::Cache,
    );
    ensure(
        err.contains(&expected),
        format!("grant refusal {err:?} does not carry {expected:?}"),
    )
}

fn the_version_pin_names_the_share_and_grant_constants() -> Result<(), String> {
    let source = include_str!("../schema.rs");
    let pin = source
        .find("fn every_on_disk_store_version_is_pinned")
        .ok_or("the pin test is gone")?;
    for name in ["SHARES_FILE_VERSION", "GRANT_RECORD_VERSION"] {
        ensure(
            source[pin..].contains(name),
            format!("the pin does not name {name}"),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Fixtures: the handlers
// ---------------------------------------------------------------------------------------

const GRANT_ID: &str = "0123456789abcdef";
const ORIGIN: [u8; NAME_HASH_LEN] = [7; NAME_HASH_LEN];
/// A response frame around a `bin32` body: the array and request-id headers plus the
/// body's own header.
const RESPONSE_FRAME_OVERHEAD: usize = 24;

struct TestSource {
    root: Option<PathBuf>,
    serving: Option<Arc<FetchServing>>,
}

impl ShareSource for TestSource {
    fn share_root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    fn serving(&self) -> Option<Arc<FetchServing>> {
        self.serving.clone()
    }
}

struct FailingReader;

impl FileReader for FailingReader {
    fn read_bounded(&self, _file: fs::File, _limit: u64) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("the disk went away"))
    }
}

/// Hands back `limit` bytes whatever the file holds, as a file that grew past the limit
/// between the stat and the read would.
struct GrowingReader;

impl FileReader for GrowingReader {
    fn read_bounded(&self, _file: fs::File, limit: u64) -> std::io::Result<Vec<u8>> {
        Ok(vec![b'x'; limit as usize])
    }
}

struct NoKnocks;

impl KnockSink for NoKnocks {
    fn knock(&self, _: KnockEvent) {}
}

/// A share root with the global share list beside it, served to one peer through the
/// `/list` and `/fetch` handlers.
struct Serve {
    tmp: TempDir,
    root: PathBuf,
    serving: Arc<FetchServing>,
    source: Arc<TestSource>,
    identity: PrivateIdentity,
    identity_hex: String,
    destination: String,
    hooks: MeshHooks,
}

impl Serve {
    fn new(tag: &str, max_bytes: u64, allow: &[&str]) -> Self {
        Self::build(tag, max_bytes, allow, &[], None)
    }

    /// `inbox_dir` is relative to the share root, as a `mesh.fetch.inbox_dir` pointed
    /// inside a shared workspace is.
    fn build(
        tag: &str,
        max_bytes: u64,
        allow: &[&str],
        deny: &[&str],
        inbox_dir: Option<&str>,
    ) -> Self {
        let tmp = TempDir::new(tag);
        let root = tmp.path.join("workspace");
        fs::create_dir_all(&root).unwrap();
        let allow: Vec<(&str, Option<&str>)> =
            allow.iter().map(|pattern| (*pattern, None)).collect();
        let shares = mesh_config_dir(&tmp.path.join("config")).join("shares.yaml");
        fs::create_dir_all(shares.parent().unwrap()).unwrap();
        fs::write(&shares, shares_yaml(&allow, deny, &[])).unwrap();
        let cache_dir = tmp.path.join("cache");
        let hooks = MeshHooks::default();
        let serving = Arc::new(FetchServing::new(
            tmp.path.join("config"),
            cache_dir.clone(),
            inbox_dir.map(|dir| root.join(dir)),
            max_bytes,
            GrantStore::new(&cache_dir, "inst"),
            hooks.clone(),
        ));
        let source = Arc::new(TestSource {
            root: Some(root.clone()),
            serving: Some(Arc::clone(&serving)),
        });
        let identity = PrivateIdentity::new_from_rand(OsRng);
        let identity_hex = identity.as_identity().address_hash.to_hex_string();
        Self {
            tmp,
            root,
            serving,
            source,
            identity,
            identity_hex,
            destination: fake_hash(0x2b),
            hooks,
        }
    }

    fn shares_file(&self) -> PathBuf {
        mesh_config_dir(&self.tmp.path.join("config")).join("shares.yaml")
    }

    fn file(&self, relative: &str, bytes: &[u8]) {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn admitted(&self, path: &str, body: Value) -> AdmittedRequest {
        AdmittedRequest {
            link_id: LinkId::new_from_rand(OsRng),
            identity: *self.identity.as_identity(),
            destination_hash: AddressHash::new_from_hex_string(&self.destination).unwrap(),
            request_id: RequestId::from([1u8; 16]),
            path_hash: PathHash::of(path),
            requested_at: 1_700_000_000.0,
            body,
            branch: SizeBranch::Packet,
        }
    }

    fn list(&self, body: Value) -> Reply {
        list_via(&self.source, self.admitted(LIST_PATH, body))
    }

    fn fetch(&self, body: Value) -> Reply {
        self.fetch_with(FetchHandler::new(weak(&self.source)), body)
    }

    fn fetch_with(&self, handler: FetchHandler, body: Value) -> Reply {
        block_on(handler.handle(self.admitted(FETCH_PATH, body)))
    }

    fn grant(&self, paths: &[&str]) {
        self.serving
            .grants()
            .grant(
                GRANT_ID,
                &self.destination,
                &strings(paths),
                None,
                SystemTime::now(),
            )
            .unwrap();
    }

    /// The uses left on the one granted path of the one grant on file.
    fn uses_left(&self) -> u32 {
        let records = self.serving.grants().list().unwrap();
        let [record] = records.as_slice() else {
            panic!("one grant record, got {}", records.len());
        };
        let [granted] = record.paths.as_slice() else {
            panic!("one granted path, got {}", record.paths.len());
        };
        granted.uses_left
    }

    /// The handler's reply for `identity` once the dispatcher has judged it under `trust`.
    fn dispatch(
        &self,
        trust: &TrustList,
        identity: &PrivateIdentity,
        path: &str,
        body: Value,
    ) -> Reply {
        let (store, _tmp) = trust.open("dispatch-trust");
        let dispatcher = Dispatcher::new(store, Arc::new(NoKnocks));
        let registered = dispatcher
            .register(LIST_PATH, Arc::new(ListHandler::new(weak(&self.source))))
            .and_then(|_| {
                dispatcher.register(FETCH_PATH, Arc::new(FetchHandler::new(weak(&self.source))))
            });
        assert!(registered.is_ok(), "the list and fetch paths register");
        let request = InboundRequest {
            link_id: LinkId::new_from_rand(OsRng),
            identity: Some(*identity.as_identity()),
            request_id: RequestId::from([1u8; 16]),
            path_hash: PathHash::of(path),
            requested_at: 0.0,
            data: Envelope::new(OriginName(ORIGIN), body).into_value(),
            branch: SizeBranch::Packet,
        };
        block_on(RequestHandler::handle(&dispatcher, request))
    }
}

fn weak(source: &Arc<TestSource>) -> Weak<dyn ShareSource> {
    Arc::downgrade(source) as Weak<dyn ShareSource>
}

fn list_via(source: &Arc<TestSource>, request: AdmittedRequest) -> Reply {
    block_on(ListHandler::new(weak(source)).handle(request))
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn describe(reply: &Reply) -> String {
    match reply {
        Reply::Value(value) => format!("Value({value})"),
        Reply::Settled { value, .. } => format!("Settled({value})"),
        Reply::Code(code) => format!("Code({code:?})"),
        Reply::Silent => "Silent".to_string(),
    }
}

/// The value of an answered reply; a settled one is dropped unsent.
fn answered(reply: Reply) -> Result<Value, String> {
    match reply {
        Reply::Value(value) | Reply::Settled { value, .. } => Ok(value),
        other => Err(format!("expected a value, observed {}", describe(&other))),
    }
}

fn refused(reply: &Reply) -> Result<(), String> {
    ensure(
        matches!(reply, Reply::Code(RefusalCode::InvalidData)),
        format!("expected InvalidData, observed {}", describe(reply)),
    )
}

fn key_of<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    field(value.as_map()?, key)
}

fn keys_of(value: &Value) -> Vec<&str> {
    value
        .as_map()
        .into_iter()
        .flatten()
        .filter_map(|(key, _)| key.as_str())
        .collect()
}

fn status_of(value: &Value) -> Option<&str> {
    key_of(value, "status")?.as_str()
}

fn entry_paths(page: &Value) -> Vec<String> {
    key_of(page, "entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| key_of(entry, "path")?.as_str().map(str::to_string))
        .collect()
}

fn next_of(page: &Value) -> Option<String> {
    key_of(page, "next")?.as_str().map(str::to_string)
}

fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .map(|(key, value)| (Value::from(key), value))
            .collect(),
    )
}

fn bin(bytes: &[u8]) -> Value {
    Value::Binary(bytes.to_vec())
}

fn list_body(prefix: Option<&str>, cursor: Option<&str>) -> Value {
    map(vec![
        ("v", Value::from(PEER_WIRE_VERSION)),
        ("prefix", prefix.map_or(Value::Nil, Value::from)),
        ("cursor", cursor.map_or(Value::Nil, Value::from)),
    ])
}

fn fetch_body(path: &str, if_sha256: Option<[u8; 32]>) -> Value {
    map(vec![
        ("v", Value::from(PEER_WIRE_VERSION)),
        ("path", Value::from(path)),
        ("if_sha256", if_sha256.map_or(Value::Nil, |hash| bin(&hash))),
    ])
}

/// `body` with `key` set, replacing an earlier value under it.
fn with(body: Value, key: &str, value: Value) -> Value {
    let Value::Map(mut entries) = body else {
        unreachable!("bodies are maps")
    };
    entries.retain(|(name, _)| name.as_str() != Some(key));
    entries.push((Value::from(key), value));
    Value::Map(entries)
}

fn without(body: Value, key: &str) -> Value {
    let Value::Map(mut entries) = body else {
        unreachable!("bodies are maps")
    };
    entries.retain(|(name, _)| name.as_str() != Some(key));
    Value::Map(entries)
}

/// A page as a peer sends it.
fn page_value(entries: Vec<Value>, next: Value) -> Value {
    map(vec![
        ("v", Value::from(PEER_WIRE_VERSION)),
        ("entries", Value::Array(entries)),
        ("next", next),
    ])
}

fn entry_of(path: &str) -> Value {
    map(vec![
        ("path", Value::from(path)),
        ("size", Value::from(1u64)),
        ("sha256", bin(&[0x5a; 32])),
        ("mtime", Value::F64(1_700_000_000.0)),
    ])
}

fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// The bytes a reply with this value puts on the wire.
fn framed(value: Value) -> Vec<u8> {
    ResponseFrame {
        request_id: RequestId::from([1u8; 16]),
        data: value,
    }
    .encode()
}

fn encoded_len(value: &Value) -> usize {
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, value).unwrap();
    bytes.len()
}

fn frame_of(request_id: RequestId, total: usize) -> Result<Vec<u8>, String> {
    let bytes = ResponseFrame {
        request_id,
        data: Value::Binary(vec![0x5a; total - RESPONSE_FRAME_OVERHEAD]),
    }
    .encode();
    same("frame length", bytes.len(), total)?;
    Ok(bytes)
}

/// The requester's end of one pending request, as `R3Client::request` holds it.
type Pending = tokio::sync::oneshot::Receiver<Result<(Value, SizeBranch), R3Error>>;

fn pending_on(
    client: &R3Client,
    request_id: RequestId,
    link_id: LinkId,
    path: &str,
) -> Result<Pending, String> {
    client
        .insert_pending(request_id, link_id, path, None)
        .map_err(|err| format!("insert_pending: {err:?}"))
}

fn list_check(id: &'static str, kind: Kind, check: Check) -> Vector {
    row(id, kind, Case::ListServe(ListProbe::Check(check)))
}

fn fetch_check(id: &'static str, kind: Kind, check: Check) -> Vector {
    row(id, kind, Case::FetchServe(FetchProbe::Check(check)))
}

fn client_check(id: &'static str, kind: Kind, check: Check) -> Vector {
    row(id, kind, Case::FetchClient(ClientProbe::Check(check)))
}

fn list_request(
    id: &'static str,
    kind: Kind,
    files: &'static [&'static str],
    body: Value,
    expect: ListAnswer,
) -> Vector {
    row(
        id,
        kind,
        Case::ListServe(ListProbe::Request {
            files,
            body,
            expect,
        }),
    )
}

fn page_of(paths: &[&'static str], next: Option<String>) -> ListAnswer {
    ListAnswer::Page {
        paths: paths.to_vec(),
        next,
    }
}

fn fetch_request(
    id: &'static str,
    kind: Kind,
    files: &'static [&'static str],
    body: Value,
    expect: FetchAnswer,
) -> Vector {
    row(
        id,
        kind,
        Case::FetchServe(FetchProbe::Request {
            files,
            body,
            expect,
        }),
    )
}

fn status(status: &'static str) -> FetchAnswer {
    FetchAnswer::Status { status, rule: None }
}

fn invalid_path(rule: &'static str) -> FetchAnswer {
    FetchAnswer::Status {
        status: "invalid_path",
        rule: Some(rule),
    }
}

fn page_row(
    id: &'static str,
    kind: Kind,
    value: Value,
    expect: Result<(usize, Option<String>), &'static str>,
) -> Vector {
    row(
        id,
        kind,
        Case::FetchClient(ClientProbe::Page { value, expect }),
    )
}

fn entry_row(id: &'static str, kind: Kind, value: Value, kept: bool) -> Vector {
    row(
        id,
        kind,
        Case::FetchClient(ClientProbe::Entry { value, kept }),
    )
}

fn reply_row(
    id: &'static str,
    kind: Kind,
    value: Value,
    expect: Result<FetchReply, ReplyError>,
) -> Vector {
    row(
        id,
        kind,
        Case::FetchClient(ClientProbe::Reply { value, expect }),
    )
}

/// A `/fetch` reply of this build's version carrying `status` and nothing else.
fn fetch_reply(status: &str) -> Value {
    map(vec![
        ("v", Value::from(PEER_WIRE_VERSION)),
        ("status", Value::from(status)),
    ])
}

/// An `ok` reply for `bytes`, its `size` and `sha256` as an honest peer sends them.
fn ok_reply(bytes: &[u8]) -> Value {
    let reply = with(fetch_reply("ok"), "bytes", bin(bytes));
    let reply = with(reply, "size", Value::from(bytes.len() as u64));
    with(reply, "sha256", bin(&Sha256::digest(bytes)))
}

fn ok_read(bytes: &[u8]) -> Result<FetchReply, ReplyError> {
    Ok(FetchReply::Ok {
        bytes: bytes.to_vec(),
        size: bytes.len() as u64,
        sha256: Sha256::digest(bytes).into(),
    })
}

fn not_modified_reply(sha256: &[u8]) -> Value {
    with(fetch_reply("not_modified"), "sha256", bin(sha256))
}

fn malformed(key: &'static str) -> Result<FetchReply, ReplyError> {
    Err(ReplyError::Malformed(key))
}

// ---------------------------------------------------------------------------------------
// Checks: the list handler
// ---------------------------------------------------------------------------------------

fn a_listing_is_the_share_set_for_the_requester_and_never_the_tree() -> Result<(), String> {
    let serve = Serve::build(
        "list-set",
        MAX_FETCH_FILE_BYTES,
        &["docs/**", "top.*"],
        &["docs/secret.md"],
        None,
    );
    for path in [
        "docs/a.md",
        "docs/secret.md",
        "top.md",
        "top.pem",
        "src/g.rs",
        "other.md",
    ] {
        serve.file(path, b"x");
    }
    serve.grant(&["src/g.rs"]);

    let page = answered(serve.list(list_body(None, None)))?;
    same(
        "paths",
        entry_paths(&page),
        strings(&["docs/a.md", "top.md"]),
    )?;
    let granted = answered(serve.fetch(fetch_body("src/g.rs", None)))?;
    same(
        "the granted file still fetches",
        status_of(&granted),
        Some("ok"),
    )
}

fn a_protected_inbox_inside_the_root_is_left_off_the_listing() -> Result<(), String> {
    let serve = Serve::build(
        "list-protected",
        MAX_FETCH_FILE_BYTES,
        &["**"],
        &[],
        Some("inbox"),
    );
    serve.file("inbox/inst/peer/a.md", b"x");
    serve.file("top.md", b"x");

    let page = answered(serve.list(list_body(None, None)))?;
    same("paths", entry_paths(&page), strings(&["top.md"]))
}

fn a_page_holds_a_thousand_entries_and_the_cursor_resumes_after_the_last() -> Result<(), String> {
    let serve = Serve::new("list-thousand", MAX_FETCH_FILE_BYTES, &["**"]);
    let all: Vec<String> = (0..=LIST_PAGE_SIZE)
        .map(|n| format!("f{n:04}.md"))
        .collect();
    for path in &all {
        serve.file(path, b"x");
    }

    let first = answered(serve.list(list_body(None, None)))?;
    same(
        "first page",
        entry_paths(&first),
        all[..LIST_PAGE_SIZE].to_vec(),
    )?;
    same(
        "next",
        next_of(&first),
        Some(list_cursor(&all[LIST_PAGE_SIZE - 1])),
    )?;
    let second = answered(serve.list(list_body(None, next_of(&first).as_deref())))?;
    same(
        "second page",
        entry_paths(&second),
        all[LIST_PAGE_SIZE..].to_vec(),
    )?;
    same("the last page carries no cursor", next_of(&second), None)?;
    same("page size", LIST_PAGE_SIZE, 1_000)
}

/// Four hundred entries fit the share set's page but not the wire: the page is cut where
/// the next entry would overflow the frame, and the cursor resumes after it.
fn a_page_is_cut_by_encoded_bytes_before_the_entry_count() -> Result<(), String> {
    let serve = Serve::new("list-cut", MAX_FETCH_FILE_BYTES, &["docs/**"]);
    let deep = ["a", "b", "c", "d"].map(|c| c.repeat(200)).join("/");
    let all: Vec<String> = (0..400).map(|n| format!("docs/{deep}/{n:03}.md")).collect();
    for path in &all {
        ensure(
            path.len() <= WIRE_PATH_MAX_BYTES,
            "a fixture path is over the wire cap",
        )?;
        serve.file(path, b"x");
    }

    let first = answered(serve.list(list_body(None, None)))?;
    let frame = framed(first.clone());
    ensure(
        frame.len() <= MAX_R3_PAYLOAD_BYTES,
        format!("the page frame is {} bytes", frame.len()),
    )?;
    let kept = entry_paths(&first);
    ensure(
        !kept.is_empty() && kept.len() < all.len(),
        format!("{} entries kept", kept.len()),
    )?;
    same("kept in order", kept.as_slice(), &all[..kept.len()])?;
    let cursor = next_of(&first).ok_or("a cut page carries a cursor")?;
    same("cursor", cursor.clone(), list_cursor(kept.last().unwrap()))?;
    let second = answered(serve.list(list_body(None, Some(&cursor))))?;
    same(
        "second page",
        entry_paths(&second),
        all[kept.len()..].to_vec(),
    )?;
    let entries = key_of(&first, "entries").ok_or("no entries")?;
    let next_entry = key_of(&second, "entries")
        .and_then(Value::as_array)
        .and_then(|entries| entries.first())
        .ok_or("the second page is empty")?;
    ensure(
        encoded_len(entries) + encoded_len(next_entry) + LIST_PAGE_HEADROOM > MAX_R3_PAYLOAD_BYTES,
        "the page was cut before the headroom was reached",
    )?;
    same("headroom", LIST_PAGE_HEADROOM, 2048)
}

fn the_cursor_is_the_first_half_of_the_sha256_of_the_path() -> Result<(), String> {
    same(
        "docs/a.md",
        list_cursor("docs/a.md"),
        "5231f8a11b65145a1b0727cb8d209819".to_string(),
    )?;
    same(
        "half the digest",
        list_cursor("docs/a.md"),
        hex_lower(&sha256_of(b"docs/a.md")[..16]),
    )
}

fn a_bounded_walk_truncates_the_listing_and_the_wire_page_carries_no_flag() -> Result<(), String> {
    let fx = Fixture::new("list-bound");
    fx.write(Layer::Global, &allow_all());
    for path in ["a.md", "b.md", "c.md", "d.md", "e.md", "f.md"] {
        fx.file(path);
    }
    let (identity, destination) = named_peer();
    let peer = PeerRef {
        identity: &identity,
        destination: &destination,
    };
    let set = fx.load();

    let bounded = set.list(&peer, None, None, false, 3);
    let whole = set.list(&peer, None, None, false, DEFAULT_LIST_WALK_BOUND);
    ensure(bounded.truncated, "a walk of 3 over 6 entries is truncated")?;
    ensure(
        bounded.entries.len() < whole.entries.len(),
        format!(
            "{} bounded entries of {}",
            bounded.entries.len(),
            whole.entries.len()
        ),
    )?;
    ensure(!whole.truncated, "the default bound truncates 6 entries")?;
    same("whole", whole.entries.len(), 6)?;
    same("default bound", DEFAULT_LIST_WALK_BOUND, 100_000)?;

    let serve = Serve::new("list-flag", MAX_FETCH_FILE_BYTES, &["**"]);
    serve.file("a.md", b"x");
    let page = answered(serve.list(list_body(None, None)))?;
    same("page keys", keys_of(&page), vec!["v", "entries", "next"])
}

/// A cursor on an entry that is never hashed still resumes after it: the walk names the
/// candidates and only the page returned is opened.
fn a_listing_hashes_only_the_page_it_returns() -> Result<(), String> {
    let serve = Serve::new("list-page-hash", MAX_FETCH_FILE_BYTES, &["**"]);
    for name in ["a.md", "b.md", "c.md"] {
        serve.file(name, name.as_bytes());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(serve.root.join("b.md"), fs::Permissions::from_mode(0o000)).unwrap();
    }

    let after_b = answered(serve.list(list_body(None, Some(&list_cursor("b.md")))))?;
    let first = answered(serve.list(list_body(None, None)))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(serve.root.join("b.md"), fs::Permissions::from_mode(0o644)).unwrap();
        same(
            "the unreadable file is dropped from its page",
            entry_paths(&first),
            strings(&["a.md", "c.md"]),
        )?;
    }
    #[cfg(not(unix))]
    same(
        "first page",
        entry_paths(&first),
        strings(&["a.md", "b.md", "c.md"]),
    )?;
    same(
        "the cursor on b.md resumes after it",
        entry_paths(&after_b),
        strings(&["c.md"]),
    )
}

fn no_root_no_serving_or_a_broken_share_list_answers_the_empty_page() -> Result<(), String> {
    let serve = Serve::new("list-empty", MAX_FETCH_FILE_BYTES, &["**"]);
    serve.file("a.md", b"x");
    let empty = page_value(Vec::new(), Value::Nil);
    let request = || serve.admitted(LIST_PATH, list_body(None, None));

    let no_root = Arc::new(TestSource {
        root: None,
        serving: Some(Arc::clone(&serve.serving)),
    });
    same(
        "no share root",
        answered(list_via(&no_root, request()))?,
        empty.clone(),
    )?;
    let no_serving = Arc::new(TestSource {
        root: Some(serve.root.clone()),
        serving: None,
    });
    same(
        "no serving state",
        answered(list_via(&no_serving, request()))?,
        empty.clone(),
    )?;
    let unprobable = Arc::new(TestSource {
        root: Some(serve.root.join("missing")),
        serving: Some(Arc::clone(&serve.serving)),
    });
    same(
        "a root that cannot be probed",
        answered(list_via(&unprobable, request()))?,
        empty.clone(),
    )?;
    fs::write(serve.shares_file(), "version: 1\nallow: 7\n").unwrap();
    same(
        "a refused share list",
        answered(serve.list(list_body(None, None)))?,
        empty,
    )
}

fn an_unknown_or_blocked_identity_hears_silence_on_list_and_fetch() -> Result<(), String> {
    let serve = Serve::new("dispatch-silence", MAX_FETCH_FILE_BYTES, &["**"]);
    serve.file("a.md", b"x");
    let identity = PrivateIdentity::new_from_rand(OsRng);
    let identity_hex = identity.as_identity().address_hash.to_hex_string();
    let silenced = [
        ("unknown", TrustList::default()),
        ("blocked", TrustList::default().block(&identity_hex)),
    ];
    let trusted = TrustList::default().identity(&identity_hex, true);

    for (path, body) in [
        (LIST_PATH, list_body(None, None)),
        (FETCH_PATH, fetch_body("a.md", None)),
    ] {
        for (shape, trust) in &silenced {
            let reply = serve.dispatch(trust, &identity, path, body.clone());
            ensure(
                matches!(reply, Reply::Silent),
                format!(
                    "{shape} identity on {path}: expected silence, observed {}",
                    describe(&reply)
                ),
            )?;
        }
        let reply = serve.dispatch(&trusted, &identity, path, body);
        ensure(
            matches!(reply, Reply::Value(_) | Reply::Settled { .. }),
            format!(
                "trusted identity on {path}: expected an answer, observed {}",
                describe(&reply)
            ),
        )?;
    }
    Ok(())
}

fn a_listing_waits_a_round_trip() -> Result<(), String> {
    same(
        "PEER_REQUEST_TIMEOUT",
        PEER_REQUEST_TIMEOUT,
        Duration::from_secs(15),
    )
}

// ---------------------------------------------------------------------------------------
// Checks: the fetch handler
// ---------------------------------------------------------------------------------------

fn an_invalid_path_is_refused_before_the_root_is_probed() -> Result<(), String> {
    let serve = Serve::new("fetch-grammar-first", MAX_FETCH_FILE_BYTES, &["**"]);
    for (text, rule) in [
        ("../../.bashrc", "segment"),
        ("docs\\a.md", "backslash"),
        ("/etc/passwd", "leading_slash"),
        ("", "empty"),
    ] {
        let value = answered(serve.fetch(fetch_body(text, None)))?;
        same(
            &format!("{text:?} status"),
            status_of(&value),
            Some("invalid_path"),
        )?;
        same(
            &format!("{text:?} rule"),
            key_of(&value, "rule").and_then(Value::as_str),
            Some(rule),
        )?;
    }
    ensure(
        serve.serving.probed_case_for(&serve.root).is_none(),
        "the root was probed for a path the grammar refuses",
    )?;
    same(
        "entries under the root",
        fs::read_dir(&serve.root).unwrap().count(),
        0,
    )
}

fn too_large_is_decided_at_the_stat_before_a_grant_use_is_spent() -> Result<(), String> {
    let serve = Serve::new("fetch-too-large-grant", 8, &[]);
    serve.file("big.bin", &[b'x'; 9]);
    serve.grant(&["big.bin"]);
    let lent = serve.uses_left();

    let value = answered(serve.fetch(fetch_body("big.bin", None)))?;
    same("status", status_of(&value), Some("too_large"))?;
    same(
        "limit",
        key_of(&value, "limit").and_then(Value::as_u64),
        Some(8),
    )?;
    same("uses left", serve.uses_left(), lent)
}

fn a_failed_read_is_not_shared_and_a_file_that_grew_is_too_large() -> Result<(), String> {
    let serve = Serve::new("fetch-read", 8, &["**"]);
    serve.file("a.md", b"x");
    let missing = answered(serve.fetch(fetch_body("missing.md", None)))?;

    let failed = answered(serve.fetch_with(
        FetchHandler::with_reader(weak(&serve.source), Arc::new(FailingReader)),
        fetch_body("a.md", None),
    ))?;
    same(
        "a failed read is not_shared, byte for byte",
        framed(failed),
        framed(missing),
    )?;
    let grown = answered(serve.fetch_with(
        FetchHandler::with_reader(weak(&serve.source), Arc::new(GrowingReader)),
        fetch_body("a.md", None),
    ))?;
    same("grown status", status_of(&grown), Some("too_large"))?;
    same(
        "grown limit",
        key_of(&grown, "limit").and_then(Value::as_u64),
        Some(8),
    )
}

fn a_matching_if_sha256_is_not_modified_without_a_body_and_spends_the_use() -> Result<(), String> {
    let serve = Serve::new("fetch-not-modified", MAX_FETCH_FILE_BYTES, &[]);
    let bytes = b"# docs\n";
    serve.file("docs/a.md", bytes);
    serve.grant(&["docs/a.md"]);
    let lent = serve.uses_left();

    let value = answered(serve.fetch(fetch_body("docs/a.md", Some(sha256_of(bytes)))))?;
    same("status", status_of(&value), Some("not_modified"))?;
    same("keys", keys_of(&value), vec!["v", "status", "sha256"])?;
    same(
        "sha256",
        key_of(&value, "sha256").cloned(),
        Some(bin(&sha256_of(bytes))),
    )?;
    same("uses left", serve.uses_left(), lent - 1)
}

fn an_ok_reply_carries_the_size_the_digest_and_the_bytes() -> Result<(), String> {
    let serve = Serve::new("fetch-ok", MAX_FETCH_FILE_BYTES, &["**"]);
    let bytes = b"# docs\n";
    serve.file("docs/a.md", bytes);

    let value = answered(serve.fetch(fetch_body("docs/a.md", None)))?;
    same("status", status_of(&value), Some("ok"))?;
    same(
        "keys",
        keys_of(&value),
        vec!["v", "status", "size", "sha256", "bytes"],
    )?;
    same(
        "size",
        key_of(&value, "size").and_then(Value::as_u64),
        Some(bytes.len() as u64),
    )?;
    same(
        "sha256",
        key_of(&value, "sha256").cloned(),
        Some(bin(&sha256_of(bytes))),
    )?;
    same("bytes", key_of(&value, "bytes").cloned(), Some(bin(bytes)))?;
    let stale = answered(serve.fetch(fetch_body("docs/a.md", Some([0u8; 32]))))?;
    same(
        "a stale if_sha256 is answered ok",
        status_of(&stale),
        Some("ok"),
    )
}

fn not_shared_is_byte_identical_for_a_missing_an_unshared_and_a_denied_file() -> Result<(), String>
{
    let serve = Serve::new("fetch-not-shared", MAX_FETCH_FILE_BYTES, &["docs/**"]);
    serve.file("src/x.rs", b"x");
    serve.file("docs/.env", b"x");

    let missing = answered(serve.fetch(fetch_body("docs/missing.md", None)))?;
    same("status", status_of(&missing), Some("not_shared"))?;
    same("keys", keys_of(&missing), vec!["v", "status"])?;
    let missing = framed(missing);
    let unshared = framed(answered(serve.fetch(fetch_body("src/x.rs", None)))?);
    let denied = framed(answered(serve.fetch(fetch_body("docs/.env", None)))?);
    same("unshared", unshared, missing.clone())?;
    same("builtin-denied", denied, missing)
}

fn the_serving_limit_is_the_configured_bytes_under_the_ceiling() -> Result<(), String> {
    let small = Serve::new("fetch-limit-small", 8, &["**"]);
    same("small limit", small.serving.serving_limit(), 8)?;
    let full = Serve::new("fetch-limit-full", MAX_FETCH_FILE_BYTES, &["**"]);
    same(
        "capped limit",
        full.serving.serving_limit(),
        MAX_FETCH_FILE_BYTES.min(SINGLE_SEGMENT_FETCH_CEILING),
    )?;
    same("file ceiling", MAX_FETCH_FILE_BYTES, 4_194_304)?;
    small.file("big.bin", &[b'x'; 9]);
    let value = answered(small.fetch(fetch_body("big.bin", None)))?;
    same("status", status_of(&value), Some("too_large"))?;
    same(
        "limit",
        key_of(&value, "limit").and_then(Value::as_u64),
        Some(8),
    )
}

fn a_file_one_past_the_ceiling_is_too_large_and_one_at_it_is_ok() -> Result<(), String> {
    let serve = Serve::new("fetch-ceiling", MAX_FETCH_FILE_BYTES, &["**"]);
    let ceiling = SINGLE_SEGMENT_FETCH_CEILING as usize;
    serve.file("over.bin", &vec![b'x'; ceiling + 1]);
    serve.file("at.bin", &vec![b'x'; ceiling]);

    let over = answered(serve.fetch(fetch_body("over.bin", None)))?;
    same("over status", status_of(&over), Some("too_large"))?;
    same(
        "over limit",
        key_of(&over, "limit").and_then(Value::as_u64),
        Some(1_048_447),
    )?;
    let at = answered(serve.fetch(fetch_body("at.bin", None)))?;
    same("at status", status_of(&at), Some("ok"))?;
    same(
        "at size",
        key_of(&at, "size").and_then(Value::as_u64),
        Some(SINGLE_SEGMENT_FETCH_CEILING),
    )
}

fn a_served_fetch_fires_once_sent_with_peer_size_and_hash_prefix_and_no_path() -> Result<(), String>
{
    let serve = Serve::new("fetch-hook", MAX_FETCH_FILE_BYTES, &["docs/**"]);
    let bytes = b"# docs\n";
    serve.file("docs/a.md", bytes);
    let sink = RecordingHookSink::attach(&serve.hooks);

    let refused = answered(serve.fetch(fetch_body("docs/missing.md", None)))?;
    same("refused", status_of(&refused), Some("not_shared"))?;
    ensure(sink.drain().is_empty(), "a not_shared reply fired a hook")?;
    let unsent = serve.fetch(fetch_body("docs/a.md", None));
    ensure(
        matches!(unsent, Reply::Settled { .. }),
        format!("expected a settled ok, observed {}", describe(&unsent)),
    )?;
    drop(unsent);
    ensure(sink.drain().is_empty(), "an unsent reply fired a hook")?;

    let Reply::Settled { settlement, .. } = serve.fetch(fetch_body("docs/a.md", None)) else {
        return Err("an ok reply settles on send".to_string());
    };
    settlement.sent();
    let fired = sink.drain();
    let [(event, envs)] = fired.as_slice() else {
        return Err(format!("expected one fire, observed {fired:?}"));
    };
    same("event", event, &HookEvent::MeshFetchServed)?;
    same(
        "identity",
        env_value(envs, "COYOTE_MESH_PEER_IDENTITY"),
        Some(serve.identity_hex.as_str()),
    )?;
    same(
        "destination",
        env_value(envs, "COYOTE_MESH_PEER_DESTINATION"),
        Some(serve.destination.as_str()),
    )?;
    same(
        "size",
        env_value(envs, "COYOTE_MESH_SIZE"),
        Some(bytes.len().to_string().as_str()),
    )?;
    same(
        "hash prefix",
        env_value(envs, "COYOTE_MESH_HASH_PREFIX"),
        Some(&hex_lower(&sha256_of(bytes))[..8]),
    )?;
    ensure(
        envs.iter()
            .all(|(_, value)| !value.contains("docs") && !value.contains("a.md")),
        format!("a path leaked into the hook environment: {envs:?}"),
    )
}

fn an_ok_dropped_unsent_refunds_the_grant_use_and_one_sent_keeps_it_spent() -> Result<(), String> {
    let serve = Serve::new("fetch-refund", MAX_FETCH_FILE_BYTES, &[]);
    serve.file("docs/a.md", b"x");
    serve.grant(&["docs/a.md"]);
    let lent = serve.uses_left();

    let unsent = serve.fetch(fetch_body("docs/a.md", None));
    ensure(
        matches!(unsent, Reply::Settled { .. }),
        format!("expected a settled ok, observed {}", describe(&unsent)),
    )?;
    same("spent while in flight", serve.uses_left(), lent - 1)?;
    drop(unsent);
    same("refunded when dropped unsent", serve.uses_left(), lent)?;

    let Reply::Settled { settlement, .. } = serve.fetch(fetch_body("docs/a.md", None)) else {
        return Err("an ok reply settles on send".to_string());
    };
    settlement.sent();
    same("spent once sent", serve.uses_left(), lent - 1)
}

// ---------------------------------------------------------------------------------------
// Checks: the requester
// ---------------------------------------------------------------------------------------

fn a_card_with_a_malformed_about_or_caps_keeps_the_rest() -> Result<(), String> {
    let card = |about: Value, caps: Value| {
        StatusCard::from_value(&map(vec![
            ("v", Value::from(STATUS_CARD_VERSION)),
            ("state", map(vec![("code", Value::from(STATE_IDLE))])),
            ("served_at_secs", Value::from(1_700_000_000u64)),
            ("about", about),
            ("caps", caps),
        ]))
        .map_err(|err| format!("{err:?}"))
    };

    let malformed = card(Value::from(7), Value::from(7))?;
    same("about", malformed.about, None)?;
    same("caps", malformed.caps, Vec::<String>::new())?;
    same("state", malformed.state.code, STATE_IDLE)?;
    let well = card(
        Value::from("a node"),
        Value::Array(vec![
            Value::from(7),
            Value::from("fetch"),
            Value::from("   "),
            Value::from("x".repeat(40)),
        ]),
    )?;
    same("about kept", well.about, Some("a node".to_string()))?;
    same("caps", well.caps, vec!["fetch".to_string(), "x".repeat(32)])
}

fn an_unknown_status_is_a_client_error_with_the_pinned_wording() -> Result<(), String> {
    same(
        "wording",
        FetchError::UnknownStatus.to_string(),
        "peer sent an unknown status".to_string(),
    )
}

fn a_rule_is_kept_when_known_read_as_unknown_otherwise_and_malformed_when_not_text()
-> Result<(), String> {
    same(
        "known",
        rule_of(&Value::from("segment")),
        Some("segment".to_string()),
    )?;
    same(
        "other text",
        rule_of(&Value::from("the path was bad")),
        Some("unknown".to_string()),
    )?;
    same("not text", rule_of(&Value::from(7)), None)?;
    same("nil", rule_of(&Value::Nil), None)
}

fn a_fetch_response_at_its_bound_is_delivered_and_one_byte_over_is_dropped() -> Result<(), String> {
    block_on(async {
        let client = R3Client::new();
        let link_id = LinkId::new_from_rand(OsRng);

        let over_id = RequestId::from([0xa1u8; 16]);
        let mut over = pending_on(&client, over_id, link_id, FETCH_PATH)?;
        client.deliver(
            link_id,
            &frame_of(over_id, MAX_FETCH_RESPONSE_BYTES + 1)?,
            SizeBranch::Resource,
        );
        ensure(
            matches!(over.try_recv(), Err(TryRecvError::Empty)),
            "one byte over the fetch bound was delivered or failed the request",
        )?;

        let at_id = RequestId::from([0xa2u8; 16]);
        let at = pending_on(&client, at_id, link_id, FETCH_PATH)?;
        client.deliver(
            link_id,
            &frame_of(at_id, MAX_FETCH_RESPONSE_BYTES)?,
            SizeBranch::Resource,
        );
        let (value, branch) = at
            .await
            .map_err(|err| format!("the pending request was dropped: {err}"))?
            .map_err(|err| format!("the response failed: {err:?}"))?;
        same("branch", branch, SizeBranch::Resource)?;
        same(
            "payload",
            value.as_slice().map(<[u8]>::len),
            Some(MAX_FETCH_RESPONSE_BYTES - RESPONSE_FRAME_OVERHEAD),
        )?;
        same(
            "bound",
            MAX_FETCH_RESPONSE_BYTES,
            MAX_FETCH_FILE_BYTES as usize + 4096,
        )
    })
}

fn the_bound_is_found_from_the_frame_prefix_before_the_frame_is_decoded() -> Result<(), String> {
    install_log_collector();
    block_on(async {
        let client = R3Client::new();
        let link_id = LinkId::new_from_rand(OsRng);
        let oversize_line = |len: usize, max: usize| {
            format!(
                "Dropped an oversize mesh response on link {} ({len} bytes, max {max})",
                link_id.to_hex_string()
            )
        };

        // One frame, one byte over the common bound: dropped for a pending `/status`,
        // delivered for a pending `/fetch`.
        let status_id = RequestId::from([0xb1u8; 16]);
        let mut status = pending_on(&client, status_id, link_id, STATUS_PATH)?;
        client.deliver(
            link_id,
            &frame_of(status_id, MAX_R3_PAYLOAD_BYTES + 1)?,
            SizeBranch::Resource,
        );
        ensure(
            matches!(status.try_recv(), Err(TryRecvError::Empty)),
            "a status response over the common bound was delivered or failed the request",
        )?;
        ensure(
            debug_snapshot().contains(&oversize_line(
                MAX_R3_PAYLOAD_BYTES + 1,
                MAX_R3_PAYLOAD_BYTES,
            )),
            "the status drop was not logged against the common bound",
        )?;
        let fetch_id = RequestId::from([0xb2u8; 16]);
        let fetch = pending_on(&client, fetch_id, link_id, FETCH_PATH)?;
        client.deliver(
            link_id,
            &frame_of(fetch_id, MAX_R3_PAYLOAD_BYTES + 1)?,
            SizeBranch::Resource,
        );
        let (value, _) = fetch
            .await
            .map_err(|err| format!("the pending request was dropped: {err}"))?
            .map_err(|err| format!("the response failed: {err:?}"))?;
        same(
            "fetch payload",
            value.as_slice().map(<[u8]>::len),
            Some(MAX_R3_PAYLOAD_BYTES + 1 - RESPONSE_FRAME_OVERHEAD),
        )?;

        // No frame prefix: the coarse bound.
        client.deliver(
            link_id,
            &vec![0xc0; MAX_FETCH_RESPONSE_BYTES + 1],
            SizeBranch::Resource,
        );
        ensure(
            debug_snapshot().contains(&oversize_line(
                MAX_FETCH_RESPONSE_BYTES + 1,
                MAX_FETCH_RESPONSE_BYTES,
            )),
            "bytes without a frame prefix were not judged under the coarse bound",
        )?;

        // A frame for an id nothing is pending on: the coarse bound, so one byte over the
        // common bound reaches the decoder and fails to match.
        let stray_id = RequestId::from([0xb3u8; 16]);
        client.deliver(
            link_id,
            &frame_of(stray_id, MAX_R3_PAYLOAD_BYTES + 1)?,
            SizeBranch::Resource,
        );
        let unmatched = format!("Unmatched mesh response {}", stray_id.to_hex_string());
        ensure(
            debug_snapshot()
                .iter()
                .any(|line| line.contains(&unmatched)),
            "a frame for no pending request was not judged under the coarse bound",
        )?;
        same("prefix", RESPONSE_FRAME_PREFIX, [0x92, 0xc4, 0x10])
    })
}

fn the_inbox_stages_under_the_instance_and_peer_and_refuses_a_collision() -> Result<(), String> {
    let tmp = TempDir::new("inbox-stage");
    let cache_dir = tmp.path.join("cache");
    let staging = InboxStaging::for_instance_under(None, &cache_dir, "inst");
    same(
        "inbox under the cache dir",
        staging.root().to_path_buf(),
        inbox_root(&cache_dir, "inst"),
    )?;
    let configured =
        InboxStaging::for_instance_under(Some(&tmp.path.join("drop")), &cache_dir, "inst");
    same(
        "configured inbox",
        configured.root().to_path_buf(),
        tmp.path.join("drop").join("inst"),
    )?;

    let destination = "ABCDEF0123456789abcdef0123456789";
    let path = WirePath::parse("docs/a.md").map_err(|invalid| invalid.rule.to_string())?;
    let stage = |bytes: &[u8]| staging.stage(destination, &path, &sha256_of(bytes), bytes);
    let first = b"one";
    let staged = stage(first).map_err(|err| err.to_string())?;
    let expected = dunce::canonicalize(staging.root())
        .unwrap()
        .join("abcdef0123456789abcdef0123456789")
        .join("docs")
        .join("a.md");
    same("staged path", staged.clone(), expected.clone())?;
    same("staged bytes", fs::read(&staged).unwrap(), first.to_vec())?;
    same(
        "the same bytes reuse the file",
        stage(first).map_err(|err| err.to_string())?,
        expected.clone(),
    )?;
    let second = b"two";
    let sibling = stage(second).map_err(|err| err.to_string())?;
    same(
        "other bytes land beside it",
        sibling.clone(),
        expected.with_file_name(format!("a-{}.md", &hex_lower(&sha256_of(second))[..8])),
    )?;
    fs::write(&sibling, b"three").unwrap();
    ensure(
        matches!(stage(second), Err(StageError::Collision)),
        "a target and a sibling both holding other bytes did not collide",
    )?;
    same(
        "the target is untouched",
        fs::read(&staged).unwrap(),
        first.to_vec(),
    )
}

fn a_fetched_file_yields_its_path_size_and_digest_and_never_its_bytes() -> Result<(), String> {
    let fetched = Fetched::Staged {
        path: PathBuf::from("inbox/a.md"),
        size: 3,
        sha256: sha256_of(b"one"),
    };
    // Destructured without a rest pattern, so a bytes field would fail to compile here.
    let Fetched::Staged { path, size, sha256 } = &fetched else {
        return Err("a staged fetch".to_string());
    };
    same("path", path.as_path(), Path::new("inbox/a.md"))?;
    same("size", *size, 3)?;
    same("digest", *sha256, sha256_of(b"one"))
}

fn a_fetch_waits_two_minutes() -> Result<(), String> {
    same(
        "FILE_FETCH_REQUEST_TIMEOUT",
        FILE_FETCH_REQUEST_TIMEOUT,
        Duration::from_secs(120),
    )
}

// ---------------------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------------------

fn vectors() -> Vec<Vector> {
    let mut rows = Vec::new();
    rows.extend(wire_path_rows());
    rows.extend(share_set_rows());
    rows.extend(grant_store_rows());
    rows.extend(store_schema_rows());
    rows.extend(list_serve_rows());
    rows.extend(fetch_serve_rows());
    rows.extend(fetch_client_rows());
    rows
}

fn wire_path_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    let sixty_four = vec!["a"; WIRE_PATH_MAX_SEGMENTS].join("/");
    let sixty_five = vec!["a"; WIRE_PATH_MAX_SEGMENTS + 1].join("/");
    let mut rows = vec![
        // The fourteen rules, in `RULES` order.
        parse("MESH-FETCH-003", Invalid, "", Err("empty")),
        parse(
            "MESH-FETCH-003",
            Invalid,
            "a".repeat(WIRE_PATH_MAX_BYTES + 1),
            Err("length"),
        ),
        parse(
            "MESH-FETCH-003",
            Boundary,
            "a".repeat(WIRE_PATH_MAX_BYTES),
            Ok(()),
        ),
        parse("MESH-FETCH-003", Invalid, "a\tb", Err("control")),
        parse("MESH-FETCH-003", Invalid, "a\u{200B}b", Err("invisible")),
        parse("MESH-FETCH-003", Invalid, "docs\\a.md", Err("backslash")),
        parse(
            "MESH-FETCH-003",
            Invalid,
            "/etc/passwd",
            Err("leading_slash"),
        ),
        parse("MESH-FETCH-003", Invalid, "c:/x", Err("drive_letter")),
        parse(
            "MESH-FETCH-003",
            Invalid,
            "docs/x:Zone.Identifier",
            Err("colon"),
        ),
        parse("MESH-FETCH-003", Invalid, "cafe\u{301}.md", Err("nfc")),
        parse("MESH-FETCH-003", Valid, "caf\u{E9}.md", Ok(())),
        parse("MESH-FETCH-003", Invalid, sixty_five, Err("segments")),
        parse("MESH-FETCH-003", Boundary, sixty_four, Ok(())),
        parse("MESH-FETCH-003", Invalid, "docs//a.md", Err("segment")),
        parse("MESH-FETCH-003", Invalid, "docs/../a.md", Err("segment")),
        parse("MESH-FETCH-003", Invalid, "docs./a.md", Err("trailing_dot")),
        parse(
            "MESH-FETCH-003",
            Invalid,
            "docs /a.md",
            Err("trailing_space"),
        ),
        parse(
            "MESH-FETCH-003",
            Invalid,
            "docs/CON.txt",
            Err("reserved_name"),
        ),
        parse("MESH-FETCH-003", Invalid, "com1.md", Err("reserved_name")),
        parse("MESH-FETCH-003", Valid, "docs/a.md", Ok(())),
        parse("MESH-FETCH-003", Valid, "src/mesh/peer.rs", Ok(())),
        // Precedence: the first rule broken names the refusal.
        parse("MESH-FETCH-003", Invalid, "a\0b", Err("control")),
        parse("MESH-FETCH-003", Invalid, "C:\\x", Err("backslash")),
        parse("MESH-FETCH-003", Invalid, "C:x", Err("drive_letter")),
        parse("MESH-FETCH-003", Invalid, "cafe\u{301}/../x", Err("nfc")),
        parse("MESH-FETCH-003", Invalid, ".", Err("segment")),
        parse("MESH-FETCH-003", Invalid, "..", Err("segment")),
        parse("MESH-FETCH-003", Invalid, "con.", Err("trailing_dot")),
        // The rule vocabulary.
        row(
            "MESH-FETCH-003",
            Invalid,
            Case::WirePath(WireProbe::RuleId {
                rule: "unknown",
                known: false,
            }),
        ),
        row(
            "MESH-FETCH-003",
            Invalid,
            Case::WirePath(WireProbe::RuleId {
                rule: "Segment",
                known: false,
            }),
        ),
        row(
            "MESH-FETCH-003",
            Invalid,
            Case::WirePath(WireProbe::RuleId {
                rule: "",
                known: false,
            }),
        ),
        // Every path on the wire passes the same gate.
        row(
            "MESH-FETCH-005",
            Valid,
            Case::WirePath(WireProbe::AccessPaths {
                paths: &["docs/a.md", "src/b.rs"],
                accepted: true,
            }),
        ),
        row(
            "MESH-FETCH-005",
            Invalid,
            Case::WirePath(WireProbe::AccessPaths {
                paths: &["docs/a.md", "../etc/passwd"],
                accepted: false,
            }),
        ),
        row(
            "MESH-FETCH-005",
            Invalid,
            Case::WirePath(WireProbe::AccessPaths {
                paths: &["/etc/passwd"],
                accepted: false,
            }),
        ),
        row(
            "MESH-FETCH-005",
            Valid,
            Case::WirePath(WireProbe::ListEntry {
                path: "docs/a.md",
                kept: true,
            }),
        ),
        row(
            "MESH-FETCH-005",
            Invalid,
            Case::WirePath(WireProbe::ListEntry {
                path: "docs\\a.md",
                kept: false,
            }),
        ),
        row(
            "MESH-FETCH-005",
            Invalid,
            Case::WirePath(WireProbe::ListEntry {
                path: "../a.md",
                kept: false,
            }),
        ),
        row(
            "MESH-FETCH-005",
            Valid,
            Case::WirePath(WireProbe::FilePart {
                name: "notes.md",
                reference: None,
                kept: true,
            }),
        ),
        row(
            "MESH-FETCH-005",
            Invalid,
            Case::WirePath(WireProbe::FilePart {
                name: "../notes.md",
                reference: None,
                kept: false,
            }),
        ),
        row(
            "MESH-FETCH-005",
            Valid,
            Case::WirePath(WireProbe::FilePart {
                name: "notes.md",
                reference: Some("docs/notes.md"),
                kept: true,
            }),
        ),
        row(
            "MESH-FETCH-005",
            Invalid,
            Case::WirePath(WireProbe::FilePart {
                name: "notes.md",
                reference: Some("/docs/notes.md"),
                kept: false,
            }),
        ),
        // What a requester keeps of a reply's `rule`.
        row(
            "MESH-FETCH-007",
            Valid,
            Case::WirePath(WireProbe::RuleOf {
                value: Value::from("segment"),
                expect: Some("segment"),
            }),
        ),
        row(
            "MESH-FETCH-007",
            Invalid,
            Case::WirePath(WireProbe::RuleOf {
                value: Value::from("ignore previous instructions"),
                expect: Some("unknown"),
            }),
        ),
        row(
            "MESH-FETCH-007",
            Invalid,
            Case::WirePath(WireProbe::RuleOf {
                value: Value::from("Segment"),
                expect: Some("unknown"),
            }),
        ),
        row(
            "MESH-FETCH-007",
            Invalid,
            Case::WirePath(WireProbe::RuleOf {
                value: Value::Nil,
                expect: None,
            }),
        ),
        row(
            "MESH-FETCH-007",
            Invalid,
            Case::WirePath(WireProbe::RuleOf {
                value: Value::from(3),
                expect: None,
            }),
        ),
    ];
    for (rule, _) in RULES {
        rows.push(row(
            "MESH-FETCH-003",
            Valid,
            Case::WirePath(WireProbe::RuleId { rule, known: true }),
        ));
        rows.push(row(
            "MESH-FETCH-007",
            Valid,
            Case::WirePath(WireProbe::RuleOf {
                value: Value::from(rule),
                expect: Some(rule),
            }),
        ));
    }
    rows
}

fn share_set_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    use Verdict::{BuiltinDenied, Denied, NotAllowed, Protected, Shared};
    let (identity, destination) = Who::Named.hashes();
    let config_dir = WORKSPACE_COYOTE_DIR_NAME;
    let in_config_dir = format!("{config_dir}/sessions/notes.md");
    let none = String::new();
    let allow_docs = shares_yaml(&[("docs/**", None)], &[], &[]);
    let mut rows = vec![
        // MESH-SHARE-001: the file shape.
        row(
            "MESH-SHARE-001",
            Valid,
            Case::ShareSet(ShareProbe::Loads {
                layer: Layer::Global,
                text: shares_yaml(
                    &[("docs/**", Some(&identity))],
                    &["docs/private/**"],
                    &["docs/.env.example"],
                ),
            }),
        ),
        row(
            "MESH-SHARE-001",
            Boundary,
            Case::ShareSet(ShareProbe::Loads {
                layer: Layer::Workspace,
                text: format!("version: {SHARES_FILE_VERSION}\n"),
            }),
        ),
        row(
            "MESH-SHARE-001",
            Invalid,
            Case::ShareSet(ShareProbe::Poisoned {
                layer: Layer::Global,
                text: format!("version: {SHARES_FILE_VERSION}\nallow: []\nextra: 1\n"),
            }),
        ),
        row(
            "MESH-SHARE-001",
            Invalid,
            Case::ShareSet(ShareProbe::Poisoned {
                layer: Layer::Global,
                text: format!(
                    "version: {SHARES_FILE_VERSION}\nallow:\n- pattern: docs/**\n  peers: everyone\n"
                ),
            }),
        ),
        share_check(
            "MESH-SHARE-001",
            Invalid,
            a_noncanonical_peer_is_refused_at_write_and_matches_nobody_on_disk,
        ),
        // MESH-SHARE-002: patterns and overrides.
        row(
            "MESH-SHARE-002",
            Valid,
            Case::ShareSet(ShareProbe::Pattern {
                text: "docs/**",
                valid: true,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Valid,
            Case::ShareSet(ShareProbe::Pattern {
                text: "src/*.rs",
                valid: true,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Valid,
            Case::ShareSet(ShareProbe::Pattern {
                text: "**",
                valid: true,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Pattern {
                text: "docs\\**",
                valid: false,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Pattern {
                text: "/docs/**",
                valid: false,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Pattern {
                text: "C:/docs/**",
                valid: false,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Pattern {
                text: "docs/../**",
                valid: false,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Pattern {
                text: "./docs/**",
                valid: false,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Pattern {
                text: "docs//**",
                valid: false,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Valid,
            Case::ShareSet(ShareProbe::Override {
                text: "docs/.env.example",
                valid: true,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Override {
                text: "docs/*",
                valid: false,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Override {
                text: "docs/.env?",
                valid: false,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Override {
                text: "/etc/.env",
                valid: false,
            }),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Override {
                text: "docs/../.env",
                valid: false,
            }),
        ),
        verdict(
            "MESH-SHARE-002",
            Valid,
            shares_yaml(&[("docs/*", None)], &[], &[]),
            none.clone(),
            &["docs/a.md"],
            "docs/a.md",
            Some(Shared),
        ),
        verdict(
            "MESH-SHARE-002",
            Boundary,
            shares_yaml(&[("docs/*", None)], &[], &[]),
            none.clone(),
            &["docs/sub/a.md"],
            "docs/sub/a.md",
            Some(NotAllowed),
        ),
        verdict(
            "MESH-SHARE-002",
            Valid,
            shares_yaml(&[("docs/**", None)], &[], &[]),
            none.clone(),
            &["docs/sub/a.md"],
            "docs/sub/a.md",
            Some(Shared),
        ),
        verdict(
            "MESH-SHARE-002",
            Boundary,
            shares_yaml(&[("docs/a?.md", None)], &[], &[]),
            none.clone(),
            &["docs/a/.md"],
            "docs/a/.md",
            Some(NotAllowed),
        ),
        row(
            "MESH-SHARE-002",
            Invalid,
            Case::ShareSet(ShareProbe::Poisoned {
                layer: Layer::Global,
                text: shares_yaml(&[], &[], &["docs/*"]),
            }),
        ),
        // MESH-SHARE-003: a refused file fails the set closed.
        row(
            "MESH-SHARE-003",
            Invalid,
            Case::ShareSet(ShareProbe::Poisoned {
                layer: Layer::Global,
                text: shares_yaml(&[], &["/secrets/**"], &[]),
            }),
        ),
        row(
            "MESH-SHARE-003",
            Invalid,
            Case::ShareSet(ShareProbe::Poisoned {
                layer: Layer::Workspace,
                text: shares_yaml(&[("docs/../**", None)], &[], &[]),
            }),
        ),
        row(
            "MESH-SHARE-003",
            Invalid,
            Case::ShareSet(ShareProbe::Poisoned {
                layer: Layer::Workspace,
                text: "allow: [\n".to_string(),
            }),
        ),
        share_check(
            "MESH-SHARE-003",
            Invalid,
            a_refused_file_serves_nothing_from_either_layer_and_takes_no_mutation,
        ),
        // MESH-SHARE-004: the order of judgement.
        verdict(
            "MESH-SHARE-004",
            Invalid,
            shares_yaml(&[("**", None)], &[".git/**"], &[".git/config"]),
            none.clone(),
            &[".git/config"],
            ".git/config",
            Some(Protected),
        ),
        verdict(
            "MESH-SHARE-004",
            Invalid,
            shares_yaml(&[("**", None)], &[".env"], &[".env"]),
            none.clone(),
            &[".env"],
            ".env",
            Some(Denied),
        ),
        verdict(
            "MESH-SHARE-004",
            Invalid,
            allow_all(),
            none.clone(),
            &[".env"],
            ".env",
            Some(BuiltinDenied),
        ),
        verdict(
            "MESH-SHARE-004",
            Valid,
            shares_yaml(&[("**", None)], &[], &[".env"]),
            none.clone(),
            &[".env"],
            ".env",
            Some(Shared),
        ),
        verdict(
            "MESH-SHARE-004",
            Valid,
            allow_docs.clone(),
            none.clone(),
            &["docs/a.md"],
            "docs/a.md",
            Some(Shared),
        ),
        verdict(
            "MESH-SHARE-004",
            Boundary,
            allow_docs.clone(),
            none.clone(),
            &["src/a.rs"],
            "src/a.rs",
            Some(NotAllowed),
        ),
        verdict(
            "MESH-SHARE-004",
            Invalid,
            allow_all(),
            shares_yaml(&[], &["docs/**"], &[]),
            &["docs/a.md"],
            "docs/a.md",
            Some(Denied),
        ),
        verdict(
            "MESH-SHARE-004",
            Invalid,
            shares_yaml(&[], &["docs/**"], &[]),
            allow_all(),
            &["docs/a.md"],
            "docs/a.md",
            Some(Denied),
        ),
        verdict(
            "MESH-SHARE-004",
            Valid,
            shares_yaml(&[], &["docs/**"], &[]),
            allow_all(),
            &["src/a.rs"],
            "src/a.rs",
            Some(Shared),
        ),
        verdict(
            "MESH-SHARE-004",
            Boundary,
            none.clone(),
            none.clone(),
            &["docs/a.md"],
            "docs/missing.md",
            None,
        ),
        // MESH-SHARE-005: protected directories.
        verdict(
            "MESH-SHARE-005",
            Invalid,
            shares_yaml(&[("**", None)], &[], &[".git/HEAD"]),
            none.clone(),
            &[".git/HEAD"],
            ".git/HEAD",
            Some(Protected),
        ),
        verdict(
            "MESH-SHARE-005",
            Invalid,
            allow_all(),
            none.clone(),
            &["vendor/lib/.git/config"],
            "vendor/lib/.git/config",
            Some(Protected),
        ),
        verdict(
            "MESH-SHARE-005",
            Invalid,
            shares_yaml(&[("**", None)], &[], &[&in_config_dir]),
            none.clone(),
            &[&in_config_dir],
            &in_config_dir,
            Some(Protected),
        ),
        verdict(
            "MESH-SHARE-005",
            Invalid,
            allow_all(),
            allow_all(),
            &[],
            &format!("{config_dir}/mesh-shares.yaml"),
            Some(Protected),
        ),
        verdict(
            "MESH-SHARE-005",
            Valid,
            allow_all(),
            none.clone(),
            &["gitlog/a.md"],
            "gitlog/a.md",
            Some(Shared),
        ),
        share_check(
            "MESH-SHARE-005",
            Invalid,
            the_global_config_dir_inside_the_root_is_protected,
        ),
        share_check(
            "MESH-SHARE-005",
            Invalid,
            the_cache_dir_and_a_configured_inbox_are_protected,
        ),
        // MESH-SHARE-006: a user deny from either layer, on the sent name.
        verdict(
            "MESH-SHARE-006",
            Invalid,
            shares_yaml(&[("**", None)], &["docs/**"], &[]),
            none.clone(),
            &["docs/a.md"],
            "docs/a.md",
            Some(Denied),
        ),
        verdict(
            "MESH-SHARE-006",
            Invalid,
            none.clone(),
            shares_yaml(&[("**", None)], &["docs/**"], &[]),
            &["docs/a.md"],
            "docs/a.md",
            Some(Denied),
        ),
        verdict(
            "MESH-SHARE-006",
            Invalid,
            allow_all(),
            shares_yaml(&[], &["**/*.md"], &[]),
            &["deep/er/a.md"],
            "deep/er/a.md",
            Some(Denied),
        ),
        verdict(
            "MESH-SHARE-006",
            Valid,
            shares_yaml(&[("**", None)], &["docs/**"], &[]),
            none.clone(),
            &["src/a.rs"],
            "src/a.rs",
            Some(Shared),
        ),
        // MESH-SHARE-007: the built-in deny.
        verdict(
            "MESH-SHARE-007",
            Invalid,
            allow_all(),
            none.clone(),
            &[".env"],
            ".env",
            Some(BuiltinDenied),
        ),
        verdict(
            "MESH-SHARE-007",
            Invalid,
            allow_all(),
            none.clone(),
            &[".env.local"],
            ".env.local",
            Some(BuiltinDenied),
        ),
        verdict(
            "MESH-SHARE-007",
            Invalid,
            allow_all(),
            none.clone(),
            &["certs/server.pem"],
            "certs/server.pem",
            Some(BuiltinDenied),
        ),
        verdict(
            "MESH-SHARE-007",
            Invalid,
            allow_all(),
            none.clone(),
            &["certs/server.key"],
            "certs/server.key",
            Some(BuiltinDenied),
        ),
        verdict(
            "MESH-SHARE-007",
            Invalid,
            allow_all(),
            none.clone(),
            &["home/.ssh/id_ed25519"],
            "home/.ssh/id_ed25519",
            Some(BuiltinDenied),
        ),
        verdict(
            "MESH-SHARE-007",
            Invalid,
            allow_all(),
            none.clone(),
            &["deep/er/.env"],
            "deep/er/.env",
            Some(BuiltinDenied),
        ),
        verdict(
            "MESH-SHARE-007",
            Invalid,
            allow_all(),
            none.clone(),
            &[".git/config"],
            ".git/config",
            Some(Protected),
        ),
        verdict(
            "MESH-SHARE-007",
            Invalid,
            allow_all(),
            none.clone(),
            &[&in_config_dir],
            &in_config_dir,
            Some(Protected),
        ),
        verdict(
            "MESH-SHARE-007",
            Valid,
            allow_all(),
            none.clone(),
            &["docs/env.md"],
            "docs/env.md",
            Some(Shared),
        ),
        verdict(
            "MESH-SHARE-007",
            Valid,
            allow_all(),
            none.clone(),
            &["docs/keys.md"],
            "docs/keys.md",
            Some(Shared),
        ),
        verdict(
            "MESH-SHARE-007",
            Boundary,
            allow_all(),
            none.clone(),
            &["environment"],
            "environment",
            Some(Shared),
        ),
        // MESH-SHARE-008: the override.
        verdict(
            "MESH-SHARE-008",
            Valid,
            shares_yaml(&[("**", None)], &[], &[".env.example"]),
            none.clone(),
            &[".env.example"],
            ".env.example",
            Some(Shared),
        ),
        verdict(
            "MESH-SHARE-008",
            Invalid,
            shares_yaml(&[("**", None)], &[], &[".env.example"]),
            none.clone(),
            &[".env"],
            ".env",
            Some(BuiltinDenied),
        ),
        verdict(
            "MESH-SHARE-008",
            Invalid,
            shares_yaml(&[], &[], &[".env.example"]),
            none.clone(),
            &[".env.example"],
            ".env.example",
            Some(NotAllowed),
        ),
        verdict(
            "MESH-SHARE-008",
            Invalid,
            shares_yaml(&[("**", None)], &[".env.*"], &[".env.example"]),
            none.clone(),
            &[".env.example"],
            ".env.example",
            Some(Denied),
        ),
        verdict(
            "MESH-SHARE-008",
            Invalid,
            allow_all(),
            shares_yaml(&[], &[], &[".env.example"]),
            &[".env.example"],
            ".env.example",
            Some(BuiltinDenied),
        ),
        verdict(
            "MESH-SHARE-008",
            Invalid,
            shares_yaml(&[("**", None)], &[], &[".git/config"]),
            none.clone(),
            &[".git/config"],
            ".git/config",
            Some(Protected),
        ),
        share_check(
            "MESH-SHARE-008",
            Invalid,
            a_workspace_override_is_read_and_shown_but_never_applied,
        ),
        // MESH-SHARE-009: an allow scoped to its peer.
        verdict_as(
            "MESH-SHARE-009",
            Valid,
            VerdictRow {
                global: shares_yaml(&[("docs/**", Some(&identity))], &[], &[]),
                workspace: none.clone(),
                files: &["docs/a.md"],
                who: Who::Named,
                path: "docs/a.md",
                folded: false,
                expect: Some(Shared),
            },
        ),
        verdict_as(
            "MESH-SHARE-009",
            Valid,
            VerdictRow {
                global: shares_yaml(&[("docs/**", Some(&destination))], &[], &[]),
                workspace: none.clone(),
                files: &["docs/a.md"],
                who: Who::Named,
                path: "docs/a.md",
                folded: false,
                expect: Some(Shared),
            },
        ),
        verdict_as(
            "MESH-SHARE-009",
            Invalid,
            VerdictRow {
                global: shares_yaml(&[("docs/**", Some(&identity))], &[], &[]),
                workspace: none.clone(),
                files: &["docs/a.md"],
                who: Who::Other,
                path: "docs/a.md",
                folded: false,
                expect: Some(NotAllowed),
            },
        ),
        verdict_as(
            "MESH-SHARE-009",
            Valid,
            VerdictRow {
                global: allow_docs.clone(),
                workspace: none.clone(),
                files: &["docs/a.md"],
                who: Who::Other,
                path: "docs/a.md",
                folded: false,
                expect: Some(Shared),
            },
        ),
        // MESH-SHARE-010: the case fold.
        verdict_as(
            "MESH-SHARE-010",
            Invalid,
            VerdictRow {
                global: shares_yaml(&[("**", None)], &["DOCS/**"], &[]),
                workspace: none.clone(),
                files: &["docs/a.md"],
                who: Who::Named,
                path: "docs/a.md",
                folded: true,
                expect: Some(Denied),
            },
        ),
        verdict_as(
            "MESH-SHARE-010",
            Valid,
            VerdictRow {
                global: shares_yaml(&[("**", None)], &["DOCS/**"], &[]),
                workspace: none.clone(),
                files: &["docs/a.md"],
                who: Who::Named,
                path: "docs/a.md",
                folded: false,
                expect: Some(Shared),
            },
        ),
        verdict_as(
            "MESH-SHARE-010",
            Valid,
            VerdictRow {
                global: shares_yaml(&[("DOCS/**", None)], &[], &[]),
                workspace: none.clone(),
                files: &["docs/a.md"],
                who: Who::Named,
                path: "docs/a.md",
                folded: true,
                expect: Some(Shared),
            },
        ),
        verdict_as(
            "MESH-SHARE-010",
            Boundary,
            VerdictRow {
                global: shares_yaml(&[("DOCS/**", None)], &[], &[]),
                workspace: none.clone(),
                files: &["docs/a.md"],
                who: Who::Named,
                path: "docs/a.md",
                folded: false,
                expect: Some(NotAllowed),
            },
        ),
        verdict_as(
            "MESH-SHARE-010",
            Invalid,
            VerdictRow {
                global: allow_all(),
                workspace: none.clone(),
                files: &["docs/.ENV"],
                who: Who::Named,
                path: "docs/.ENV",
                folded: true,
                expect: Some(BuiltinDenied),
            },
        ),
        verdict_as(
            "MESH-SHARE-010",
            Valid,
            VerdictRow {
                global: shares_yaml(&[("**", None)], &[], &["docs/.Env.Example"]),
                workspace: none.clone(),
                files: &["docs/.env.example"],
                who: Who::Named,
                path: "docs/.env.example",
                folded: true,
                expect: Some(Shared),
            },
        ),
        verdict_as(
            "MESH-SHARE-010",
            Invalid,
            VerdictRow {
                global: shares_yaml(&[("**", None)], &[], &["docs/.Env.Example"]),
                workspace: none.clone(),
                files: &["docs/.env.example"],
                who: Who::Named,
                path: "docs/.env.example",
                folded: false,
                expect: Some(BuiltinDenied),
            },
        ),
        share_check(
            "MESH-SHARE-010",
            Valid,
            the_case_probe_answers_and_leaves_no_file_behind,
        ),
        // MESH-SHARE-011
        share_check(
            "MESH-SHARE-011",
            Valid,
            the_module_never_reads_the_current_directory,
        ),
        // MESH-SHARE-012: where a mutation lands and how it is written.
        row(
            "MESH-SHARE-012",
            Valid,
            Case::ShareSet(ShareProbe::WriteTarget {
                workspace_exists: false,
                scope: WriteScope::Auto,
                layer: Layer::Global,
            }),
        ),
        row(
            "MESH-SHARE-012",
            Valid,
            Case::ShareSet(ShareProbe::WriteTarget {
                workspace_exists: true,
                scope: WriteScope::Auto,
                layer: Layer::Workspace,
            }),
        ),
        row(
            "MESH-SHARE-012",
            Valid,
            Case::ShareSet(ShareProbe::WriteTarget {
                workspace_exists: true,
                scope: WriteScope::Global,
                layer: Layer::Global,
            }),
        ),
        row(
            "MESH-SHARE-012",
            Valid,
            Case::ShareSet(ShareProbe::WriteTarget {
                workspace_exists: false,
                scope: WriteScope::Workspace,
                layer: Layer::Workspace,
            }),
        ),
        share_check(
            "MESH-SHARE-012",
            Valid,
            apply_validates_first_and_writes_the_target_atomically,
        ),
        // MESH-SHARE-013
        share_check(
            "MESH-SHARE-013",
            Valid,
            mutation_logs_name_the_file_and_never_a_pattern,
        ),
        // MESH-SHARE-014: grants after the share set.
        share_check(
            "MESH-SHARE-014",
            Valid,
            a_grant_serves_a_not_allowed_file_once_and_never_a_denied_one,
        ),
        share_check(
            "MESH-SHARE-014",
            Invalid,
            a_grant_matches_byte_for_byte_without_glob_or_case_fold,
        ),
    ];
    rows.push(verdict(
        "MESH-SHARE-002",
        Invalid,
        shares_yaml(&[("docs/*/a.md", None)], &[], &[]),
        none,
        &["docs/x/y/a.md"],
        "docs/x/y/a.md",
        Some(NotAllowed),
    ));
    rows
}

fn grant_store_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    vec![
        grant_check(
            "MESH-SHARE-015",
            Invalid,
            a_grant_with_an_id_that_is_not_a_wire_id_is_refused,
        ),
        grant_check(
            "MESH-SHARE-015",
            Invalid,
            a_grant_for_a_peer_that_is_not_a_canonical_hash_is_refused,
        ),
        grant_check("MESH-SHARE-015", Invalid, a_grant_with_no_paths_is_refused),
        grant_check(
            "MESH-SHARE-015",
            Invalid,
            a_grant_with_a_path_that_is_not_a_wire_path_is_refused,
        ),
        grant_check(
            "MESH-SHARE-015",
            Boundary,
            a_grant_over_the_path_cap_is_refused_and_one_at_it_is_written,
        ),
        grant_check(
            "MESH-SHARE-015",
            Valid,
            repeats_are_dropped_and_each_path_lends_one_use_until_the_ttl,
        ),
        grant_check(
            "MESH-SHARE-015",
            Valid,
            a_grant_under_the_same_id_replaces_the_earlier_one_for_that_peer_alone,
        ),
        grant_check(
            "MESH-SHARE-016",
            Valid,
            a_refund_restores_the_use_and_never_above_what_was_lent,
        ),
        grant_check(
            "MESH-SHARE-016",
            Boundary,
            an_exhausted_grant_stays_on_file_until_it_expires,
        ),
        grant_check(
            "MESH-SHARE-017",
            Valid,
            revoke_takes_one_peers_grant_under_the_id_and_leaves_the_rest,
        ),
        grant_check(
            "MESH-SHARE-018",
            Boundary,
            expired_grants_are_swept_on_open_and_on_every_check,
        ),
        grant_check(
            "MESH-SHARE-018",
            Invalid,
            a_line_whose_expires_does_not_parse_refuses_the_store,
        ),
        grant_check(
            "MESH-SHARE-018",
            Invalid,
            a_line_with_more_uses_left_than_lent_refuses_the_store,
        ),
    ]
}

fn store_schema_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    let newer = SHARES_FILE_VERSION + 1;
    vec![
        // MESH-SCHEMA-001: the share files.
        row(
            "MESH-SCHEMA-001",
            Valid,
            Case::StoreSchema(SchemaProbe::SharesFile {
                text: allow_all(),
                phrases: &[],
            }),
        ),
        row(
            "MESH-SCHEMA-001",
            Invalid,
            Case::StoreSchema(SchemaProbe::SharesFile {
                text: format!("version: {newer}\nallow: []\nfuture_section: {{}}\n"),
                phrases: &[
                    "version 2",
                    "upgrade Coyote",
                    "move the file aside",
                    "shares nothing",
                ],
            }),
        ),
        row(
            "MESH-SCHEMA-001",
            Invalid,
            Case::StoreSchema(SchemaProbe::SharesFile {
                text: "version: 0\nallow: []\n".to_string(),
                phrases: &["version 0", "no migration", "shares nothing"],
            }),
        ),
        row(
            "MESH-SCHEMA-001",
            Invalid,
            Case::StoreSchema(SchemaProbe::SharesFile {
                text: "allow:\n- pattern: '**'\n".to_string(),
                phrases: &["no readable `version` field", "shares nothing"],
            }),
        ),
        schema_check(
            "MESH-SCHEMA-001",
            Invalid,
            a_refused_share_file_keeps_the_other_layer_from_loading_alone,
        ),
        // MESH-SCHEMA-002: the grant store.
        row(
            "MESH-SCHEMA-002",
            Valid,
            Case::StoreSchema(SchemaProbe::GrantLine {
                edit: |_| {},
                phrases: &[],
            }),
        ),
        row(
            "MESH-SCHEMA-002",
            Invalid,
            Case::StoreSchema(SchemaProbe::GrantLine {
                edit: |line| {
                    line["version"] = serde_json::json!(GRANT_RECORD_VERSION + 1);
                    line["future_field"] = serde_json::json!(1);
                },
                phrases: &[
                    "line 1",
                    "version 2",
                    "upgrade Coyote",
                    "move the file aside",
                ],
            }),
        ),
        row(
            "MESH-SCHEMA-002",
            Invalid,
            Case::StoreSchema(SchemaProbe::GrantLine {
                edit: |line| {
                    line["version"] = serde_json::json!(0);
                },
                phrases: &["line 1", "version 0", "no migration"],
            }),
        ),
        row(
            "MESH-SCHEMA-002",
            Invalid,
            Case::StoreSchema(SchemaProbe::GrantLine {
                edit: |line| {
                    line.as_object_mut().unwrap().remove("version");
                },
                phrases: &[
                    "line 1",
                    "no readable `version` field",
                    "move the file aside",
                ],
            }),
        ),
        row(
            "MESH-SCHEMA-002",
            Invalid,
            Case::StoreSchema(SchemaProbe::GrantLine {
                edit: |line| {
                    line["note"] = serde_json::json!("added by hand");
                },
                phrases: &["line 1"],
            }),
        ),
        // MESH-SCHEMA-003: the inbound record.
        row(
            "MESH-SCHEMA-003",
            Valid,
            Case::StoreSchema(SchemaProbe::InboundLine {
                json: inbound_json(r#","kind":"access","paths":["src/x.rs"],"reason":"need it""#),
                expect: Some((InboundKind::Access, &["src/x.rs"], "need it")),
            }),
        ),
        row(
            "MESH-SCHEMA-003",
            Valid,
            Case::StoreSchema(SchemaProbe::InboundLine {
                json: inbound_json(r#","kind":"question","paths":[],"reason":"""#),
                expect: Some((InboundKind::Question, &[], "")),
            }),
        ),
        row(
            "MESH-SCHEMA-003",
            Boundary,
            Case::StoreSchema(SchemaProbe::InboundLine {
                json: inbound_json(""),
                expect: Some((InboundKind::Question, &[], "")),
            }),
        ),
        row(
            "MESH-SCHEMA-003",
            Invalid,
            Case::StoreSchema(SchemaProbe::InboundLine {
                json: inbound_json(r#","kind":"grant""#),
                expect: None,
            }),
        ),
        row(
            "MESH-SCHEMA-003",
            Invalid,
            Case::StoreSchema(SchemaProbe::InboundLine {
                json: inbound_json(r#","decided":true"#),
                expect: None,
            }),
        ),
        // MESH-SCHEMA-004: the common wording and the pin.
        schema_check(
            "MESH-SCHEMA-004",
            Valid,
            the_share_list_and_grant_store_refuse_in_the_common_wording,
        ),
        schema_check(
            "MESH-SCHEMA-004",
            Boundary,
            the_share_and_grant_versions_are_the_baseline,
        ),
        schema_check(
            "MESH-SCHEMA-004",
            Valid,
            the_version_pin_names_the_share_and_grant_constants,
        ),
    ]
}

fn list_serve_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    const TWO: &[&str] = &["docs/a.md", "src/b.rs"];
    let sixty_four = "a".repeat(CURSOR_MAX_BYTES);
    let sixty_five = "a".repeat(CURSOR_MAX_BYTES + 1);
    vec![
        list_request(
            "MESH-LIST-001",
            Invalid,
            TWO,
            without(list_body(None, None), "v"),
            ListAnswer::Refused,
        ),
        list_request(
            "MESH-LIST-001",
            Invalid,
            TWO,
            with(list_body(None, None), "v", Value::from(2u64)),
            ListAnswer::Refused,
        ),
        list_request(
            "MESH-LIST-001",
            Invalid,
            TWO,
            with(list_body(None, None), "v", Value::from("1")),
            ListAnswer::Refused,
        ),
        list_request(
            "MESH-LIST-001",
            Invalid,
            TWO,
            Value::from("list"),
            ListAnswer::Refused,
        ),
        list_request(
            "MESH-LIST-001",
            Valid,
            TWO,
            list_body(None, None),
            page_of(TWO, None),
        ),
        list_request(
            "MESH-LIST-002",
            Invalid,
            TWO,
            with(list_body(None, None), "prefix", Value::from(7)),
            ListAnswer::Refused,
        ),
        list_request(
            "MESH-LIST-002",
            Invalid,
            TWO,
            with(list_body(None, None), "prefix", bin(b"docs")),
            ListAnswer::Refused,
        ),
        list_request(
            "MESH-LIST-002",
            Valid,
            TWO,
            list_body(Some("docs/"), None),
            page_of(&["docs/a.md"], None),
        ),
        list_request(
            "MESH-LIST-002",
            Valid,
            TWO,
            without(list_body(None, None), "prefix"),
            page_of(TWO, None),
        ),
        list_request(
            "MESH-LIST-003",
            Invalid,
            TWO,
            with(list_body(None, None), "cursor", Value::from(7)),
            ListAnswer::Refused,
        ),
        list_request(
            "MESH-LIST-003",
            Invalid,
            TWO,
            list_body(None, Some(&sixty_five)),
            ListAnswer::Refused,
        ),
        list_request(
            "MESH-LIST-003",
            Boundary,
            TWO,
            list_body(None, Some(&sixty_four)),
            page_of(TWO, None),
        ),
        list_request(
            "MESH-LIST-004",
            Valid,
            TWO,
            list_body(None, Some("00000000000000000000000000000000")),
            page_of(TWO, None),
        ),
        list_request(
            "MESH-LIST-004",
            Valid,
            TWO,
            list_body(None, Some(&list_cursor("docs/a.md"))),
            page_of(&["src/b.rs"], None),
        ),
        list_request(
            "MESH-LIST-005",
            Valid,
            TWO,
            with(list_body(None, None), "extra", Value::from(7)),
            page_of(TWO, None),
        ),
        list_request(
            "MESH-LIST-005",
            Valid,
            TWO,
            with(
                list_body(None, None),
                "paths",
                Value::Array(vec![Value::from("x")]),
            ),
            page_of(TWO, None),
        ),
        list_check(
            "MESH-LIST-016",
            Valid,
            a_listing_is_the_share_set_for_the_requester_and_never_the_tree,
        ),
        list_check(
            "MESH-LIST-016",
            Valid,
            a_protected_inbox_inside_the_root_is_left_off_the_listing,
        ),
        list_request(
            "MESH-LIST-017",
            Valid,
            &["a.md", "Z.md", "B.md", "docs/x.md", "docs-x.md"],
            list_body(None, None),
            page_of(&["B.md", "Z.md", "a.md", "docs-x.md", "docs/x.md"], None),
        ),
        list_request(
            "MESH-LIST-017",
            Valid,
            &["a.md", "Z.md", "B.md", "docs/x.md", "docs-x.md"],
            list_body(Some("docs"), None),
            page_of(&["docs-x.md", "docs/x.md"], None),
        ),
        list_check(
            "MESH-LIST-018",
            Boundary,
            a_page_holds_a_thousand_entries_and_the_cursor_resumes_after_the_last,
        ),
        list_check(
            "MESH-LIST-018",
            Boundary,
            a_page_is_cut_by_encoded_bytes_before_the_entry_count,
        ),
        list_check(
            "MESH-LIST-019",
            Valid,
            the_cursor_is_the_first_half_of_the_sha256_of_the_path,
        ),
        list_check(
            "MESH-LIST-019",
            Valid,
            a_page_holds_a_thousand_entries_and_the_cursor_resumes_after_the_last,
        ),
        list_check(
            "MESH-LIST-021",
            Valid,
            a_bounded_walk_truncates_the_listing_and_the_wire_page_carries_no_flag,
        ),
        list_check(
            "MESH-LIST-022",
            Valid,
            a_listing_hashes_only_the_page_it_returns,
        ),
        list_check(
            "MESH-LIST-023",
            Valid,
            no_root_no_serving_or_a_broken_share_list_answers_the_empty_page,
        ),
        list_check(
            "MESH-LIST-024",
            Invalid,
            an_unknown_or_blocked_identity_hears_silence_on_list_and_fetch,
        ),
        list_check("MESH-LIST-025", Valid, a_listing_waits_a_round_trip),
    ]
}

fn fetch_serve_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    const ONE: &[&str] = &["docs/a.md"];
    let body = || fetch_body("docs/a.md", None);
    vec![
        fetch_request(
            "MESH-FETCH-004",
            Invalid,
            ONE,
            fetch_body("docs/../a.md", None),
            invalid_path("segment"),
        ),
        fetch_check(
            "MESH-FETCH-004",
            Invalid,
            an_invalid_path_is_refused_before_the_root_is_probed,
        ),
        fetch_request(
            "MESH-FETCH-006",
            Invalid,
            ONE,
            fetch_body("docs", None),
            status("not_shared"),
        ),
        fetch_request("MESH-FETCH-006", Valid, ONE, body(), status("ok")),
        fetch_request(
            "MESH-FETCH-008",
            Invalid,
            ONE,
            without(body(), "v"),
            FetchAnswer::Refused,
        ),
        fetch_request(
            "MESH-FETCH-008",
            Invalid,
            ONE,
            with(body(), "v", Value::from(2u64)),
            FetchAnswer::Refused,
        ),
        fetch_request(
            "MESH-FETCH-008",
            Invalid,
            ONE,
            Value::from("fetch"),
            FetchAnswer::Refused,
        ),
        fetch_request("MESH-FETCH-008", Valid, ONE, body(), status("ok")),
        fetch_request(
            "MESH-FETCH-009",
            Invalid,
            ONE,
            without(body(), "path"),
            FetchAnswer::Refused,
        ),
        fetch_request(
            "MESH-FETCH-009",
            Invalid,
            ONE,
            with(body(), "path", Value::Nil),
            FetchAnswer::Refused,
        ),
        fetch_request(
            "MESH-FETCH-009",
            Invalid,
            ONE,
            with(body(), "path", Value::from(7)),
            FetchAnswer::Refused,
        ),
        fetch_request(
            "MESH-FETCH-009",
            Invalid,
            ONE,
            with(body(), "path", bin(b"docs/a.md")),
            FetchAnswer::Refused,
        ),
        fetch_request(
            "MESH-FETCH-010",
            Invalid,
            ONE,
            fetch_body("", None),
            invalid_path("empty"),
        ),
        fetch_request(
            "MESH-FETCH-010",
            Invalid,
            ONE,
            fetch_body("docs//a.md", None),
            invalid_path("segment"),
        ),
        fetch_request(
            "MESH-FETCH-010",
            Invalid,
            ONE,
            fetch_body("docs\\a.md", None),
            invalid_path("backslash"),
        ),
        fetch_request(
            "MESH-FETCH-010",
            Invalid,
            ONE,
            fetch_body("/docs/a.md", None),
            invalid_path("leading_slash"),
        ),
        fetch_request(
            "MESH-FETCH-010",
            Invalid,
            ONE,
            fetch_body("c:/a.md", None),
            invalid_path("drive_letter"),
        ),
        fetch_request(
            "MESH-FETCH-010",
            Invalid,
            ONE,
            fetch_body("cafe\u{301}.md", None),
            invalid_path("nfc"),
        ),
        fetch_request(
            "MESH-FETCH-010",
            Invalid,
            ONE,
            fetch_body("docs/a\tb.md", None),
            invalid_path("control"),
        ),
        fetch_request(
            "MESH-FETCH-010",
            Invalid,
            ONE,
            fetch_body("docs./a.md", None),
            invalid_path("trailing_dot"),
        ),
        fetch_request(
            "MESH-FETCH-010",
            Invalid,
            ONE,
            fetch_body("con.md", None),
            invalid_path("reserved_name"),
        ),
        fetch_request(
            "MESH-FETCH-011",
            Invalid,
            ONE,
            with(body(), "if_sha256", Value::from("abc")),
            FetchAnswer::Refused,
        ),
        fetch_request(
            "MESH-FETCH-011",
            Invalid,
            ONE,
            with(body(), "if_sha256", bin(&[0u8; 31])),
            FetchAnswer::Refused,
        ),
        fetch_request(
            "MESH-FETCH-011",
            Invalid,
            ONE,
            with(body(), "if_sha256", bin(&[0u8; 33])),
            FetchAnswer::Refused,
        ),
        fetch_request(
            "MESH-FETCH-011",
            Boundary,
            ONE,
            fetch_body("docs/a.md", Some([0u8; 32])),
            status("ok"),
        ),
        fetch_request(
            "MESH-FETCH-011",
            Valid,
            ONE,
            without(body(), "if_sha256"),
            status("ok"),
        ),
        fetch_request(
            "MESH-FETCH-012",
            Valid,
            ONE,
            with(body(), "extra", Value::from(7)),
            status("ok"),
        ),
        fetch_request(
            "MESH-FETCH-012",
            Valid,
            ONE,
            with(body(), "prefix", Value::from("docs/")),
            status("ok"),
        ),
        fetch_request(
            "MESH-FETCH-024",
            Invalid,
            &[],
            fetch_body("../a.md", None),
            invalid_path("segment"),
        ),
        fetch_request(
            "MESH-FETCH-024",
            Invalid,
            ONE,
            fetch_body("docs/missing.md", None),
            status("not_shared"),
        ),
        fetch_request(
            "MESH-FETCH-024",
            Invalid,
            ONE,
            fetch_body("docs", None),
            status("not_shared"),
        ),
        fetch_check(
            "MESH-FETCH-024",
            Invalid,
            too_large_is_decided_at_the_stat_before_a_grant_use_is_spent,
        ),
        fetch_check(
            "MESH-FETCH-024",
            Invalid,
            a_failed_read_is_not_shared_and_a_file_that_grew_is_too_large,
        ),
        fetch_check(
            "MESH-FETCH-024",
            Valid,
            a_matching_if_sha256_is_not_modified_without_a_body_and_spends_the_use,
        ),
        fetch_check(
            "MESH-FETCH-024",
            Valid,
            an_ok_reply_carries_the_size_the_digest_and_the_bytes,
        ),
        fetch_check(
            "MESH-FETCH-025",
            Invalid,
            not_shared_is_byte_identical_for_a_missing_an_unshared_and_a_denied_file,
        ),
        fetch_check(
            "MESH-FETCH-026",
            Valid,
            the_serving_limit_is_the_configured_bytes_under_the_ceiling,
        ),
        fetch_check(
            "MESH-FETCH-027",
            Boundary,
            a_file_one_past_the_ceiling_is_too_large_and_one_at_it_is_ok,
        ),
        fetch_check(
            "MESH-FETCH-033",
            Valid,
            a_served_fetch_fires_once_sent_with_peer_size_and_hash_prefix_and_no_path,
        ),
        fetch_check(
            "MESH-FETCH-034",
            Valid,
            an_ok_dropped_unsent_refunds_the_grant_use_and_one_sent_keeps_it_spent,
        ),
    ]
}

fn fetch_client_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    let good = || entry_of("docs/a.md");
    let one = || page_value(vec![good()], Value::Nil);
    let hello = b"hello";
    let hash = [0x11; 32];
    let at_bound = vec![0x5a; MAX_FETCH_FILE_BYTES as usize];
    let over_bound = vec![0x5a; MAX_FETCH_FILE_BYTES as usize + 1];
    let sixty_four = "a".repeat(CURSOR_MAX_BYTES);
    let sixty_five = "a".repeat(CURSOR_MAX_BYTES + 1);
    let thousand_and_one: Vec<Value> = (0..=LIST_PAGE_SIZE)
        .map(|n| entry_of(&format!("f{n:04}.md")))
        .collect();
    vec![
        client_check(
            "MESH-FETCH-001",
            Invalid,
            a_card_with_a_malformed_about_or_caps_keeps_the_rest,
        ),
        client_check(
            "MESH-FETCH-002",
            Invalid,
            a_card_with_a_malformed_about_or_caps_keeps_the_rest,
        ),
        page_row("MESH-LIST-006", Invalid, without(one(), "v"), Err("v")),
        page_row(
            "MESH-LIST-006",
            Invalid,
            with(one(), "v", Value::from(2u64)),
            Err("v"),
        ),
        page_row("MESH-LIST-006", Invalid, Value::from("page"), Err("map")),
        page_row("MESH-LIST-006", Valid, one(), Ok((1, None))),
        page_row(
            "MESH-LIST-007",
            Invalid,
            without(one(), "entries"),
            Err("entries"),
        ),
        page_row(
            "MESH-LIST-007",
            Invalid,
            with(one(), "entries", Value::from(7)),
            Err("entries"),
        ),
        page_row(
            "MESH-LIST-007",
            Invalid,
            with(one(), "entries", Value::Nil),
            Err("entries"),
        ),
        page_row(
            "MESH-LIST-008",
            Boundary,
            page_value(thousand_and_one, Value::Nil),
            Ok((LIST_PAGE_SIZE, None)),
        ),
        page_row(
            "MESH-LIST-008",
            Invalid,
            page_value(vec![Value::from(7), good()], Value::Nil),
            Ok((1, None)),
        ),
        page_row(
            "MESH-LIST-008",
            Invalid,
            page_value(vec![entry_of("docs//a.md"), good()], Value::Nil),
            Ok((1, None)),
        ),
        page_row(
            "MESH-LIST-009",
            Invalid,
            with(one(), "next", Value::from(7)),
            Err("next"),
        ),
        page_row(
            "MESH-LIST-009",
            Invalid,
            with(one(), "next", Value::from(sixty_five.as_str())),
            Err("next"),
        ),
        page_row(
            "MESH-LIST-009",
            Boundary,
            with(one(), "next", Value::from(sixty_four.as_str())),
            Ok((1, Some(sixty_four.clone()))),
        ),
        page_row(
            "MESH-LIST-010",
            Valid,
            with(one(), "extra", Value::from(7)),
            Ok((1, None)),
        ),
        page_row(
            "MESH-LIST-010",
            Valid,
            with(one(), "truncated", Value::Boolean(true)),
            Ok((1, None)),
        ),
        // The requester keeps the cursor verbatim; `list_shares` is the only builder of
        // the request that hands it back and needs a runtime, so the pin stops here.
        page_row(
            "MESH-LIST-020",
            Valid,
            with(one(), "next", Value::from("AbC-opaque!?")),
            Ok((1, Some("AbC-opaque!?".to_string()))),
        ),
        entry_row("MESH-LIST-011", Invalid, without(good(), "path"), false),
        entry_row(
            "MESH-LIST-011",
            Invalid,
            with(good(), "path", Value::from(7)),
            false,
        ),
        entry_row("MESH-LIST-011", Invalid, entry_of("docs//a.md"), false),
        entry_row("MESH-LIST-011", Invalid, entry_of("../a.md"), false),
        entry_row("MESH-LIST-011", Invalid, entry_of(""), false),
        entry_row("MESH-LIST-011", Valid, good(), true),
        entry_row("MESH-LIST-012", Invalid, without(good(), "size"), false),
        entry_row(
            "MESH-LIST-012",
            Invalid,
            with(good(), "size", Value::from(-1i64)),
            false,
        ),
        entry_row(
            "MESH-LIST-012",
            Invalid,
            with(good(), "size", Value::from("1")),
            false,
        ),
        entry_row(
            "MESH-LIST-012",
            Invalid,
            with(good(), "size", Value::F64(1.0)),
            false,
        ),
        entry_row(
            "MESH-LIST-012",
            Boundary,
            with(good(), "size", Value::from(0u64)),
            true,
        ),
        entry_row(
            "MESH-LIST-012",
            Boundary,
            with(good(), "size", Value::from(u64::MAX)),
            true,
        ),
        entry_row("MESH-LIST-013", Invalid, without(good(), "sha256"), false),
        entry_row(
            "MESH-LIST-013",
            Invalid,
            with(good(), "sha256", Value::from("a".repeat(32).as_str())),
            false,
        ),
        entry_row(
            "MESH-LIST-013",
            Invalid,
            with(good(), "sha256", bin(&[0u8; 31])),
            false,
        ),
        entry_row(
            "MESH-LIST-013",
            Invalid,
            with(good(), "sha256", bin(&[0u8; 33])),
            false,
        ),
        entry_row(
            "MESH-LIST-013",
            Boundary,
            with(good(), "sha256", bin(&[0u8; 32])),
            true,
        ),
        entry_row("MESH-LIST-014", Invalid, without(good(), "mtime"), false),
        entry_row(
            "MESH-LIST-014",
            Invalid,
            with(good(), "mtime", Value::from("1")),
            false,
        ),
        entry_row(
            "MESH-LIST-014",
            Invalid,
            with(good(), "mtime", Value::F64(f64::NAN)),
            false,
        ),
        entry_row(
            "MESH-LIST-014",
            Invalid,
            with(good(), "mtime", Value::F64(f64::INFINITY)),
            false,
        ),
        entry_row(
            "MESH-LIST-014",
            Invalid,
            with(good(), "mtime", Value::F32(f32::NAN)),
            false,
        ),
        entry_row(
            "MESH-LIST-014",
            Valid,
            with(good(), "mtime", Value::from(1u64)),
            true,
        ),
        entry_row(
            "MESH-LIST-014",
            Valid,
            with(good(), "mtime", Value::from(-1i64)),
            true,
        ),
        entry_row(
            "MESH-LIST-014",
            Valid,
            with(good(), "mtime", Value::F32(1.5)),
            true,
        ),
        entry_row(
            "MESH-LIST-014",
            Boundary,
            with(good(), "mtime", Value::F64(0.0)),
            true,
        ),
        entry_row(
            "MESH-LIST-015",
            Valid,
            with(good(), "extra", Value::from(7)),
            true,
        ),
        entry_row(
            "MESH-LIST-015",
            Valid,
            with(good(), "mode", Value::from("0644")),
            true,
        ),
        client_check(
            "MESH-FETCH-015",
            Invalid,
            an_unknown_status_is_a_client_error_with_the_pinned_wording,
        ),
        client_check(
            "MESH-FETCH-021",
            Invalid,
            a_rule_is_kept_when_known_read_as_unknown_otherwise_and_malformed_when_not_text,
        ),
        reply_row("MESH-FETCH-013", Invalid, Value::from(7), malformed("map")),
        reply_row(
            "MESH-FETCH-013",
            Invalid,
            without(fetch_reply("not_shared"), "v"),
            malformed("v"),
        ),
        reply_row(
            "MESH-FETCH-013",
            Invalid,
            with(fetch_reply("not_shared"), "v", Value::from(2)),
            malformed("v"),
        ),
        reply_row(
            "MESH-FETCH-013",
            Invalid,
            DispatchError::NoProvider {
                path: FETCH_PATH.to_string(),
            }
            .to_value(),
            Err(ReplyError::NotServed),
        ),
        reply_row(
            "MESH-FETCH-014",
            Invalid,
            without(ok_reply(hello), "status"),
            malformed("status"),
        ),
        reply_row(
            "MESH-FETCH-014",
            Invalid,
            with(ok_reply(hello), "status", Value::from(1)),
            malformed("status"),
        ),
        reply_row(
            "MESH-FETCH-015",
            Invalid,
            fetch_reply("maybe"),
            Err(ReplyError::UnknownStatus),
        ),
        reply_row(
            "MESH-FETCH-015",
            Invalid,
            with(ok_reply(hello), "status", Value::from("OK")),
            Err(ReplyError::UnknownStatus),
        ),
        reply_row(
            "MESH-FETCH-016",
            Invalid,
            without(ok_reply(hello), "size"),
            malformed("size"),
        ),
        reply_row(
            "MESH-FETCH-016",
            Invalid,
            with(ok_reply(hello), "size", Value::from(-5)),
            malformed("size"),
        ),
        reply_row(
            "MESH-FETCH-016",
            Invalid,
            with(ok_reply(hello), "size", Value::from(hello.len() - 1)),
            malformed("size"),
        ),
        reply_row("MESH-FETCH-016", Valid, ok_reply(hello), ok_read(hello)),
        reply_row(
            "MESH-FETCH-017",
            Invalid,
            without(ok_reply(hello), "sha256"),
            malformed("sha256"),
        ),
        reply_row(
            "MESH-FETCH-017",
            Invalid,
            with(
                ok_reply(hello),
                "sha256",
                Value::from(hex_lower(&Sha256::digest(hello))),
            ),
            malformed("sha256"),
        ),
        reply_row(
            "MESH-FETCH-017",
            Invalid,
            with(ok_reply(hello), "sha256", bin(&hash[..31])),
            malformed("sha256"),
        ),
        reply_row(
            "MESH-FETCH-017",
            Invalid,
            fetch_reply("not_modified"),
            malformed("sha256"),
        ),
        reply_row(
            "MESH-FETCH-017",
            Invalid,
            with(fetch_reply("not_modified"), "sha256", Value::from("abc")),
            malformed("sha256"),
        ),
        reply_row(
            "MESH-FETCH-017",
            Invalid,
            not_modified_reply(&hash[..31]),
            malformed("sha256"),
        ),
        reply_row(
            "MESH-FETCH-017",
            Valid,
            not_modified_reply(&hash),
            Ok(FetchReply::NotModified { sha256: hash }),
        ),
        reply_row(
            "MESH-FETCH-018",
            Invalid,
            with(ok_reply(hello), "sha256", bin(&hash)),
            Err(ReplyError::Corrupt),
        ),
        reply_row(
            "MESH-FETCH-019",
            Invalid,
            without(ok_reply(hello), "bytes"),
            malformed("bytes"),
        ),
        reply_row(
            "MESH-FETCH-019",
            Invalid,
            with(ok_reply(hello), "bytes", Value::from("hello")),
            malformed("bytes"),
        ),
        reply_row(
            "MESH-FETCH-020",
            Invalid,
            with(
                with(fetch_reply("ok"), "bytes", bin(&over_bound)),
                "size",
                Value::from(1),
            ),
            Err(ReplyError::Oversize(over_bound.len())),
        ),
        reply_row(
            "MESH-FETCH-020",
            Boundary,
            ok_reply(&at_bound),
            ok_read(&at_bound),
        ),
        reply_row(
            "MESH-FETCH-021",
            Invalid,
            fetch_reply("invalid_path"),
            malformed("rule"),
        ),
        reply_row(
            "MESH-FETCH-021",
            Invalid,
            with(fetch_reply("invalid_path"), "rule", Value::from(3)),
            malformed("rule"),
        ),
        reply_row(
            "MESH-FETCH-021",
            Boundary,
            with(
                fetch_reply("invalid_path"),
                "rule",
                Value::from("the path was bad"),
            ),
            Ok(FetchReply::InvalidPath {
                rule: "unknown".to_string(),
            }),
        ),
        reply_row(
            "MESH-FETCH-021",
            Valid,
            with(fetch_reply("invalid_path"), "rule", Value::from("segment")),
            Ok(FetchReply::InvalidPath {
                rule: "segment".to_string(),
            }),
        ),
        reply_row(
            "MESH-FETCH-022",
            Invalid,
            fetch_reply("too_large"),
            malformed("limit"),
        ),
        reply_row(
            "MESH-FETCH-022",
            Invalid,
            with(fetch_reply("too_large"), "limit", Value::from("4096")),
            malformed("limit"),
        ),
        reply_row(
            "MESH-FETCH-022",
            Valid,
            with(fetch_reply("too_large"), "limit", Value::from(4096)),
            Ok(FetchReply::TooLarge { limit: 4096 }),
        ),
        reply_row(
            "MESH-FETCH-023",
            Valid,
            with(
                with(fetch_reply("not_shared"), "bytes", bin(hello)),
                "limit",
                Value::from(1),
            ),
            Ok(FetchReply::NotShared),
        ),
        reply_row(
            "MESH-FETCH-023",
            Valid,
            with(ok_reply(hello), "rule", Value::from("segment")),
            ok_read(hello),
        ),
        reply_row(
            "MESH-FETCH-023",
            Valid,
            with(not_modified_reply(&hash), "extra", Value::from(7)),
            Ok(FetchReply::NotModified { sha256: hash }),
        ),
        reply_row(
            "MESH-FETCH-023",
            Valid,
            with(
                with(fetch_reply("invalid_path"), "rule", Value::from("segment")),
                "size",
                Value::from(9),
            ),
            Ok(FetchReply::InvalidPath {
                rule: "segment".to_string(),
            }),
        ),
        reply_row(
            "MESH-FETCH-023",
            Valid,
            with(
                with(fetch_reply("too_large"), "limit", Value::from(4096)),
                "sha256",
                bin(&hash),
            ),
            Ok(FetchReply::TooLarge { limit: 4096 }),
        ),
        client_check(
            "MESH-FETCH-028",
            Boundary,
            a_fetch_response_at_its_bound_is_delivered_and_one_byte_over_is_dropped,
        ),
        client_check(
            "MESH-FETCH-029",
            Valid,
            the_bound_is_found_from_the_frame_prefix_before_the_frame_is_decoded,
        ),
        client_check(
            "MESH-FETCH-030",
            Valid,
            the_inbox_stages_under_the_instance_and_peer_and_refuses_a_collision,
        ),
        client_check(
            "MESH-FETCH-031",
            Valid,
            a_fetched_file_yields_its_path_size_and_digest_and_never_its_bytes,
        ),
        client_check("MESH-FETCH-032", Valid, a_fetch_waits_two_minutes),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn run_family(family: &str) {
        let mut ran = 0;
        let mut failures = Vec::new();
        for vector in vectors()
            .iter()
            .filter(|vector| vector.case.family() == family)
        {
            ran += 1;
            let tag = format!("{} [{family}, {:?}]", vector.id, vector.kind);
            match (run(&vector.case), vector.known_divergence) {
                (Ok(()), None) => {}
                (Err(detail), None) => failures.push(format!("{tag}: {detail}")),
                (Err(detail), Some(note)) => {
                    println!("{tag}: known divergence, not asserted: {note}; observed: {detail}");
                }
                (Ok(()), Some(note)) => failures.push(format!(
                    "{tag}: flagged as a known divergence but passes; drop the flag ({note})"
                )),
            }
        }
        assert!(ran > 0, "no {family} vectors");
        assert!(
            failures.is_empty(),
            "{} of {ran} {family} vectors failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    const TESTED: [&str; 7] = [
        "WirePath",
        "ShareSet",
        "GrantStore",
        "StoreSchema",
        "ListServe",
        "FetchServe",
        "FetchClient",
    ];

    #[test]
    fn wire_paths_are_held_to_the_fourteen_rules_of_section_10_13() {
        run_family("WirePath");
    }

    #[test]
    fn share_sets_judge_as_section_10_17_mandates() {
        run_family("ShareSet");
    }

    #[test]
    fn grant_stores_lend_spend_and_sweep_as_section_10_17_mandates() {
        run_family("GrantStore");
    }

    #[test]
    fn store_schemas_refuse_and_default_as_section_14_1_mandates() {
        run_family("StoreSchema");
    }

    #[test]
    fn list_handlers_answer_as_section_10_14_mandates() {
        run_family("ListServe");
    }

    #[test]
    fn fetch_handlers_answer_as_section_10_15_mandates() {
        run_family("FetchServe");
    }

    #[test]
    fn requesters_read_pages_and_replies_as_sections_10_14_and_10_15_mandate() {
        run_family("FetchClient");
    }

    #[test]
    fn every_family_has_a_test_and_every_row_a_kind() {
        let rows = vectors();
        let families: BTreeSet<&str> = rows.iter().map(|vector| vector.case.family()).collect();
        let tested: BTreeSet<&str> = TESTED.into_iter().collect();
        assert_eq!(
            families, tested,
            "every family needs a test above and vice versa"
        );
        assert!(rows.iter().any(|vector| vector.kind == Kind::Boundary));
        assert!(rows.iter().any(|vector| vector.kind == Kind::Invalid));
        assert!(rows.iter().any(|vector| vector.kind == Kind::Valid));
    }
}
