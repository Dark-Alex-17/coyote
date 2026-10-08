//! Requirement-id keyed vectors for the access path of section 10.16 and the envoy's
//! disposition contract of section 10.10: the `/access` request table and the rate rule
//! as the `AccessHandler` applies them over a bare `MeshSlot`, the reply table as the
//! requester's `decode_access_response` reads it, the LXMF carriage read by
//! `decode_access_message` and routed by `AccessRouting`, the human's decision as
//! `decision_reply` shapes it and the requester correlates it, and the outcome wording of
//! `classify_answer`, `escalated_notice` and `envoy_reply`. Every row runs in-process; an
//! id that needs a share set, a grant store or a live envoy run has no row here.
//!
//! Every row names the id it exercises and the receiver action the spec mandates for it. A
//! row written faithfully from the spec that the code does not honour is kept as written
//! and flagged with `known_divergence`; the executor prints such a row instead of asserting
//! it, and fails when the flag goes stale.

use super::{Kind, Listed};
use crate::config::UnavailableReason;
use crate::config::mesh_envoy::{
    DECLINED_FALLBACK_TEXT, EnvoyOutcome, classify_answer, envoy_reply, escalated_notice,
};
use crate::hooks::HookEvent;
use crate::mesh::access::{
    ACCESS_MAX_PATHS, ACCESS_MAX_PENDING_PER_IDENTITY, ACCESS_REASON_MAX_CHARS, ACCESS_TYPE,
    AccessError, AccessHandler, AccessMessage, AccessOutcome, AccessRefusal, AccessRouting,
    AccessSurface, ValidAccess, access_message, access_reply, decision_reply,
    decode_access_message, decode_access_response, validate_access,
};
use crate::mesh::events::{AccessDecision, RecordingHookSink, env_value};
use crate::mesh::idle::{IdleNotify, IdleSink};
use crate::mesh::message::{
    Disposition, OutboundPeer, PEER_WIRE_VERSION, Part, PartLimits, PeerKind, PeerMessage, PeerVia,
    RawPart, RawPeerMessage, from_r3_body, to_r3_body,
};
use crate::mesh::node::MeshSlot;
use crate::mesh::pending::{
    INBOUND_RECORD_VERSION, InboundKind, InboundRecord, InboundStore, PENDING_RECORD_VERSION,
    PendingRecord, PendingState,
};
use crate::mesh::r3::{ACCESS_PATH, DispatchError};
use crate::mesh::snapshot::MeshSnapshot;
use crate::mesh::test_support::{
    AdmittedRequest, Handler, InboundMessage, InboundSink, NAME_HASH_LEN, OriginName, PathHash,
    RefusalCode, Reply, RequestId, SizeBranch, TempDir, TrustList, snapshot_fixture,
};
use crate::mesh::{destination_address, hex_lower, rfc3339_utc};

use lxmf_core::constants::{FIELD_CUSTOM_DATA, FIELD_CUSTOM_TYPE};
use rand_core::OsRng;
use rmpv::Value;
use rns_transport::destination::link::LinkId;
use rns_transport::hash::AddressHash;
use rns_transport::identity::PrivateIdentity;
use serde_json::json;
use std::fmt::Debug;
use std::fs;
use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    /// `AccessHandler` over a bare `MeshSlot` with an inbound store and no share set,
    /// section 10.16: the request table and the rate rule.
    AccessRequest(RequestProbe),
    /// `decode_access_response`, the requester's reading of the reply table.
    AccessReply(ReplyProbe),
    /// `decode_access_message` and `AccessRouting`, the LXMF carriage of section 10.16.
    AccessLxmf(LxmfProbe),
    /// `decision_reply` and the requester's correlation of it, MESH-ACCESS-018.
    Decision(Check),
    /// The envoy's outcome wording: `classify_answer`, `escalated_notice` and
    /// `envoy_reply`, section 10.10.
    Disposition(Check),
}

enum RequestProbe {
    /// A body the handler refuses with `InvalidData`, filing nothing.
    Refused(Value),
    Check(Check),
}

enum ReplyProbe {
    /// What `decode_access_response` makes of `value` for the request sent as `a-1`.
    Decode {
        value: Value,
        expect: Result<AccessOutcome, AccessError>,
    },
    Check(Check),
}

enum LxmfProbe {
    /// What `decode_access_message` makes of a fetched message.
    Decode {
        message: Box<InboundMessage>,
        expect: AccessMessage,
    },
    Check(Check),
}

impl Case {
    fn family(&self) -> &'static str {
        match self {
            Self::AccessRequest(_) => "AccessRequest",
            Self::AccessReply(_) => "AccessReply",
            Self::AccessLxmf(_) => "AccessLxmf",
            Self::Decision(_) => "Decision",
            Self::Disposition(_) => "Disposition",
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
        Case::AccessRequest(RequestProbe::Refused(body)) => {
            let fixture = bare_slot("access-refused");
            let reply = fixture.ask(body.clone());
            same(
                "the refusal",
                code_of(&reply),
                Some(RefusalCode::InvalidData),
            )?;
            fixture.nothing_filed()
        }
        Case::AccessRequest(RequestProbe::Check(check))
        | Case::AccessReply(ReplyProbe::Check(check))
        | Case::AccessLxmf(LxmfProbe::Check(check))
        | Case::Decision(check)
        | Case::Disposition(check) => check(),
        Case::AccessReply(ReplyProbe::Decode { value, expect }) => same(
            "the decoded reply",
            decode_access_response(value, SENT_ID),
            expect.clone(),
        ),
        Case::AccessLxmf(LxmfProbe::Decode { message, expect }) => same(
            "the decoded message",
            decode_access_message(message),
            expect.clone(),
        ),
    }
}

// ---------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------

const SENT_ID: &str = "a-1";
const IDENTITY: [u8; 16] = [0xab; 16];
const DESTINATION: [u8; 16] = [0x2b; 16];
const EXPIRES_SECS: u64 = 1_790_000_900;
const EXPIRES_TEXT: &str = "2026-09-21T14:28:20Z";

fn identity() -> String {
    hex_lower(&IDENTITY)
}

fn destination() -> String {
    hex_lower(&DESTINATION)
}

fn expires() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(EXPIRES_SECS)
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

fn strings(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| (*path).to_string()).collect()
}

fn numbered(count: usize) -> Vec<String> {
    (0..count).map(|n| format!("src/{n}.rs")).collect()
}

fn refs(paths: &[String]) -> Vec<&str> {
    paths.iter().map(String::as_str).collect()
}

fn body(id: &str, paths: &[&str], reason: &str) -> Value {
    map(vec![
        ("v", Value::from(PEER_WIRE_VERSION)),
        ("id", Value::from(id)),
        ("paths", list(paths)),
        ("reason", Value::from(reason)),
    ])
}

/// A reply for `a-1` with `v` and `id` in place and the given rest.
fn reply_with(entries: Vec<(&str, Value)>) -> Value {
    let mut all = vec![
        ("v", Value::from(PEER_WIRE_VERSION)),
        ("id", Value::from(SENT_ID)),
    ];
    all.extend(entries);
    map(all)
}

fn code_of(reply: &Reply) -> Option<RefusalCode> {
    match reply {
        Reply::Code(code) => Some(*code),
        _ => None,
    }
}

