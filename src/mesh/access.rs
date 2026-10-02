//! Access requests: a trusted peer asking to read paths this node does not share. The
//! request is answered at once when the share rules already serve every path, refused
//! when the same peer is already waiting on the same set or on too many, and otherwise
//! held in the inbound store until the person at the keyboard grants or refuses it.
//!
//! Only the person at the keyboard sees what was asked for: the paths and the reason go
//! to the inbound record and the human line and nowhere else, never to the envoy, a
//! model note or a log line.

use crate::mesh::events::{AccessDecision, MeshEvent};
use crate::mesh::fetch::{FetchServing, ShareSource, field, versioned_map};
use crate::mesh::grants::DEFAULT_GRANT_TTL;
use crate::mesh::idle::{IdleNotify, Origin};
use crate::mesh::message::{PEER_WIRE_VERSION, PeerVia, is_wire_id};
use crate::mesh::node::MeshSlot;
use crate::mesh::notify::Source;
use crate::mesh::pending::{INBOUND_RECORD_VERSION, InboundKind, InboundRecord};
use crate::mesh::r3::{AdmittedRequest, Handler, RefusalCode, Reply};
use crate::mesh::shares::PeerRef;
use crate::mesh::wire_path::WirePath;
use crate::mesh::{display_text, redact_hashes, rfc3339_utc, short};

use async_trait::async_trait;
use rmpv::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Weak;
use std::time::{SystemTime, UNIX_EPOCH};

/// Matches `GRANT_MAX_PATHS`: a request for more is a share list by another name.
pub(crate) const ACCESS_MAX_PATHS: usize = 16;
pub(crate) const ACCESS_REASON_MAX_CHARS: usize = 500;
/// Open requests one identity may have waiting on the human at once; the next is
/// refused rather than filed, so a peer cannot fill the inbound store by asking.
pub(crate) const ACCESS_MAX_PENDING_PER_IDENTITY: usize = 5;

/// A request body that passed every rule: a wire id, one to `ACCESS_MAX_PATHS` wire
/// paths with exact repeats dropped, and a reason cleaned for display.
pub(crate) struct ValidAccess {
    pub id: String,
    pub paths: Vec<String>,
    pub reason: String,
}

/// One admitted request as the surface sees it, whichever route it arrived by.
pub(crate) struct InboundAccess {
    pub identity_hash: String,
    pub destination_hash: String,
    pub request: ValidAccess,
    pub via: PeerVia,
}

pub(crate) enum AccessOutcome {
    Pending,
    /// `expires` is unix seconds, the grant's end as the peer should plan around it.
    Granted {
        expires: f64,
    },
    Refused(AccessRefusal),
}

impl AccessOutcome {
    fn status(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Granted { .. } => "granted",
            Self::Refused(_) => "refused",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessRefusal {
    Duplicate,
    TooManyPending,
}

impl AccessRefusal {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::TooManyPending => "too_many_pending",
        }
    }
}

/// Where an admitted access request is decided. Admission reads and writes the inbound
/// store, so it is called off the request loop.
pub(crate) trait AccessSurface: Send + Sync {
    fn admit_access(&self, request: InboundAccess) -> AccessOutcome;
}

/// Serves `/access`. The surface is held weakly because the slot owns the runtime that
/// owns the dispatcher that owns this handler.
pub(crate) struct AccessHandler {
    surface: Weak<dyn AccessSurface>,
}

impl AccessHandler {
    pub(crate) fn new(surface: Weak<dyn AccessSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Handler for AccessHandler {
    async fn handle(&self, request: AdmittedRequest) -> Reply {
        let identity_hash = request.identity.address_hash.to_hex_string();
        let destination_hash = request.destination_hash.to_hex_string();
        let id8 = short(&identity_hash).to_string();
        let dest8 = short(&destination_hash).to_string();
        let access = match decode_access(&request.body) {
            Ok(access) => access,
            Err(why) => {
                debug!(
                    "Mesh /access from {id8} (instance {dest8}) refused: {}",
                    redact_hashes(why)
                );
                return Reply::Code(RefusalCode::InvalidData);
            }
        };
        let Some(surface) = self.surface.upgrade() else {
            debug!(
                "Mesh /access from {id8} (instance {dest8}) dropped: the session slot behind the provider is gone"
            );
            return Reply::Silent;
        };
        let id = access.id.clone();
        let admitted = tokio::task::spawn_blocking(move || {
            surface.admit_access(InboundAccess {
                identity_hash,
                destination_hash,
                request: access,
                via: PeerVia::Direct,
            })
        })
        .await;
        let outcome = match admitted {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!(
                    "Mesh /access from {id8} (instance {dest8}) was not answered: {}",
                    redact_hashes(&err.to_string())
                );
                return Reply::Silent;
            }
        };
        match &outcome {
            AccessOutcome::Refused(refusal) => debug!(
                "Mesh /access from {id8} (instance {dest8}) refused: {}",
                refusal.wire_name()
            ),
            answered => debug!(
                "Mesh /access from {id8} (instance {dest8}) answered {}",
                answered.status()
            ),
        }
        Reply::Value(access_reply(&id, &outcome))
    }
}

/// The one encoder for the three reply shapes: a flat map of `v`, `id`, `status` and,
/// for a grant its `expires`, for a refusal its `reason`.
pub(crate) fn access_reply(id: &str, outcome: &AccessOutcome) -> Value {
    let mut entries = vec![
        (Value::from("v"), Value::from(PEER_WIRE_VERSION)),
        (Value::from("id"), Value::from(id)),
        (Value::from("status"), Value::from(outcome.status())),
    ];
    match outcome {
        AccessOutcome::Pending => {}
        AccessOutcome::Granted { expires } => {
            entries.push((Value::from("expires"), Value::from(*expires)));
        }
        AccessOutcome::Refused(refusal) => {
            entries.push((Value::from("reason"), Value::from(refusal.wire_name())));
        }
    }
    Value::Map(entries)
}

fn decode_access(body: &Value) -> Result<ValidAccess, &'static str> {
    let entries = versioned_map(body)?;
    let id = field(entries, "id")
        .and_then(Value::as_str)
        .ok_or("id is missing or not text")?;
    let paths = field(entries, "paths")
        .and_then(Value::as_array)
        .ok_or("paths is missing or not a list")?
        .iter()
        .map(|path| {
            path.as_str()
                .map(str::to_string)
                .ok_or("a path is not text")
        })
        .collect::<Result<Vec<String>, _>>()?;
    let reason = match field(entries, "reason") {
        None => "",
        Some(value) => value.as_str().ok_or("reason is not text")?,
    };
    validate_access(id, paths, reason)
}

