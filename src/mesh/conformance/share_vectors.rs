//! Requirement-id keyed vectors for the share side of the protocol: the wire-path grammar
//! of section 10.13 and the three places that reuse it, the share set and the grant store
//! of section 10.17, and the on-disk versioning of section 14.1 for the stores they read.
//! Every row runs in-process against this crate's own `WirePath`, `ShareSet`, `GrantStore`
//! and record types, on a share root the row builds in a temporary directory.
//!
//! Every row names the id it exercises and the receiver action the spec mandates for it. A
//! row written faithfully from the spec that the code does not honour is kept as written
//! and flagged with `known_divergence`; the executor prints such a row instead of asserting
//! it, and fails when the flag goes stale.

use super::{Kind, Listed};
use crate::config::WORKSPACE_COYOTE_DIR_NAME;
use crate::config::mesh_config::MAX_FETCH_FILE_BYTES;
use crate::mesh::access::validate_access;
use crate::mesh::fetch::{SharesPage, rule_of};
use crate::mesh::grants::{
    DEFAULT_GRANT_TTL, DEFAULT_GRANT_USES, GRANT_MAX_PATHS, GRANT_RECORD_VERSION, GrantStore,
};
use crate::mesh::message::{PartLimits, RawPart, admit_parts};
use crate::mesh::pending::{INBOUND_RECORD_VERSION, InboundKind, InboundRecord};
use crate::mesh::schema::{Remedy, version_refusal};
use crate::mesh::shares::{
    Layer, Mutation, PeerRef, SHARES_FILE_VERSION, Served, ServedFile, ShareLocations, ShareSet,
    Verdict, Via, WriteScope, probe_case_insensitive, validate_override, validate_pattern,
    write_target,
};
use crate::mesh::test_support::{TempDir, siblings_of};
use crate::mesh::wire_path::{
    RULES, WIRE_PATH_MAX_BYTES, WIRE_PATH_MAX_SEGMENTS, WirePath, is_rule_id,
};
use crate::mesh::{hex_lower, rfc3339_utc};
use crate::testing::{debug_snapshot, install_log_collector, warn_snapshot};

use rmpv::Value;
use sha2::{Digest, Sha256};
use std::fmt::Debug;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

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

impl Case {
    fn family(&self) -> &'static str {
        match self {
            Self::WirePath(_) => "WirePath",
            Self::ShareSet(_) => "ShareSet",
            Self::GrantStore(_) => "GrantStore",
            Self::StoreSchema(_) => "StoreSchema",
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
        ShareLocations::with_dir_name(
            &self.config_dir,
            &self.root,
            WORKSPACE_COYOTE_DIR_NAME.to_string(),
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
        global,
        workspace,
        files,
        Who::Named,
        path,
        false,
        expect,
    )
}

#[allow(clippy::too_many_arguments)]
fn verdict_as(
    id: &'static str,
    kind: Kind,
    global: String,
    workspace: String,
    files: &[&str],
    who: Who,
    path: &str,
    folded: bool,
    expect: Option<Verdict>,
) -> Vector {
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
    let source = include_str!("../shares.rs");
    let production = &source[..source.find("#[cfg(test)]").unwrap()];
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
// The table
// ---------------------------------------------------------------------------------------

fn vectors() -> Vec<Vector> {
    let mut rows = Vec::new();
    rows.extend(wire_path_rows());
    rows.extend(share_set_rows());
    rows.extend(grant_store_rows());
    rows.extend(store_schema_rows());
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
            shares_yaml(&[("docs/**", Some(&identity))], &[], &[]),
            none.clone(),
            &["docs/a.md"],
            Who::Named,
            "docs/a.md",
            false,
            Some(Shared),
        ),
        verdict_as(
            "MESH-SHARE-009",
            Valid,
            shares_yaml(&[("docs/**", Some(&destination))], &[], &[]),
            none.clone(),
            &["docs/a.md"],
            Who::Named,
            "docs/a.md",
            false,
            Some(Shared),
        ),
        verdict_as(
            "MESH-SHARE-009",
            Invalid,
            shares_yaml(&[("docs/**", Some(&identity))], &[], &[]),
            none.clone(),
            &["docs/a.md"],
            Who::Other,
            "docs/a.md",
            false,
            Some(NotAllowed),
        ),
        verdict_as(
            "MESH-SHARE-009",
            Valid,
            allow_docs.clone(),
            none.clone(),
            &["docs/a.md"],
            Who::Other,
            "docs/a.md",
            false,
            Some(Shared),
        ),
        // MESH-SHARE-010: the case fold.
        verdict_as(
            "MESH-SHARE-010",
            Invalid,
            shares_yaml(&[("**", None)], &["DOCS/**"], &[]),
            none.clone(),
            &["docs/a.md"],
            Who::Named,
            "docs/a.md",
            true,
            Some(Denied),
        ),
        verdict_as(
            "MESH-SHARE-010",
            Valid,
            shares_yaml(&[("**", None)], &["DOCS/**"], &[]),
            none.clone(),
            &["docs/a.md"],
            Who::Named,
            "docs/a.md",
            false,
            Some(Shared),
        ),
        verdict_as(
            "MESH-SHARE-010",
            Valid,
            shares_yaml(&[("DOCS/**", None)], &[], &[]),
            none.clone(),
            &["docs/a.md"],
            Who::Named,
            "docs/a.md",
            true,
            Some(Shared),
        ),
        verdict_as(
            "MESH-SHARE-010",
            Boundary,
            shares_yaml(&[("DOCS/**", None)], &[], &[]),
            none.clone(),
            &["docs/a.md"],
            Who::Named,
            "docs/a.md",
            false,
            Some(NotAllowed),
        ),
        verdict_as(
            "MESH-SHARE-010",
            Invalid,
            allow_all(),
            none.clone(),
            &["docs/.ENV"],
            Who::Named,
            "docs/.ENV",
            true,
            Some(BuiltinDenied),
        ),
        verdict_as(
            "MESH-SHARE-010",
            Valid,
            shares_yaml(&[("**", None)], &[], &["docs/.Env.Example"]),
            none.clone(),
            &["docs/.env.example"],
            Who::Named,
            "docs/.env.example",
            true,
            Some(Shared),
        ),
        verdict_as(
            "MESH-SHARE-010",
            Invalid,
            shares_yaml(&[("**", None)], &[], &["docs/.Env.Example"]),
            none.clone(),
            &["docs/.env.example"],
            Who::Named,
            "docs/.env.example",
            false,
            Some(BuiltinDenied),
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

    const TESTED: [&str; 4] = ["WirePath", "ShareSet", "GrantStore", "StoreSchema"];

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