fn value_of(reply: &Reply) -> Result<&Value, String> {
    match reply {
        Reply::Value(value) | Reply::Settled { value, .. } => Ok(value),
        Reply::Code(code) => Err(format!("refused with {code:?}")),
        Reply::Silent => Err("silent".to_string()),
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[derive(Default)]
struct RecordingIdleSink(parking_lot::Mutex<Vec<IdleNotify>>);

impl RecordingIdleSink {
    fn count(&self) -> usize {
        self.0.lock().len()
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
/// sink, a hook sink and a snapshot whose `cwd` is a workspace under the directory. With
/// no node there is no serving state, so nothing is ever granted at once.
struct BareSlot {
    slot: Arc<MeshSlot>,
    idle: Arc<RecordingIdleSink>,
    hooks: Arc<RecordingHookSink>,
    peer: PrivateIdentity,
    _tmp: TempDir,
}

fn bare_slot(tag: &str) -> BareSlot {
    let tmp = TempDir::new(tag);
    let root = tmp.path.join("ws");
    fs::create_dir_all(&root).unwrap();
    let slot = Arc::new(MeshSlot::default());
    slot.set_inbound_store_for_tests(Arc::new(InboundStore::new(&tmp.path, "inst")));
    let fixture = BareSlot::around(slot, tmp);
    fixture.slot.publish(MeshSnapshot {
        cwd: root,
        ..snapshot_fixture()
    });
    fixture
}

/// A slot that was never given an inbound store, so nothing can be filed.
fn storeless_slot(tag: &str) -> BareSlot {
    BareSlot::around(Arc::new(MeshSlot::default()), TempDir::new(tag))
}

impl BareSlot {
    fn around(slot: Arc<MeshSlot>, tmp: TempDir) -> Self {
        let idle = Arc::new(RecordingIdleSink::default());
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let hooks = RecordingHookSink::attach(&slot.hooks());
        Self {
            slot,
            idle,
            hooks,
            peer: PrivateIdentity::new_from_rand(OsRng),
            _tmp: tmp,
        }
    }

    fn peer_hash(&self) -> String {
        self.peer.as_identity().address_hash.to_hex_string()
    }

    fn surface(&self) -> Weak<dyn AccessSurface> {
        Arc::downgrade(&self.slot) as Weak<dyn AccessSurface>
    }

    fn admitted(&self, body: Value, peer: &PrivateIdentity) -> AdmittedRequest {
        AdmittedRequest {
            link_id: LinkId::new_from_rand(OsRng),
            identity: *peer.as_identity(),
            destination_hash: AddressHash::new_from_hex_string(&destination()).unwrap(),
            request_id: RequestId::from([1u8; 16]),
            path_hash: PathHash::of(ACCESS_PATH),
            requested_at: 1_700_000_000.0,
            body,
            branch: SizeBranch::Packet,
        }
    }

    fn ask(&self, body: Value) -> Reply {
        self.ask_as(body, &self.peer)
    }

    fn ask_as(&self, body: Value, peer: &PrivateIdentity) -> Reply {
        block_on(AccessHandler::new(self.surface()).handle(self.admitted(body, peer)))
    }

    /// The handler's answer to `body`, read back as the requester would.
    fn outcome(&self, body: Value) -> Result<AccessOutcome, String> {
        self.outcome_as(body, &self.peer)
    }

    fn outcome_as(&self, body: Value, peer: &PrivateIdentity) -> Result<AccessOutcome, String> {
        let reply = self.ask_as(body.clone(), peer);
        let value = value_of(&reply)?;
        let id = body
            .as_map()
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|(key, _)| key.as_str() == Some("id"))
                    .and_then(|(_, id)| id.as_str())
            })
            .ok_or("the body carries no id to decode the reply under")?;
        decode_access_response(value, id).map_err(|err| format!("{err:?}"))
    }

    fn store(&self) -> Arc<InboundStore> {
        self.slot.inbound_store().unwrap()
    }

    fn records(&self) -> Result<Vec<InboundRecord>, String> {
        self.store()
            .list(SystemTime::now())
            .map_err(|err| err.to_string())
    }

    fn record(&self, id: &str) -> Result<InboundRecord, String> {
        self.records()?
            .into_iter()
            .find(|record| record.id == id)
            .ok_or(format!("no record filed under {id}"))
    }

    fn nothing_filed(&self) -> Result<(), String> {
        if self.slot.inbound_store().is_some() {
            ensure(self.records()?.is_empty(), "a record was filed")?;
        }
        ensure(self.idle.count() == 0, "the human was told")?;
        ensure(self.hooks.snapshot().is_empty(), "a hook fired")
    }

    /// A question record from `identity`, as the envoy files one it escalated.
    fn question(&self, id: &str, identity: &str) -> Result<(), String> {
        self.store()
            .upsert(
                InboundRecord {
                    version: INBOUND_RECORD_VERSION,
                    id: id.to_string(),
                    peer_destination: destination(),
                    peer_identity: identity.to_string(),
                    thread: id.to_string(),
                    question: "may I?".to_string(),
                    envoy_question: String::new(),
                    received_at: rfc3339_utc(SystemTime::now()),
                    kind: InboundKind::Question,
                    paths: Vec::new(),
                    reason: String::new(),
                },
                SystemTime::now(),
            )
            .map_err(|err| err.to_string())
    }
}

fn pending() -> Result<AccessOutcome, String> {
    Ok(AccessOutcome::Pending)
}

fn refused(refusal: AccessRefusal) -> Result<AccessOutcome, String> {
    Ok(AccessOutcome::Refused(refusal))
}

/// `message` as a fetch hands it on once the signer is verified.
fn fetched(
    title: Option<Vec<u8>>,
    content: Option<Vec<u8>>,
    fields: Option<Value>,
) -> InboundMessage {
    InboundMessage {
        transient_id: [1u8; 32],
        message_id: [2u8; 32],
        source_identity_hash: identity(),
        source_delivery_hash: hex_lower(&[3u8; 16]),
        timestamp: 1_700_000_000.0,
        title,
        content,
        fields,
        stamp_value: None,
    }
}

fn origin() -> OriginName {
    OriginName([7u8; NAME_HASH_LEN])
}

/// The custom data map of a stored access request, in emission order.
fn custom_data(name_hash: Option<Value>, id: Option<Value>, paths: Option<Value>) -> Value {
    let mut entries = Vec::new();
    if let Some(name_hash) = name_hash {
        entries.push(("name_hash", name_hash));
    }
    if let Some(id) = id {
        entries.push(("id", id));
    }
    if let Some(paths) = paths {
        entries.push(("paths", paths));
    }
    map(entries)
}

fn good_data() -> Value {
    custom_data(
        Some(Value::Binary(origin().0.to_vec())),
        Some(Value::from(SENT_ID)),
        Some(list(&["src/x.rs"])),
    )
}

fn lxmf_fields(kind: Value, data: Value) -> Value {
    Value::Map(vec![
        (Value::from(FIELD_CUSTOM_TYPE), kind),
        (Value::from(FIELD_CUSTOM_DATA), data),
    ])
}

/// A stored access request for `a-1` on `src/x.rs` with `data` as its custom data.
fn stored(data: Value) -> InboundMessage {
    fetched(
        None,
        Some(b"need the struct".to_vec()),
        Some(lxmf_fields(Value::from(ACCESS_TYPE), data)),
    )
}

fn good_request() -> ValidAccess {
    validate_access(SENT_ID, strings(&["src/x.rs"]), "need the struct").unwrap()
}

fn good_access() -> AccessMessage {
    AccessMessage::Access {
        name_hash: origin().0,
        request: good_request(),
    }
}

#[derive(Default)]
struct CountingSink(parking_lot::Mutex<Vec<InboundMessage>>);

impl CountingSink {
    fn count(&self) -> usize {
        self.0.lock().len()
    }
}

impl InboundSink for CountingSink {
    fn deliver(&self, message: InboundMessage) {
        self.0.lock().push(message);
    }
}

/// The destination a stored request signed by `identity()` from `origin()` names.
fn origin_destination() -> String {
    destination_address(
        &origin().0,
        &AddressHash::new_from_hex_string(&identity()).unwrap(),
    )
    .to_hex_string()
}

fn granted(path_count: usize, expires: Option<SystemTime>, standing: bool) -> OutboundPeer {
    decision_reply(
        SENT_ID,
        SENT_ID,
        path_count,
        AccessDecision::Granted,
        expires,
        standing,
        &PartLimits::default(),
    )
    .unwrap()
}

fn denied(path_count: usize, thread: &str) -> OutboundPeer {
    decision_reply(
        SENT_ID,
        thread,
        path_count,
        AccessDecision::Denied,
        None,
        false,
        &PartLimits::default(),
    )
    .unwrap()
}

/// Whether any map in `value`, at any depth, has a key named `key`.
fn has_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Map(entries) => entries
            .iter()
            .any(|(name, inner)| name.as_str() == Some(key) || has_key(inner, key)),
        Value::Array(items) => items.iter().any(|item| has_key(item, key)),
        _ => false,
    }
}

fn keys_of(value: &Value) -> Vec<&str> {
    value
        .as_map()
        .map(|entries| entries.iter().filter_map(|(key, _)| key.as_str()).collect())
        .unwrap_or_default()
}

/// The common shape of every decision reply; the content and the data part are the
/// caller's to check.
fn decision_shape(reply: &OutboundPeer, thread: &str) -> Result<(), String> {
    same("kind", reply.kind, PeerKind::Reply)?;
    same("in_reply_to", reply.in_reply_to.as_deref(), Some(SENT_ID))?;
    same("thread", reply.thread.as_deref(), Some(thread))?;
    same(
        "disposition",
        reply.disposition,
        Some(Disposition::Answered),
    )?;
    same("retry_after", reply.retry_after, None)?;
    same("title", reply.title.as_deref(), None)?;
    same("fields", reply.fields.as_ref(), None)?;
    same("one part", reply.parts.len(), 1)?;
    let body = to_r3_body(reply, 1_700_000_000.0);
    same(
        "the wire keys",
        keys_of(&body),
        vec![
            "v",
            "kind",
            "id",
            "in_reply_to",
            "thread",
            "content",
            "disposition",
            "parts",
            "ts",
        ],
    )?;
    ensure(
        !has_key(&body, "paths"),
        format!("a path travelled: {body}"),
    )?;
    ensure(
        !has_key(&body, "retry_after"),
        format!("retry_after travelled: {body}"),
    )
}

fn data_part(reply: &OutboundPeer) -> Result<serde_json::Value, String> {
    match reply.parts.first() {
        Some(RawPart::Data { data }) => Ok(data.clone()),
        other => Err(format!("not a data part: {other:?}")),
    }
}