/// The rules an access request is held to on every route it can arrive by. The path cap
/// is judged on what the peer sent and the floor on what is left once exact repeats are
/// dropped, so a padded request is refused and a repeated path is not asked for twice.
/// The reason is cleaned as `display_text` cleans any peer text; an absent or blank one
/// is empty.
pub(crate) fn validate_access(
    id: &str,
    paths: Vec<String>,
    reason: &str,
) -> Result<ValidAccess, &'static str> {
    if !is_wire_id(id) {
        return Err("id is not a wire id");
    }
    if paths.len() > ACCESS_MAX_PATHS {
        return Err("paths names more than the cap allows");
    }
    let mut kept: Vec<String> = Vec::with_capacity(paths.len());
    for path in paths {
        if WirePath::parse(&path).is_err() {
            return Err("a path is not a wire path");
        }
        if !kept.contains(&path) {
            kept.push(path);
        }
    }
    if kept.is_empty() {
        return Err("paths is empty");
    }
    let reason = match display_text(reason, usize::MAX) {
        None => String::new(),
        Some(cleaned) if cleaned.chars().count() > ACCESS_REASON_MAX_CHARS => {
            return Err("reason is longer than the cap allows");
        }
        Some(cleaned) => cleaned,
    };
    Ok(ValidAccess {
        id: id.to_string(),
        paths: kept,
        reason,
    })
}

impl AccessSurface for MeshSlot {
    fn admit_access(&self, request: InboundAccess) -> AccessOutcome {
        let InboundAccess {
            identity_hash,
            destination_hash,
            request,
            via,
        } = request;
        let id8 = short(&identity_hash).to_string();
        let dest8 = short(&destination_hash).to_string();
        let now = SystemTime::now();
        let hooks = self.hooks();
        let root = ShareSource::share_root(self);
        let peer = PeerRef {
            identity: &identity_hash,
            destination: &destination_hash,
        };
        if let (Some(root), Some(serving)) = (&root, ShareSource::serving(self))
            && already_shared(&serving, root, &peer, &request.paths)
        {
            let expires = (now + DEFAULT_GRANT_TTL)
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            hooks.fire(MeshEvent::AccessRequested {
                identity: identity_hash.clone(),
                destination: destination_hash.clone(),
                access_id: request.id.clone(),
                path_count: request.paths.len(),
            });
            hooks.fire(MeshEvent::AccessDecided {
                identity: identity_hash,
                destination: destination_hash,
                access_id: request.id,
                decision: AccessDecision::Granted,
            });
            debug!(
                "Mesh /access from {id8} (instance {dest8}) via {} granted at once: every path is already shared",
                via_word(via)
            );
            return AccessOutcome::Granted { expires };
        }
        // Once a node is installed the store is there, and filing is what makes a
        // request pending. A request that cannot be filed is refused with the one word
        // that tells the peer to ask again later, rather than left pending in a store
        // that never heard of it.
        let Some(store) = self.inbound_store() else {
            warn!("Mesh /access from {id8} (instance {dest8}) was not filed: the mesh is off");
            return AccessOutcome::Refused(AccessRefusal::TooManyPending);
        };
        let open = match store.list(now) {
            Ok(records) => records,
            Err(err) => {
                warn!(
                    "Mesh /access from {id8} (instance {dest8}) was not filed: {}",
                    redact_hashes(&format!("{err:#}"))
                );
                return AccessOutcome::Refused(AccessRefusal::TooManyPending);
            }
        };
        if let Some(refusal) = rate_rule(&open, &identity_hash, &request.paths) {
            debug!(
                "Mesh /access from {id8} (instance {dest8}) via {} refused: {}",
                via_word(via),
                refusal.wire_name()
            );
            return AccessOutcome::Refused(refusal);
        }
        let record = InboundRecord {
            version: INBOUND_RECORD_VERSION,
            id: request.id.clone(),
            peer_destination: destination_hash.clone(),
            peer_identity: identity_hash.clone(),
            thread: request.id.clone(),
            question: String::new(),
            envoy_question: String::new(),
            received_at: rfc3339_utc(now),
            kind: InboundKind::Access,
            paths: request.paths.clone(),
            reason: request.reason.clone(),
        };
        if let Err(err) = store.upsert(record, now) {
            warn!(
                "Mesh /access from {id8} (instance {dest8}) was not filed: {}",
                redact_hashes(&format!("{err:#}"))
            );
            return AccessOutcome::Refused(AccessRefusal::TooManyPending);
        }
        hooks.fire(MeshEvent::AccessRequested {
            identity: identity_hash.clone(),
            destination: destination_hash.clone(),
            access_id: request.id.clone(),
            path_count: request.paths.len(),
        });
        let label = self.peer_label(&identity_hash, &destination_hash);
        self.push_idle(IdleNotify {
            source: Source::Access,
            origin: Origin::Peer(id8.clone()),
            text: access_text(&label, &request, root.as_deref()),
            model_note: None,
        });
        debug!(
            "Mesh /access from {id8} (instance {dest8}) via {} is pending on {} paths",
            via_word(via),
            request.paths.len()
        );
        AccessOutcome::Pending
    }
}