/// `reply` as the requester's node sees it once it has crossed the wire.
fn delivered(reply: &OutboundPeer) -> PeerMessage {
    let body = from_r3_body(&to_r3_body(reply, 1_700_000_000.0)).unwrap();
    PeerMessage::new(RawPeerMessage {
        source_identity: identity(),
        source_destination: destination(),
        destination: hex_lower(&[0x11; 16]),
        title: body.title,
        content: body.content,
        fields: body.fields,
        timestamp: body.timestamp.unwrap_or(0.0),
        message_id: body.id,
        in_reply_to: body.in_reply_to,
        kind: body.kind,
        via: PeerVia::Direct,
        thread: body.thread,
        disposition: body.disposition,
        retry_after: body.retry_after,
        parts: body.parts,
        dropped_parts: body.dropped_parts,
    })
}

/// A peer's question as the envoy holds it, with or without a thread of its own.
fn question(id: &str, thread: Option<&str>) -> PeerMessage {
    PeerMessage {
        source_identity: identity(),
        source_destination: destination(),
        destination: hex_lower(&[0x11; 16]),
        title: None,
        content: "may I merge?".to_string(),
        fields: None,
        timestamp: 1_700_000_000.0,
        message_id: id.to_string(),
        in_reply_to: None,
        kind: PeerKind::Ask,
        via: PeerVia::Direct,
        thread: thread.map(str::to_string),
        disposition: None,
        retry_after: None,
        parts: Vec::new(),
        dropped_parts: 0,
    }
}

fn declined(text: &str) -> Result<String, String> {
    match classify_answer(text) {
        EnvoyOutcome::Declined(words) => Ok(words),
        EnvoyOutcome::Answered(words) => Err(format!("{text:?} read as the answer {words:?}")),
        EnvoyOutcome::Failed(why) => Err(format!("{text:?} read as a failure: {why}")),
        _ => Err(format!("{text:?} read as neither an answer nor a decline")),
    }
}

fn answered(text: &str) -> Result<String, String> {
    match classify_answer(text) {
        EnvoyOutcome::Answered(words) => Ok(words),
        EnvoyOutcome::Declined(words) => Err(format!("{text:?} read as the decline {words:?}")),
        EnvoyOutcome::Failed(why) => Err(format!("{text:?} read as a failure: {why}")),
        _ => Err(format!("{text:?} read as neither an answer nor a decline")),
    }
}

fn row(id: &'static str, kind: Kind, case: Case) -> Vector {
    Vector {
        id,
        kind,
        case,
        known_divergence: None,
    }
}

fn refused_body(id: &'static str, kind: Kind, body: Value) -> Vector {
    row(id, kind, Case::AccessRequest(RequestProbe::Refused(body)))
}

fn request_check(id: &'static str, kind: Kind, check: Check) -> Vector {
    row(id, kind, Case::AccessRequest(RequestProbe::Check(check)))
}

fn decode(
    id: &'static str,
    kind: Kind,
    value: Value,
    expect: Result<AccessOutcome, AccessError>,
) -> Vector {
    row(
        id,
        kind,
        Case::AccessReply(ReplyProbe::Decode { value, expect }),
    )
}

fn malformed(id: &'static str, kind: Kind, value: Value, why: &'static str) -> Vector {
    decode(id, kind, value, Err(AccessError::Malformed(why)))
}

fn unknown_status(id: &'static str, kind: Kind, value: Value) -> Vector {
    decode(id, kind, value, Err(AccessError::UnknownStatus))
}

fn reply_check(id: &'static str, kind: Kind, check: Check) -> Vector {
    row(id, kind, Case::AccessReply(ReplyProbe::Check(check)))
}

fn lxmf(id: &'static str, kind: Kind, message: InboundMessage, expect: AccessMessage) -> Vector {
    row(
        id,
        kind,
        Case::AccessLxmf(LxmfProbe::Decode {
            message: Box::new(message),
            expect,
        }),
    )
}

fn lxmf_check(id: &'static str, kind: Kind, check: Check) -> Vector {
    row(id, kind, Case::AccessLxmf(LxmfProbe::Check(check)))
}

// ---------------------------------------------------------------------------------------
// Checks: the request path
// ---------------------------------------------------------------------------------------

fn a_well_formed_request_is_filed_as_pending_with_its_paths_and_reason() -> Result<(), String> {
    let fixture = bare_slot("access-pending");
    same(
        "the answer",
        fixture.outcome(body(SENT_ID, &["src/x.rs", "docs/y.md"], "need the struct")),
        pending(),
    )?;
    let records = fixture.records()?;
    same("records filed", records.len(), 1)?;
    let record = &records[0];
    same("kind", record.kind, InboundKind::Access)?;
    same("id", record.id.as_str(), SENT_ID)?;
    same("thread", record.thread.as_str(), SENT_ID)?;
    same(
        "peer identity",
        record.peer_identity.clone(),
        fixture.peer_hash(),
    )?;
    same(
        "peer destination",
        record.peer_destination.clone(),
        destination(),
    )?;
    same(
        "paths",
        record.paths.clone(),
        strings(&["src/x.rs", "docs/y.md"]),
    )?;
    same("reason", record.reason.as_str(), "need the struct")?;
    ensure(
        record.question.is_empty(),
        "an access record carries no question",
    )?;
    same("the human was told once", fixture.idle.count(), 1)
}

fn a_request_at_the_path_cap_is_filed_and_repeats_collapse_before_the_floor() -> Result<(), String>
{
    let fixture = bare_slot("access-path-cap");
    let sixteen = numbered(ACCESS_MAX_PATHS);
    same(
        "sixteen paths",
        fixture.outcome(body(SENT_ID, &refs(&sixteen), "")),
        pending(),
    )?;
    same(
        "a path repeated is asked for once",
        fixture.outcome(body("a-2", &["src/x.rs", "src/x.rs"], "")),
        pending(),
    )?;
    same("records filed", fixture.records()?.len(), 2)?;
    same(
        "the first keeps every path",
        fixture.record(SENT_ID)?.paths,
        sixteen,
    )?;
    same(
        "the second keeps the one path",
        fixture.record("a-2")?.paths,
        strings(&["src/x.rs"]),
    )
}

fn a_reason_at_the_cap_is_kept_and_an_absent_one_reads_as_empty() -> Result<(), String> {
    let fixture = bare_slot("access-reason-cap");
    let at_cap = "r".repeat(ACCESS_REASON_MAX_CHARS);
    same(
        "a reason of exactly the cap",
        fixture.outcome(body(SENT_ID, &["src/x.rs"], &at_cap)),
        pending(),
    )?;
    let without_reason = map(vec![
        ("v", Value::from(PEER_WIRE_VERSION)),
        ("id", Value::from("a-2")),
        ("paths", list(&["docs/y.md"])),
    ]);
    same("no reason", fixture.outcome(without_reason), pending())?;
    same(
        "a blank reason",
        fixture.outcome(body("a-3", &["docs/z.md"], "   ")),
        pending(),
    )?;
    same("records filed", fixture.records()?.len(), 3)?;
    same(
        "the reason at the cap",
        fixture.record(SENT_ID)?.reason,
        at_cap,
    )?;
    same(
        "the absent reason",
        fixture.record("a-2")?.reason.as_str(),
        "",
    )?;
    same(
        "the blank reason",
        fixture.record("a-3")?.reason.as_str(),
        "",
    )
}

fn an_unknown_request_key_is_ignored() -> Result<(), String> {
    let fixture = bare_slot("access-unknown-key");
    let padded = map(vec![
        ("v", Value::from(PEER_WIRE_VERSION)),
        ("id", Value::from(SENT_ID)),
        ("paths", list(&["src/x.rs"])),
        ("reason", Value::from("why")),
        ("urgency", Value::from("high")),
        ("thread", Value::from("t-9")),
    ]);
    same("the answer", fixture.outcome(padded), pending())?;
    let records = fixture.records()?;
    same("records filed", records.len(), 1)?;
    same("the thread is the id", records[0].thread.as_str(), SENT_ID)
}

fn every_broken_rule_draws_the_one_invalid_data_refusal() -> Result<(), String> {
    let fixture = bare_slot("access-one-refusal");
    let seventeen = numbered(ACCESS_MAX_PATHS + 1);
    let long_reason = "r".repeat(ACCESS_REASON_MAX_CHARS + 1);
    let cases: Vec<(&str, Value)> = vec![
        (
            "missing v",
            map(vec![
                ("id", Value::from(SENT_ID)),
                ("paths", list(&["src/x.rs"])),
            ]),
        ),
        ("non-wire id", body("a 1", &["src/x.rs"], "")),
        ("no paths", body(SENT_ID, &[], "")),
        ("seventeen paths", body(SENT_ID, &refs(&seventeen), "")),
        ("a dot-dot path", body(SENT_ID, &["../x.rs"], "")),
        ("a backslash path", body(SENT_ID, &["src\\x.rs"], "")),
        (
            "a reason past the cap",
            body(SENT_ID, &["src/x.rs"], &long_reason),
        ),
        (
            "paths not a list",
            map(vec![
                ("v", Value::from(PEER_WIRE_VERSION)),
                ("id", Value::from(SENT_ID)),
                ("paths", Value::from("src/x.rs")),
            ]),
        ),
        ("the body not a map", Value::from("src/x.rs")),
    ];
    let mut drawn = Vec::new();
    for (case, body) in cases {
        let reply = fixture.ask(body);
        let code = code_of(&reply).ok_or(format!("{case}: not a refusal code"))?;
        drawn.push((case, code.to_wire()));
    }
    let first = drawn[0].1.clone();
    for (case, wire) in &drawn {
        same(&format!("{case} on the wire"), wire.clone(), first.clone())?;
    }
    same("the one code", first, RefusalCode::InvalidData.to_wire())?;
    fixture.nothing_filed()
}

fn the_rate_rule_fires_in_order_and_only_open_records_count() -> Result<(), String> {
    let fixture = bare_slot("access-rate-rule");
    let other = PrivateIdentity::new_from_rand(OsRng);
    fixture.question("q-1", &hex_lower(&[0x77; 16]))?;
    same(
        "an id open as another peer's question is a duplicate",
        fixture.outcome(body("q-1", &["src/x.rs"], "")),
        refused(AccessRefusal::Duplicate),
    )?;
    same(
        "the first request is filed",
        fixture.outcome(body(SENT_ID, &["src/x.rs", "docs/y.md"], "")),
        pending(),
    )?;
    same(
        "the same id with other paths is a duplicate",
        fixture.outcome(body(SENT_ID, &["src/secrets.rs"], "")),
        refused(AccessRefusal::Duplicate),
    )?;
    same(
        "the same id from another peer is a duplicate",
        fixture.outcome_as(body(SENT_ID, &["src/other.rs"], ""), &other),
        refused(AccessRefusal::Duplicate),
    )?;
    same(
        "the same path set in another order is a duplicate",
        fixture.outcome(body("a-2", &["docs/y.md", "src/x.rs"], "")),
        refused(AccessRefusal::Duplicate),
    )?;
    same(
        "the same path set from another identity is its own request",
        fixture.outcome_as(body("a-3", &["docs/y.md", "src/x.rs"], ""), &other),
        pending(),
    )?;
    same(
        "a subset is its own request",
        fixture.outcome(body("a-4", &["src/x.rs"], "")),
        pending(),
    )?;
    let records = fixture.records()?;
    same(
        "the first paths stand",
        records
            .iter()
            .find(|record| record.id == SENT_ID)
            .map(|record| record.paths.clone()),
        Some(strings(&["src/x.rs", "docs/y.md"])),
    )?;
    same("records filed", records.len(), 4)?;
    same("the human was told per filing", fixture.idle.count(), 3)?;
    same("one hook per filing", fixture.hooks.snapshot().len(), 3)?;

    ensure(
        fixture
            .store()
            .remove("a-4")
            .map_err(|err| err.to_string())?,
        "the record was on file",
    )?;
    same(
        "a decided set can be asked for again",
        fixture.outcome(body("a-5", &["src/x.rs"], "")),
        pending(),
    )
}

fn the_sixth_open_request_from_one_identity_is_too_many_pending() -> Result<(), String> {
    let fixture = bare_slot("access-too-many");
    for n in 0..ACCESS_MAX_PENDING_PER_IDENTITY {
        same(
            &format!("request {n}"),
            fixture.outcome(body(&format!("a-{n}"), &[&format!("src/{n}.rs")], "")),
            pending(),
        )?;
    }
    same(
        "the sixth",
        fixture.outcome(body("a-6", &["src/6.rs"], "")),
        refused(AccessRefusal::TooManyPending),
    )?;
    same(
        "a duplicate id is judged before the count",
        fixture.outcome(body("a-0", &["src/6.rs"], "")),
        refused(AccessRefusal::Duplicate),
    )?;
    let other = PrivateIdentity::new_from_rand(OsRng);
    same(
        "another identity is not counted against it",
        fixture.outcome_as(body("b-1", &["src/6.rs"], ""), &other),
        pending(),
    )?;
    same(
        "records filed",
        fixture.records()?.len(),
        ACCESS_MAX_PENDING_PER_IDENTITY + 1,
    )?;
    same(
        "the human was told per filing",
        fixture.idle.count(),
        ACCESS_MAX_PENDING_PER_IDENTITY + 1,
    )
}

fn a_request_with_no_store_to_file_in_is_too_many_pending_and_leaves_no_trace() -> Result<(), String>
{
    let fixture = storeless_slot("access-no-store");
    same(
        "the answer",
        fixture.outcome(body(SENT_ID, &["src/x.rs"], "why")),
        refused(AccessRefusal::TooManyPending),
    )?;
    fixture.nothing_filed()
}

fn a_request_the_store_cannot_write_is_too_many_pending_and_leaves_no_trace() -> Result<(), String>
{
    let fixture = bare_slot("access-unwritable");
    fs::create_dir_all(fixture.store().path()).map_err(|err| err.to_string())?;
    same(
        "the answer",
        fixture.outcome(body(SENT_ID, &["src/x.rs"], "why")),
        refused(AccessRefusal::TooManyPending),
    )?;
    ensure(fixture.idle.count() == 0, "the human was told")?;
    ensure(fixture.hooks.snapshot().is_empty(), "a hook fired")
}