fn via_word(via: PeerVia) -> &'static str {
    match via {
        PeerVia::Direct => "its link",
        PeerVia::StoreAndForward => "a propagation node",
    }
}

/// Whether the share rules already serve every path to `peer`. A root whose case could
/// not be probed shares nothing, as it does for a fetch.
fn already_shared(
    serving: &FetchServing,
    root: &Path,
    peer: &PeerRef<'_>,
    paths: &[String],
) -> bool {
    let Some(case_insensitive) = serving.case_insensitive_for(root) else {
        return false;
    };
    let shares = serving.shares_under(root);
    paths
        .iter()
        .all(|path| shares.is_allowed(peer, path, case_insensitive))
}

/// The refusal, if any, that `identity`'s open access requests earn a new one for
/// `paths`: the same set of paths already waiting, in any order, is a duplicate, and past
/// `ACCESS_MAX_PENDING_PER_IDENTITY` open requests nothing more is filed.
fn rate_rule(open: &[InboundRecord], identity: &str, paths: &[String]) -> Option<AccessRefusal> {
    let mine: Vec<&InboundRecord> = open
        .iter()
        .filter(|record| {
            record.kind == InboundKind::Access
                && record.peer_identity.eq_ignore_ascii_case(identity)
        })
        .collect();
    let asked = path_set(paths);
    if mine.iter().any(|record| path_set(&record.paths) == asked) {
        return Some(AccessRefusal::Duplicate);
    }
    if mine.len() >= ACCESS_MAX_PENDING_PER_IDENTITY {
        return Some(AccessRefusal::TooManyPending);
    }
    None
}

fn path_set(paths: &[String]) -> BTreeSet<&str> {
    paths.iter().map(String::as_str).collect()
}

/// The one line the person at the keyboard sees: who asks, each path with whether it is
/// here and how big, the reason, and the two verbs that settle it. The label and the
/// reason are quoted and any quote inside them becomes an apostrophe, so peer text
/// cannot close its own quotes and pose as the frame.
fn access_text(label: &str, request: &ValidAccess, root: Option<&Path>) -> String {
    let who = label.replace('"', "'");
    let count = request.paths.len();
    let plural = if count == 1 { "" } else { "s" };
    let listed = request
        .paths
        .iter()
        .map(|path| format!("{path} ({})", path_state(root, path)))
        .collect::<Vec<String>>()
        .join(", ");
    let reason = if request.reason.is_empty() {
        String::new()
    } else {
        format!(" — \"{}\"", request.reason.replace('"', "'"))
    };
    format!(
        "\"{who}\" asks for {count} path{plural}: {listed}{reason} · grant: .mesh grant {id} [--standing] | refuse: .mesh refuse {id}",
        id = request.id,
    )
}

/// `exists, <size>` for a regular file the path names under `root` once both are
/// resolved, so a symlink out of the tree reads as missing; `missing` otherwise. With no
/// root yet there is nothing to look at.
fn path_state(root: Option<&Path>, path: &str) -> String {
    let Some(root) = root else {
        return "unknown until a turn completes".to_string();
    };
    let Ok(wire) = WirePath::parse(path) else {
        return "missing".to_string();
    };
    dunce::canonicalize(root)
        .ok()
        .zip(dunce::canonicalize(root.join(wire.to_relative_path())).ok())
        .filter(|(root, file)| file.starts_with(root))
        .and_then(|(_, file)| fs::metadata(file).ok())
        .filter(fs::Metadata::is_file)
        .map_or_else(
            || "missing".to_string(),
            |meta| format!("exists, {}", human_size(meta.len())),
        )
}