fn the_requested_hook_carries_peer_id_and_count_and_never_a_path_or_the_reason()
-> Result<(), String> {
    let fixture = bare_slot("access-hook");
    let paths = ["src/x.rs", "docs/secret-plan.md"];
    same(
        "the answer",
        fixture.outcome(body(SENT_ID, &paths, "probe-leak-reason")),
        pending(),
    )?;
    same(
        "a duplicate fires nothing",
        fixture.outcome(body("a-2", &paths, "probe-leak-reason")),
        refused(AccessRefusal::Duplicate),
    )?;
    let fired = fixture.hooks.drain();
    same("fires", fired.len(), 1)?;
    let (event, envs) = &fired[0];
    same("the event", *event, HookEvent::MeshAccessRequested)?;
    same(
        "COYOTE_MESH_PEER_IDENTITY",
        env_value(envs, "COYOTE_MESH_PEER_IDENTITY"),
        Some(fixture.peer_hash().as_str()),
    )?;
    same(
        "COYOTE_MESH_PEER_DESTINATION",
        env_value(envs, "COYOTE_MESH_PEER_DESTINATION"),
        Some(destination().as_str()),
    )?;
    same(
        "COYOTE_MESH_ACCESS_ID",
        env_value(envs, "COYOTE_MESH_ACCESS_ID"),
        Some(SENT_ID),
    )?;
    same(
        "COYOTE_MESH_PATH_COUNT",
        env_value(envs, "COYOTE_MESH_PATH_COUNT"),
        Some("2"),
    )?;
    same("the env has those four keys", envs.len(), 4)?;
    for (key, value) in envs {
        for path in paths {
            ensure(!value.contains(path), format!("{key} carries a path"))?;
        }
        ensure(
            !value.contains("probe-leak"),
            format!("{key} carries the reason"),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Checks: the reply, the LXMF route, the decision
// ---------------------------------------------------------------------------------------

fn every_outcome_round_trips_through_the_reply_encoder() -> Result<(), String> {
    for outcome in [
        AccessOutcome::Pending,
        AccessOutcome::Granted {
            expires: 1_790_000_900.0,
        },
        AccessOutcome::Refused(AccessRefusal::Duplicate),
        AccessOutcome::Refused(AccessRefusal::TooManyPending),
    ] {
        let reply = access_reply(SENT_ID, &outcome);
        same(
            &format!("the keys of {}", outcome.status()),
            keys_of(&reply),
            match outcome {
                AccessOutcome::Pending => vec!["v", "id", "status"],
                AccessOutcome::Granted { .. } => vec!["v", "id", "status", "expires"],
                AccessOutcome::Refused(_) => vec!["v", "id", "status", "reason"],
            },
        )?;
        ensure(
            !has_key(&reply, "retry_after"),
            "an access reply carries no retry_after",
        )?;
        same(
            &format!("{} read back", outcome.status()),
            decode_access_response(&reply, SENT_ID),
            Ok(outcome),
        )?;
    }
    Ok(())
}

fn a_dispatch_error_body_is_told_apart_before_the_reply_is_read() -> Result<(), String> {
    let no_provider = DispatchError::NoProvider {
        path: ACCESS_PATH.to_string(),
    };
    same(
        "a no_provider map is a dispatch error",
        DispatchError::from_value(&no_provider.to_value()),
        Some(no_provider),
    )?;
    for outcome in [
        AccessOutcome::Pending,
        AccessOutcome::Refused(AccessRefusal::Duplicate),
    ] {
        same(
            "an access reply is not one",
            DispatchError::from_value(&access_reply(SENT_ID, &outcome)),
            None,
        )?;
    }
    Ok(())
}

fn the_sender_leaves_the_title_absent_and_puts_the_reason_as_the_content() -> Result<(), String> {
    let message = access_message(&good_request(), &origin());
    same("title", message.title.clone(), None)?;
    same(
        "content",
        message.content.clone(),
        b"need the struct".to_vec(),
    )?;
    same(
        "the fields",
        message.fields.clone(),
        Some(lxmf_fields(Value::from(ACCESS_TYPE), good_data())),
    )?;
    same(
        "read back",
        decode_access_message(&fetched(None, Some(message.content), message.fields)),
        good_access(),
    )?;
    let silent = validate_access(SENT_ID, strings(&["src/x.rs"]), "").unwrap();
    let message = access_message(&silent, &origin());
    same(
        "an empty reason is empty content",
        message.content.clone(),
        Vec::new(),
    )?;
    match decode_access_message(&fetched(None, Some(Vec::new()), message.fields)) {
        AccessMessage::Access { request, .. } => same("the reason", request.reason.as_str(), ""),
        other => Err(format!("empty content read as {other:?}")),
    }
}

fn the_content_is_read_as_lossy_utf8_and_held_to_the_reason_cap() -> Result<(), String> {
    let at_cap = "r".repeat(ACCESS_REASON_MAX_CHARS).into_bytes();
    match decode_access_message(&fetched(
        None,
        Some(at_cap.clone()),
        Some(lxmf_fields(Value::from(ACCESS_TYPE), good_data())),
    )) {
        AccessMessage::Access { request, .. } => {
            same("a reason at the cap", request.reason.len(), at_cap.len())?;
        }
        other => return Err(format!("a reason at the cap read as {other:?}")),
    }
    let mut past = at_cap;
    past.push(b'r');
    same(
        "one past the cap",
        decode_access_message(&fetched(
            None,
            Some(past),
            Some(lxmf_fields(Value::from(ACCESS_TYPE), good_data())),
        )),
        AccessMessage::Malformed("reason is longer than the cap allows"),
    )?;
    match decode_access_message(&fetched(
        None,
        Some(vec![0xff, b'w', b'h', b'y']),
        Some(lxmf_fields(Value::from(ACCESS_TYPE), good_data())),
    )) {
        AccessMessage::Access { request, .. } => ensure(
            request.reason.contains("why"),
            format!("the readable bytes survive: {:?}", request.reason),
        ),
        other => Err(format!("invalid UTF-8 read as {other:?}")),
    }?;
    match decode_access_message(&fetched(
        None,
        None,
        Some(lxmf_fields(Value::from(ACCESS_TYPE), good_data())),
    )) {
        AccessMessage::Access { request, .. } => same("no content", request.reason.as_str(), ""),
        other => Err(format!("no content read as {other:?}")),
    }
}

fn a_stored_request_is_dropped_untrusted_and_filed_trusted_with_nothing_forwarded()
-> Result<(), String> {
    let message = stored(good_data());
    let inner = CountingSink::default();

    let fixture = bare_slot("access-route-untrusted");
    let (known_only, _known_dir) = TrustList::default()
        .identity(&identity(), false)
        .open("access-route-untrusted-trust");
    let routing = AccessRouting {
        trust: &known_only,
        surface: Some(Arc::clone(&fixture.slot) as Arc<dyn AccessSurface>),
        inner: &inner,
    };
    routing.deliver(message.clone());
    fixture.nothing_filed()?;
    same("nothing forwarded", inner.count(), 0)?;
    routing.deliver(stored(Value::from("src/x.rs")));
    same("a malformed one is dropped", inner.count(), 0)?;
    routing.deliver(fetched(None, Some(b"hi".to_vec()), None));
    same("a plain message goes on", inner.count(), 1)?;

    let fixture = bare_slot("access-route-trusted");
    let (trusting, _trusting_dir) = TrustList::default()
        .destination(&origin_destination(), &identity())
        .open("access-route-trusted-trust");
    let routing = AccessRouting {
        trust: &trusting,
        surface: Some(Arc::clone(&fixture.slot) as Arc<dyn AccessSurface>),
        inner: &inner,
    };
    routing.deliver(message);
    let records = fixture.records()?;
    same("records filed", records.len(), 1)?;
    same("id", records[0].id.as_str(), SENT_ID)?;
    same(
        "peer identity",
        records[0].peer_identity.clone(),
        identity(),
    )?;
    same(
        "peer destination",
        records[0].peer_destination.clone(),
        origin_destination(),
    )?;
    same("paths", records[0].paths.clone(), strings(&["src/x.rs"]))?;
    same("reason", records[0].reason.as_str(), "need the struct")?;
    same("the human was told", fixture.idle.count(), 1)?;
    same("nothing more forwarded", inner.count(), 1)
}

fn a_one_off_grant_is_answered_with_one_data_part_the_expiry_and_no_paths() -> Result<(), String> {
    let reply = granted(2, Some(expires()), false);
    decision_shape(&reply, SENT_ID)?;
    same(
        "the data part",
        data_part(&reply)?,
        json!({ "access": { "status": "granted", "expires": 1_790_000_900.0 } }),
    )?;
    same(
        "content",
        reply.content.as_str(),
        &format!("access granted: 2 paths until {EXPIRES_TEXT}"),
    )?;
    let one = granted(1, Some(expires()), false);
    same(
        "one path",
        one.content.as_str(),
        &format!("access granted: 1 path until {EXPIRES_TEXT}"),
    )
}

fn a_standing_grant_and_an_unconvertible_expiry_carry_no_expires() -> Result<(), String> {
    let standing = granted(1, None, true);
    decision_shape(&standing, SENT_ID)?;
    same(
        "the standing data part",
        data_part(&standing)?,
        json!({ "access": { "status": "granted" } }),
    )?;
    same(
        "standing content",
        standing.content.as_str(),
        "access granted: 1 path, standing",
    )?;
    let bare = granted(2, None, false);
    decision_shape(&bare, SENT_ID)?;
    same(
        "the bare data part",
        data_part(&bare)?,
        json!({ "access": { "status": "granted" } }),
    )?;
    same(
        "bare content",
        bare.content.as_str(),
        "access granted: 2 paths",
    )
}

fn a_denial_is_the_same_shape_with_status_denied_in_the_access_thread() -> Result<(), String> {
    let reply = denied(3, "t-9");
    decision_shape(&reply, "t-9")?;
    same(
        "the data part",
        data_part(&reply)?,
        json!({ "access": { "status": "denied" } }),
    )?;
    same("content", reply.content.as_str(), "access denied: 3 paths")?;
    ensure(
        !has_key(&to_r3_body(&reply, 1_700_000_000.0), "expires"),
        "a denial carries no expires",
    )?;
    same(
        "one path",
        denied(1, SENT_ID).content.as_str(),
        "access denied: 1 path",
    )
}

fn the_requester_collects_the_decision_under_the_access_id() -> Result<(), String> {
    let slot = MeshSlot::default();
    let now = SystemTime::now();
    slot.correlations()
        .open(PendingRecord {
            version: PENDING_RECORD_VERSION,
            id: SENT_ID.to_string(),
            peer_destination: destination(),
            peer_identity: identity(),
            thread: SENT_ID.to_string(),
            question: "access to src/x.rs".to_string(),
            sent_at: rfc3339_utc(now),
            timeout_at: rfc3339_utc(now + Duration::from_secs(60)),
            state: PendingState::Open,
            reply: None,
        })
        .map_err(|err| err.to_string())?;

    slot.deliver_peer(delivered(&granted(1, Some(expires()), false)));

    let answer = slot
        .correlations()
        .take_answer(SENT_ID)
        .ok_or("no answer was filed under the access id")?;
    same(
        "the decision",
        answer.parts.clone(),
        vec![Part::Data {
            data: json!({ "access": { "status": "granted", "expires": 1_790_000_900.0 } }),
        }],
    )?;
    same("kind", answer.kind, PeerKind::Reply)?;
    same(
        "disposition",
        answer.disposition,
        Some(Disposition::Answered),
    )?;
    same("in_reply_to", answer.in_reply_to.as_deref(), Some(SENT_ID))?;
    same("thread", answer.thread.as_deref(), Some(SENT_ID))?;
    same("nothing dropped", answer.dropped_parts, 0)?;
    ensure(
        slot.correlations().take_answer(SENT_ID).is_none(),
        "the answer is taken once",
    )
}

// ---------------------------------------------------------------------------------------
// Checks: the envoy's outcome wording
// ---------------------------------------------------------------------------------------

fn an_answer_is_answered_and_a_mid_sentence_marker_stays_one() -> Result<(), String> {
    same(
        "plain words",
        answered("yes, merge it")?,
        "yes, merge it".to_string(),
    )?;
    same(
        "the marker mid-sentence",
        answered("I REFUSED: x")?,
        "I REFUSED: x".to_string(),
    )?;
    same(
        "the human's decision goes out as answered",
        granted(1, None, true).disposition,
        Some(Disposition::Answered),
    )
}

fn the_escalated_notice_names_the_question_in_its_thread() -> Result<(), String> {
    let notice = escalated_notice(&question("q-1", Some("t-1"))).map_err(|err| err.to_string())?;
    same("kind", notice.kind, PeerKind::Reply)?;
    same("in_reply_to", notice.in_reply_to.as_deref(), Some("q-1"))?;
    same("thread", notice.thread.as_deref(), Some("t-1"))?;
    same(
        "disposition",
        notice.disposition,
        Some(Disposition::Escalated),
    )?;
    same("retry_after", notice.retry_after, None)?;
    same(
        "content",
        notice.content.as_str(),
        "a human has been asked; the answer will follow (ref q-1)",
    )?;
    same("title", notice.title.as_deref(), None)?;
    same("fields", notice.fields.as_ref(), None)?;
    ensure(notice.parts.is_empty(), "the notice carries no part")?;
    let rooted = escalated_notice(&question("q-2", None)).map_err(|err| err.to_string())?;
    same(
        "a question without a thread is its own",
        rooted.thread.as_deref(),
        Some("q-2"),
    )?;
    same(
        "content names that id",
        rooted.content.as_str(),
        "a human has been asked; the answer will follow (ref q-2)",
    )
}

fn a_leading_marker_is_a_decline_with_the_words_after_it() -> Result<(), String> {
    same(
        "words after the marker",
        declined("REFUSED: ask via /access")?,
        "ask via /access".to_string(),
    )?;
    same(
        "no space after the marker",
        declined("REFUSED:ask via /access")?,
        "ask via /access".to_string(),
    )?;
    same(
        "an invisible character ahead of the marker is cleaned first",
        declined("\u{200B}REFUSED: x")?,
        "x".to_string(),
    )?;
    same(
        "leading whitespace is cleaned first",
        declined("  REFUSED: x")?,
        "x".to_string(),
    )
}

fn a_bare_marker_declines_with_the_fallback_text() -> Result<(), String> {
    same(
        "the fallback",
        DECLINED_FALLBACK_TEXT,
        "this node will not handle that request",
    )?;
    same(
        "the bare marker",
        declined("REFUSED:")?,
        DECLINED_FALLBACK_TEXT.to_string(),
    )?;
    same(
        "the marker and spaces",
        declined("REFUSED:   ")?,
        DECLINED_FALLBACK_TEXT.to_string(),
    )?;
    ensure(
        matches!(classify_answer(""), EnvoyOutcome::Failed(_)),
        "blank text is a failure, not a decline",
    )
}

fn a_decline_and_every_answerless_run_go_out_refused_with_no_retry_hint() -> Result<(), String> {
    let message = question("q-1", Some("t-1"));
    let sent = |outcome: &EnvoyOutcome, human: Option<&str>| {
        envoy_reply(outcome, human, "the words".to_string(), &message)
            .map_err(|err| err.to_string())
    };
    for (label, outcome) in [
        ("declined", EnvoyOutcome::Declined("no".to_string())),
        ("timed out", EnvoyOutcome::TimedOut),
        ("interrupted", EnvoyOutcome::Interrupted),
        (
            "unavailable",
            EnvoyOutcome::Unavailable(UnavailableReason::NoSource),
        ),
        ("failed", EnvoyOutcome::Failed("boom".to_string())),
    ] {
        let out = sent(&outcome, None)?;
        same(&format!("{label}: kind"), out.kind, PeerKind::Reply)?;
        same(
            &format!("{label}: disposition"),
            out.disposition,
            Some(Disposition::Refused),
        )?;
        same(&format!("{label}: retry_after"), out.retry_after, None)?;
        same(
            &format!("{label}: in_reply_to"),
            out.in_reply_to.as_deref(),
            Some(message.message_id.as_str()),
        )?;
        same(
            &format!("{label}: thread"),
            out.thread.as_deref(),
            Some(message.thread()),
        )?;
        ensure(out.parts.is_empty(), format!("{label} carried a part"))?;
        same(
            &format!("{label}: content"),
            out.content.as_str(),
            "the words",
        )?;
    }
    let answered = sent(&EnvoyOutcome::Answered("x".to_string()), None)?;
    same(
        "an answer is answered",
        answered.disposition,
        Some(Disposition::Answered),
    )?;
    same("with no retry hint", answered.retry_after, None)?;
    let human = sent(
        &EnvoyOutcome::Declined("no".to_string()),
        Some("human words"),
    )?;
    same(
        "the human's answer is answered",
        human.disposition,
        Some(Disposition::Answered),
    )?;
    same(
        "in the human's words",
        human.content.as_str(),
        "human words",
    )
}

// ---------------------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------------------

fn vectors() -> Vec<Vector> {
    let mut rows = Vec::new();
    rows.extend(access_request_rows());
    rows.extend(access_reply_rows());
    rows.extend(access_lxmf_rows());
    rows.extend(decision_rows());
    rows.extend(disposition_rows());
    rows
}

fn access_request_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    let seventeen = numbered(ACCESS_MAX_PATHS + 1);
    let mut padded = numbered(ACCESS_MAX_PATHS - 1);
    padded.push("src/0.rs".to_string());
    padded.push("src/1.rs".to_string());
    vec![
        refused_body(
            "MESH-ACCESS-001",
            Invalid,
            map(vec![
                ("id", Value::from(SENT_ID)),
                ("paths", list(&["src/x.rs"])),
            ]),
        ),
        refused_body(
            "MESH-ACCESS-001",
            Invalid,
            map(vec![
                ("v", Value::from(2u64)),
                ("id", Value::from(SENT_ID)),
                ("paths", list(&["src/x.rs"])),
            ]),
        ),
        refused_body(
            "MESH-ACCESS-001",
            Invalid,
            map(vec![
                ("v", Value::from("1")),
                ("id", Value::from(SENT_ID)),
                ("paths", list(&["src/x.rs"])),
            ]),
        ),
        refused_body("MESH-ACCESS-001", Invalid, Value::from("src/x.rs")),
        refused_body("MESH-ACCESS-001", Invalid, Value::Array(vec![])),
        request_check(
            "MESH-ACCESS-001",
            Valid,
            a_well_formed_request_is_filed_as_pending_with_its_paths_and_reason,
        ),
        refused_body(
            "MESH-ACCESS-002",
            Invalid,
            map(vec![
                ("v", Value::from(PEER_WIRE_VERSION)),
                ("paths", list(&["src/x.rs"])),
            ]),
        ),
        refused_body(
            "MESH-ACCESS-002",
            Invalid,
            map(vec![
                ("v", Value::from(PEER_WIRE_VERSION)),
                ("id", Value::from(7u64)),
                ("paths", list(&["src/x.rs"])),
            ]),
        ),
        refused_body("MESH-ACCESS-002", Invalid, body("a 1", &["src/x.rs"], "")),
        refused_body("MESH-ACCESS-002", Invalid, body("", &["src/x.rs"], "")),
        refused_body(
            "MESH-ACCESS-003",
            Invalid,
            map(vec![
                ("v", Value::from(PEER_WIRE_VERSION)),
                ("id", Value::from(SENT_ID)),
            ]),
        ),
        refused_body(
            "MESH-ACCESS-003",
            Invalid,
            map(vec![
                ("v", Value::from(PEER_WIRE_VERSION)),
                ("id", Value::from(SENT_ID)),
                ("paths", Value::from("src/x.rs")),
            ]),
        ),
        refused_body(
            "MESH-ACCESS-003",
            Invalid,
            map(vec![
                ("v", Value::from(PEER_WIRE_VERSION)),
                ("id", Value::from(SENT_ID)),
                ("paths", Value::Array(vec![Value::from(1u64)])),
            ]),
        ),
        refused_body("MESH-ACCESS-003", Invalid, body(SENT_ID, &["../x.rs"], "")),
        refused_body(
            "MESH-ACCESS-003",
            Invalid,
            body(SENT_ID, &["src\\x.rs"], ""),
        ),
        refused_body(
            "MESH-ACCESS-003",
            Invalid,
            body(SENT_ID, &["/etc/passwd"], ""),
        ),
        refused_body(
            "MESH-ACCESS-003",
            Boundary,
            body(SENT_ID, &refs(&seventeen), ""),
        ),
        refused_body(
            "MESH-ACCESS-003",
            Boundary,
            body(SENT_ID, &refs(&padded), ""),
        ),
        refused_body("MESH-ACCESS-003", Invalid, body(SENT_ID, &[], "")),
        request_check(
            "MESH-ACCESS-003",
            Boundary,
            a_request_at_the_path_cap_is_filed_and_repeats_collapse_before_the_floor,
        ),
        refused_body(
            "MESH-ACCESS-004",
            Invalid,
            map(vec![
                ("v", Value::from(PEER_WIRE_VERSION)),
                ("id", Value::from(SENT_ID)),
                ("paths", list(&["src/x.rs"])),
                ("reason", Value::from(7u64)),
            ]),
        ),
        refused_body(
            "MESH-ACCESS-004",
            Boundary,
            body(
                SENT_ID,
                &["src/x.rs"],
                &"r".repeat(ACCESS_REASON_MAX_CHARS + 1),
            ),
        ),
        request_check(
            "MESH-ACCESS-004",
            Boundary,
            a_reason_at_the_cap_is_kept_and_an_absent_one_reads_as_empty,
        ),
        request_check("MESH-ACCESS-005", Valid, an_unknown_request_key_is_ignored),
        request_check(
            "MESH-ACCESS-006",
            Invalid,
            every_broken_rule_draws_the_one_invalid_data_refusal,
        ),
        request_check(
            "MESH-ACCESS-016",
            Valid,
            the_rate_rule_fires_in_order_and_only_open_records_count,
        ),
        request_check(
            "MESH-ACCESS-016",
            Boundary,
            the_sixth_open_request_from_one_identity_is_too_many_pending,
        ),
        request_check(
            "MESH-ACCESS-017",
            Invalid,
            a_request_with_no_store_to_file_in_is_too_many_pending_and_leaves_no_trace,
        ),
        request_check(
            "MESH-ACCESS-017",
            Invalid,
            a_request_the_store_cannot_write_is_too_many_pending_and_leaves_no_trace,
        ),
        request_check(
            "MESH-ACCESS-029",
            Valid,
            the_requested_hook_carries_peer_id_and_count_and_never_a_path_or_the_reason,
        ),
    ]
}

fn access_reply_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    vec![
        decode(
            "MESH-ACCESS-007",
            Valid,
            reply_with(vec![("status", Value::from("pending"))]),
            Ok(AccessOutcome::Pending),
        ),
        malformed(
            "MESH-ACCESS-007",
            Invalid,
            Value::from("pending"),
            "the body is not a map",
        ),
        malformed(
            "MESH-ACCESS-007",
            Invalid,
            map(vec![
                ("id", Value::from(SENT_ID)),
                ("status", Value::from("pending")),
            ]),
            "v is missing or not the supported version",
        ),
        malformed(
            "MESH-ACCESS-007",
            Invalid,
            map(vec![
                ("v", Value::from(2u64)),
                ("id", Value::from(SENT_ID)),
                ("status", Value::from("pending")),
            ]),
            "v is missing or not the supported version",
        ),
        malformed(
            "MESH-ACCESS-007",
            Invalid,
            map(vec![
                ("v", Value::from("1")),
                ("id", Value::from(SENT_ID)),
                ("status", Value::from("pending")),
            ]),
            "v is missing or not the supported version",
        ),
        reply_check(
            "MESH-ACCESS-007",
            Valid,
            a_dispatch_error_body_is_told_apart_before_the_reply_is_read,
        ),
        malformed(
            "MESH-ACCESS-008",
            Invalid,
            map(vec![
                ("v", Value::from(PEER_WIRE_VERSION)),
                ("status", Value::from("pending")),
            ]),
            "id is missing or not the one sent",
        ),
        malformed(
            "MESH-ACCESS-008",
            Invalid,
            map(vec![
                ("v", Value::from(PEER_WIRE_VERSION)),
                ("id", Value::from("a-2")),
                ("status", Value::from("pending")),
            ]),
            "id is missing or not the one sent",
        ),
        malformed(
            "MESH-ACCESS-008",
            Invalid,
            map(vec![
                ("v", Value::from(PEER_WIRE_VERSION)),
                ("id", Value::from(1u64)),
                ("status", Value::from("pending")),
            ]),
            "id is missing or not the one sent",
        ),
        malformed(
            "MESH-ACCESS-009",
            Invalid,
            reply_with(vec![]),
            "status is missing or not text",
        ),
        malformed(
            "MESH-ACCESS-009",
            Invalid,
            reply_with(vec![("status", Value::from(2u64))]),
            "status is missing or not text",
        ),
        malformed(
            "MESH-ACCESS-009",
            Invalid,
            reply_with(vec![("status", Value::Nil)]),
            "status is missing or not text",
        ),
        unknown_status(
            "MESH-ACCESS-010",
            Invalid,
            reply_with(vec![("status", Value::from("later"))]),
        ),
        unknown_status(
            "MESH-ACCESS-010",
            Invalid,
            reply_with(vec![("status", Value::from("GRANTED"))]),
        ),
        unknown_status(
            "MESH-ACCESS-010",
            Invalid,
            reply_with(vec![
                ("status", Value::from("denied")),
                ("expires", Value::from("soon")),
            ]),
        ),
        unknown_status(
            "MESH-ACCESS-010",
            Invalid,
            reply_with(vec![("status", Value::from(""))]),
        ),
        decode(
            "MESH-ACCESS-011",
            Valid,
            reply_with(vec![
                ("status", Value::from("granted")),
                ("expires", Value::from(1_790_000_900.0_f64)),
            ]),
            Ok(AccessOutcome::Granted {
                expires: 1_790_000_900.0,
            }),
        ),
        malformed(
            "MESH-ACCESS-011",
            Invalid,
            reply_with(vec![("status", Value::from("granted"))]),
            "expires is missing or not a number",
        ),
        malformed(
            "MESH-ACCESS-011",
            Invalid,
            reply_with(vec![
                ("status", Value::from("granted")),
                ("expires", Value::from("soon")),
            ]),
            "expires is missing or not a number",
        ),
        malformed(
            "MESH-ACCESS-011",
            Invalid,
            reply_with(vec![
                ("status", Value::from("granted")),
                ("expires", Value::Nil),
            ]),
            "expires is missing or not a number",
        ),
        decode(
            "MESH-ACCESS-012",
            Valid,
            reply_with(vec![
                ("status", Value::from("refused")),
                ("reason", Value::from("duplicate")),
            ]),
            Ok(AccessOutcome::Refused(AccessRefusal::Duplicate)),
        ),
        decode(
            "MESH-ACCESS-012",
            Valid,
            reply_with(vec![
                ("status", Value::from("refused")),
                ("reason", Value::from("too_many_pending")),
            ]),
            Ok(AccessOutcome::Refused(AccessRefusal::TooManyPending)),
        ),
        unknown_status(
            "MESH-ACCESS-012",
            Invalid,
            reply_with(vec![("status", Value::from("refused"))]),
        ),
        unknown_status(
            "MESH-ACCESS-012",
            Invalid,
            reply_with(vec![
                ("status", Value::from("refused")),
                ("reason", Value::from("busy")),
            ]),
        ),
        unknown_status(
            "MESH-ACCESS-012",
            Invalid,
            reply_with(vec![
                ("status", Value::from("refused")),
                ("reason", Value::from(1u64)),
            ]),
        ),
        decode(
            "MESH-ACCESS-013",
            Valid,
            reply_with(vec![
                ("status", Value::from("pending")),
                ("eta", Value::from(90u64)),
                ("note", Value::from("the human is away")),
            ]),
            Ok(AccessOutcome::Pending),
        ),
        decode(
            "MESH-ACCESS-013",
            Boundary,
            reply_with(vec![
                ("status", Value::from("pending")),
                ("expires", Value::from(1_790_000_900.0_f64)),
                ("reason", Value::from("duplicate")),
            ]),
            Ok(AccessOutcome::Pending),
        ),
        decode(
            "MESH-ACCESS-013",
            Boundary,
            reply_with(vec![
                ("status", Value::from("refused")),
                ("reason", Value::from("duplicate")),
                ("expires", Value::from(1_790_000_900.0_f64)),
            ]),
            Ok(AccessOutcome::Refused(AccessRefusal::Duplicate)),
        ),
        decode(
            "MESH-ACCESS-013",
            Boundary,
            reply_with(vec![
                ("status", Value::from("granted")),
                ("expires", Value::from(1_790_000_900.0_f64)),
                ("reason", Value::from("busy")),
            ]),
            Ok(AccessOutcome::Granted {
                expires: 1_790_000_900.0,
            }),
        ),
        reply_check(
            "MESH-ACCESS-013",
            Valid,
            every_outcome_round_trips_through_the_reply_encoder,
        ),
    ]
}

fn access_lxmf_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    let name_hash = || Value::Binary(origin().0.to_vec());
    let id = || Value::from(SENT_ID);
    let sixteen = numbered(ACCESS_MAX_PATHS);
    let seventeen = numbered(ACCESS_MAX_PATHS + 1);
    vec![
        lxmf("MESH-ACCESS-020", Valid, stored(good_data()), good_access()),
        lxmf(
            "MESH-ACCESS-020",
            Valid,
            fetched(
                None,
                Some(b"need the struct".to_vec()),
                Some(lxmf_fields(
                    Value::Binary(ACCESS_TYPE.as_bytes().to_vec()),
                    good_data(),
                )),
            ),
            good_access(),
        ),
        lxmf(
            "MESH-ACCESS-020",
            Invalid,
            fetched(None, Some(b"hi".to_vec()), None),
            AccessMessage::NotAnAccess,
        ),
        lxmf(
            "MESH-ACCESS-020",
            Invalid,
            fetched(
                None,
                Some(b"hi".to_vec()),
                Some(Value::Map(vec![(
                    Value::from(FIELD_CUSTOM_DATA),
                    good_data(),
                )])),
            ),
            AccessMessage::NotAnAccess,
        ),
        lxmf(
            "MESH-ACCESS-020",
            Invalid,
            fetched(
                None,
                Some(b"hi".to_vec()),
                Some(lxmf_fields(Value::from("scope.knock/1"), good_data())),
            ),
            AccessMessage::NotAnAccess,
        ),
        lxmf(
            "MESH-ACCESS-020",
            Boundary,
            fetched(
                None,
                Some(b"hi".to_vec()),
                Some(lxmf_fields(Value::from("scope.access/2"), good_data())),
            ),
            AccessMessage::NotAnAccess,
        ),
        lxmf(
            "MESH-ACCESS-020",
            Invalid,
            fetched(
                None,
                Some(b"hi".to_vec()),
                Some(lxmf_fields(Value::from(1u64), good_data())),
            ),
            AccessMessage::NotAnAccess,
        ),
        lxmf(
            "MESH-ACCESS-021",
            Invalid,
            fetched(
                None,
                Some(b"hi".to_vec()),
                Some(Value::Map(vec![(
                    Value::from(FIELD_CUSTOM_TYPE),
                    Value::from(ACCESS_TYPE),
                )])),
            ),
            AccessMessage::Malformed("custom data is missing or not a map"),
        ),
        lxmf(
            "MESH-ACCESS-021",
            Invalid,
            stored(Value::from("src/x.rs")),
            AccessMessage::Malformed("custom data is missing or not a map"),
        ),
        lxmf(
            "MESH-ACCESS-021",
            Invalid,
            stored(Value::Array(vec![good_data()])),
            AccessMessage::Malformed("custom data is missing or not a map"),
        ),
        lxmf(
            "MESH-ACCESS-022",
            Valid,
            fetched(
                None,
                Some(b"need the struct".to_vec()),
                Some(Value::Map(vec![
                    (Value::from(FIELD_CUSTOM_TYPE), Value::from(ACCESS_TYPE)),
                    (Value::from(0xf0u8), Value::from("later")),
                    (Value::from(FIELD_CUSTOM_DATA), good_data()),
                    (Value::from(0x01u8), Value::from(7u64)),
                ])),
            ),
            good_access(),
        ),
        lxmf(
            "MESH-ACCESS-023",
            Invalid,
            stored(custom_data(None, Some(id()), Some(list(&["src/x.rs"])))),
            AccessMessage::Malformed("name_hash is missing or not binary"),
        ),
        lxmf(
            "MESH-ACCESS-023",
            Invalid,
            stored(custom_data(
                Some(Value::from(hex_lower(&origin().0))),
                Some(id()),
                Some(list(&["src/x.rs"])),
            )),
            AccessMessage::Malformed("name_hash is missing or not binary"),
        ),
        lxmf(
            "MESH-ACCESS-023",
            Boundary,
            stored(custom_data(
                Some(Value::Binary(vec![7u8; NAME_HASH_LEN - 1])),
                Some(id()),
                Some(list(&["src/x.rs"])),
            )),
            AccessMessage::Malformed("name_hash is not 10 bytes"),
        ),
        lxmf(
            "MESH-ACCESS-023",
            Boundary,
            stored(custom_data(
                Some(Value::Binary(vec![7u8; NAME_HASH_LEN + 1])),
                Some(id()),
                Some(list(&["src/x.rs"])),
            )),
            AccessMessage::Malformed("name_hash is not 10 bytes"),
        ),
        lxmf(
            "MESH-ACCESS-024",
            Invalid,
            stored(custom_data(
                Some(name_hash()),
                None,
                Some(list(&["src/x.rs"])),
            )),
            AccessMessage::Malformed("id is missing or not text"),
        ),
        lxmf(
            "MESH-ACCESS-024",
            Invalid,
            stored(custom_data(
                Some(name_hash()),
                Some(Value::from(7u64)),
                Some(list(&["src/x.rs"])),
            )),
            AccessMessage::Malformed("id is missing or not text"),
        ),
        lxmf(
            "MESH-ACCESS-024",
            Invalid,
            stored(custom_data(
                Some(name_hash()),
                Some(Value::from("a 1")),
                Some(list(&["src/x.rs"])),
            )),
            AccessMessage::Malformed("id is not a wire id"),
        ),
        lxmf(
            "MESH-ACCESS-025",
            Invalid,
            stored(custom_data(Some(name_hash()), Some(id()), None)),
            AccessMessage::Malformed("paths is missing or not a list"),
        ),
        lxmf(
            "MESH-ACCESS-025",
            Invalid,
            stored(custom_data(
                Some(name_hash()),
                Some(id()),
                Some(Value::from("src/x.rs")),
            )),
            AccessMessage::Malformed("paths is missing or not a list"),
        ),
        lxmf(
            "MESH-ACCESS-025",
            Invalid,
            stored(custom_data(
                Some(name_hash()),
                Some(id()),
                Some(Value::Array(vec![Value::from(1u64)])),
            )),
            AccessMessage::Malformed("a path is not text"),
        ),
        lxmf(
            "MESH-ACCESS-025",
            Invalid,
            stored(custom_data(
                Some(name_hash()),
                Some(id()),
                Some(list(&["../x.rs"])),
            )),
            AccessMessage::Malformed("a path is not a wire path"),
        ),
        lxmf(
            "MESH-ACCESS-025",
            Boundary,
            stored(custom_data(
                Some(name_hash()),
                Some(id()),
                Some(list(&refs(&seventeen))),
            )),
            AccessMessage::Malformed("paths names more than the cap allows"),
        ),
        lxmf(
            "MESH-ACCESS-025",
            Boundary,
            stored(custom_data(
                Some(name_hash()),
                Some(id()),
                Some(list(&refs(&sixteen))),
            )),
            AccessMessage::Access {
                name_hash: origin().0,
                request: validate_access(SENT_ID, sixteen.clone(), "need the struct").unwrap(),
            },
        ),
        lxmf(
            "MESH-ACCESS-025",
            Invalid,
            stored(custom_data(Some(name_hash()), Some(id()), Some(list(&[])))),
            AccessMessage::Malformed("paths is empty"),
        ),
        lxmf(
            "MESH-ACCESS-026",
            Valid,
            stored(map(vec![
                ("name_hash", name_hash()),
                ("id", id()),
                ("paths", list(&["src/x.rs"])),
                ("reason", Value::from("ignored")),
                ("thread", Value::from("t-9")),
            ])),
            good_access(),
        ),
        lxmf_check(
            "MESH-ACCESS-027",
            Valid,
            the_sender_leaves_the_title_absent_and_puts_the_reason_as_the_content,
        ),
        lxmf_check(
            "MESH-ACCESS-027",
            Boundary,
            the_content_is_read_as_lossy_utf8_and_held_to_the_reason_cap,
        ),
        lxmf_check(
            "MESH-ACCESS-028",
            Invalid,
            a_stored_request_is_dropped_untrusted_and_filed_trusted_with_nothing_forwarded,
        ),
    ]
}

fn decision_rows() -> Vec<Vector> {
    use Kind::{Boundary, Valid};
    vec![
        row(
            "MESH-ACCESS-018",
            Valid,
            Case::Decision(a_one_off_grant_is_answered_with_one_data_part_the_expiry_and_no_paths),
        ),
        row(
            "MESH-ACCESS-018",
            Boundary,
            Case::Decision(a_standing_grant_and_an_unconvertible_expiry_carry_no_expires),
        ),
        row(
            "MESH-ACCESS-018",
            Valid,
            Case::Decision(a_denial_is_the_same_shape_with_status_denied_in_the_access_thread),
        ),
        row(
            "MESH-ACCESS-018",
            Valid,
            Case::Decision(the_requester_collects_the_decision_under_the_access_id),
        ),
    ]
}

fn disposition_rows() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    vec![
        row(
            "MESH-DISP-015",
            Valid,
            Case::Disposition(an_answer_is_answered_and_a_mid_sentence_marker_stays_one),
        ),
        row(
            "MESH-DISP-016",
            Valid,
            Case::Disposition(the_escalated_notice_names_the_question_in_its_thread),
        ),
        row(
            "MESH-DISP-018",
            Invalid,
            Case::Disposition(a_leading_marker_is_a_decline_with_the_words_after_it),
        ),
        row(
            "MESH-DISP-018",
            Boundary,
            Case::Disposition(a_bare_marker_declines_with_the_fallback_text),
        ),
        row(
            "MESH-DISP-018",
            Valid,
            Case::Disposition(a_decline_and_every_answerless_run_go_out_refused_with_no_retry_hint),
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

    const TESTED: [&str; 5] = [
        "AccessRequest",
        "AccessReply",
        "AccessLxmf",
        "Decision",
        "Disposition",
    ];

    #[test]
    fn access_requests_are_answered_as_section_10_16_mandates() {
        run_family("AccessRequest");
    }

    #[test]
    fn access_replies_are_read_as_section_10_16_mandates() {
        run_family("AccessReply");
    }

    #[test]
    fn stored_access_requests_are_read_and_routed_as_section_10_16_mandates() {
        run_family("AccessLxmf");
    }

    #[test]
    fn access_decisions_travel_as_section_10_16_mandates() {
        run_family("Decision");
    }

    #[test]
    fn envoy_outcomes_are_worded_as_section_10_10_mandates() {
        run_family("Disposition");
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