/// `0 bytes`, `1 byte`, `512 bytes`, then decimal units: one decimal below ten of a unit
/// (`1.2 KB`), whole above (`16 KB`, `3 MB`), with a `.0` dropped (`2 KB`).
fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, f64); 3] = [("GB", 1e9), ("MB", 1e6), ("KB", 1e3)];
    let size = bytes as f64;
    match UNITS.iter().find(|(_, unit)| size >= *unit) {
        None if bytes == 1 => "1 byte".to_string(),
        None => format!("{bytes} bytes"),
        Some((name, unit)) => {
            let scaled = size / unit;
            let shown = if scaled < 10.0 {
                format!("{scaled:.1}")
            } else {
                format!("{scaled:.0}")
            };
            format!("{} {name}", shown.strip_suffix(".0").unwrap_or(&shown))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookEvent;
    use crate::mesh::events::{RecordingHookSink, env_value};
    use crate::mesh::grants::GRANT_MAX_PATHS;
    use crate::mesh::hex_lower;
    use crate::mesh::idle::IdleSink;
    use crate::mesh::pending::InboundStore;
    use crate::mesh::r3::{ACCESS_PATH, PathHash, RequestId, SizeBranch};
    use crate::mesh::test_support::{TempDir, snapshot_fixture};
    use rand_core::OsRng;
    use rns_transport::destination::link::LinkId;
    use rns_transport::hash::AddressHash;
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[cfg(unix)]
    use crate::config::Session;
    #[cfg(unix)]
    use crate::mesh::envoy::{EnvoyJob, EnvoySink};
    #[cfg(unix)]
    use crate::mesh::limits::PeerRefusal;
    #[cfg(unix)]
    use crate::mesh::mesh_config_dir;
    #[cfg(unix)]
    use crate::mesh::node::{MeshRuntime, NodeOptions};
    #[cfg(unix)]
    use crate::mesh::peers::PeerSighting;
    #[cfg(unix)]
    use crate::mesh::protocol::MESH_PROTOCOL_VERSION;
    #[cfg(unix)]
    use crate::mesh::r3::NAME_HASH_LEN;
    #[cfg(unix)]
    use crate::mesh::test_support::{loopback_relay, mesh_paths, private_config};
    #[cfg(unix)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use tokio::task::JoinHandle;

    const IDENTITY: [u8; 16] = [0xab; 16];
    const DESTINATION: [u8; 16] = [0x2b; 16];

    fn identity() -> String {
        hex_lower(&IDENTITY)
    }

    fn destination() -> String {
        hex_lower(&DESTINATION)
    }

    fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(
            entries
                .into_iter()
                .map(|(key, value)| (Value::from(key), value))
                .collect(),
        )
    }

    fn list(paths: &[&str]) -> Value {
        Value::Array(paths.iter().map(|path| Value::from(*path)).collect())
    }

    fn body(id: &str, paths: &[&str], reason: &str) -> Value {
        map(vec![
            ("v", Value::from(PEER_WIRE_VERSION)),
            ("id", Value::from(id)),
            ("paths", list(paths)),
            ("reason", Value::from(reason)),
        ])
    }

    fn admitted(body: Value, identity: &TransportIdentity) -> AdmittedRequest {
        AdmittedRequest {
            link_id: LinkId::new_from_rand(OsRng),
            identity: *identity.as_identity(),
            destination_hash: AddressHash::new_from_hex_string(&destination()).unwrap(),
            request_id: RequestId::from([1u8; 16]),
            path_hash: PathHash::of(ACCESS_PATH),
            requested_at: 1_700_000_000.0,
            body,
            branch: SizeBranch::Packet,
        }
    }

    fn strings(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_string()).collect()
    }

    fn inbound(identity: &str, id: &str, paths: &[&str], reason: &str) -> InboundAccess {
        InboundAccess {
            identity_hash: identity.to_string(),
            destination_hash: destination(),
            request: validate_access(id, strings(paths), reason).unwrap(),
            via: PeerVia::Direct,
        }
    }

    #[derive(Default)]
    struct RecordingIdleSink(parking_lot::Mutex<Vec<IdleNotify>>);

    impl RecordingIdleSink {
        fn texts(&self) -> Vec<String> {
            self.0.lock().iter().map(|note| note.text.clone()).collect()
        }
    }

    impl IdleSink for RecordingIdleSink {
        fn push(&self, note: IdleNotify) -> Result<(), IdleNotify> {
            self.0.lock().push(note);
            Ok(())
        }

        fn request_sync(&self) {}
    }

    /// A slot with everything admission touches but a node: the inbound store, an idle
    /// sink, a hook sink and a snapshot whose `cwd` is `root`. With no node there is no
    /// serving state, so nothing is ever granted at once.
    struct BareSlot {
        slot: Arc<MeshSlot>,
        idle: Arc<RecordingIdleSink>,
        hooks: Arc<RecordingHookSink>,
        root: PathBuf,
        tmp: TempDir,
    }

    fn bare_slot(tag: &str) -> BareSlot {
        let tmp = TempDir::new(tag);
        let root = tmp.path.join("ws");
        fs::create_dir_all(&root).unwrap();
        let slot = Arc::new(MeshSlot::default());
        slot.set_inbound_store_for_tests(Arc::new(InboundStore::new(&tmp.path, "inst")));
        let idle = Arc::new(RecordingIdleSink::default());
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let hooks = RecordingHookSink::attach(&slot.hooks());
        slot.publish(crate::mesh::snapshot::MeshSnapshot {
            cwd: root.clone(),
            ..snapshot_fixture()
        });
        BareSlot {
            slot,
            idle,
            hooks,
            root,
            tmp,
        }
    }

    fn store_of(slot: &MeshSlot) -> Arc<InboundStore> {
        slot.inbound_store().unwrap()
    }

    fn access_records(slot: &MeshSlot) -> Vec<InboundRecord> {
        store_of(slot).list(SystemTime::now()).unwrap()
    }

    #[test]
    fn access_limits_match_the_grant_store() {
        assert_eq!(ACCESS_MAX_PATHS, GRANT_MAX_PATHS);
    }

    #[test]
    fn an_access_reply_is_a_flat_map_with_status_and_sibling_keys() {
        assert_eq!(
            access_reply("a-1", &AccessOutcome::Pending),
            map(vec![
                ("v", Value::from(1u64)),
                ("id", Value::from("a-1")),
                ("status", Value::from("pending")),
            ])
        );
        assert_eq!(
            access_reply(
                "a-1",
                &AccessOutcome::Granted {
                    expires: 1_790_000_900.0
                }
            ),
            map(vec![
                ("v", Value::from(1u64)),
                ("id", Value::from("a-1")),
                ("status", Value::from("granted")),
                ("expires", Value::from(1_790_000_900.0_f64)),
            ])
        );
        for (refusal, word) in [
            (AccessRefusal::Duplicate, "duplicate"),
            (AccessRefusal::TooManyPending, "too_many_pending"),
        ] {
            assert_eq!(
                access_reply("a-1", &AccessOutcome::Refused(refusal)),
                map(vec![
                    ("v", Value::from(1u64)),
                    ("id", Value::from("a-1")),
                    ("status", Value::from("refused")),
                    ("reason", Value::from(word)),
                ])
            );
        }
    }

    #[tokio::test]
    async fn an_access_body_that_fails_any_rule_earns_the_same_invalid_data_refusal() {
        let fixture = bare_slot("access-invalid");
        let handler = AccessHandler::new(Arc::downgrade(&fixture.slot) as Weak<dyn AccessSurface>);
        let peer = TransportIdentity::new_from_rand(OsRng);
        let seventeen: Vec<String> = (0..17).map(|n| format!("src/{n}.rs")).collect();
        let seventeen: Vec<&str> = seventeen.iter().map(String::as_str).collect();
        let long_reason = "r".repeat(ACCESS_REASON_MAX_CHARS + 1);
        let cases: Vec<(&str, Value)> = vec![
            (
                "missing v",
                map(vec![
                    ("id", Value::from("a-1")),
                    ("paths", list(&["src/x.rs"])),
                ]),
            ),
            ("non-wire id", body("a 1", &["src/x.rs"], "")),
            ("no paths", body("a-1", &[], "")),
            ("seventeen paths", body("a-1", &seventeen, "")),
            ("a dot-dot path", body("a-1", &["../x.rs"], "")),
            ("a backslash path", body("a-1", &["src\\x.rs"], "")),
            (
                "a reason past the cap",
                body("a-1", &["src/x.rs"], &long_reason),
            ),
            (
                "paths not a list",
                map(vec![
                    ("v", Value::from(PEER_WIRE_VERSION)),
                    ("id", Value::from("a-1")),
                    ("paths", Value::from("src/x.rs")),
                ]),
            ),
            ("the body not a map", Value::from("src/x.rs")),
        ];
        for (case, body) in cases {
            let reply = handler.handle(admitted(body, &peer)).await;
            assert!(
                matches!(reply, Reply::Code(RefusalCode::InvalidData)),
                "{case}"
            );
        }
        assert!(access_records(&fixture.slot).is_empty());
        assert!(fixture.idle.texts().is_empty());
        assert!(fixture.hooks.snapshot().is_empty());
    }

    #[test]
    fn a_reason_of_exactly_the_cap_is_accepted() {
        let reason = "r".repeat(ACCESS_REASON_MAX_CHARS);
        let valid = validate_access("a-1", strings(&["src/x.rs"]), &reason).unwrap();
        assert_eq!(valid.reason, reason);
        assert!(validate_access("a-1", strings(&["src/x.rs"]), &format!("{reason}r")).is_err());
    }

    #[test]
    fn duplicate_paths_within_one_request_collapse_before_the_count() {
        let mut padded: Vec<String> = (0..15).map(|n| format!("src/{n}.rs")).collect();
        padded.push("src/0.rs".to_string());
        padded.push("src/1.rs".to_string());
        assert_eq!(padded.len(), 17);
        assert!(validate_access("a-1", padded, "").is_err());

        let mut repeated: Vec<String> = (0..14).map(|n| format!("src/{n}.rs")).collect();
        repeated.push("src/0.rs".to_string());
        repeated.push("src/1.rs".to_string());
        let valid = validate_access("a-1", repeated, "").unwrap();
        assert_eq!(valid.paths.len(), 14);
        assert_eq!(valid.paths[0], "src/0.rs");
        assert_eq!(valid.paths[13], "src/13.rs");

        let only_repeats = validate_access("a-1", strings(&["src/x.rs", "src/x.rs"]), "").unwrap();
        assert_eq!(only_repeats.paths, vec!["src/x.rs".to_string()]);
    }

    #[test]
    fn an_access_request_for_an_unshared_path_is_pending_and_stored_as_an_access_record() {
        let fixture = bare_slot("access-pending");
        let outcome = fixture.slot.admit_access(inbound(
            &identity(),
            "a-1",
            &["src/x.rs", "docs/y.md"],
            "need the struct",
        ));
        assert!(matches!(outcome, AccessOutcome::Pending));
        let records = access_records(&fixture.slot);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.kind, InboundKind::Access);
        assert_eq!(record.id, "a-1");
        assert_eq!(record.thread, "a-1");
        assert_eq!(record.peer_identity, identity());
        assert_eq!(record.peer_destination, destination());
        assert_eq!(record.paths, strings(&["src/x.rs", "docs/y.md"]));
        assert_eq!(record.reason, "need the struct");
        assert!(record.question.is_empty());
        assert_eq!(fixture.idle.texts().len(), 1);
        let fired = fixture.hooks.drain();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, HookEvent::MeshAccessRequested);
        assert_eq!(env_value(&fired[0].1, "COYOTE_MESH_PATH_COUNT"), Some("2"));
        assert_eq!(env_value(&fired[0].1, "COYOTE_MESH_ACCESS_ID"), Some("a-1"));
    }

    #[test]
    fn a_second_request_for_the_same_path_set_while_the_first_is_pending_is_refused_as_duplicate() {
        let fixture = bare_slot("access-duplicate");
        let first =
            fixture
                .slot
                .admit_access(inbound(&identity(), "a-1", &["src/x.rs", "docs/y.md"], ""));
        assert!(matches!(first, AccessOutcome::Pending));

        let again =
            fixture
                .slot
                .admit_access(inbound(&identity(), "a-2", &["src/x.rs", "docs/y.md"], ""));
        assert!(matches!(
            again,
            AccessOutcome::Refused(AccessRefusal::Duplicate)
        ));
        let reordered =
            fixture
                .slot
                .admit_access(inbound(&identity(), "a-3", &["docs/y.md", "src/x.rs"], ""));
        assert!(matches!(
            reordered,
            AccessOutcome::Refused(AccessRefusal::Duplicate)
        ));
        let different = fixture
            .slot
            .admit_access(inbound(&identity(), "a-4", &["src/x.rs"], ""));
        assert!(matches!(different, AccessOutcome::Pending));

        assert_eq!(access_records(&fixture.slot).len(), 2);
        assert_eq!(fixture.idle.texts().len(), 2);
        assert_eq!(fixture.hooks.snapshot().len(), 2);
    }

    #[test]
    fn a_removed_access_record_frees_its_path_set_to_be_asked_again() {
        let fixture = bare_slot("access-removed");
        let first = fixture
            .slot
            .admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""));
        assert!(matches!(first, AccessOutcome::Pending));
        assert!(store_of(&fixture.slot).remove("a-1").unwrap());

        let again = fixture
            .slot
            .admit_access(inbound(&identity(), "a-2", &["src/x.rs"], ""));
        assert!(matches!(again, AccessOutcome::Pending));
        assert_eq!(access_records(&fixture.slot).len(), 1);
    }

    #[test]
    fn a_sixth_pending_request_from_one_identity_is_refused_as_too_many_pending() {
        let fixture = bare_slot("access-too-many");
        for n in 0..ACCESS_MAX_PENDING_PER_IDENTITY {
            let path = format!("src/{n}.rs");
            let outcome =
                fixture
                    .slot
                    .admit_access(inbound(&identity(), &format!("a-{n}"), &[&path], ""));
            assert!(matches!(outcome, AccessOutcome::Pending), "{n}");
        }
        let sixth = fixture
            .slot
            .admit_access(inbound(&identity(), "a-6", &["src/6.rs"], ""));
        assert!(matches!(
            sixth,
            AccessOutcome::Refused(AccessRefusal::TooManyPending)
        ));
        let other = hex_lower(&[0xcd; 16]);
        let elsewhere = fixture
            .slot
            .admit_access(inbound(&other, "b-1", &["src/6.rs"], ""));
        assert!(matches!(elsewhere, AccessOutcome::Pending));
        assert_eq!(
            access_records(&fixture.slot).len(),
            ACCESS_MAX_PENDING_PER_IDENTITY + 1
        );
        assert_eq!(
            fixture.idle.texts().len(),
            ACCESS_MAX_PENDING_PER_IDENTITY + 1
        );
    }

    #[test]
    fn the_human_line_names_the_peer_the_paths_with_existence_and_size_and_both_verbs() {
        let fixture = bare_slot("access-line");
        fs::create_dir_all(fixture.root.join("src")).unwrap();
        fs::write(fixture.root.join("src/x.rs"), vec![b'x'; 2048]).unwrap();
        fixture.slot.admit_access(inbound(
            &identity(),
            "a-1",
            &["src/x.rs", "src/gone.rs"],
            "need the struct",
        ));
        assert_eq!(
            fixture.idle.texts(),
            vec![
                "\"abababab\" asks for 2 paths: src/x.rs (exists, 2 KB), src/gone.rs (missing) — \"need the struct\" · grant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
                    .to_string()
            ]
        );
        let notes = fixture.idle.0.lock();
        let note = &notes[0];
        assert_eq!(note.source, Source::Access);
        assert_eq!(note.origin, Origin::Peer("abababab".to_string()));
        assert!(note.model_note.is_none());
        assert!(!note.text.ends_with('\n'));
    }

    #[test]
    fn a_single_path_without_a_reason_reads_as_one_path_and_no_reason_clause() {
        let fixture = bare_slot("access-line-one");
        fixture
            .slot
            .admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""));
        assert_eq!(
            fixture.idle.texts(),
            vec![
                "\"abababab\" asks for 1 path: src/x.rs (missing) · grant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
                    .to_string()
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_reached_through_a_link_out_of_the_root_reads_as_missing() {
        let fixture = bare_slot("access-line-link");
        let outside = fixture.tmp.path.join("outside.txt");
        fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, fixture.root.join("alias.txt")).unwrap();
        assert_eq!(path_state(Some(&fixture.root), "alias.txt"), "missing");
        fs::write(fixture.root.join("here.txt"), b"x").unwrap();
        assert_eq!(
            path_state(Some(&fixture.root), "here.txt"),
            "exists, 1 byte"
        );
    }

    #[test]
    fn without_a_share_root_the_human_line_says_the_paths_are_unknown_until_a_turn_completes() {
        let tmp = TempDir::new("access-no-root");
        let slot = Arc::new(MeshSlot::default());
        slot.set_inbound_store_for_tests(Arc::new(InboundStore::new(&tmp.path, "inst")));
        let idle = Arc::new(RecordingIdleSink::default());
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        slot.admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""));
        assert_eq!(
            idle.texts(),
            vec![
                "\"abababab\" asks for 1 path: src/x.rs (unknown until a turn completes) · grant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
                    .to_string()
            ]
        );
    }

    #[test]
    fn the_reason_is_sanitised_before_it_is_shown() {
        let fixture = bare_slot("access-reason");
        fixture.slot.admit_access(inbound(
            &identity(),
            "a-1",
            &["src/x.rs"],
            "\u{1b}[31mthe \"struct\"\u{2028}please\u{7}",
        ));
        let texts = fixture.idle.texts();
        assert_eq!(texts.len(), 1);
        assert!(
            texts[0].contains(" — \"the 'struct' please\" · "),
            "{}",
            texts[0]
        );
        assert_eq!(
            access_records(&fixture.slot)[0].reason,
            "the \"struct\" please"
        );
    }

    #[test]
    fn sizes_read_in_bytes_then_decimal_units() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(1), "1 byte");
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(999), "999 bytes");
        assert_eq!(human_size(1_200), "1.2 KB");
        assert_eq!(human_size(2_048), "2 KB");
        assert_eq!(human_size(16_384), "16 KB");
        assert_eq!(human_size(3_000_000), "3 MB");
        assert_eq!(human_size(2_500_000_000), "2.5 GB");
    }

    /// An envoy that counts what it is offered; an access request must never reach it.
    #[cfg(unix)]
    #[derive(Default)]
    struct CountingEnvoy(AtomicUsize);

    #[cfg(unix)]
    impl EnvoySink for CountingEnvoy {
        fn accept(&self, _job: EnvoyJob) -> Result<(), PeerRefusal> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn answer(&self, _id: &str, _text: &str) -> bool {
            false
        }

        fn interrupt(&self) {}
    }

    /// A slot with a node installed, so the serving state and the peer table are live:
    /// what the immediate grant and the peer's display name need.
    #[cfg(unix)]
    struct Installed {
        slot: Arc<MeshSlot>,
        runtime: Arc<MeshRuntime>,
        idle: Arc<RecordingIdleSink>,
        hooks: Arc<RecordingHookSink>,
        root: PathBuf,
        relay: JoinHandle<()>,
        tmp: TempDir,
    }

    #[cfg(unix)]
    impl Installed {
        async fn start(tag: &str) -> Self {
            let (addr, relay, _) = loopback_relay().await;
            let tmp = TempDir::new(tag);
            let root = tmp.path.join("ws");
            fs::create_dir_all(root.join("src")).unwrap();
            let slot = Arc::new(MeshSlot::default());
            let hooks = RecordingHookSink::attach(&slot.hooks());
            let mut session = Session::default();
            let runtime = MeshRuntime::start(
                &private_config(addr.port()),
                true,
                &mut session,
                mesh_paths(&tmp),
                NodeOptions {
                    hooks: slot.hooks(),
                    ..NodeOptions::default()
                },
            )
            .await
            .unwrap();
            slot.install(Arc::clone(&runtime)).unwrap();
            let idle = Arc::new(RecordingIdleSink::default());
            slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
            slot.publish(crate::mesh::snapshot::MeshSnapshot {
                cwd: root.clone(),
                ..snapshot_fixture()
            });
            hooks.drain();
            Self {
                slot,
                runtime,
                idle,
                hooks,
                root,
                relay,
                tmp,
            }
        }

        /// Shares `pattern` with the peer whose identity is `peer` through the global list.
        fn share_with(&self, pattern: &str, peer: &str) {
            let path = mesh_config_dir(&self.tmp.path.join("config")).join("shares.yaml");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!("version: 1\nallow:\n- pattern: '{pattern}'\n  peer: '{peer}'\n"),
            )
            .unwrap();
        }

        fn name_peer(&self, name: &str) {
            self.runtime.peers().observe(
                PeerSighting {
                    destination_hash: destination(),
                    identity_hash: identity(),
                    name_hash: hex_lower(&[7u8; NAME_HASH_LEN]),
                    display_name: Some(name.to_string()),
                    protocol_version: MESH_PROTOCOL_VERSION,
                    hops: 1,
                },
                SystemTime::now(),
            );
        }

        async fn stop(self) {
            assert!(self.slot.stop().await.unwrap());
            self.relay.abort();
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_attaches_the_slot_as_the_access_surface() {
        let installed = Installed::start("access-attached").await;
        assert!(installed.runtime.access_surface().is_some());
        installed.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_access_request_for_paths_already_shared_with_the_peer_is_granted_at_once_without_a_human()
     {
        let installed = Installed::start("access-granted").await;
        fs::write(installed.root.join("src/x.rs"), b"struct X;").unwrap();
        installed.share_with("src/x.rs", &identity());
        let before = SystemTime::now();

        let outcome = tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || slot.admit_access(inbound(&identity(), "a-1", &["src/x.rs"], "please"))
        })
        .await
        .unwrap();

        let AccessOutcome::Granted { expires } = outcome else {
            panic!("not granted");
        };
        let floor = (before + DEFAULT_GRANT_TTL)
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        assert!(expires >= floor && expires < floor + 60.0, "{expires}");
        assert!(access_records(&installed.slot).is_empty());
        assert!(installed.idle.texts().is_empty());
        let fired = installed.hooks.drain();
        assert_eq!(fired.len(), 2, "{fired:?}");
        assert_eq!(fired[0].0, HookEvent::MeshAccessRequested);
        assert_eq!(fired[1].0, HookEvent::MeshAccessDecided);
        assert_eq!(
            env_value(&fired[1].1, "COYOTE_MESH_DECISION"),
            Some("granted")
        );

        let partly = tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || slot.admit_access(inbound(&identity(), "a-2", &["src/x.rs", "src/y.rs"], ""))
        })
        .await
        .unwrap();
        assert!(matches!(partly, AccessOutcome::Pending));
        installed.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_human_line_quotes_the_peers_display_name_with_its_quotes_made_apostrophes() {
        let installed = Installed::start("access-named").await;
        installed.name_peer("Ada \"the\" peer");

        tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || slot.admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""))
        })
        .await
        .unwrap();

        assert_eq!(
            installed.idle.texts(),
            vec![
                "\"Ada 'the' peer\" asks for 1 path: src/x.rs (missing) · grant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
                    .to_string()
            ]
        );
        installed.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_access_request_never_reaches_the_envoy_sink() {
        let installed = Installed::start("access-envoy").await;
        let envoy = Arc::new(CountingEnvoy::default());
        installed
            .slot
            .set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);
        let handler =
            AccessHandler::new(Arc::downgrade(&installed.slot) as Weak<dyn AccessSurface>);
        let peer = TransportIdentity::new_from_rand(OsRng);

        let reply = handler
            .handle(admitted(body("a-1", &["src/x.rs"], "need it"), &peer))
            .await;

        let Reply::Value(value) = reply else {
            panic!("not a value reply");
        };
        assert_eq!(value, access_reply("a-1", &AccessOutcome::Pending));
        assert_eq!(envoy.0.load(Ordering::SeqCst), 0);
        assert_eq!(access_records(&installed.slot).len(), 1);
        assert_eq!(installed.idle.texts().len(), 1);
        installed.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn access_events_carry_peer_count_and_decision_but_never_a_path() {
        let installed = Installed::start("access-events").await;
        fs::write(installed.root.join("src/x.rs"), b"struct X;").unwrap();
        installed.share_with("src/x.rs", &identity());
        let paths = ["src/x.rs", "docs/secret-plan.md"];

        let outcomes = tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || {
                (
                    slot.admit_access(inbound(&identity(), "a-1", &paths, "why")),
                    slot.admit_access(inbound(&identity(), "a-2", &paths[..1], "why")),
                )
            }
        })
        .await
        .unwrap();
        assert!(matches!(outcomes.0, AccessOutcome::Pending));
        assert!(matches!(outcomes.1, AccessOutcome::Granted { .. }));

        let fired = installed.hooks.drain();
        let events: Vec<HookEvent> = fired.iter().map(|(event, _)| *event).collect();
        assert_eq!(
            events,
            vec![
                HookEvent::MeshAccessRequested,
                HookEvent::MeshAccessRequested,
                HookEvent::MeshAccessDecided,
            ]
        );
        let requested = &fired[0].1;
        assert_eq!(
            env_value(requested, "COYOTE_MESH_PEER_IDENTITY"),
            Some(identity().as_str())
        );
        assert_eq!(
            env_value(requested, "COYOTE_MESH_PEER_DESTINATION"),
            Some(destination().as_str())
        );
        assert_eq!(env_value(requested, "COYOTE_MESH_ACCESS_ID"), Some("a-1"));
        assert_eq!(env_value(requested, "COYOTE_MESH_PATH_COUNT"), Some("2"));
        assert_eq!(requested.len(), 4);
        let decided = &fired[2].1;
        assert_eq!(env_value(decided, "COYOTE_MESH_ACCESS_ID"), Some("a-2"));
        assert_eq!(env_value(decided, "COYOTE_MESH_DECISION"), Some("granted"));
        assert_eq!(decided.len(), 4);
        for (_, envs) in &fired {
            for (_, value) in envs {
                for path in paths {
                    assert!(!value.contains(path), "{value}");
                }
                assert!(!value.contains("why"), "{value}");
            }
        }
        installed.stop().await;
    }
}
