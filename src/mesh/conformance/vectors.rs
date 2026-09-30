//! Requirement-id keyed vectors run against this crate's codecs: announce application data,
//! propagation node announces, the status card, the message body, the knock body, the LXMF
//! forms of both, the canonical text forms, the destination derivations, the trust verdict
//! and the code-point registry.
//!
//! Every row names the id it exercises and the receiver action the spec
//! mandates for it. A row written faithfully from the spec that the code does not honour is
//! kept as written and flagged with `known_divergence`; the executor prints such a row
//! instead of asserting it, and fails when the flag goes stale. No row is flagged today.

use super::{Kind, Listed};
use crate::config::mesh_config::MeshInterface;
use crate::config::{MeshConfig, Session};
use crate::mesh::announce::{
    ANNOUNCE_MAGIC, AnnounceAppData, HEARTBEAT_SECS, MAX_DISPLAY_NAME_BYTES,
    PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT, REANNOUNCE_FLOOR_SECS, announce_app_data,
};
use crate::mesh::card::{
    ABOUT_MAX_CHARS, BRANCH_MAX_CHARS, CardPlan, CardRepo, CardState, CardTodo,
    DISPLAY_NAME_MAX_CHARS, OBJECTIVE_MAX_CHARS, PLAN_TITLE_MAX_CHARS, REPO_NAME_MAX_CHARS,
    STATE_IDLE, STATE_UNKNOWN, STATE_WORKING, STATUS_CARD_VERSION, StatusCard, StatusError,
    TODO_GOAL_MAX_CHARS,
};
use crate::mesh::identity::{PREDECESSOR_RECORD_VERSION, Predecessor};
use crate::mesh::knock::{
    KNOCK_TYPE, KnockError, KnockIntro, KnockMessage, decode_knock_message, intro_from_r3_body,
    knock_message,
};
use crate::mesh::knocks::{KNOCK_INTRO_MAX_CHARS, KNOCK_RECORD_VERSION, KnockRecord};
use crate::mesh::limits::{PEER_RETRY_AFTER_CAPACITY, PeerRefusal, RefusalReason};
use crate::mesh::message::{
    Disposition, LxmfPeer, OutboundPeer, PEER_CONTENT_MAX_CHARS, PEER_FIELDS_MAX_BYTES,
    PEER_FIELDS_MAX_DEPTH, PEER_ID_MAX_CHARS, PEER_MESSAGE_TYPE, PEER_TITLE_MAX_CHARS,
    PEER_WIRE_VERSION, PeerBody, PeerKind, PeerLxmf, PeerMessage, PeerVia, RawPeerMessage,
    SendError, decode_peer_lxmf, from_r3_body, is_received_reply, is_wire_id, peer_lxmf_message,
    received_reply, to_r3_body,
};
use crate::mesh::peers::{
    PEER_STALE_AFTER, PEER_TABLE_MAX_ENTRIES, PEER_TABLE_VERSION, PEER_TTL, PeerRecord,
    PeerSighting, PeerTable, PeerTableFile,
};
use crate::mesh::pending::{
    INBOUND_RECORD_VERSION, InboundKind, InboundRecord, PENDING_RECORD_VERSION, PendingRecord,
    PendingState,
};
use crate::mesh::propagation::{
    MAX_ACCEPTED_STAMP_COST, OutboundMessage, PropagationError, PropagationNode,
    PropagationNodeError, build_signed_message, lxmf_delivery_hash, pn_announce_slots,
    prepare_envelope,
};
use crate::mesh::propagation_fetch::{
    Discard, InboundMessage, MAX_FETCHED_MESSAGE_BYTES, MIN_FETCHED_MESSAGE_BYTES,
    PROPAGATION_STORE_VERSION, check_bounds,
};
use crate::mesh::propagation_nodes::{PROPAGATION_NODE_TABLE_MAX_ENTRIES, PropagationNodeTable};
use crate::mesh::protocol::{
    Compatibility, MESH_PROTOCOL_MIN_SUPPORTED, MESH_PROTOCOL_VERSION, VersionRefusal,
    protocol_supported,
};
use crate::mesh::r3::{
    DispatchError, KNOCK_PATH, MESSAGE_PATH, NAME_HASH_LEN, OriginName, RefusalCode, STATUS_PATH,
};
use crate::mesh::schema::{Remedy, unversioned_refusal, version_refusal};
use crate::mesh::test_support::{TempDir, TrustList};
use crate::mesh::trust::{
    Decision, LiveMesh, Rule, TRUST_FILE_VERSION, TrustOptions, TrustStore, Verdict,
};
use crate::mesh::{
    canonical_hash, destination_address, display_text, hex_lower, session_destination_name,
};

use lxmf_core::constants::{FIELD_CUSTOM_DATA, FIELD_CUSTOM_TYPE};
use lxmf_core::identity::PrivateIdentity as LxmfIdentity;
use lxmf_core::message::WireMessage;
use lxmf_core::stamp::{PROPAGATION_STAMP_SIZE, validate_propagation_stamp};
use rand_core::OsRng;
use rmpv::Value;
use rns_transport::destination::{DestinationDesc, DestinationName, SingleOutputDestination};
use rns_transport::hash::AddressHash;
use rns_transport::identity::PrivateIdentity;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::fmt::Debug;
use std::future::Future;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

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

/// The input and expectation of a row, one variant per codec under test. The variant name
/// is the family the coverage report groups rows by.
enum Case {
    /// `AnnounceAppData::decode` over raw application data.
    Announce {
        app_data: Vec<u8>,
        expect: AnnounceAction,
    },
    /// `AnnounceAppData::encode`, the sender's side of section 5.1.
    AnnounceEncode {
        version: u16,
        display_name: Option<String>,
        expect: EncodeAction,
    },
    /// `announce_app_data` under a config, the display name policy of section 5.2.
    AnnouncePolicy {
        display_name: Option<&'static str>,
        interfaces: Vec<MeshInterface>,
        on_public: bool,
        expect: Option<&'static str>,
    },
    /// `PropagationNode::from_announce` over app data announced on `lxmf.propagation` or,
    /// with `propagation_name_hash` false, on some other destination.
    PnAnnounce {
        app_data: Vec<u8>,
        propagation_name_hash: bool,
        expect: PnAction,
    },
    /// `StatusCard::from_value`.
    Card { value: Value, expect: CardAction },
    /// `StatusCard::to_value`, compared value for value so key order is pinned.
    CardEncode { card: StatusCard, expect: Value },
    /// `from_r3_body`.
    MessageBody { value: Value, expect: BodyAction },
    /// `to_r3_body`: the emitted key order, no key twice, and a clean round trip.
    MessageBodyEncode {
        message: OutboundPeer,
        timestamp: f64,
        expect_keys: Vec<&'static str>,
    },
    /// `OutboundPeer::new`, the sender's cleaning and minting of section 10.1.
    Outbound {
        kind: PeerKind,
        content: String,
        title: Option<String>,
        in_reply_to: Option<&'static str>,
        fields: Option<serde_json::Value>,
        expect: OutboundAction,
    },
    /// `is_received_reply` against the id the sender used.
    Ack {
        value: Value,
        id: &'static str,
        expect: bool,
    },
    /// `intro_from_r3_body`.
    KnockBody {
        body: Option<Value>,
        expect: Option<String>,
    },
    /// `KnockIntro::new`, the sender's side of section 8.1.
    KnockIntro {
        text: String,
        expect: Result<String, KnockError>,
    },
    /// `decode_knock_message` over a fetched LXMF message.
    LxmfKnock {
        message: InboundMessage,
        expect: KnockMessage,
    },
    /// `decode_peer_lxmf` over a fetched LXMF message.
    LxmfPeer {
        message: InboundMessage,
        expect: PeerLxmf,
    },
    /// `display_text` at a cap.
    Text {
        input: String,
        max_chars: usize,
        expect: Option<String>,
    },
    /// `canonical_hash`.
    HashText {
        input: &'static str,
        expect: Option<&'static str>,
    },
    /// `TrustStore::authorize` over a trust list written to disk.
    Trust {
        list: TrustList,
        identity: &'static str,
        destination: &'static str,
        expect: Verdict,
    },
    /// The hash derivations of section 4, computed from known inputs.
    Derivation(fn() -> Result<(), String>),
    /// The code points of section 13 against the live constants.
    Registry(fn() -> Result<(), String>),
    /// Anything that needs a fixture the typed families do not carry, still keyed by id.
    Custom(fn() -> Result<(), String>),
}

#[derive(Debug, PartialEq)]
enum AnnounceAction {
    Ignored,
    Recorded {
        version: u16,
        display_name: Option<String>,
    },
}

#[derive(Debug, PartialEq)]
enum EncodeAction {
    Bytes(Vec<u8>),
    Refused,
}

#[derive(Debug, PartialEq)]
enum PnAction {
    Filed {
        enabled: bool,
        stamp_cost: u32,
        per_transfer_kb: u64,
    },
    /// `InvalidAnnounce` with the exact reason the spec names.
    Refused(PropagationNodeError),
    /// `InvalidAnnounce` with whatever reason upstream's validator gives.
    Malformed,
}

#[derive(Debug, PartialEq)]
enum CardAction {
    Accepted(Box<StatusCard>),
    /// `Malformed` whose reason contains the text.
    Malformed(&'static str),
    UnsupportedVersion {
        found: u64,
        supported: u64,
    },
}

#[derive(Debug, PartialEq)]
enum BodyAction {
    Accepted(Box<PeerBody>),
    InvalidData(&'static str),
}

#[derive(Debug, PartialEq)]
enum OutboundAction {
    Minted {
        content: String,
        title: Option<String>,
        in_reply_to: Option<String>,
    },
    Refused(SendError),
}

impl Case {
    fn family(&self) -> &'static str {
        match self {
            Self::Announce { .. } => "Announce",
            Self::AnnounceEncode { .. } => "AnnounceEncode",
            Self::AnnouncePolicy { .. } => "AnnouncePolicy",
            Self::PnAnnounce { .. } => "PnAnnounce",
            Self::Card { .. } => "Card",
            Self::CardEncode { .. } => "CardEncode",
            Self::MessageBody { .. } => "MessageBody",
            Self::MessageBodyEncode { .. } => "MessageBodyEncode",
            Self::Outbound { .. } => "Outbound",
            Self::Ack { .. } => "Ack",
            Self::KnockBody { .. } => "KnockBody",
            Self::KnockIntro { .. } => "KnockIntro",
            Self::LxmfKnock { .. } => "LxmfKnock",
            Self::LxmfPeer { .. } => "LxmfPeer",
            Self::Text { .. } => "Text",
            Self::HashText { .. } => "HashText",
            Self::Trust { .. } => "Trust",
            Self::Derivation(_) => "Derivation",
            Self::Registry(_) => "Registry",
            Self::Custom(_) => "Custom",
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
        Case::Announce { app_data, expect } => {
            let observed = match AnnounceAppData::decode(app_data) {
                None => AnnounceAction::Ignored,
                Some(decoded) => AnnounceAction::Recorded {
                    version: decoded.version,
                    display_name: decoded.display_name,
                },
            };
            same("decode", &observed, expect)
        }
        Case::AnnounceEncode {
            version,
            display_name,
            expect,
        } => {
            let observed = match (AnnounceAppData {
                version: *version,
                display_name: display_name.clone(),
            })
            .encode()
            {
                Ok(bytes) => EncodeAction::Bytes(bytes),
                Err(_) => EncodeAction::Refused,
            };
            same("encode", &observed, expect)
        }
        Case::AnnouncePolicy {
            display_name,
            interfaces,
            on_public,
            expect,
        } => {
            let config = MeshConfig {
                display_name: display_name.map(str::to_string),
                interfaces: interfaces.clone(),
                display_name_on_public: *on_public,
                ..MeshConfig::default()
            };
            let bytes = announce_app_data(&config).map_err(|err| err.to_string())?;
            let decoded = AnnounceAppData::decode(&bytes).ok_or("own announce does not decode")?;
            same("version", decoded.version, MESH_PROTOCOL_VERSION)?;
            same("display name", decoded.display_name.as_deref(), *expect)
        }
        Case::PnAnnounce {
            app_data,
            propagation_name_hash,
            expect,
        } => {
            let aspect = if *propagation_name_hash {
                "propagation"
            } else {
                "delivery"
            };
            let observed = match PropagationNode::from_announce(&lxmf_desc(aspect), app_data) {
                Ok(node) => PnAction::Filed {
                    enabled: node.propagation_enabled,
                    stamp_cost: node.stamp_cost,
                    per_transfer_kb: node.per_transfer_limit_kb,
                },
                Err(PropagationNodeError::InvalidAnnounce(_))
                    if matches!(expect, PnAction::Malformed) =>
                {
                    PnAction::Malformed
                }
                Err(err) => PnAction::Refused(err),
            };
            same("from_announce", &observed, expect)
        }
        Case::Card { value, expect } => {
            let observed = match StatusCard::from_value(value) {
                Ok(card) => CardAction::Accepted(Box::new(card)),
                Err(StatusError::Malformed(reason)) => match expect {
                    CardAction::Malformed(text) if reason.contains(text) => {
                        CardAction::Malformed(text)
                    }
                    _ => return Err(format!("Malformed({reason:?}), expected {expect:?}")),
                },
                Err(StatusError::UnsupportedVersion { found, supported }) => {
                    CardAction::UnsupportedVersion { found, supported }
                }
                Err(other) => return Err(format!("{other:?}, expected {expect:?}")),
            };
            same("from_value", &observed, expect)
        }
        Case::CardEncode { card, expect } => {
            let value = card.to_value();
            no_duplicate_keys(&value)?;
            same("to_value", &value, expect)?;
            same(
                "round trip",
                StatusCard::from_value(&value),
                Ok(card.clone()),
            )
        }
        Case::MessageBody { value, expect } => {
            let observed = match from_r3_body(value) {
                Ok(body) => BodyAction::Accepted(Box::new(body)),
                Err(reason) => BodyAction::InvalidData(reason),
            };
            same("from_r3_body", &observed, expect)
        }
        Case::MessageBodyEncode {
            message,
            timestamp,
            expect_keys,
        } => {
            let value = to_r3_body(message, *timestamp);
            no_duplicate_keys(&value)?;
            same("keys", keys_of(&value), expect_keys.clone())?;
            let body = from_r3_body(&value)?;
            same("kind", body.kind, message.kind)?;
            same("id", &body.id, &message.id)?;
            same("in_reply_to", &body.in_reply_to, &message.in_reply_to)?;
            same("title", &body.title, &message.title)?;
            same("content", &body.content, &message.content)?;
            same("fields", &body.fields, &message.fields)?;
            same("ts", body.timestamp, *timestamp)
        }
        Case::Outbound {
            kind,
            content,
            title,
            in_reply_to,
            fields,
            expect,
        } => {
            let observed = match OutboundPeer::new(
                *kind,
                content,
                title.as_deref(),
                *in_reply_to,
                fields.clone(),
            ) {
                Ok(message) => {
                    ensure(
                        is_wire_id(&message.id) && message.id.len() == 32,
                        format!("minted id {:?} is not a 32-character wire id", message.id),
                    )?;
                    ensure(
                        message
                            .id
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                        format!("minted id {:?} is not lowercase hex", message.id),
                    )?;
                    OutboundAction::Minted {
                        content: message.content,
                        title: message.title,
                        in_reply_to: message.in_reply_to,
                    }
                }
                Err(err) => OutboundAction::Refused(err),
            };
            same("OutboundPeer::new", &observed, expect)
        }
        Case::Ack { value, id, expect } => {
            same("is_received_reply", is_received_reply(value, id), *expect)
        }
        Case::KnockBody { body, expect } => same(
            "intro_from_r3_body",
            intro_from_r3_body(body.as_ref()),
            expect.clone(),
        ),
        Case::KnockIntro { text, expect } => same(
            "KnockIntro::new",
            KnockIntro::new(text).map(|intro| intro.as_str().to_string()),
            expect.clone(),
        ),
        Case::LxmfKnock { message, expect } => same(
            "decode_knock_message",
            &decode_knock_message(message),
            expect,
        ),
        Case::LxmfPeer { message, expect } => {
            same("decode_peer_lxmf", &decode_peer_lxmf(message), expect)
        }
        Case::Text {
            input,
            max_chars,
            expect,
        } => same(
            "display_text",
            display_text(input, *max_chars),
            expect.clone(),
        ),
        Case::HashText { input, expect } => {
            same("canonical_hash", canonical_hash(input).as_deref(), *expect)
        }
        Case::Trust {
            list,
            identity,
            destination,
            expect,
        } => {
            let (store, _tmp) = list.open("conformance-trust");
            same("authorize", store.authorize(identity, destination), *expect)
        }
        Case::Derivation(check) | Case::Registry(check) | Case::Custom(check) => check(),
    }
}

fn keys_of(value: &Value) -> Vec<&str> {
    value
        .as_map()
        .map(|entries| entries.iter().filter_map(|(key, _)| key.as_str()).collect())
        .unwrap_or_default()
}

/// MESH-CANON-013 for every emitted map and sub-map.
fn no_duplicate_keys(value: &Value) -> Result<(), String> {
    let Some(entries) = value.as_map() else {
        return Ok(());
    };
    for (index, (key, nested)) in entries.iter().enumerate() {
        ensure(
            !entries[..index].iter().any(|(earlier, _)| earlier == key),
            format!("key {key} is emitted twice"),
        )?;
        ensure(!nested.is_nil(), format!("key {key} is emitted as nil"))?;
        no_duplicate_keys(nested)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------

const SERVED_AT: u64 = 1_700_000_000;
const ORIGIN: [u8; NAME_HASH_LEN] = [7; NAME_HASH_LEN];
const IDENTITY_A: &str = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";
const IDENTITY_B: &str = "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b";
const DESTINATION_A: &str = "d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1";
const DESTINATION_B: &str = "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2";

fn row(id: &'static str, kind: Kind, case: Case) -> Vector {
    Vector {
        id,
        kind,
        case,
        known_divergence: None,
    }
}

fn packed(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, value).unwrap();
    bytes
}

fn unpacked(bytes: &[u8]) -> Value {
    rmpv::decode::read_value(&mut Cursor::new(bytes)).unwrap()
}

fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .map(|(key, value)| (Value::from(key), value))
            .collect(),
    )
}

/// `value` with `key` replaced, or appended when absent.
fn set(value: Value, key: &str, new: Value) -> Value {
    let Value::Map(mut entries) = value else {
        unreachable!("fixtures are maps")
    };
    entries.retain(|(name, _)| name.as_str() != Some(key));
    entries.push((Value::from(key), new));
    Value::Map(entries)
}

fn without(value: Value, key: &str) -> Value {
    let Value::Map(mut entries) = value else {
        unreachable!("fixtures are maps")
    };
    entries.retain(|(name, _)| name.as_str() != Some(key));
    Value::Map(entries)
}

/// `value` with one more `(key, new)` entry, duplicates included.
fn with(value: Value, key: &str, new: Value) -> Value {
    let Value::Map(mut entries) = value else {
        unreachable!("fixtures are maps")
    };
    entries.push((Value::from(key), new));
    Value::Map(entries)
}

fn text(chars: usize) -> String {
    "a".repeat(chars)
}

fn t(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn app(name: &str) -> Vec<u8> {
    let mut bytes = ANNOUNCE_MAGIC.to_vec();
    bytes.extend_from_slice(&[0x00, 0x01]);
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

fn lxmf_desc(aspect: &str) -> DestinationDesc {
    SingleOutputDestination::new(
        *PrivateIdentity::new_from_rand(OsRng).as_identity(),
        DestinationName::new("lxmf", aspect),
    )
    .desc
}

fn slots() -> Vec<Value> {
    pn_announce_slots(true, 20, 128)
}

fn slots_with(index: usize, value: Value) -> Vec<Value> {
    let mut slots = slots();
    slots[index] = value;
    slots
}

fn costs(cost: Value) -> Value {
    Value::Array(vec![cost, Value::from(3), Value::from(18)])
}

fn pn(slots: Vec<Value>) -> Vec<u8> {
    packed(&Value::Array(slots))
}

const REFERENCE_NODE: PnAction = PnAction::Filed {
    enabled: true,
    stamp_cost: 20,
    per_transfer_kb: 128,
};

fn state(code: Value) -> Value {
    map(vec![("code", code)])
}

fn card_value() -> Value {
    map(vec![
        ("v", Value::from(STATUS_CARD_VERSION)),
        ("state", state(Value::from(STATE_IDLE))),
        ("served_at_secs", Value::from(SERVED_AT)),
    ])
}

fn card_sub(sub: &str, entries: Vec<(&str, Value)>) -> Value {
    set(card_value(), sub, map(entries))
}

fn minimal_card() -> StatusCard {
    StatusCard {
        display_name: None,
        objective: None,
        state: CardState {
            code: STATE_IDLE,
            since_secs: None,
        },
        repo: None,
        plan: None,
        todo: None,
        about: None,
        caps: Vec::new(),
        snapshot_age_secs: None,
        served_at_secs: SERVED_AT,
    }
}

fn maximal_card() -> StatusCard {
    StatusCard {
        display_name: Some(text(DISPLAY_NAME_MAX_CHARS)),
        objective: Some(text(OBJECTIVE_MAX_CHARS)),
        state: CardState {
            code: STATE_WORKING,
            since_secs: Some(60),
        },
        repo: Some(CardRepo {
            name: text(REPO_NAME_MAX_CHARS),
            branch: Some(text(BRANCH_MAX_CHARS)),
        }),
        plan: Some(CardPlan {
            title: text(PLAN_TITLE_MAX_CHARS),
        }),
        todo: Some(CardTodo {
            goal: Some(text(TODO_GOAL_MAX_CHARS)),
            done: 3,
            total: 7,
        }),
        about: Some(text(ABOUT_MAX_CHARS)),
        caps: vec!["fetch".to_string()],
        snapshot_age_secs: Some(5),
        served_at_secs: SERVED_AT,
    }
}

fn repo(name: &str, branch: Option<&str>) -> CardRepo {
    CardRepo {
        name: name.to_string(),
        branch: branch.map(str::to_string),
    }
}

fn accepted(card: StatusCard) -> CardAction {
    CardAction::Accepted(Box::new(card))
}

fn body_value() -> Value {
    map(vec![
        ("v", Value::from(PEER_WIRE_VERSION)),
        ("kind", Value::from("message")),
        ("id", Value::from("m-1")),
        ("content", Value::from("hi")),
        ("ts", Value::F64(1.5)),
    ])
}

fn body() -> PeerBody {
    PeerBody {
        kind: PeerKind::Message,
        id: "m-1".to_string(),
        in_reply_to: None,
        title: None,
        content: "hi".to_string(),
        fields: None,
        timestamp: 1.5,
        thread: None,
        disposition: None,
        retry_after: None,
        parts: Vec::new(),
    }
}

fn accepted_body(body: PeerBody) -> BodyAction {
    BodyAction::Accepted(Box::new(body))
}

/// `depth` maps nested in one another, the innermost empty.
fn nested_maps(depth: usize) -> Value {
    (1..depth).fold(Value::Map(vec![]), |inner, _| map(vec![("n", inner)]))
}

fn outbound(kind: PeerKind, content: &str) -> OutboundPeer {
    OutboundPeer::new(kind, content, None, None, None).unwrap()
}

fn lxmf_fields(tag: Option<Value>, data: Option<Value>) -> Value {
    let mut entries = Vec::new();
    if let Some(tag) = tag {
        entries.push((Value::from(FIELD_CUSTOM_TYPE), tag));
    }
    if let Some(data) = data {
        entries.push((Value::from(FIELD_CUSTOM_DATA), data));
    }
    Value::Map(entries)
}

fn knock_fields(data: Value) -> Value {
    lxmf_fields(Some(Value::from(KNOCK_TYPE)), Some(data))
}

fn knock_data() -> Value {
    map(vec![("name_hash", Value::Binary(ORIGIN.to_vec()))])
}

fn peer_fields(data: Value) -> Value {
    lxmf_fields(Some(Value::from(PEER_MESSAGE_TYPE)), Some(data))
}

fn peer_data() -> Value {
    map(vec![
        ("kind", Value::from("message")),
        ("id", Value::from("m-1")),
        ("name_hash", Value::Binary(ORIGIN.to_vec())),
    ])
}

fn inbound(fields: Option<Value>, title: Option<&[u8]>, content: Option<&[u8]>) -> InboundMessage {
    InboundMessage {
        transient_id: [1u8; 32],
        message_id: [2u8; 32],
        source_identity_hash: IDENTITY_A.to_string(),
        source_delivery_hash: DESTINATION_A.to_string(),
        timestamp: 1_700_000_000.0,
        title: title.map(<[u8]>::to_vec),
        content: content.map(<[u8]>::to_vec),
        fields,
        stamp_value: None,
    }
}

fn knock_inbound(fields: Value, content: &[u8]) -> InboundMessage {
    inbound(Some(fields), None, Some(content))
}

fn peer_inbound(fields: Value) -> InboundMessage {
    inbound(
        Some(fields),
        Some(b"ping".as_slice()),
        Some(b"hi".as_slice()),
    )
}

fn knock(intro: Option<&str>) -> KnockMessage {
    KnockMessage::Knock {
        name_hash: ORIGIN,
        intro: intro.map(str::to_string),
    }
}

fn peer(kind: PeerKind, id: &str, in_reply_to: Option<&str>) -> PeerLxmf {
    peer_text(kind, id, in_reply_to, Some("ping"), "hi", None)
}

fn peer_with_fields(fields: serde_json::Value) -> PeerLxmf {
    peer_text(
        PeerKind::Message,
        "m-1",
        None,
        Some("ping"),
        "hi",
        Some(fields),
    )
}

fn peer_text(
    kind: PeerKind,
    id: &str,
    in_reply_to: Option<&str>,
    title: Option<&str>,
    content: &str,
    fields: Option<serde_json::Value>,
) -> PeerLxmf {
    PeerLxmf::Peer(Box::new(LxmfPeer {
        name_hash: ORIGIN,
        kind,
        id: id.to_string(),
        in_reply_to: in_reply_to.map(str::to_string),
        title: title.map(str::to_string),
        content: content.to_string(),
        fields,
        thread: None,
        disposition: (kind == PeerKind::Reply).then_some(Disposition::Answered),
        retry_after: None,
        parts: Vec::new(),
    }))
}

fn verdict(decision: Decision, rule: Rule) -> Verdict {
    Verdict { decision, rule }
}

fn raw(fields: Option<serde_json::Value>, title: Option<&str>, content: &str) -> RawPeerMessage {
    RawPeerMessage {
        source_identity: IDENTITY_A.to_string(),
        source_destination: DESTINATION_A.to_string(),
        destination: DESTINATION_B.to_string(),
        title: title.map(str::to_string),
        content: content.to_string(),
        fields,
        timestamp: 1_700_000_000.0,
        message_id: "m-1".to_string(),
        in_reply_to: None,
        kind: PeerKind::Message,
        via: PeerVia::StoreAndForward,
        thread: None,
        disposition: None,
        retry_after: None,
        parts: Vec::new(),
    }
}

/// A Coyote announce's worth of hashes, derived as the transport derives them.
struct Announced {
    destination_hash: String,
    identity_hash: String,
    name_hash: String,
}

fn announced(instance_id: &str) -> Announced {
    let identity = PrivateIdentity::new_from_rand(OsRng);
    let name = session_destination_name(instance_id);
    let desc = SingleOutputDestination::new(*identity.as_identity(), name).desc;
    Announced {
        destination_hash: desc.address_hash.to_hex_string(),
        identity_hash: desc.identity.address_hash.to_hex_string(),
        name_hash: hex_lower(name.as_name_hash_slice()),
    }
}

fn sighting(peer: &Announced) -> PeerSighting {
    PeerSighting {
        destination_hash: peer.destination_hash.clone(),
        identity_hash: peer.identity_hash.clone(),
        name_hash: peer.name_hash.clone(),
        display_name: Some("Bob".to_string()),
        protocol_version: MESH_PROTOCOL_VERSION,
        hops: 2,
    }
}

fn peer_table(tag: &str) -> (Arc<PeerTable>, TempDir) {
    let tmp = TempDir::new(tag);
    let table = PeerTable::load(tmp.path.join("peers.json"), t(1_000)).unwrap();
    (Arc::new(table), tmp)
}

/// A running mesh as `trust_destination` sees it: the peer table and no knock cache.
struct PeersOnly(Arc<PeerTable>);

impl LiveMesh for PeersOnly {
    fn peers(&self) -> Option<Arc<PeerTable>> {
        Some(self.0.clone())
    }
}

fn trust_via_table(
    sighting: PeerSighting,
    destination: &str,
) -> (anyhow::Result<String>, Vec<String>) {
    let (peers, tmp) = peer_table("conformance-dest");
    peers.observe(sighting, t(2_000));
    let store = TrustStore::open(&tmp.path).unwrap();
    let outcome = store
        .trust_destination(
            &PeersOnly(peers),
            destination,
            TrustOptions::default(),
            t(3_000),
        )
        .map(|outcome| outcome.identity_hash);
    let recorded = store
        .records()
        .into_iter()
        .map(|record| record.hash)
        .collect();
    (outcome, recorded)
}

fn propagation_node(stamp_cost: u32, per_transfer_limit_kb: u64) -> PropagationNode {
    PropagationNode {
        destination: lxmf_desc("propagation"),
        stamp_cost,
        per_transfer_limit_kb,
        propagation_enabled: true,
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

// ---------------------------------------------------------------------------------------
// Section 4: destination naming
// ---------------------------------------------------------------------------------------

fn dest_vectors() -> Vec<Vector> {
    vec![
        row(
            "MESH-DEST-001",
            Kind::Valid,
            Case::Derivation(|| {
                let identity = PrivateIdentity::new_from_rand(OsRng);
                let identity = identity.as_identity();
                let digest = Sha256::new()
                    .chain_update(identity.public_key_bytes())
                    .chain_update(identity.verifying_key_bytes())
                    .finalize();
                same(
                    "identity hash",
                    identity.address_hash.as_slice(),
                    &digest[..16],
                )
            }),
        ),
        row(
            "MESH-DEST-001",
            Kind::Invalid,
            Case::Derivation(|| {
                let identity = PrivateIdentity::new_from_rand(OsRng);
                let identity = identity.as_identity();
                let swapped = Sha256::new()
                    .chain_update(identity.verifying_key_bytes())
                    .chain_update(identity.public_key_bytes())
                    .finalize();
                ensure(
                    identity.address_hash.as_slice() != &swapped[..16],
                    "the keys hashed in the other order reproduce the identity hash",
                )
            }),
        ),
        row(
            "MESH-DEST-002",
            Kind::Valid,
            Case::Derivation(|| {
                let mut session = Session::default();
                let minted = session.ensure_mesh_instance_id().to_string();
                ensure(
                    Session::is_valid_mesh_instance_id(&minted),
                    format!("minted instance id {minted:?} is not 32 lowercase hex digits"),
                )?;
                same(
                    "reused within the lineage",
                    session.ensure_mesh_instance_id(),
                    &minted,
                )?;
                same("length", minted.len(), 32)?;
                same(
                    "uuid text",
                    canonical_hash(&minted).as_deref(),
                    Some(minted.as_str()),
                )
            }),
        ),
        row(
            "MESH-DEST-002",
            Kind::Invalid,
            Case::Derivation(|| {
                for id in [
                    "0123456789ABCDEF0123456789abcdef",
                    "0123456789abcdef0123456789abcde",
                    "0123456789abcdef0123456789abcdef0",
                    "01234567-89ab-cdef-0123-456789abcdef",
                    "",
                ] {
                    ensure(
                        !Session::is_valid_mesh_instance_id(id),
                        format!("{id:?} passes as an instance id"),
                    )?;
                }
                Ok(())
            }),
        ),
        row(
            "MESH-DEST-003",
            Kind::Valid,
            Case::Derivation(|| {
                let instance = "0123456789abcdef0123456789abcdef";
                let peer = announced(instance);
                let name = DestinationName::new("scope", &format!("session.{instance}"));
                same(
                    "name hash",
                    peer.name_hash,
                    hex_lower(name.as_name_hash_slice()),
                )
            }),
        ),
        row(
            "MESH-DEST-003",
            Kind::Invalid,
            Case::Derivation(|| {
                let instance = "0123456789abcdef0123456789abcdef";
                let ours = session_destination_name(instance);
                for (app, aspect) in [
                    ("scope", instance.to_string()),
                    (
                        "scope",
                        format!("session.{}", instance.to_ascii_uppercase()),
                    ),
                    ("lxmf", format!("session.{instance}")),
                    ("scope", format!("session.{instance}.extra")),
                ] {
                    let other = DestinationName::new(app, &aspect);
                    ensure(
                        other.as_name_hash_slice() != ours.as_name_hash_slice(),
                        format!("{app}.{aspect} shares the name hash of scope.session.<id>"),
                    )?;
                }
                Ok(())
            }),
        ),
        row(
            "MESH-DEST-004",
            Kind::Valid,
            Case::Derivation(|| {
                let instance = "0123456789abcdef0123456789abcdef";
                let name = session_destination_name(instance);
                let digest = Sha256::digest(format!("scope.session.{instance}").as_bytes());
                same(
                    "name hash",
                    name.as_name_hash_slice(),
                    &digest[..NAME_HASH_LEN],
                )?;
                same("name hash length", name.as_name_hash_slice().len(), 10)
            }),
        ),
        row(
            "MESH-DEST-005",
            Kind::Valid,
            Case::Derivation(|| {
                let identity = PrivateIdentity::new_from_rand(OsRng);
                let name = session_destination_name("0123456789abcdef0123456789abcdef");
                let upstream = SingleOutputDestination::new(*identity.as_identity(), name).desc;
                let name_hash: [u8; NAME_HASH_LEN] = name.as_name_hash_slice().try_into().unwrap();
                let ours = destination_address(&name_hash, &identity.as_identity().address_hash);
                same("destination_address", ours, upstream.address_hash)?;
                let digest = Sha256::new()
                    .chain_update(name_hash)
                    .chain_update(identity.as_identity().address_hash.as_slice())
                    .finalize();
                same(
                    "trunc_16(H(name_hash || identity_hash))",
                    ours.as_slice(),
                    &digest[..16],
                )
            }),
        ),
        row(
            "MESH-DEST-005",
            Kind::Invalid,
            Case::Derivation(|| {
                let identity = PrivateIdentity::new_from_rand(OsRng);
                let name_hash = [7u8; NAME_HASH_LEN];
                let ours = destination_address(&name_hash, &identity.as_identity().address_hash);
                let swapped = Sha256::new()
                    .chain_update(identity.as_identity().address_hash.as_slice())
                    .chain_update(name_hash)
                    .finalize();
                ensure(
                    ours.as_slice() != &swapped[..16],
                    "identity then name hash reproduces the destination hash",
                )
            }),
        ),
        row(
            "MESH-DEST-006",
            Kind::Valid,
            Case::Custom(|| {
                let peer = announced("alpha");
                let (outcome, recorded) = trust_via_table(sighting(&peer), &peer.destination_hash);
                let identity = outcome.map_err(|err| err.to_string())?;
                same(
                    "identity the formula proves",
                    identity,
                    peer.identity_hash.clone(),
                )?;
                same(
                    "records",
                    recorded,
                    vec![peer.identity_hash.clone(), peer.destination_hash.clone()],
                )
            }),
        ),
        row(
            "MESH-DEST-006",
            Kind::Valid,
            Case::Custom(|| {
                let peer = announced("alpha");
                let (outcome, _) =
                    trust_via_table(sighting(&peer), &peer.destination_hash.to_ascii_uppercase());
                same(
                    "identity for an uppercase destination",
                    outcome.map_err(|err| err.to_string())?,
                    peer.identity_hash.clone(),
                )
            }),
        ),
        row(
            "MESH-DEST-007",
            Kind::Invalid,
            Case::Custom(|| {
                let peer = announced("alpha");
                let forged = [7u8; NAME_HASH_LEN];
                let (outcome, recorded) = trust_via_table(
                    PeerSighting {
                        name_hash: hex_lower(&forged),
                        ..sighting(&peer)
                    },
                    &peer.destination_hash,
                );
                let err = match outcome {
                    Ok(identity) => {
                        return Err(format!("trusted {identity} on a forged name hash"));
                    }
                    Err(err) => err.to_string(),
                };
                let would_announce = destination_address(
                    &forged,
                    &AddressHash::new_from_hex_string(&peer.identity_hash).unwrap(),
                )
                .to_hex_string();
                ensure(
                    err.contains("does not match"),
                    format!("refusal text: {err}"),
                )?;
                ensure(
                    err.contains(&would_announce),
                    format!(
                        "refusal does not name the destination the identity would announce: {err}"
                    ),
                )?;
                same("nothing trusted", recorded, Vec::<String>::new())
            }),
        ),
        row(
            "MESH-DEST-008",
            Kind::Invalid,
            Case::Custom(|| {
                let peer = announced("alpha");
                let stranger = announced("beta");
                let (outcome, recorded) = trust_via_table(
                    PeerSighting {
                        identity_hash: stranger.identity_hash,
                        ..sighting(&peer)
                    },
                    &peer.destination_hash,
                );
                ensure(
                    outcome.is_err(),
                    "another identity was allowed to name this instance",
                )?;
                same("nothing trusted", recorded, Vec::<String>::new())
            }),
        ),
        row(
            "MESH-DEST-008",
            Kind::Valid,
            Case::Trust {
                list: TrustList::default().destination(DESTINATION_A, IDENTITY_A),
                identity: IDENTITY_A,
                destination: DESTINATION_A,
                expect: verdict(Decision::Allow, Rule::DestinationTrusted),
            },
        ),
        row(
            "MESH-DEST-008",
            Kind::Invalid,
            Case::Trust {
                list: TrustList::default()
                    .destination(DESTINATION_A, IDENTITY_A)
                    .identity(IDENTITY_B, false),
                identity: IDENTITY_B,
                destination: DESTINATION_A,
                expect: verdict(Decision::Refuse, Rule::DefaultClosed),
            },
        ),
        row(
            "MESH-DEST-009",
            Kind::Valid,
            Case::Derivation(|| {
                let identity = PrivateIdentity::new_from_rand(OsRng);
                let identity = identity.as_identity();
                let delivery_name: [u8; NAME_HASH_LEN] = Sha256::digest(b"lxmf.delivery")
                    [..NAME_HASH_LEN]
                    .try_into()
                    .unwrap();
                same(
                    "lxmf_delivery_hash",
                    lxmf_delivery_hash(identity),
                    destination_address(&delivery_name, &identity.address_hash),
                )
            }),
        ),
        row(
            "MESH-DEST-009",
            Kind::Valid,
            Case::Custom(|| {
                let sender = LxmfIdentity::new_from_rand(OsRng);
                let recipient = PrivateIdentity::new_from_rand(OsRng);
                let recipient_delivery = lxmf_delivery_hash(recipient.as_identity());
                let sender_delivery = lxmf_delivery_hash(
                    &rns_transport::identity_bridge::to_transport_identity(sender.as_identity()),
                );
                let wire = build_signed_message(
                    &sender,
                    &recipient_delivery,
                    &OutboundMessage {
                        title: None,
                        content: b"hi".to_vec(),
                        fields: None,
                    },
                    1_700_000_000.0,
                )
                .map_err(|err| err.to_string())?;
                same(
                    "destination",
                    &wire.destination[..],
                    recipient_delivery.as_slice(),
                )?;
                same("source", &wire.source[..], sender_delivery.as_slice())
            }),
        ),
        row(
            "MESH-DEST-010",
            Kind::Valid,
            Case::Derivation(|| {
                let name = DestinationName::new("lxmf", "propagation");
                let digest = Sha256::digest(b"lxmf.propagation");
                same(
                    "propagation name hash",
                    name.as_name_hash_slice(),
                    &digest[..NAME_HASH_LEN],
                )
            }),
        ),
        row(
            "MESH-DEST-010",
            Kind::Valid,
            Case::PnAnnounce {
                app_data: pn(slots()),
                propagation_name_hash: true,
                expect: REFERENCE_NODE,
            },
        ),
        row(
            "MESH-DEST-010",
            Kind::Invalid,
            Case::PnAnnounce {
                app_data: pn(slots()),
                propagation_name_hash: false,
                expect: PnAction::Refused(PropagationNodeError::NotAPropagationNode),
            },
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Section 5.1 and 5.2: the Coyote announce
// ---------------------------------------------------------------------------------------

fn announce(id: &'static str, kind: Kind, app_data: Vec<u8>, expect: AnnounceAction) -> Vector {
    row(id, kind, Case::Announce { app_data, expect })
}

fn recorded(version: u16, display_name: Option<&str>) -> AnnounceAction {
    AnnounceAction::Recorded {
        version,
        display_name: display_name.map(str::to_string),
    }
}

fn encode(
    id: &'static str,
    kind: Kind,
    display_name: Option<&str>,
    expect: EncodeAction,
) -> Vector {
    row(
        id,
        kind,
        Case::AnnounceEncode {
            version: MESH_PROTOCOL_VERSION,
            display_name: display_name.map(str::to_string),
            expect,
        },
    )
}

fn public() -> MeshInterface {
    MeshInterface::Public {
        host: "relay.example.com".to_string(),
        port: 4242,
    }
}

fn private() -> MeshInterface {
    MeshInterface::Private {
        host: "relay.internal".to_string(),
        port: 4242,
    }
}

fn policy(
    id: &'static str,
    kind: Kind,
    display_name: Option<&'static str>,
    interfaces: Vec<MeshInterface>,
    on_public: bool,
    expect: Option<&'static str>,
) -> Vector {
    row(
        id,
        kind,
        Case::AnnouncePolicy {
            display_name,
            interfaces,
            on_public,
            expect,
        },
    )
}

fn announce_vectors() -> Vec<Vector> {
    let sixty_four = text(MAX_DISPLAY_NAME_BYTES);
    let sixty_four_multibyte = "é".repeat(MAX_DISPLAY_NAME_BYTES / 2);
    vec![
        announce(
            "MESH-ANN-001",
            Kind::Invalid,
            b"".to_vec(),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-001",
            Kind::Invalid,
            b"SCOPE\x00".to_vec(),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-001",
            Kind::Invalid,
            b"LXMF\x00\x01Alex".to_vec(),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-001",
            Kind::Invalid,
            b"scope\x00\x01Alex".to_vec(),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-001",
            Kind::Invalid,
            b"SCOP".to_vec(),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-001",
            Kind::Boundary,
            b"SCOPE\x00\x01".to_vec(),
            recorded(1, None),
        ),
        announce(
            "MESH-ANN-002",
            Kind::Valid,
            app("Alex"),
            recorded(1, Some("Alex")),
        ),
        announce(
            "MESH-ANN-002",
            Kind::Valid,
            b"SCOPE\x01\x02".to_vec(),
            recorded(0x0102, None),
        ),
        announce(
            "MESH-ANN-002",
            Kind::Boundary,
            b"SCOPE\x00\x00".to_vec(),
            recorded(0, None),
        ),
        announce(
            "MESH-ANN-002",
            Kind::Boundary,
            b"SCOPE\xff\xffAlex".to_vec(),
            recorded(0xffff, Some("Alex")),
        ),
        row(
            "MESH-ANN-002",
            Kind::Valid,
            Case::Custom(|| {
                let (table, _tmp) = peer_table("conformance-ann-002");
                let peer = announced("alpha");
                table.observe(
                    PeerSighting {
                        protocol_version: 2,
                        ..sighting(&peer)
                    },
                    t(1_000),
                );
                let record = table
                    .get(&peer.destination_hash)
                    .ok_or("an out-of-window announce was not recorded")?;
                same("protocol_version", record.protocol_version, 2)?;
                same(
                    "mark",
                    record.compatibility,
                    Compatibility::Incompatible { found: 2 },
                )?;
                same(
                    "Compatibility::of(0)",
                    Compatibility::of(0),
                    Compatibility::Incompatible { found: 0 },
                )?;
                same(
                    "Compatibility::of(1)",
                    Compatibility::of(1),
                    Compatibility::Compatible,
                )
            }),
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app(&text(MAX_DISPLAY_NAME_BYTES + 1)),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app(&format!("{sixty_four_multibyte}a")),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            b"SCOPE\x00\x01\xff\xfe".to_vec(),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            b"SCOPE\x00\x01Al\xc3".to_vec(),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("name\n"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("na\tme"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("\u{1b}[2Jname"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("\u{7f}name"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("Al\u{202E}ex"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("zero\u{200B}width"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("so\u{00AD}ft"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("\u{FEFF}bom"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("tag\u{E0041}"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("line\u{2028}break"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Invalid,
            app("mongol\u{180E}ian"),
            AnnounceAction::Ignored,
        ),
        announce(
            "MESH-ANN-003",
            Kind::Boundary,
            app(&sixty_four),
            recorded(1, Some(&sixty_four)),
        ),
        announce(
            "MESH-ANN-003",
            Kind::Boundary,
            app(&sixty_four_multibyte),
            recorded(1, Some(&sixty_four_multibyte)),
        ),
        announce(
            "MESH-ANN-003",
            Kind::Valid,
            app("Zoë 🌊 ok"),
            recorded(1, Some("Zoë 🌊 ok")),
        ),
        announce(
            "MESH-ANN-004",
            Kind::Valid,
            b"SCOPE\x00\x01".to_vec(),
            recorded(1, None),
        ),
        announce(
            "MESH-ANN-005",
            Kind::Valid,
            app("SCOPE"),
            recorded(1, Some("SCOPE")),
        ),
        announce(
            "MESH-ANN-005",
            Kind::Valid,
            app("1Alex"),
            recorded(1, Some("1Alex")),
        ),
        announce(
            "MESH-ANN-005",
            Kind::Valid,
            app("Alex Smith"),
            recorded(1, Some("Alex Smith")),
        ),
        announce(
            "MESH-ANN-005",
            Kind::Valid,
            app(" "),
            recorded(1, Some(" ")),
        ),
        encode(
            "MESH-ANN-006",
            Kind::Invalid,
            Some(&text(MAX_DISPLAY_NAME_BYTES + 1)),
            EncodeAction::Refused,
        ),
        encode(
            "MESH-ANN-006",
            Kind::Invalid,
            Some(&format!("{sixty_four_multibyte}a")),
            EncodeAction::Refused,
        ),
        encode(
            "MESH-ANN-006",
            Kind::Invalid,
            Some("\u{1b}[2Jname"),
            EncodeAction::Refused,
        ),
        encode(
            "MESH-ANN-006",
            Kind::Invalid,
            Some("Al\u{202E}ex"),
            EncodeAction::Refused,
        ),
        encode(
            "MESH-ANN-006",
            Kind::Invalid,
            Some("zero\u{200B}width"),
            EncodeAction::Refused,
        ),
        encode(
            "MESH-ANN-006",
            Kind::Invalid,
            Some("name\n"),
            EncodeAction::Refused,
        ),
        encode(
            "MESH-ANN-006",
            Kind::Boundary,
            Some(&sixty_four),
            EncodeAction::Bytes(app(&sixty_four)),
        ),
        encode(
            "MESH-ANN-006",
            Kind::Valid,
            Some("Alex"),
            EncodeAction::Bytes(vec![
                0x53, 0x43, 0x4f, 0x50, 0x45, 0x00, 0x01, 0x41, 0x6c, 0x65, 0x78,
            ]),
        ),
        encode(
            "MESH-ANN-006",
            Kind::Valid,
            None,
            EncodeAction::Bytes(vec![0x53, 0x43, 0x4f, 0x50, 0x45, 0x00, 0x01]),
        ),
        encode(
            "MESH-ANN-006",
            Kind::Valid,
            Some("Zoë"),
            EncodeAction::Bytes(app("Zoë")),
        ),
        row(
            "MESH-ANN-006",
            Kind::Valid,
            Case::AnnounceEncode {
                version: 0x0102,
                display_name: None,
                expect: EncodeAction::Bytes(vec![0x53, 0x43, 0x4f, 0x50, 0x45, 0x01, 0x02]),
            },
        ),
        announce(
            "MESH-ANN-007",
            Kind::Valid,
            app("Alex \u{2764}\u{FE0F}"),
            recorded(1, Some("Alex \u{2764}\u{FE0F}")),
        ),
        announce(
            "MESH-ANN-007",
            Kind::Valid,
            app("a\u{FE00}b"),
            recorded(1, Some("a\u{FE00}b")),
        ),
        announce(
            "MESH-ANN-007",
            Kind::Valid,
            app("a\u{E0100}b"),
            recorded(1, Some("a\u{E0100}b")),
        ),
        announce(
            "MESH-ANN-007",
            Kind::Valid,
            app("a\u{E01EF}b"),
            recorded(1, Some("a\u{E01EF}b")),
        ),
        encode(
            "MESH-ANN-007",
            Kind::Valid,
            Some("Alex \u{2764}\u{FE0F}"),
            EncodeAction::Bytes(app("Alex \u{2764}\u{FE0F}")),
        ),
        row(
            "MESH-ANN-007",
            Kind::Valid,
            Case::Text {
                input: "Alex \u{2764}\u{FE0F}".to_string(),
                max_chars: DISPLAY_NAME_MAX_CHARS,
                expect: Some("Alex \u{2764}".to_string()),
            },
        ),
        policy(
            "MESH-ANN-008",
            Kind::Valid,
            None,
            vec![MeshInterface::Lan],
            false,
            None,
        ),
        policy(
            "MESH-ANN-008",
            Kind::Valid,
            Some("Alex"),
            vec![MeshInterface::Lan],
            false,
            Some("Alex"),
        ),
        policy(
            "MESH-ANN-008",
            Kind::Valid,
            None,
            vec![MeshInterface::Lan],
            true,
            None,
        ),
        policy(
            "MESH-ANN-009",
            Kind::Invalid,
            Some("Alex"),
            vec![MeshInterface::Lan, public()],
            false,
            None,
        ),
        policy(
            "MESH-ANN-009",
            Kind::Invalid,
            Some("Alex"),
            vec![public()],
            false,
            None,
        ),
        policy(
            "MESH-ANN-009",
            Kind::Valid,
            Some("Alex"),
            vec![MeshInterface::Lan, public()],
            true,
            Some("Alex"),
        ),
        policy(
            "MESH-ANN-009",
            Kind::Valid,
            Some("Alex"),
            vec![MeshInterface::Lan, private()],
            false,
            Some("Alex"),
        ),
        row(
            "MESH-ANN-010",
            Kind::Valid,
            Case::Custom(|| {
                let (table, _tmp) = peer_table("conformance-ann-010");
                let peer = announced("alpha");
                let decoded = AnnounceAppData::decode(&app("Alex")).unwrap();
                table.observe(
                    PeerSighting {
                        display_name: decoded.display_name.clone(),
                        protocol_version: decoded.version,
                        hops: 3,
                        ..sighting(&peer)
                    },
                    t(1_000),
                );
                let record = table.get(&peer.destination_hash).ok_or("not recorded")?;
                same(
                    "destination",
                    &record.destination_hash,
                    &peer.destination_hash,
                )?;
                same("identity", &record.identity_hash, &peer.identity_hash)?;
                same("name hash", &record.name_hash, &peer.name_hash)?;
                same("display name", record.display_name.as_deref(), Some("Alex"))?;
                same("version", record.protocol_version, 1)?;
                same("hops", record.hops, 3)?;
                same("last seen", record.last_seen, t(1_000))
            }),
        ),
        row(
            "MESH-ANN-010",
            Kind::Valid,
            Case::Custom(|| {
                let (table, _tmp) = peer_table("conformance-ann-010-none");
                let peer = announced("alpha");
                table.observe(
                    PeerSighting {
                        display_name: AnnounceAppData::decode(&app("")).unwrap().display_name,
                        ..sighting(&peer)
                    },
                    t(1_000),
                );
                same(
                    "absent display name",
                    table
                        .get(&peer.destination_hash)
                        .and_then(|record| record.display_name),
                    None,
                )
            }),
        ),
        row(
            "MESH-ANN-011",
            Kind::Valid,
            Case::Custom(|| {
                let (table, _tmp) = peer_table("conformance-ann-011");
                let peer = announced("alpha");
                table.observe(sighting(&peer), t(1_000));
                table.mark_incompatible(&peer.destination_hash, 2);
                same(
                    "wire-learned mark",
                    table.get(&peer.destination_hash).unwrap().compatibility,
                    Compatibility::Incompatible { found: 2 },
                )?;
                table.observe(sighting(&peer), t(1_001));
                same(
                    "re-judged from the announce",
                    table.get(&peer.destination_hash).unwrap().compatibility,
                    Compatibility::Compatible,
                )
            }),
        ),
        row(
            "MESH-ANN-011",
            Kind::Valid,
            Case::Custom(|| {
                let (table, _tmp) = peer_table("conformance-ann-011-newer");
                let peer = announced("alpha");
                table.observe(sighting(&peer), t(1_000));
                table.observe(
                    PeerSighting {
                        protocol_version: 2,
                        ..sighting(&peer)
                    },
                    t(1_001),
                );
                same(
                    "a newer announce re-judges",
                    table.get(&peer.destination_hash).unwrap().compatibility,
                    Compatibility::Incompatible { found: 2 },
                )
            }),
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Section 5.4: propagation node announces
// ---------------------------------------------------------------------------------------

fn pn_row(id: &'static str, kind: Kind, app_data: Vec<u8>, expect: PnAction) -> Vector {
    row(
        id,
        kind,
        Case::PnAnnounce {
            app_data,
            propagation_name_hash: true,
            expect,
        },
    )
}

fn filed(enabled: bool, stamp_cost: u32, per_transfer_kb: u64) -> PnAction {
    PnAction::Filed {
        enabled,
        stamp_cost,
        per_transfer_kb,
    }
}

fn invalid(reason: &str) -> PnAction {
    PnAction::Refused(PropagationNodeError::InvalidAnnounce(reason.to_string()))
}

fn pn_vectors() -> Vec<Vector> {
    let with_cost = |cost: Value| pn(slots_with(5, costs(cost)));
    vec![
        pn_row("MESH-ANN-012", Kind::Valid, pn(slots()), REFERENCE_NODE),
        row(
            "MESH-ANN-012",
            Kind::Invalid,
            Case::PnAnnounce {
                app_data: pn(slots()),
                propagation_name_hash: false,
                expect: PnAction::Refused(PropagationNodeError::NotAPropagationNode),
            },
        ),
        row(
            "MESH-ANN-012",
            Kind::Invalid,
            Case::Custom(|| {
                let coyote = SingleOutputDestination::new(
                    *PrivateIdentity::new_from_rand(OsRng).as_identity(),
                    session_destination_name("0123456789abcdef0123456789abcdef"),
                )
                .desc;
                same(
                    "a Coyote destination",
                    PropagationNode::from_announce(&coyote, &pn(slots()))
                        .map(|node| node.stamp_cost),
                    Err(PropagationNodeError::NotAPropagationNode),
                )
            }),
        ),
        pn_row(
            "MESH-ANN-013",
            Kind::Invalid,
            b"\xc1not msgpack".to_vec(),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-013",
            Kind::Invalid,
            Vec::new(),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-013",
            Kind::Invalid,
            packed(&Value::from(7)),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-013",
            Kind::Invalid,
            packed(&Value::from("node")),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-013",
            Kind::Invalid,
            packed(&Value::Map(vec![])),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-013",
            Kind::Invalid,
            packed(&Value::Nil),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-013",
            Kind::Invalid,
            vec![0x93, 0xc2],
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-014",
            Kind::Invalid,
            pn(Vec::new()),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-014",
            Kind::Invalid,
            pn(slots()[..6].to_vec()),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-014",
            Kind::Invalid,
            pn(vec![Value::Boolean(false); 5]),
            PnAction::Malformed,
        ),
        pn_row("MESH-ANN-014", Kind::Boundary, pn(slots()), REFERENCE_NODE),
        pn_row(
            "MESH-ANN-015",
            Kind::Valid,
            pn(slots_with(0, Value::Boolean(true))),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-015",
            Kind::Valid,
            pn(slots_with(0, Value::from("anything"))),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-015",
            Kind::Valid,
            pn(slots_with(0, Value::Nil)),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-015",
            Kind::Valid,
            pn(slots_with(0, Value::Array(vec![Value::from(1)]))),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-016",
            Kind::Invalid,
            pn(slots_with(1, Value::from("now"))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-016",
            Kind::Invalid,
            pn(slots_with(1, Value::Nil)),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-016",
            Kind::Invalid,
            pn(slots_with(1, Value::F64(1.7e9))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-016",
            Kind::Invalid,
            pn(slots_with(1, Value::from(u64::MAX))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-016",
            Kind::Valid,
            pn(slots_with(1, Value::from(-5))),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-016",
            Kind::Boundary,
            pn(slots_with(1, Value::from(i64::MAX))),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-017",
            Kind::Valid,
            pn(slots_with(2, Value::Boolean(true))),
            filed(true, 20, 128),
        ),
        pn_row(
            "MESH-ANN-017",
            Kind::Valid,
            pn(slots_with(2, Value::Boolean(false))),
            filed(false, 20, 128),
        ),
        pn_row(
            "MESH-ANN-018",
            Kind::Invalid,
            pn(slots_with(2, Value::from(1))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-018",
            Kind::Invalid,
            pn(slots_with(2, Value::from("true"))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-018",
            Kind::Invalid,
            pn(slots_with(2, Value::Nil)),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-019",
            Kind::Valid,
            pn(slots_with(3, Value::from(256))),
            filed(true, 20, 256),
        ),
        pn_row(
            "MESH-ANN-019",
            Kind::Boundary,
            pn(slots_with(3, Value::from(0))),
            filed(true, 20, 0),
        ),
        pn_row(
            "MESH-ANN-019",
            Kind::Boundary,
            pn(slots_with(3, Value::from(i64::MAX))),
            filed(true, 20, i64::MAX as u64),
        ),
        pn_row(
            "MESH-ANN-020",
            Kind::Invalid,
            pn(slots_with(3, Value::from("128"))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-020",
            Kind::Invalid,
            pn(slots_with(3, Value::F64(128.0))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-020",
            Kind::Invalid,
            pn(slots_with(3, Value::Nil)),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-021",
            Kind::Invalid,
            pn(slots_with(3, Value::from(-5))),
            invalid("per-transfer limit is not a non-negative integer"),
        ),
        pn_row(
            "MESH-ANN-021",
            Kind::Invalid,
            pn(slots_with(3, Value::from(-1))),
            invalid("per-transfer limit is not a non-negative integer"),
        ),
        pn_row(
            "MESH-ANN-022",
            Kind::Invalid,
            pn(slots_with(4, Value::from("x"))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-022",
            Kind::Invalid,
            pn(slots_with(4, Value::Nil)),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-022",
            Kind::Invalid,
            pn(slots_with(4, Value::Boolean(true))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-022",
            Kind::Valid,
            pn(slots_with(4, Value::from(-1))),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Invalid,
            pn(slots_with(5, Value::Nil)),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Invalid,
            pn(slots_with(5, Value::from(20))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Invalid,
            pn(slots_with(5, Value::Map(vec![]))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Invalid,
            pn(slots_with(5, Value::Array(vec![]))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Invalid,
            pn(slots_with(5, Value::Array(vec![Value::from(20)]))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Invalid,
            pn(slots_with(
                5,
                Value::Array(vec![Value::from(20), Value::from(3)]),
            )),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Invalid,
            pn(slots_with(
                5,
                Value::Array(vec![Value::from("20"), Value::from(3), Value::from(18)]),
            )),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Invalid,
            pn(slots_with(
                5,
                Value::Array(vec![Value::from(20), Value::Nil, Value::from(18)]),
            )),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Invalid,
            pn(slots_with(
                5,
                Value::Array(vec![Value::from(20), Value::from(3), Value::F64(18.0)]),
            )),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Boundary,
            pn(slots_with(5, costs(Value::from(20)))),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-023",
            Kind::Valid,
            pn(slots_with(
                5,
                Value::Array(vec![
                    Value::from(20),
                    Value::from(3),
                    Value::from(18),
                    Value::from("more"),
                ]),
            )),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-024",
            Kind::Valid,
            with_cost(Value::from(20)),
            filed(true, 20, 128),
        ),
        pn_row(
            "MESH-ANN-024",
            Kind::Valid,
            with_cost(Value::from(13)),
            filed(true, 13, 128),
        ),
        pn_row(
            "MESH-ANN-024",
            Kind::Boundary,
            with_cost(Value::from(0)),
            filed(true, 0, 128),
        ),
        pn_row(
            "MESH-ANN-025",
            Kind::Invalid,
            with_cost(Value::from(-1)),
            PnAction::Refused(PropagationNodeError::NegativeStampCost(-1)),
        ),
        pn_row(
            "MESH-ANN-025",
            Kind::Invalid,
            with_cost(Value::from(i64::MIN)),
            PnAction::Refused(PropagationNodeError::NegativeStampCost(i64::MIN)),
        ),
        pn_row(
            "MESH-ANN-026",
            Kind::Invalid,
            with_cost(Value::from(u64::from(u32::MAX) + 1)),
            invalid("stamp cost does not fit a u32"),
        ),
        pn_row(
            "MESH-ANN-026",
            Kind::Invalid,
            with_cost(Value::from(i64::MAX)),
            invalid("stamp cost does not fit a u32"),
        ),
        pn_row(
            "MESH-ANN-026",
            Kind::Invalid,
            with_cost(Value::from(u64::MAX)),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-026",
            Kind::Boundary,
            with_cost(Value::from(u32::MAX)),
            filed(true, u32::MAX, 128),
        ),
        pn_row(
            "MESH-ANN-027",
            Kind::Valid,
            with_cost(Value::from(MAX_ACCEPTED_STAMP_COST + 1)),
            filed(true, 27, 128),
        ),
        pn_row(
            "MESH-ANN-027",
            Kind::Boundary,
            with_cost(Value::from(MAX_ACCEPTED_STAMP_COST)),
            filed(true, 26, 128),
        ),
        pn_row(
            "MESH-ANN-027",
            Kind::Valid,
            with_cost(Value::from(1_000)),
            filed(true, 1_000, 128),
        ),
        pn_row(
            "MESH-ANN-028",
            Kind::Invalid,
            pn(slots_with(6, Value::Array(vec![]))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-028",
            Kind::Invalid,
            pn(slots_with(6, Value::Nil)),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-028",
            Kind::Invalid,
            pn(slots_with(6, Value::from("meta"))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-028",
            Kind::Invalid,
            pn(slots_with(6, Value::from(7))),
            PnAction::Malformed,
        ),
        pn_row(
            "MESH-ANN-028",
            Kind::Valid,
            pn(slots_with(
                6,
                Value::Map(vec![(Value::from(1), Value::from("Node"))]),
            )),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-029",
            Kind::Valid,
            pn([slots(), vec![Value::from("extra")]].concat()),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-029",
            Kind::Valid,
            pn([
                slots(),
                vec![Value::Nil, Value::from(1), Value::Map(vec![])],
            ]
            .concat()),
            REFERENCE_NODE,
        ),
        row(
            "MESH-ANN-030",
            Kind::Boundary,
            Case::Custom(|| {
                let table = PropagationNodeTable::new();
                let mut nodes = Vec::new();
                for index in 0..PROPAGATION_NODE_TABLE_MAX_ENTRIES {
                    let node = propagation_node(20, 128);
                    nodes.push(node.destination.address_hash.to_hex_string());
                    table.observe(node, 1, t(1_000 + index as u64));
                }
                same(
                    "at the cap",
                    table.snapshot().len(),
                    PROPAGATION_NODE_TABLE_MAX_ENTRIES,
                )?;
                table.observe(propagation_node(20, 128), 1, t(5_000));
                let kept: Vec<String> = table
                    .snapshot()
                    .into_iter()
                    .map(|record| record.node.destination.address_hash.to_hex_string())
                    .collect();
                same(
                    "still at the cap",
                    kept.len(),
                    PROPAGATION_NODE_TABLE_MAX_ENTRIES,
                )?;
                ensure(
                    !kept.contains(&nodes[0]),
                    "the least recently heard node was kept",
                )?;
                ensure(
                    nodes[1..].iter().all(|hash| kept.contains(hash)),
                    "a fresher node was evicted",
                )
            }),
        ),
        row(
            "MESH-ANN-031",
            Kind::Valid,
            Case::Custom(|| {
                let table = PropagationNodeTable::new();
                let node = propagation_node(20, 128);
                let hash = node.destination.address_hash.to_hex_string();
                table.observe(node, 1, t(1_000));
                same(
                    "selected long after the sighting",
                    table
                        .select()
                        .map(|node| node.destination.address_hash.to_hex_string()),
                    Ok(hash),
                )
            }),
        ),
        row(
            "MESH-ANN-032",
            Kind::Valid,
            Case::Custom(|| {
                let table = PropagationNodeTable::new();
                let far = propagation_node(20, 128);
                let near_old = propagation_node(20, 128);
                let near_new = propagation_node(20, 128);
                let expected = near_new.destination.address_hash.to_hex_string();
                table.observe(far, 1, t(9_000));
                table.observe(near_old, 0, t(1_000));
                table.observe(near_new, 0, t(2_000));
                same(
                    "fewest hops, most recent first",
                    table
                        .select()
                        .map(|node| node.destination.address_hash.to_hex_string()),
                    Ok(expected),
                )
            }),
        ),
        pn_row(
            "MESH-ANN-033",
            Kind::Valid,
            [pn(slots()), b"trailing".to_vec()].concat(),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-033",
            Kind::Valid,
            [pn(slots()), vec![0xc1]].concat(),
            REFERENCE_NODE,
        ),
        pn_row(
            "MESH-ANN-033",
            Kind::Valid,
            [pn(slots()), pn(slots_with(5, costs(Value::from(-1))))].concat(),
            REFERENCE_NODE,
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Section 3: canonical forms
// ---------------------------------------------------------------------------------------

fn text_row(
    id: &'static str,
    kind: Kind,
    input: &str,
    max_chars: usize,
    expect: Option<&str>,
) -> Vector {
    row(
        id,
        kind,
        Case::Text {
            input: input.to_string(),
            max_chars,
            expect: expect.map(str::to_string),
        },
    )
}

fn hash_row(
    id: &'static str,
    kind: Kind,
    input: &'static str,
    expect: Option<&'static str>,
) -> Vector {
    row(id, kind, Case::HashText { input, expect })
}

fn canon_vectors() -> Vec<Vector> {
    vec![
        row(
            "MESH-CANON-001",
            Kind::Valid,
            Case::Custom(|| {
                same(
                    "hex_lower",
                    hex_lower(&[0x00, 0xab, 0xff]),
                    "00abff".to_string(),
                )?;
                let hash = AddressHash::new_from_rand(OsRng);
                same(
                    "transport hex",
                    hash.to_hex_string(),
                    hex_lower(hash.as_slice()),
                )?;
                let peer = announced("alpha");
                for hash in [&peer.destination_hash, &peer.identity_hash, &peer.name_hash] {
                    ensure(
                        hash.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                        format!("{hash} is not lowercase hex"),
                    )?;
                }
                same("destination digits", peer.destination_hash.len(), 32)?;
                same("identity digits", peer.identity_hash.len(), 32)?;
                same("name hash digits", peer.name_hash.len(), 20)
            }),
        ),
        hash_row(
            "MESH-CANON-002",
            Kind::Valid,
            "ABCDEF0123456789ABCDEF0123456789",
            Some("abcdef0123456789abcdef0123456789"),
        ),
        hash_row(
            "MESH-CANON-002",
            Kind::Boundary,
            "abcdef0123456789abcdef0123456789",
            Some("abcdef0123456789abcdef0123456789"),
        ),
        hash_row(
            "MESH-CANON-002",
            Kind::Valid,
            "0000000000000000000000000000000F",
            Some("0000000000000000000000000000000f"),
        ),
        hash_row(
            "MESH-CANON-002",
            Kind::Invalid,
            "abcdef0123456789abcdef012345678",
            None,
        ),
        hash_row(
            "MESH-CANON-002",
            Kind::Invalid,
            "abcdef0123456789abcdef0123456789a",
            None,
        ),
        hash_row(
            "MESH-CANON-002",
            Kind::Invalid,
            "abcdef0123456789abcdef012345678g",
            None,
        ),
        hash_row(
            "MESH-CANON-002",
            Kind::Invalid,
            "abcdef0123456789abcdef012345678 ",
            None,
        ),
        hash_row(
            "MESH-CANON-002",
            Kind::Invalid,
            " abcdef0123456789abcdef012345678",
            None,
        ),
        hash_row(
            "MESH-CANON-002",
            Kind::Invalid,
            "0xbcdef0123456789abcdef0123456789",
            None,
        ),
        hash_row(
            "MESH-CANON-002",
            Kind::Invalid,
            "abcdef0123456789abcdef01234567é",
            None,
        ),
        hash_row("MESH-CANON-002", Kind::Invalid, "", None),
        row(
            "MESH-CANON-003",
            Kind::Invalid,
            Case::Custom(|| {
                for (what, list) in [
                    (
                        "uppercase identity",
                        TrustList::default().identity(&IDENTITY_A.to_ascii_uppercase(), false),
                    ),
                    (
                        "uppercase destination",
                        TrustList::default()
                            .destination(&DESTINATION_A.to_ascii_uppercase(), IDENTITY_A),
                    ),
                    (
                        "short blocked identity",
                        TrustList::default().block(&IDENTITY_A[..31]),
                    ),
                    (
                        "non-hex denied destination",
                        TrustList::default().deny("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"),
                    ),
                ] {
                    let tmp = TempDir::new("conformance-canon-003");
                    list.write(&tmp.path);
                    let err = match TrustStore::open(&tmp.path) {
                        Ok(_) => return Err(format!("a trust list with a {what} key opened")),
                        Err(err) => err.to_string(),
                    };
                    ensure(err.contains("non-canonical hash"), format!("{what}: {err}"))?;
                }
                Ok(())
            }),
        ),
        row(
            "MESH-CANON-003",
            Kind::Valid,
            Case::Custom(|| {
                let tmp = TempDir::new("conformance-canon-003-ok");
                TrustList::default()
                    .destination(DESTINATION_A, IDENTITY_A)
                    .deny(DESTINATION_B)
                    .block(IDENTITY_B)
                    .write(&tmp.path);
                let store = TrustStore::open(&tmp.path).map_err(|err| err.to_string())?;
                same(
                    "trusted",
                    store.authorize(IDENTITY_A, DESTINATION_A),
                    verdict(Decision::Allow, Rule::DestinationTrusted),
                )?;
                same(
                    "denied",
                    store.authorize(IDENTITY_A, DESTINATION_B),
                    verdict(Decision::Refuse, Rule::DestinationDenied),
                )?;
                same(
                    "blocked",
                    store.authorize(IDENTITY_B, DESTINATION_A),
                    verdict(Decision::Refuse, Rule::IdentityBlocked),
                )
            }),
        ),
        row(
            "MESH-CANON-004",
            Kind::Valid,
            Case::Trust {
                list: TrustList::default().destination(DESTINATION_A, IDENTITY_A),
                identity: "0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A",
                destination: "D1D1D1D1D1D1D1D1D1D1D1D1D1D1D1D1",
                expect: verdict(Decision::Allow, Rule::DestinationTrusted),
            },
        ),
        row(
            "MESH-CANON-004",
            Kind::Invalid,
            Case::Trust {
                list: TrustList::default().destination(DESTINATION_A, IDENTITY_A),
                identity: "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0b",
                destination: DESTINATION_A,
                expect: verdict(Decision::Refuse, Rule::DefaultClosed),
            },
        ),
        row(
            "MESH-CANON-004",
            Kind::Invalid,
            Case::Trust {
                list: TrustList::default().destination(DESTINATION_A, IDENTITY_A),
                identity: "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a",
                destination: DESTINATION_A,
                expect: verdict(Decision::Refuse, Rule::DefaultClosed),
            },
        ),
        text_row(
            "MESH-CANON-005",
            Kind::Valid,
            "\u{1b}[31mred\u{1b}[0m",
            64,
            Some("red"),
        ),
        text_row(
            "MESH-CANON-005",
            Kind::Valid,
            "\u{1b}]0;title\u{7}name",
            64,
            Some("name"),
        ),
        text_row(
            "MESH-CANON-005",
            Kind::Valid,
            "\u{1b}]0;title\u{1b}\\name",
            64,
            Some("name"),
        ),
        text_row("MESH-CANON-005", Kind::Valid, "\u{1b}Xab", 64, Some("ab")),
        text_row("MESH-CANON-005", Kind::Valid, "a\tb", 64, Some("a b")),
        text_row("MESH-CANON-005", Kind::Valid, "a\r\nb", 64, Some("a  b")),
        text_row(
            "MESH-CANON-005",
            Kind::Valid,
            "one\u{2028}two\u{2029}three",
            64,
            Some("one two three"),
        ),
        text_row(
            "MESH-CANON-005",
            Kind::Valid,
            "zero\u{200B}width",
            64,
            Some("zerowidth"),
        ),
        text_row(
            "MESH-CANON-005",
            Kind::Valid,
            "Al\u{202E}ex",
            64,
            Some("Alex"),
        ),
        text_row(
            "MESH-CANON-005",
            Kind::Valid,
            "so\u{00AD}ft",
            64,
            Some("soft"),
        ),
        text_row("MESH-CANON-005", Kind::Valid, "a\u{E0041}b", 64, Some("ab")),
        text_row(
            "MESH-CANON-005",
            Kind::Valid,
            "a\u{FE0F}b\u{E0100}c",
            64,
            Some("abc"),
        ),
        text_row("MESH-CANON-005", Kind::Valid, "  hi  ", 64, Some("hi")),
        text_row("MESH-CANON-005", Kind::Valid, "\t hi \n", 64, Some("hi")),
        text_row("MESH-CANON-005", Kind::Boundary, "héllo", 3, Some("hél")),
        text_row("MESH-CANON-005", Kind::Boundary, "🌊🌊🌊", 2, Some("🌊🌊")),
        text_row("MESH-CANON-005", Kind::Boundary, "abc", 3, Some("abc")),
        text_row("MESH-CANON-005", Kind::Boundary, "ab cd", 3, Some("ab")),
        text_row("MESH-CANON-005", Kind::Boundary, "ab  c", 4, Some("ab")),
        text_row("MESH-CANON-005", Kind::Invalid, "", 64, None),
        text_row("MESH-CANON-005", Kind::Invalid, "   ", 64, None),
        text_row(
            "MESH-CANON-005",
            Kind::Invalid,
            "\u{200B}\u{FEFF}",
            64,
            None,
        ),
        text_row("MESH-CANON-005", Kind::Invalid, "\u{1b}[2J", 64, None),
        text_row("MESH-CANON-005", Kind::Invalid, "\u{1b}", 64, None),
        text_row("MESH-CANON-005", Kind::Invalid, "\u{FE0F}", 64, None),
        row(
            "MESH-CANON-005",
            Kind::Valid,
            Case::Card {
                value: set(
                    card_value(),
                    "display_name",
                    Value::from("\u{1b}[2J Al\u{202E}ex\t"),
                ),
                expect: accepted(StatusCard {
                    display_name: Some("Alex".to_string()),
                    ..minimal_card()
                }),
            },
        ),
        row(
            "MESH-CANON-006",
            Kind::Valid,
            Case::Custom(|| {
                same("1", packed(&Value::from(1u64)), vec![0x01])?;
                same("127", packed(&Value::from(127u64)), vec![0x7f])?;
                same("128", packed(&Value::from(128u64)), vec![0xcc, 0x80])?;
                same("0xf1", packed(&Value::from(0xf1u8)), vec![0xcc, 0xf1])?;
                same("255", packed(&Value::from(255u64)), vec![0xcc, 0xff])?;
                same("256", packed(&Value::from(256u64)), vec![0xcd, 0x01, 0x00])?;
                same(
                    "65535",
                    packed(&Value::from(65_535u64)),
                    vec![0xcd, 0xff, 0xff],
                )?;
                same(
                    "65536",
                    packed(&Value::from(65_536u64)),
                    vec![0xce, 0x00, 0x01, 0x00, 0x00],
                )?;
                same(
                    "2^32",
                    packed(&Value::from(1u64 << 32)),
                    vec![0xcf, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
                )?;
                same(
                    "card v",
                    packed(&minimal_card().to_value())[..4].to_vec(),
                    vec![0x83, 0xa1, b'v', 0x01],
                )
            }),
        ),
        row(
            "MESH-CANON-007",
            Kind::Valid,
            Case::Custom(|| {
                // {v: uint16 1, state: {code: uint32 1}, served_at_secs: uint64 SERVED_AT}
                let mut bytes = vec![0x83, 0xa1, b'v', 0xcd, 0x00, 0x01, 0xa5];
                bytes.extend_from_slice(b"state");
                bytes.extend_from_slice(&[0x81, 0xa4]);
                bytes.extend_from_slice(b"code");
                bytes.extend_from_slice(&[0xce, 0x00, 0x00, 0x00, 0x01, 0xae]);
                bytes.extend_from_slice(b"served_at_secs");
                bytes.push(0xcf);
                bytes.extend_from_slice(&SERVED_AT.to_be_bytes());
                same(
                    "card",
                    StatusCard::from_value(&unpacked(&bytes)),
                    Ok(minimal_card()),
                )?;
                let wide_v = set(body_value(), "v", unpacked(&[0xcf, 0, 0, 0, 0, 0, 0, 0, 1]));
                same("body v", from_r3_body(&wide_v), Ok(body()))?;
                let wide_code = set(card_value(), "state", state(unpacked(&[0xcd, 0x00, 0x02])));
                same(
                    "state.code",
                    StatusCard::from_value(&wide_code).map(|card| card.state.code),
                    Ok(STATE_WORKING),
                )
            }),
        ),
        row(
            "MESH-CANON-008",
            Kind::Valid,
            Case::CardEncode {
                card: maximal_card(),
                expect: map(vec![
                    ("v", Value::from(1u64)),
                    ("display_name", Value::from(text(DISPLAY_NAME_MAX_CHARS))),
                    ("objective", Value::from(text(OBJECTIVE_MAX_CHARS))),
                    (
                        "state",
                        map(vec![
                            ("code", Value::from(STATE_WORKING)),
                            ("since_secs", Value::from(60u64)),
                        ]),
                    ),
                    (
                        "repo",
                        map(vec![
                            ("name", Value::from(text(REPO_NAME_MAX_CHARS))),
                            ("branch", Value::from(text(BRANCH_MAX_CHARS))),
                        ]),
                    ),
                    (
                        "plan",
                        map(vec![("title", Value::from(text(PLAN_TITLE_MAX_CHARS)))]),
                    ),
                    (
                        "todo",
                        map(vec![
                            ("goal", Value::from(text(TODO_GOAL_MAX_CHARS))),
                            ("done", Value::from(3u32)),
                            ("total", Value::from(7u32)),
                        ]),
                    ),
                    ("about", Value::from(text(ABOUT_MAX_CHARS))),
                    ("caps", Value::Array(vec![Value::from("fetch")])),
                    ("snapshot_age_secs", Value::from(5u64)),
                    ("served_at_secs", Value::from(SERVED_AT)),
                ]),
            },
        ),
        row(
            "MESH-CANON-008",
            Kind::Valid,
            Case::MessageBodyEncode {
                message: OutboundPeer::new(
                    PeerKind::Reply,
                    "the answer",
                    Some("Re: question"),
                    Some("q-1"),
                    Some(serde_json::json!({ "n": 1 })),
                )
                .unwrap(),
                timestamp: 1_700_000_000.5,
                expect_keys: vec![
                    "v",
                    "kind",
                    "id",
                    "in_reply_to",
                    "title",
                    "content",
                    "fields",
                    "ts",
                ],
            },
        ),
        row(
            "MESH-CANON-008",
            Kind::Valid,
            Case::Custom(|| {
                same(
                    "acknowledgement",
                    keys_of(&received_reply("m-1")),
                    vec!["received", "id"],
                )?;
                same(
                    "knock body",
                    keys_of(&KnockIntro::new("hi").unwrap().to_r3_body()),
                    vec!["intro"],
                )?;
                let stored = peer_lxmf_message(
                    &OutboundPeer::new(
                        PeerKind::Ask,
                        "q",
                        None,
                        Some("m-0"),
                        Some(serde_json::json!({})),
                    )
                    .unwrap(),
                    &OriginName(ORIGIN),
                );
                let Some(Value::Map(fields)) = &stored.fields else {
                    return Err("LXMF fields are not a map".to_string());
                };
                same(
                    "custom data",
                    keys_of(&fields[1].1),
                    vec!["kind", "id", "in_reply_to", "name_hash", "fields"],
                )
            }),
        ),
        row(
            "MESH-CANON-009",
            Kind::Valid,
            Case::Card {
                value: map(vec![
                    ("served_at_secs", Value::from(SERVED_AT)),
                    (
                        "todo",
                        map(vec![
                            ("total", Value::from(7u32)),
                            ("done", Value::from(3u32)),
                            ("goal", Value::from("ship")),
                        ]),
                    ),
                    (
                        "state",
                        map(vec![
                            ("since_secs", Value::from(60u64)),
                            ("code", Value::from(STATE_WORKING)),
                        ]),
                    ),
                    ("display_name", Value::from("Alex")),
                    ("v", Value::from(1u64)),
                ]),
                expect: accepted(StatusCard {
                    display_name: Some("Alex".to_string()),
                    state: CardState {
                        code: STATE_WORKING,
                        since_secs: Some(60),
                    },
                    todo: Some(CardTodo {
                        goal: Some("ship".to_string()),
                        done: 3,
                        total: 7,
                    }),
                    ..minimal_card()
                }),
            },
        ),
        row(
            "MESH-CANON-009",
            Kind::Valid,
            Case::MessageBody {
                value: map(vec![
                    ("ts", Value::F64(1.5)),
                    ("content", Value::from("hi")),
                    ("id", Value::from("m-1")),
                    ("kind", Value::from("message")),
                    ("v", Value::from(1u64)),
                ]),
                expect: accepted_body(body()),
            },
        ),
        row(
            "MESH-CANON-009",
            Kind::Valid,
            Case::Ack {
                value: map(vec![
                    ("id", Value::from("m-1")),
                    ("received", Value::from(true)),
                ]),
                id: "m-1",
                expect: true,
            },
        ),
        row(
            "MESH-CANON-010",
            Kind::Valid,
            Case::MessageBody {
                value: set(
                    set(
                        set(
                            set(
                                set(body_value(), "kind", Value::Binary(b"ask".to_vec())),
                                "id",
                                Value::Binary(b"bin-id".to_vec()),
                            ),
                            "in_reply_to",
                            Value::Binary(b"m-0".to_vec()),
                        ),
                        "title",
                        Value::Binary(b"t".to_vec()),
                    ),
                    "content",
                    Value::Binary(vec![0xff, b'h', b'i']),
                ),
                expect: accepted_body(PeerBody {
                    kind: PeerKind::Ask,
                    id: "bin-id".to_string(),
                    in_reply_to: Some("m-0".to_string()),
                    title: Some("t".to_string()),
                    content: "\u{FFFD}hi".to_string(),
                    ..body()
                }),
            },
        ),
        row(
            "MESH-CANON-010",
            Kind::Valid,
            Case::LxmfPeer {
                message: peer_inbound(lxmf_fields(
                    Some(Value::Binary(PEER_MESSAGE_TYPE.as_bytes().to_vec())),
                    Some(map(vec![
                        ("kind", Value::Binary(b"reply".to_vec())),
                        ("id", Value::Binary(b"m-1".to_vec())),
                        ("in_reply_to", Value::Binary(b"m-0".to_vec())),
                        ("name_hash", Value::Binary(ORIGIN.to_vec())),
                    ])),
                )),
                expect: peer(PeerKind::Reply, "m-1", Some("m-0")),
            },
        ),
        row(
            "MESH-CANON-010",
            Kind::Valid,
            Case::LxmfKnock {
                message: knock_inbound(
                    lxmf_fields(
                        Some(Value::Binary(KNOCK_TYPE.as_bytes().to_vec())),
                        Some(knock_data()),
                    ),
                    b"hi",
                ),
                expect: knock(Some("hi")),
            },
        ),
        row(
            "MESH-CANON-011",
            Kind::Valid,
            Case::Custom(|| {
                let int8_one = unpacked(&[0xd0, 0x01]);
                let int32_two = unpacked(&[0xd2, 0x00, 0x00, 0x00, 0x02]);
                let card = set(
                    set(card_value(), "v", int8_one.clone()),
                    "state",
                    state(int32_two),
                );
                same(
                    "card from the int family",
                    StatusCard::from_value(&card),
                    Ok(StatusCard {
                        state: CardState {
                            code: STATE_WORKING,
                            since_secs: None,
                        },
                        ..minimal_card()
                    }),
                )?;
                same(
                    "body v from the int family",
                    from_r3_body(&set(body_value(), "v", int8_one)),
                    Ok(body()),
                )?;
                same(
                    "refusal code from the int family",
                    RefusalCode::from_wire(&unpacked(&[0xd1, 0x00, 0xf4])),
                    Some(RefusalCode::InvalidData),
                )
            }),
        ),
        row(
            "MESH-CANON-012",
            Kind::Valid,
            Case::Card {
                value: with(
                    with(
                        set(card_value(), "display_name", Value::from("first")),
                        "display_name",
                        Value::from("second"),
                    ),
                    "state",
                    state(Value::from(STATE_WORKING)),
                ),
                expect: accepted(StatusCard {
                    display_name: Some("first".to_string()),
                    ..minimal_card()
                }),
            },
        ),
        row(
            "MESH-CANON-012",
            Kind::Valid,
            Case::Card {
                value: set(
                    card_value(),
                    "repo",
                    Value::Map(vec![
                        (Value::from("name"), Value::from("first")),
                        (Value::from("name"), Value::from("second")),
                    ]),
                ),
                expect: accepted(StatusCard {
                    repo: Some(repo("first", None)),
                    ..minimal_card()
                }),
            },
        ),
        row(
            "MESH-CANON-012",
            Kind::Valid,
            Case::MessageBody {
                value: with(
                    with(body_value(), "title", Value::from("first")),
                    "title",
                    Value::from("second"),
                ),
                expect: accepted_body(PeerBody {
                    title: Some("first".to_string()),
                    ..body()
                }),
            },
        ),
        row(
            "MESH-CANON-012",
            Kind::Valid,
            Case::MessageBody {
                value: set(
                    body_value(),
                    "fields",
                    Value::Map(vec![
                        (Value::from("k"), Value::from(1)),
                        (Value::from("k"), Value::from(2)),
                    ]),
                ),
                expect: accepted_body(PeerBody {
                    fields: Some(serde_json::json!({ "k": 2 })),
                    ..body()
                }),
            },
        ),
        row(
            "MESH-CANON-012",
            Kind::Valid,
            Case::KnockBody {
                body: Some(map(vec![
                    ("intro", Value::from("first")),
                    ("intro", Value::from("second")),
                ])),
                expect: Some("first".to_string()),
            },
        ),
        row(
            "MESH-CANON-012",
            Kind::Valid,
            Case::Ack {
                value: with(received_reply("m-1"), "id", Value::from("m-2")),
                id: "m-1",
                expect: true,
            },
        ),
        row(
            "MESH-CANON-012",
            Kind::Valid,
            Case::Ack {
                value: with(received_reply("m-1"), "id", Value::from("m-2")),
                id: "m-2",
                expect: false,
            },
        ),
        row(
            "MESH-CANON-012",
            Kind::Valid,
            Case::LxmfPeer {
                message: peer_inbound(peer_fields(with(peer_data(), "id", Value::from("m-2")))),
                expect: peer(PeerKind::Message, "m-1", None),
            },
        ),
        row(
            "MESH-CANON-012",
            Kind::Valid,
            Case::LxmfKnock {
                message: knock_inbound(
                    knock_fields(with(
                        knock_data(),
                        "name_hash",
                        Value::Binary(vec![9; NAME_HASH_LEN]),
                    )),
                    b"hi",
                ),
                expect: knock(Some("hi")),
            },
        ),
        row(
            "MESH-CANON-013",
            Kind::Valid,
            Case::CardEncode {
                card: minimal_card(),
                expect: card_value(),
            },
        ),
        row(
            "MESH-CANON-013",
            Kind::Valid,
            Case::MessageBodyEncode {
                message: OutboundPeer::new(
                    PeerKind::Message,
                    "hi",
                    Some("t"),
                    Some("m-0"),
                    Some(serde_json::json!({ "a": 1, "b": [2] })),
                )
                .unwrap(),
                timestamp: 2.0,
                expect_keys: vec![
                    "v",
                    "kind",
                    "id",
                    "in_reply_to",
                    "title",
                    "content",
                    "fields",
                    "ts",
                ],
            },
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Sections 12 and 13: extensibility and the code-point registry
// ---------------------------------------------------------------------------------------

fn ext_vectors() -> Vec<Vector> {
    vec![
        row(
            "MESH-EXT-001",
            Kind::Valid,
            Case::Card {
                value: with(
                    set(
                        set(
                            card_value(),
                            "state",
                            map(vec![
                                ("code", Value::from(STATE_IDLE)),
                                ("mood", Value::from("fine")),
                            ]),
                        ),
                        "repo",
                        map(vec![
                            ("name", Value::from("coyote")),
                            ("remote", Value::from("origin")),
                        ]),
                    ),
                    "x-experimental",
                    Value::Map(vec![]),
                ),
                expect: accepted(StatusCard {
                    repo: Some(repo("coyote", None)),
                    ..minimal_card()
                }),
            },
        ),
        row(
            "MESH-EXT-001",
            Kind::Valid,
            Case::MessageBody {
                value: with(body_value(), "priority", Value::from(9)),
                expect: accepted_body(body()),
            },
        ),
        row(
            "MESH-EXT-001",
            Kind::Valid,
            Case::KnockBody {
                body: Some(map(vec![
                    ("intro", Value::from("hi")),
                    ("reason", Value::from("collab")),
                ])),
                expect: Some("hi".to_string()),
            },
        ),
        row(
            "MESH-EXT-001",
            Kind::Valid,
            Case::LxmfKnock {
                message: knock_inbound(
                    knock_fields(with(knock_data(), "extra", Value::from(1))),
                    b"hi",
                ),
                expect: knock(Some("hi")),
            },
        ),
        row(
            "MESH-EXT-001",
            Kind::Valid,
            Case::LxmfPeer {
                message: peer_inbound(peer_fields(with(peer_data(), "extra", Value::from(1)))),
                expect: peer(PeerKind::Message, "m-1", None),
            },
        ),
        row(
            "MESH-EXT-002",
            Kind::Invalid,
            Case::MessageBody {
                value: set(body_value(), "kind", Value::from("shout")),
                expect: BodyAction::InvalidData("kind is missing or unknown"),
            },
        ),
        row(
            "MESH-EXT-002",
            Kind::Invalid,
            Case::LxmfPeer {
                message: peer_inbound(peer_fields(set(peer_data(), "kind", Value::from("shout")))),
                expect: PeerLxmf::Malformed("kind is missing or unknown"),
            },
        ),
        row(
            "MESH-EXT-003",
            Kind::Valid,
            Case::Card {
                value: set(card_value(), "state", state(Value::from(7u8))),
                expect: accepted(StatusCard {
                    state: CardState {
                        code: 7,
                        since_secs: None,
                    },
                    ..minimal_card()
                }),
            },
        ),
        row(
            "MESH-EXT-004",
            Kind::Valid,
            Case::Custom(|| {
                for code in [0u64, 1, 0x7f, 0xf2, 0xf7, 0xfc, 0xff, 0x100, u64::MAX] {
                    same(
                        &format!("from_wire({code:#x})"),
                        RefusalCode::from_wire(&Value::from(code)),
                        None,
                    )?;
                }
                same(
                    "a negative integer",
                    RefusalCode::from_wire(&Value::from(-0xf1i64)),
                    None,
                )?;
                same(
                    "a string",
                    RefusalCode::from_wire(&Value::from("0xf1")),
                    None,
                )
            }),
        ),
        row(
            "MESH-EXT-005",
            Kind::Valid,
            Case::Custom(|| {
                let path_hash = "0123456789abcdef0123456789abcdef".to_string();
                let error = DispatchError::UnknownPath {
                    path_hash: path_hash.clone(),
                };
                let value = error.to_value();
                same(
                    "unknown_path map",
                    &value,
                    &map(vec![
                        ("error", Value::from("unknown_path")),
                        ("path_hash", Value::from(path_hash.as_str())),
                    ]),
                )?;
                same("read back", DispatchError::from_value(&value), Some(error))?;
                same("not a refusal code", RefusalCode::from_wire(&value), None)
            }),
        ),
        row(
            "MESH-EXT-006",
            Kind::Valid,
            Case::Custom(|| {
                same(
                    "window",
                    (MESH_PROTOCOL_MIN_SUPPORTED, MESH_PROTOCOL_VERSION),
                    (1, 1),
                )?;
                same("supported 1", protocol_supported(1), true)?;
                same("supported 0", protocol_supported(0), false)?;
                same("supported 2", protocol_supported(2), false)?;
                same("supported 65535", protocol_supported(u16::MAX), false)?;
                same(
                    "of(2)",
                    Compatibility::of(2),
                    Compatibility::Incompatible { found: 2 },
                )?;
                same(
                    "of(65535)",
                    Compatibility::of(u16::MAX),
                    Compatibility::Incompatible { found: u16::MAX },
                )?;
                let refusal = VersionRefusal::current(Some(2)).to_value();
                same(
                    "refusal keys",
                    keys_of(&refusal),
                    vec!["refusal", "found", "min", "max"],
                )
            }),
        ),
        row(
            "MESH-EXT-007",
            Kind::Valid,
            Case::Card {
                value: with(
                    with(
                        set(card_value(), "display_name", Value::from("Alex")),
                        "display_name_v2",
                        Value::from("Alexandra"),
                    ),
                    "v2",
                    Value::from(2u64),
                ),
                expect: accepted(StatusCard {
                    display_name: Some("Alex".to_string()),
                    ..minimal_card()
                }),
            },
        ),
        row(
            "MESH-EXT-007",
            Kind::Valid,
            Case::MessageBody {
                value: with(
                    with(body_value(), "content_v2", Value::from("other")),
                    "kind2",
                    Value::from("shout"),
                ),
                expect: accepted_body(body()),
            },
        ),
        row(
            "MESH-EXT-008",
            Kind::Valid,
            Case::Registry(|| {
                same("STATUS_CARD_VERSION", STATUS_CARD_VERSION, 1)?;
                same("PEER_WIRE_VERSION", PEER_WIRE_VERSION, 1)?;
                same("KNOCK_TYPE", KNOCK_TYPE, "scope.knock/1")?;
                same("PEER_MESSAGE_TYPE", PEER_MESSAGE_TYPE, "scope.peer/1")?;
                same("MESH_PROTOCOL_VERSION", MESH_PROTOCOL_VERSION, 1)
            }),
        ),
    ]
}

/// A current-version `record` with one key its layout does not know, fed back through the
/// same struct the store reads: it has to come back as a refusal that names the key.
fn refuses_an_unknown_key<T: Serialize + DeserializeOwned>(
    what: &str,
    record: &T,
) -> Result<(), String> {
    let mut value = serde_json::to_value(record).map_err(|error| format!("{what}: {error}"))?;
    value
        .as_object_mut()
        .ok_or_else(|| format!("{what}: serializes to something other than an object"))?
        .insert("later_field".to_string(), serde_json::Value::from(1));
    match serde_json::from_value::<T>(value) {
        Ok(_) => Err(format!(
            "{what}: a record with a key the layout does not know was read"
        )),
        Err(error) => ensure(
            error.to_string().contains("later_field"),
            format!("{what}: the refusal does not name the unknown key: {error}"),
        ),
    }
}

fn code_vectors() -> Vec<Vector> {
    vec![
        row(
            "MESH-CODE-001",
            Kind::Valid,
            Case::Registry(|| {
                same("KNOCK_PATH", KNOCK_PATH, "/knock")?;
                same("STATUS_PATH", STATUS_PATH, "/status")?;
                same("MESSAGE_PATH", MESSAGE_PATH, "/message")?;
                for (name, byte) in [
                    ("NoIdentity", 0xf0u8),
                    ("NoAccess", 0xf1),
                    ("InvalidKey", 0xf3),
                    ("InvalidData", 0xf4),
                    ("InvalidStamp", 0xf5),
                    ("Throttled", 0xf6),
                    ("NotFound", 0xfd),
                    ("Timeout", 0xfe),
                ] {
                    let code = RefusalCode::from_wire(&Value::from(byte))
                        .ok_or_else(|| format!("{name} ({byte:#x}) is not a refusal code"))?;
                    same(name, code.to_wire(), Value::from(byte))?;
                    same(
                        &format!("{name} wire bytes"),
                        packed(&code.to_wire()),
                        vec![0xcc, byte],
                    )?;
                }
                same(
                    "NoIdentity",
                    RefusalCode::from_wire(&Value::from(0xf0u8)),
                    Some(RefusalCode::NoIdentity),
                )?;
                same(
                    "InvalidKey",
                    RefusalCode::from_wire(&Value::from(0xf3u8)),
                    Some(RefusalCode::InvalidKey),
                )?;
                same(
                    "InvalidData",
                    RefusalCode::from_wire(&Value::from(0xf4u8)),
                    Some(RefusalCode::InvalidData),
                )?;
                same(
                    "InvalidStamp",
                    RefusalCode::from_wire(&Value::from(0xf5u8)),
                    Some(RefusalCode::InvalidStamp),
                )?;
                same(
                    "Throttled",
                    RefusalCode::from_wire(&Value::from(0xf6u8)),
                    Some(RefusalCode::Throttled),
                )?;
                same(
                    "NotFound",
                    RefusalCode::from_wire(&Value::from(0xfdu8)),
                    Some(RefusalCode::NotFound),
                )?;
                same(
                    "Timeout",
                    RefusalCode::from_wire(&Value::from(0xfeu8)),
                    Some(RefusalCode::Timeout),
                )?;
                same(
                    "version refusal keys",
                    keys_of(&VersionRefusal::current(Some(2)).to_value()),
                    vec!["refusal", "found", "min", "max"],
                )?;
                same(
                    "version refusal value",
                    VersionRefusal::current(Some(2))
                        .to_value()
                        .as_map()
                        .and_then(|entries| entries[0].1.as_str()),
                    Some("unsupported_version"),
                )?;
                same(
                    "unknown_path",
                    keys_of(
                        &DispatchError::UnknownPath {
                            path_hash: "00".repeat(16),
                        }
                        .to_value(),
                    ),
                    vec!["error", "path_hash"],
                )?;
                same(
                    "no_provider",
                    DispatchError::NoProvider {
                        path: "/status".to_string(),
                    }
                    .to_value(),
                    map(vec![
                        ("error", Value::from("no_provider")),
                        ("path", Value::from("/status")),
                    ]),
                )?;
                same(
                    "knock body key",
                    keys_of(&KnockIntro::new("hi").unwrap().to_r3_body()),
                    vec!["intro"],
                )?;
                same(
                    "card keys",
                    keys_of(&maximal_card().to_value()),
                    vec![
                        "v",
                        "display_name",
                        "objective",
                        "state",
                        "repo",
                        "plan",
                        "todo",
                        "about",
                        "caps",
                        "snapshot_age_secs",
                        "served_at_secs",
                    ],
                )?;
                let card = maximal_card().to_value();
                let sub = |key: &str| -> Vec<String> {
                    card.as_map()
                        .and_then(|entries| {
                            entries.iter().find(|(name, _)| name.as_str() == Some(key))
                        })
                        .map(|(_, value)| keys_of(value).into_iter().map(str::to_string).collect())
                        .unwrap_or_default()
                };
                same(
                    "state keys",
                    sub("state"),
                    vec!["code".to_string(), "since_secs".to_string()],
                )?;
                same(
                    "repo keys",
                    sub("repo"),
                    vec!["name".to_string(), "branch".to_string()],
                )?;
                same("plan keys", sub("plan"), vec!["title".to_string()])?;
                same(
                    "todo keys",
                    sub("todo"),
                    vec!["goal".to_string(), "done".to_string(), "total".to_string()],
                )?;
                same(
                    "state codes",
                    (STATE_UNKNOWN, STATE_IDLE, STATE_WORKING),
                    (0, 1, 2),
                )?;
                same(
                    "body keys",
                    keys_of(&to_r3_body(
                        &OutboundPeer::new(
                            PeerKind::Message,
                            "c",
                            Some("t"),
                            Some("m-0"),
                            Some(serde_json::json!({})),
                        )
                        .unwrap(),
                        1.0,
                    )),
                    vec![
                        "v",
                        "kind",
                        "id",
                        "in_reply_to",
                        "title",
                        "content",
                        "fields",
                        "ts",
                    ],
                )?;
                for (name, kind) in [
                    ("message", PeerKind::Message),
                    ("ask", PeerKind::Ask),
                    ("reply", PeerKind::Reply),
                    ("bulletin", PeerKind::Bulletin),
                ] {
                    same(
                        &format!("kind {name}"),
                        from_r3_body(&set(body_value(), "kind", Value::from(name)))
                            .map(|body| body.kind),
                        Ok(kind),
                    )?;
                    same(
                        &format!("emitted kind {name}"),
                        to_r3_body(&outbound(kind, "c"), 1.0)
                            .as_map()
                            .and_then(|entries| entries[1].1.as_str()),
                        Some(name),
                    )?;
                }
                same(
                    "acknowledgement keys",
                    keys_of(&received_reply("m-1")),
                    vec!["received", "id"],
                )?;
                let refusal = PeerRefusal::capacity(RefusalReason::EnvoyBusy).fields();
                same(
                    "typed refusal keys",
                    refusal
                        .as_object()
                        .map(|fields| fields.keys().cloned().collect::<Vec<_>>()),
                    Some(vec!["refusal".to_string(), "retry_after_secs".to_string()]),
                )?;
                same(
                    "refusal reasons",
                    RefusalReason::ALL
                        .iter()
                        .map(|reason| reason.as_str())
                        .collect::<Vec<_>>(),
                    vec![
                        "rate_limited",
                        "envoy_busy",
                        "envoy_stopping",
                        "peer_concurrency",
                        "token_ceiling",
                        "cost_ceiling",
                        "loop_guard",
                    ],
                )?;
                same("KNOCK_TYPE", KNOCK_TYPE, "scope.knock/1")?;
                same("PEER_MESSAGE_TYPE", PEER_MESSAGE_TYPE, "scope.peer/1")?;
                same("FIELD_CUSTOM_TYPE", FIELD_CUSTOM_TYPE, 0xfb)?;
                same("FIELD_CUSTOM_DATA", FIELD_CUSTOM_DATA, 0xfc)?;
                same("ANNOUNCE_MAGIC", ANNOUNCE_MAGIC, *b"SCOPE")?;
                same("MESH_PROTOCOL_VERSION", MESH_PROTOCOL_VERSION, 1)?;
                same("STATUS_CARD_VERSION", STATUS_CARD_VERSION, 1)?;
                same("PEER_WIRE_VERSION", PEER_WIRE_VERSION, 1)
            }),
        ),
        row(
            "MESH-CODE-002",
            Kind::Valid,
            Case::Registry(|| {
                same(
                    "0xf2 is unallocated",
                    RefusalCode::from_wire(&Value::from(0xf2u8)),
                    None,
                )?;
                let reasons: Vec<&str> = RefusalReason::ALL
                    .iter()
                    .map(|reason| reason.as_str())
                    .collect();
                let mut unique = reasons.clone();
                unique.sort_unstable();
                unique.dedup();
                same("refusal reasons are distinct", unique.len(), reasons.len())?;
                same(
                    "an unknown kind is not aliased",
                    from_r3_body(&set(body_value(), "kind", Value::from("note"))),
                    Err("kind is missing or unknown"),
                )?;
                same(
                    "an unknown state code is kept, not folded",
                    StatusCard::from_value(&set(card_value(), "state", state(Value::from(3u8))))
                        .map(|card| card.state.code),
                    Ok(3),
                )?;
                same(
                    "an unknown dispatch error string is a reply value",
                    DispatchError::from_value(&map(vec![
                        ("error", Value::from("gone")),
                        ("path", Value::from("/status")),
                    ])),
                    None,
                )
            }),
        ),
        row(
            "MESH-CODE-003",
            Kind::Valid,
            Case::Registry(|| {
                same("TRUST_FILE_VERSION", TRUST_FILE_VERSION, 2)?;
                same("KNOCK_RECORD_VERSION", KNOCK_RECORD_VERSION, 2)?;
                same("PENDING_RECORD_VERSION", PENDING_RECORD_VERSION, 2)?;
                same("INBOUND_RECORD_VERSION", INBOUND_RECORD_VERSION, 2)?;
                same("PREDECESSOR_RECORD_VERSION", PREDECESSOR_RECORD_VERSION, 1)?;
                same("PEER_TABLE_VERSION", PEER_TABLE_VERSION, 2)?;
                same("PROPAGATION_STORE_VERSION", PROPAGATION_STORE_VERSION, 1)
            }),
        ),
        row(
            "MESH-CODE-004",
            Kind::Valid,
            Case::Registry(|| {
                let path = Path::new("knocks.jsonl");
                let newer = version_refusal("knock cache", path, Some(2), 2, 1, Remedy::Cache);
                for needle in [
                    "knocks.jsonl",
                    "line 2",
                    "version 2",
                    "version 1",
                    "upgrade Coyote",
                    "move the file aside",
                ] {
                    ensure(
                        newer.contains(needle),
                        format!("a newer version's refusal lacks {needle:?}: {newer}"),
                    )?;
                }
                let older = version_refusal("knock cache", path, Some(2), 0, 1, Remedy::Cache);
                ensure(
                    older.contains("no migration exists for versions before 1"),
                    format!("an older version's refusal does not say no migration exists: {older}"),
                )?;
                let unversioned =
                    unversioned_refusal("knock cache", path, Some(2), 1, Remedy::Cache);
                for needle in ["no readable `version` field", "writes version 1"] {
                    ensure(
                        unversioned.contains(needle),
                        format!("an unreadable version's refusal lacks {needle:?}: {unversioned}"),
                    )?;
                }
                Ok(())
            }),
        ),
        row(
            "MESH-CODE-005",
            Kind::Valid,
            Case::Registry(|| {
                refuses_an_unknown_key(
                    "KnockRecord",
                    &KnockRecord {
                        version: KNOCK_RECORD_VERSION,
                        received_at: "2027-01-15T05:13:20Z".to_string(),
                        identity_hash: "0a".repeat(16),
                        destination_hash: "0b".repeat(16),
                        name_hash: "0c".repeat(10),
                        display_name: None,
                        intro: None,
                        hops: 1,
                    },
                )?;
                refuses_an_unknown_key(
                    "PendingRecord",
                    &PendingRecord {
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
                    },
                )?;
                refuses_an_unknown_key(
                    "InboundRecord",
                    &InboundRecord {
                        version: INBOUND_RECORD_VERSION,
                        id: "q1".to_string(),
                        peer_destination: "0b".repeat(16),
                        peer_identity: "0a".repeat(16),
                        thread: "q1".to_string(),
                        question: "what time is it".to_string(),
                        envoy_question: String::new(),
                        received_at: "2027-01-15T05:13:20Z".to_string(),
                        kind: InboundKind::Question,
                        paths: Vec::new(),
                        reason: String::new(),
                    },
                )?;
                refuses_an_unknown_key(
                    "Predecessor",
                    &Predecessor {
                        version: PREDECESSOR_RECORD_VERSION,
                        identity_hash: "0a".repeat(16),
                        rotated_at: "2027-01-15T05:13:20Z".to_string(),
                        reason: "rotated".to_string(),
                    },
                )?;
                refuses_an_unknown_key(
                    "PeerTableFile",
                    &PeerTableFile {
                        version: PEER_TABLE_VERSION,
                        peers: vec![],
                    },
                )?;
                refuses_an_unknown_key(
                    "PeerRecord",
                    &PeerRecord {
                        destination_hash: "0b".repeat(16),
                        identity_hash: "0a".repeat(16),
                        name_hash: "0c".repeat(10),
                        display_name: None,
                        protocol_version: 1,
                        compatibility: Compatibility::Compatible,
                        hops: 1,
                        first_seen: UNIX_EPOCH,
                        last_seen: UNIX_EPOCH,
                    },
                )?;
                match serde_json::from_value::<Compatibility>(
                    serde_json::json!({"incompatible": {"found": 2, "later": 1}}),
                ) {
                    Ok(_) => Err(
                        "Compatibility: a variant with a key the layout does not know was read"
                            .to_string(),
                    ),
                    Err(error) => ensure(
                        error.to_string().contains("later"),
                        format!(
                            "Compatibility: the refusal does not name the unknown key: {error}"
                        ),
                    ),
                }
            }),
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Section 5.3: timing, where a pure function exists
// ---------------------------------------------------------------------------------------

fn time_vectors() -> Vec<Vector> {
    vec![
        row(
            "MESH-TIME-002",
            Kind::Valid,
            Case::Custom(|| same("HEARTBEAT_SECS", HEARTBEAT_SECS, 900)),
        ),
        row(
            "MESH-TIME-003",
            Kind::Valid,
            Case::Custom(|| {
                same("REANNOUNCE_FLOOR_SECS", REANNOUNCE_FLOOR_SECS, 300)?;
                ensure(
                    REANNOUNCE_FLOOR_SECS < HEARTBEAT_SECS,
                    "the floor would block the heartbeat",
                )
            }),
        ),
        row(
            "MESH-TIME-004",
            Kind::Boundary,
            Case::Custom(|| {
                same("PEER_TTL", PEER_TTL, Duration::from_secs(2_700))?;
                same(
                    "three heartbeats",
                    PEER_TTL,
                    Duration::from_secs(
                        HEARTBEAT_SECS * u64::from(PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT),
                    ),
                )?;
                let (table, _tmp) = peer_table("conformance-time-004");
                let peer = announced("alpha");
                let seen = t(10_000);
                table.observe(sighting(&peer), seen);
                same(
                    "one second short",
                    table.sweep(seen + PEER_TTL - Duration::from_secs(1)),
                    Vec::<String>::new(),
                )?;
                same(
                    "at the bound",
                    table.sweep(seen + PEER_TTL),
                    vec![peer.destination_hash.clone()],
                )?;
                same("gone", table.get(&peer.destination_hash).is_none(), true)
            }),
        ),
        row(
            "MESH-TIME-004",
            Kind::Valid,
            Case::Custom(|| {
                let (table, _tmp) = peer_table("conformance-time-004-future");
                let peer = announced("alpha");
                let seen = t(10_000);
                table.observe(sighting(&peer), seen);
                same(
                    "a future sighting is not expired",
                    table.sweep(seen - Duration::from_secs(60)),
                    Vec::<String>::new(),
                )
            }),
        ),
        row(
            "MESH-TIME-005",
            Kind::Boundary,
            Case::Custom(|| {
                same(
                    "PEER_STALE_AFTER",
                    PEER_STALE_AFTER,
                    Duration::from_secs(1_800),
                )?;
                let (table, _tmp) = peer_table("conformance-time-005");
                let peer = announced("alpha");
                let seen = t(10_000);
                table.observe(sighting(&peer), seen);
                let record = table.get(&peer.destination_hash).unwrap();
                same(
                    "one second short",
                    record.is_stale(seen + PEER_STALE_AFTER - Duration::from_secs(1)),
                    false,
                )?;
                same(
                    "at the bound",
                    record.is_stale(seen + PEER_STALE_AFTER),
                    true,
                )?;
                same("older", record.is_stale(seen + PEER_TTL), true)?;
                same(
                    "a future sighting",
                    record.is_stale(seen - Duration::from_secs(60)),
                    false,
                )
            }),
        ),
        row(
            "MESH-TIME-007",
            Kind::Boundary,
            Case::Custom(|| {
                same("PEER_TABLE_MAX_ENTRIES", PEER_TABLE_MAX_ENTRIES, 1_024)?;
                let (table, _tmp) = peer_table("conformance-time-007");
                let base = sighting(&announced("alpha"));
                let hash = |index: usize| format!("{index:032x}");
                for index in 0..PEER_TABLE_MAX_ENTRIES {
                    table.observe(
                        PeerSighting {
                            destination_hash: hash(index),
                            identity_hash: base.identity_hash.clone(),
                            name_hash: base.name_hash.clone(),
                            display_name: None,
                            protocol_version: 1,
                            hops: 1,
                        },
                        t(1_000 + index as u64),
                    );
                }
                same("at the cap", table.snapshot().len(), PEER_TABLE_MAX_ENTRIES)?;
                table.observe(
                    PeerSighting {
                        destination_hash: hash(PEER_TABLE_MAX_ENTRIES),
                        identity_hash: base.identity_hash.clone(),
                        name_hash: base.name_hash.clone(),
                        display_name: None,
                        protocol_version: 1,
                        hops: 1,
                    },
                    t(9_000),
                );
                same(
                    "still at the cap",
                    table.snapshot().len(),
                    PEER_TABLE_MAX_ENTRIES,
                )?;
                same(
                    "the least recently seen is gone",
                    table.get(&hash(0)).is_none(),
                    true,
                )?;
                same(
                    "the newest is kept",
                    table.get(&hash(PEER_TABLE_MAX_ENTRIES)).is_some(),
                    true,
                )
            }),
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Section 8.1 and 8.6: the knock body and the knock over LXMF
// ---------------------------------------------------------------------------------------

fn knock_body(id: &'static str, kind: Kind, body: Option<Value>, expect: Option<&str>) -> Vector {
    row(
        id,
        kind,
        Case::KnockBody {
            body,
            expect: expect.map(str::to_string),
        },
    )
}

fn intro(value: Value) -> Option<Value> {
    Some(map(vec![("intro", value)]))
}

fn lxmf_knock(
    id: &'static str,
    kind: Kind,
    message: InboundMessage,
    expect: KnockMessage,
) -> Vector {
    row(id, kind, Case::LxmfKnock { message, expect })
}

fn knock_vectors() -> Vec<Vector> {
    let two_hundred = text(KNOCK_INTRO_MAX_CHARS);
    let waves = "🌊".repeat(KNOCK_INTRO_MAX_CHARS);
    vec![
        knock_body(
            "MESH-KNOCK-001",
            Kind::Valid,
            intro(Value::from("hi")),
            Some("hi"),
        ),
        knock_body(
            "MESH-KNOCK-001",
            Kind::Valid,
            intro(Value::from("\u{1b}[2J hi\t")),
            Some("hi"),
        ),
        knock_body("MESH-KNOCK-001", Kind::Valid, intro(Value::from("")), None),
        knock_body(
            "MESH-KNOCK-001",
            Kind::Valid,
            intro(Value::from("   ")),
            None,
        ),
        knock_body("MESH-KNOCK-001", Kind::Invalid, None, None),
        knock_body("MESH-KNOCK-001", Kind::Invalid, Some(Value::from(7)), None),
        knock_body(
            "MESH-KNOCK-001",
            Kind::Invalid,
            Some(Value::from("hi")),
            None,
        ),
        knock_body("MESH-KNOCK-001", Kind::Invalid, Some(Value::Nil), None),
        knock_body(
            "MESH-KNOCK-001",
            Kind::Invalid,
            Some(Value::Map(vec![])),
            None,
        ),
        knock_body(
            "MESH-KNOCK-001",
            Kind::Invalid,
            intro(Value::Binary(b"hi".to_vec())),
            None,
        ),
        knock_body("MESH-KNOCK-001", Kind::Invalid, intro(Value::from(7)), None),
        knock_body("MESH-KNOCK-001", Kind::Invalid, intro(Value::Nil), None),
        knock_body(
            "MESH-KNOCK-001",
            Kind::Boundary,
            intro(Value::from(two_hundred.as_str())),
            Some(two_hundred.as_str()),
        ),
        knock_body(
            "MESH-KNOCK-001",
            Kind::Boundary,
            intro(Value::from(text(KNOCK_INTRO_MAX_CHARS + 1))),
            Some(two_hundred.as_str()),
        ),
        knock_body(
            "MESH-KNOCK-001",
            Kind::Boundary,
            intro(Value::from(format!("{waves}🌊"))),
            Some(waves.as_str()),
        ),
        knock_body(
            "MESH-KNOCK-002",
            Kind::Valid,
            Some(map(vec![
                ("intro", Value::from("hi")),
                ("extra", Value::from(1)),
            ])),
            Some("hi"),
        ),
        knock_body(
            "MESH-KNOCK-002",
            Kind::Valid,
            Some(map(vec![("Intro", Value::from("hi"))])),
            None,
        ),
        knock_body(
            "MESH-KNOCK-002",
            Kind::Valid,
            Some(map(vec![
                ("extra", Value::from(1)),
                ("intro", Value::from("hi")),
            ])),
            Some("hi"),
        ),
        row(
            "MESH-KNOCK-003",
            Kind::Invalid,
            Case::KnockIntro {
                text: text(KNOCK_INTRO_MAX_CHARS + 1),
                expect: Err(KnockError::IntroTooLong {
                    chars: 201,
                    max: 200,
                }),
            },
        ),
        row(
            "MESH-KNOCK-003",
            Kind::Boundary,
            Case::KnockIntro {
                text: two_hundred.clone(),
                expect: Ok(two_hundred.clone()),
            },
        ),
        row(
            "MESH-KNOCK-003",
            Kind::Boundary,
            Case::KnockIntro {
                text: format!("{two_hundred}\u{200B}"),
                expect: Ok(two_hundred.clone()),
            },
        ),
        row(
            "MESH-KNOCK-004",
            Kind::Valid,
            Case::KnockIntro {
                text: "\u{1b}[2J Al\u{202E}ex\t".to_string(),
                expect: Ok("Alex".to_string()),
            },
        ),
        row(
            "MESH-KNOCK-004",
            Kind::Valid,
            Case::KnockIntro {
                text: "".to_string(),
                expect: Ok(String::new()),
            },
        ),
        row(
            "MESH-KNOCK-004",
            Kind::Valid,
            Case::Custom(|| {
                same(
                    "body",
                    KnockIntro::new("\u{1b}[2J hi ").unwrap().to_r3_body(),
                    map(vec![("intro", Value::from("hi"))]),
                )
            }),
        ),
        lxmf_knock(
            "MESH-KNOCK-020",
            Kind::Invalid,
            knock_inbound(lxmf_fields(None, Some(knock_data())), b"hi"),
            KnockMessage::NotAKnock,
        ),
        lxmf_knock(
            "MESH-KNOCK-020",
            Kind::Invalid,
            knock_inbound(
                lxmf_fields(Some(Value::from(PEER_MESSAGE_TYPE)), Some(knock_data())),
                b"hi",
            ),
            KnockMessage::NotAKnock,
        ),
        lxmf_knock(
            "MESH-KNOCK-020",
            Kind::Invalid,
            knock_inbound(
                lxmf_fields(Some(Value::from("scope.knock/2")), Some(knock_data())),
                b"hi",
            ),
            KnockMessage::NotAKnock,
        ),
        lxmf_knock(
            "MESH-KNOCK-020",
            Kind::Invalid,
            knock_inbound(
                lxmf_fields(Some(Value::from(0xfb)), Some(knock_data())),
                b"hi",
            ),
            KnockMessage::NotAKnock,
        ),
        lxmf_knock(
            "MESH-KNOCK-020",
            Kind::Invalid,
            inbound(None, None, Some(b"hi".as_slice())),
            KnockMessage::NotAKnock,
        ),
        lxmf_knock(
            "MESH-KNOCK-020",
            Kind::Invalid,
            inbound(Some(Value::from("fields")), None, Some(b"hi".as_slice())),
            KnockMessage::NotAKnock,
        ),
        lxmf_knock(
            "MESH-KNOCK-020",
            Kind::Valid,
            knock_inbound(knock_fields(knock_data()), b"hi"),
            knock(Some("hi")),
        ),
        lxmf_knock(
            "MESH-KNOCK-021",
            Kind::Invalid,
            knock_inbound(lxmf_fields(Some(Value::from(KNOCK_TYPE)), None), b"hi"),
            KnockMessage::Malformed("custom data is missing or not a map"),
        ),
        lxmf_knock(
            "MESH-KNOCK-021",
            Kind::Invalid,
            knock_inbound(knock_fields(Value::Array(vec![])), b"hi"),
            KnockMessage::Malformed("custom data is missing or not a map"),
        ),
        lxmf_knock(
            "MESH-KNOCK-021",
            Kind::Invalid,
            knock_inbound(knock_fields(Value::Binary(ORIGIN.to_vec())), b"hi"),
            KnockMessage::Malformed("custom data is missing or not a map"),
        ),
        lxmf_knock(
            "MESH-KNOCK-021",
            Kind::Invalid,
            knock_inbound(knock_fields(Value::Nil), b"hi"),
            KnockMessage::Malformed("custom data is missing or not a map"),
        ),
        lxmf_knock(
            "MESH-KNOCK-022",
            Kind::Valid,
            knock_inbound(
                Value::Map(vec![
                    (Value::from(1), Value::from("title field")),
                    (Value::from(FIELD_CUSTOM_TYPE), Value::from(KNOCK_TYPE)),
                    (Value::from(FIELD_CUSTOM_DATA), knock_data()),
                    (Value::from(0xfd), Value::Map(vec![])),
                ]),
                b"hi",
            ),
            knock(Some("hi")),
        ),
        lxmf_knock(
            "MESH-KNOCK-023",
            Kind::Invalid,
            knock_inbound(knock_fields(Value::Map(vec![])), b"hi"),
            KnockMessage::Malformed("name_hash is missing or not binary"),
        ),
        lxmf_knock(
            "MESH-KNOCK-023",
            Kind::Invalid,
            knock_inbound(
                knock_fields(map(vec![("name_hash", Value::from(hex_lower(&ORIGIN)))])),
                b"hi",
            ),
            KnockMessage::Malformed("name_hash is missing or not binary"),
        ),
        lxmf_knock(
            "MESH-KNOCK-023",
            Kind::Invalid,
            knock_inbound(knock_fields(map(vec![("name_hash", Value::Nil)])), b"hi"),
            KnockMessage::Malformed("name_hash is missing or not binary"),
        ),
        lxmf_knock(
            "MESH-KNOCK-023",
            Kind::Invalid,
            knock_inbound(
                knock_fields(map(vec![("name_hash", Value::Binary(vec![7; 9]))])),
                b"hi",
            ),
            KnockMessage::Malformed("name_hash is not 10 bytes"),
        ),
        lxmf_knock(
            "MESH-KNOCK-023",
            Kind::Invalid,
            knock_inbound(
                knock_fields(map(vec![("name_hash", Value::Binary(vec![7; 11]))])),
                b"hi",
            ),
            KnockMessage::Malformed("name_hash is not 10 bytes"),
        ),
        lxmf_knock(
            "MESH-KNOCK-023",
            Kind::Invalid,
            knock_inbound(
                knock_fields(map(vec![("name_hash", Value::Binary(vec![]))])),
                b"hi",
            ),
            KnockMessage::Malformed("name_hash is not 10 bytes"),
        ),
        lxmf_knock(
            "MESH-KNOCK-023",
            Kind::Boundary,
            knock_inbound(knock_fields(knock_data()), b"hi"),
            knock(Some("hi")),
        ),
        lxmf_knock(
            "MESH-KNOCK-024",
            Kind::Valid,
            knock_inbound(
                knock_fields(with(knock_data(), "intro", Value::from("ignored"))),
                b"hi",
            ),
            knock(Some("hi")),
        ),
        lxmf_knock(
            "MESH-KNOCK-024",
            Kind::Valid,
            knock_inbound(
                knock_fields(with(
                    knock_data(),
                    "destination",
                    Value::Binary(vec![0; 16]),
                )),
                b"hi",
            ),
            knock(Some("hi")),
        ),
        row(
            "MESH-KNOCK-025",
            Kind::Valid,
            Case::Custom(|| {
                let stored = knock_message(&KnockIntro::new("hi").unwrap(), &OriginName(ORIGIN));
                same("title", stored.title, None)
            }),
        ),
        row(
            "MESH-KNOCK-026",
            Kind::Valid,
            Case::Custom(|| {
                let origin = OriginName(ORIGIN);
                let stored = knock_message(&KnockIntro::new("\u{1b}[2J héllo ").unwrap(), &origin);
                same("content", stored.content, "héllo".as_bytes().to_vec())?;
                same(
                    "empty intro",
                    knock_message(&KnockIntro::new("").unwrap(), &origin).content,
                    Vec::<u8>::new(),
                )?;
                same("fields", stored.fields, Some(knock_fields(knock_data())))
            }),
        ),
        lxmf_knock(
            "MESH-KNOCK-027",
            Kind::Valid,
            knock_inbound(knock_fields(knock_data()), &[0xff, b'h', b'i']),
            knock(Some("\u{FFFD}hi")),
        ),
        lxmf_knock(
            "MESH-KNOCK-027",
            Kind::Valid,
            knock_inbound(knock_fields(knock_data()), b"\x1b[2J hi\t"),
            knock(Some("hi")),
        ),
        lxmf_knock(
            "MESH-KNOCK-027",
            Kind::Valid,
            knock_inbound(knock_fields(knock_data()), b""),
            knock(None),
        ),
        lxmf_knock(
            "MESH-KNOCK-027",
            Kind::Valid,
            knock_inbound(knock_fields(knock_data()), b"   "),
            knock(None),
        ),
        lxmf_knock(
            "MESH-KNOCK-027",
            Kind::Valid,
            inbound(Some(knock_fields(knock_data())), None, None),
            knock(None),
        ),
        lxmf_knock(
            "MESH-KNOCK-027",
            Kind::Boundary,
            knock_inbound(knock_fields(knock_data()), two_hundred.as_bytes()),
            knock(Some(two_hundred.as_str())),
        ),
        lxmf_knock(
            "MESH-KNOCK-027",
            Kind::Boundary,
            knock_inbound(
                knock_fields(knock_data()),
                text(KNOCK_INTRO_MAX_CHARS + 1).as_bytes(),
            ),
            knock(Some(two_hundred.as_str())),
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Section 9: the status card
// ---------------------------------------------------------------------------------------

fn card(id: &'static str, kind: Kind, value: Value, expect: CardAction) -> Vector {
    row(id, kind, Case::Card { value, expect })
}

fn malformed(text: &'static str) -> CardAction {
    CardAction::Malformed(text)
}

fn with_state(since_secs: Option<u64>) -> StatusCard {
    StatusCard {
        state: CardState {
            code: STATE_IDLE,
            since_secs,
        },
        ..minimal_card()
    }
}

fn with_repo(name: &str, branch: Option<&str>) -> StatusCard {
    StatusCard {
        repo: Some(repo(name, branch)),
        ..minimal_card()
    }
}

fn with_plan(title: &str) -> StatusCard {
    StatusCard {
        plan: Some(CardPlan {
            title: title.to_string(),
        }),
        ..minimal_card()
    }
}

fn with_todo(goal: Option<&str>, done: u32, total: u32) -> StatusCard {
    StatusCard {
        todo: Some(CardTodo {
            goal: goal.map(str::to_string),
            done,
            total,
        }),
        ..minimal_card()
    }
}

fn todo_map(goal: Option<Value>, done: Value, total: Value) -> Value {
    let mut entries = Vec::new();
    if let Some(goal) = goal {
        entries.push(("goal", goal));
    }
    entries.push(("done", done));
    entries.push(("total", total));
    set(card_value(), "todo", map(entries))
}

fn status_vectors() -> Vec<Vector> {
    let name = |chars: usize| Value::from(text(chars));
    vec![
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            set(card_value(), "v", Value::from(2u64)),
            CardAction::UnsupportedVersion {
                found: 2,
                supported: 1,
            },
        ),
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            set(card_value(), "v", Value::from(u64::MAX)),
            CardAction::UnsupportedVersion {
                found: u64::MAX,
                supported: 1,
            },
        ),
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            without(card_value(), "v"),
            malformed("`v` is missing"),
        ),
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            set(card_value(), "v", Value::Nil),
            malformed("`v` is missing"),
        ),
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            set(card_value(), "v", Value::from(0u64)),
            malformed("`v` is 0"),
        ),
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            set(card_value(), "v", Value::from("1")),
            malformed("`v` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            set(card_value(), "v", Value::from(-1)),
            malformed("`v` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            set(card_value(), "v", Value::F64(1.0)),
            malformed("`v` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            set(card_value(), "v", Value::Boolean(true)),
            malformed("`v`"),
        ),
        card(
            "MESH-STATUS-004",
            Kind::Boundary,
            set(card_value(), "v", Value::from(1u64)),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            Value::from(7),
            malformed("not a map"),
        ),
        card(
            "MESH-STATUS-004",
            Kind::Invalid,
            Value::Nil,
            malformed("not a map"),
        ),
        card(
            "MESH-STATUS-005",
            Kind::Invalid,
            set(
                card_value(),
                "display_name",
                Value::Binary(b"Alex".to_vec()),
            ),
            malformed("`display_name` is not a string"),
        ),
        card(
            "MESH-STATUS-005",
            Kind::Invalid,
            set(card_value(), "display_name", Value::from(7)),
            malformed("`display_name` is not a string"),
        ),
        card(
            "MESH-STATUS-005",
            Kind::Invalid,
            set(card_value(), "display_name", Value::Array(vec![])),
            malformed("`display_name` is not a string"),
        ),
        card(
            "MESH-STATUS-005",
            Kind::Valid,
            card_value(),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-005",
            Kind::Valid,
            set(card_value(), "display_name", Value::Nil),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-005",
            Kind::Valid,
            set(card_value(), "display_name", Value::from("Alex")),
            accepted(StatusCard {
                display_name: Some("Alex".to_string()),
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-005",
            Kind::Valid,
            set(card_value(), "display_name", Value::from("  ")),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-005",
            Kind::Boundary,
            set(card_value(), "display_name", name(DISPLAY_NAME_MAX_CHARS)),
            accepted(StatusCard {
                display_name: Some(text(DISPLAY_NAME_MAX_CHARS)),
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-005",
            Kind::Boundary,
            set(
                card_value(),
                "display_name",
                name(DISPLAY_NAME_MAX_CHARS + 1),
            ),
            accepted(StatusCard {
                display_name: Some(text(DISPLAY_NAME_MAX_CHARS)),
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-005",
            Kind::Boundary,
            set(
                card_value(),
                "display_name",
                Value::from("é".repeat(DISPLAY_NAME_MAX_CHARS + 1)),
            ),
            accepted(StatusCard {
                display_name: Some("é".repeat(DISPLAY_NAME_MAX_CHARS)),
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-006",
            Kind::Invalid,
            set(card_value(), "objective", Value::Binary(b"ship".to_vec())),
            malformed("`objective` is not a string"),
        ),
        card(
            "MESH-STATUS-006",
            Kind::Invalid,
            set(card_value(), "objective", Value::from(7)),
            malformed("`objective` is not a string"),
        ),
        card(
            "MESH-STATUS-006",
            Kind::Valid,
            set(card_value(), "objective", Value::Nil),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-006",
            Kind::Valid,
            set(card_value(), "objective", Value::from("ship it")),
            accepted(StatusCard {
                objective: Some("ship it".to_string()),
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-006",
            Kind::Boundary,
            set(card_value(), "objective", name(OBJECTIVE_MAX_CHARS)),
            accepted(StatusCard {
                objective: Some(text(OBJECTIVE_MAX_CHARS)),
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-006",
            Kind::Boundary,
            set(card_value(), "objective", name(OBJECTIVE_MAX_CHARS + 1)),
            accepted(StatusCard {
                objective: Some(text(OBJECTIVE_MAX_CHARS)),
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-007",
            Kind::Invalid,
            without(card_value(), "state"),
            malformed("`state` is missing"),
        ),
        card(
            "MESH-STATUS-007",
            Kind::Invalid,
            set(card_value(), "state", Value::Nil),
            malformed("`state` is missing"),
        ),
        card(
            "MESH-STATUS-007",
            Kind::Invalid,
            set(card_value(), "state", Value::from(1)),
            malformed("`state` is not a map"),
        ),
        card(
            "MESH-STATUS-007",
            Kind::Invalid,
            set(card_value(), "state", Value::Array(vec![Value::from(1)])),
            malformed("`state` is not a map"),
        ),
        card(
            "MESH-STATUS-007",
            Kind::Invalid,
            set(card_value(), "state", Value::from("idle")),
            malformed("`state` is not a map"),
        ),
        card(
            "MESH-STATUS-007",
            Kind::Valid,
            set(card_value(), "state", state(Value::from(STATE_WORKING))),
            accepted(StatusCard {
                state: CardState {
                    code: STATE_WORKING,
                    since_secs: None,
                },
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-008",
            Kind::Invalid,
            set(card_value(), "repo", Value::from("coyote")),
            malformed("`repo` is not a map"),
        ),
        card(
            "MESH-STATUS-008",
            Kind::Invalid,
            set(card_value(), "repo", Value::Array(vec![])),
            malformed("`repo` is not a map"),
        ),
        card(
            "MESH-STATUS-008",
            Kind::Valid,
            set(card_value(), "repo", Value::Nil),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-008",
            Kind::Valid,
            card_sub(
                "repo",
                vec![
                    ("name", Value::from("coyote")),
                    ("branch", Value::from("main")),
                ],
            ),
            accepted(with_repo("coyote", Some("main"))),
        ),
        card(
            "MESH-STATUS-009",
            Kind::Invalid,
            set(card_value(), "plan", Value::from("title")),
            malformed("`plan` is not a map"),
        ),
        card(
            "MESH-STATUS-009",
            Kind::Invalid,
            set(card_value(), "plan", Value::from(1)),
            malformed("`plan` is not a map"),
        ),
        card(
            "MESH-STATUS-009",
            Kind::Valid,
            set(card_value(), "plan", Value::Nil),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-009",
            Kind::Valid,
            card_sub("plan", vec![("title", Value::from("Ship"))]),
            accepted(with_plan("Ship")),
        ),
        card(
            "MESH-STATUS-010",
            Kind::Invalid,
            set(card_value(), "todo", Value::from("goal")),
            malformed("`todo` is not a map"),
        ),
        card(
            "MESH-STATUS-010",
            Kind::Invalid,
            set(card_value(), "todo", Value::Array(vec![])),
            malformed("`todo` is not a map"),
        ),
        card(
            "MESH-STATUS-010",
            Kind::Valid,
            set(card_value(), "todo", Value::Nil),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-010",
            Kind::Valid,
            todo_map(
                Some(Value::from("ship")),
                Value::from(1u32),
                Value::from(2u32),
            ),
            accepted(with_todo(Some("ship"), 1, 2)),
        ),
        card(
            "MESH-STATUS-011",
            Kind::Invalid,
            set(card_value(), "snapshot_age_secs", Value::from("5")),
            malformed("`snapshot_age_secs` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-011",
            Kind::Invalid,
            set(card_value(), "snapshot_age_secs", Value::from(-1)),
            malformed("`snapshot_age_secs` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-011",
            Kind::Invalid,
            set(card_value(), "snapshot_age_secs", Value::F64(5.0)),
            malformed("`snapshot_age_secs` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-011",
            Kind::Valid,
            set(card_value(), "snapshot_age_secs", Value::Nil),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-011",
            Kind::Valid,
            set(card_value(), "snapshot_age_secs", Value::from(5u64)),
            accepted(StatusCard {
                snapshot_age_secs: Some(5),
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-011",
            Kind::Boundary,
            set(card_value(), "snapshot_age_secs", Value::from(0u64)),
            accepted(StatusCard {
                snapshot_age_secs: Some(0),
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-011",
            Kind::Boundary,
            set(card_value(), "snapshot_age_secs", Value::from(u64::MAX)),
            accepted(StatusCard {
                snapshot_age_secs: Some(u64::MAX),
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-012",
            Kind::Invalid,
            without(card_value(), "served_at_secs"),
            malformed("`served_at_secs` is missing"),
        ),
        card(
            "MESH-STATUS-012",
            Kind::Invalid,
            set(card_value(), "served_at_secs", Value::Nil),
            malformed("`served_at_secs` is missing"),
        ),
        card(
            "MESH-STATUS-012",
            Kind::Invalid,
            set(card_value(), "served_at_secs", Value::from("now")),
            malformed("`served_at_secs` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-012",
            Kind::Invalid,
            set(card_value(), "served_at_secs", Value::from(-1)),
            malformed("`served_at_secs` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-012",
            Kind::Invalid,
            set(card_value(), "served_at_secs", Value::F64(1.7e9)),
            malformed("`served_at_secs` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-012",
            Kind::Boundary,
            set(card_value(), "served_at_secs", Value::from(0u64)),
            accepted(StatusCard {
                served_at_secs: 0,
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-012",
            Kind::Boundary,
            set(card_value(), "served_at_secs", Value::from(u64::MAX)),
            accepted(StatusCard {
                served_at_secs: u64::MAX,
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-013",
            Kind::Valid,
            with(card_value(), "unknown", Value::from(1)),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-013",
            Kind::Valid,
            with(
                with(card_value(), "mood", Value::from("fine")),
                "tags",
                Value::Array(vec![Value::from("a")]),
            ),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-013",
            Kind::Valid,
            with(card_value(), "state2", Value::from("working")),
            accepted(minimal_card()),
        ),
        row(
            "MESH-STATUS-014",
            Kind::Valid,
            Case::CardEncode {
                card: maximal_card(),
                expect: map(vec![
                    ("v", Value::from(1u64)),
                    ("display_name", Value::from(text(DISPLAY_NAME_MAX_CHARS))),
                    ("objective", Value::from(text(OBJECTIVE_MAX_CHARS))),
                    (
                        "state",
                        map(vec![
                            ("code", Value::from(STATE_WORKING)),
                            ("since_secs", Value::from(60u64)),
                        ]),
                    ),
                    (
                        "repo",
                        map(vec![
                            ("name", Value::from(text(REPO_NAME_MAX_CHARS))),
                            ("branch", Value::from(text(BRANCH_MAX_CHARS))),
                        ]),
                    ),
                    (
                        "plan",
                        map(vec![("title", Value::from(text(PLAN_TITLE_MAX_CHARS)))]),
                    ),
                    (
                        "todo",
                        map(vec![
                            ("goal", Value::from(text(TODO_GOAL_MAX_CHARS))),
                            ("done", Value::from(3u32)),
                            ("total", Value::from(7u32)),
                        ]),
                    ),
                    ("about", Value::from(text(ABOUT_MAX_CHARS))),
                    ("caps", Value::Array(vec![Value::from("fetch")])),
                    ("snapshot_age_secs", Value::from(5u64)),
                    ("served_at_secs", Value::from(SERVED_AT)),
                ]),
            },
        ),
        row(
            "MESH-STATUS-014",
            Kind::Valid,
            Case::CardEncode {
                card: StatusCard {
                    objective: Some("ship".to_string()),
                    repo: Some(repo("coyote", None)),
                    todo: Some(CardTodo {
                        goal: None,
                        done: 0,
                        total: 0,
                    }),
                    ..minimal_card()
                },
                expect: map(vec![
                    ("v", Value::from(1u64)),
                    ("objective", Value::from("ship")),
                    ("state", map(vec![("code", Value::from(STATE_IDLE))])),
                    ("repo", map(vec![("name", Value::from("coyote"))])),
                    (
                        "todo",
                        map(vec![
                            ("done", Value::from(0u32)),
                            ("total", Value::from(0u32)),
                        ]),
                    ),
                    ("served_at_secs", Value::from(SERVED_AT)),
                ]),
            },
        ),
        row(
            "MESH-STATUS-015",
            Kind::Valid,
            Case::CardEncode {
                card: minimal_card(),
                expect: card_value(),
            },
        ),
        row(
            "MESH-STATUS-015",
            Kind::Valid,
            Case::CardEncode {
                card: with_repo("coyote", None),
                expect: map(vec![
                    ("v", Value::from(1u64)),
                    ("state", map(vec![("code", Value::from(STATE_IDLE))])),
                    ("repo", map(vec![("name", Value::from("coyote"))])),
                    ("served_at_secs", Value::from(SERVED_AT)),
                ]),
            },
        ),
        card(
            "MESH-STATUS-016",
            Kind::Valid,
            set(
                set(
                    set(card_value(), "display_name", Value::Nil),
                    "repo",
                    Value::Nil,
                ),
                "snapshot_age_secs",
                Value::Nil,
            ),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-016",
            Kind::Valid,
            set(
                card_value(),
                "state",
                map(vec![
                    ("code", Value::from(STATE_IDLE)),
                    ("since_secs", Value::Nil),
                ]),
            ),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-016",
            Kind::Invalid,
            set(card_value(), "v", Value::Nil),
            malformed("`v` is missing"),
        ),
        card(
            "MESH-STATUS-016",
            Kind::Invalid,
            set(card_value(), "state", Value::Nil),
            malformed("`state` is missing"),
        ),
        card(
            "MESH-STATUS-016",
            Kind::Invalid,
            set(card_value(), "served_at_secs", Value::Nil),
            malformed("`served_at_secs` is missing"),
        ),
        card(
            "MESH-STATUS-016",
            Kind::Invalid,
            set(card_value(), "state", map(vec![("code", Value::Nil)])),
            malformed("`state.code` is missing or not a byte"),
        ),
        card(
            "MESH-STATUS-017",
            Kind::Invalid,
            set(card_value(), "state", Value::Map(vec![])),
            malformed("`state.code` is missing or not a byte"),
        ),
        card(
            "MESH-STATUS-017",
            Kind::Invalid,
            set(card_value(), "state", state(Value::Nil)),
            malformed("`state.code` is missing or not a byte"),
        ),
        card(
            "MESH-STATUS-017",
            Kind::Invalid,
            set(card_value(), "state", state(Value::from(256u64))),
            malformed("`state.code` is missing or not a byte"),
        ),
        card(
            "MESH-STATUS-017",
            Kind::Invalid,
            set(card_value(), "state", state(Value::from("1"))),
            malformed("`code` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-017",
            Kind::Invalid,
            set(card_value(), "state", state(Value::from(-1))),
            malformed("`code` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-017",
            Kind::Invalid,
            set(card_value(), "state", state(Value::Boolean(true))),
            malformed("`code`"),
        ),
        card(
            "MESH-STATUS-017",
            Kind::Valid,
            set(card_value(), "state", state(Value::from(STATE_UNKNOWN))),
            accepted(StatusCard {
                state: CardState {
                    code: 0,
                    since_secs: None,
                },
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-017",
            Kind::Valid,
            set(card_value(), "state", state(Value::from(STATE_WORKING))),
            accepted(StatusCard {
                state: CardState {
                    code: 2,
                    since_secs: None,
                },
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-017",
            Kind::Boundary,
            set(card_value(), "state", state(Value::from(255u64))),
            accepted(StatusCard {
                state: CardState {
                    code: 255,
                    since_secs: None,
                },
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-018",
            Kind::Invalid,
            set(
                card_value(),
                "state",
                map(vec![
                    ("code", Value::from(STATE_IDLE)),
                    ("since_secs", Value::from("60")),
                ]),
            ),
            malformed("`since_secs` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-018",
            Kind::Invalid,
            set(
                card_value(),
                "state",
                map(vec![
                    ("code", Value::from(STATE_IDLE)),
                    ("since_secs", Value::from(-60)),
                ]),
            ),
            malformed("`since_secs` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-018",
            Kind::Valid,
            card_value(),
            accepted(with_state(None)),
        ),
        card(
            "MESH-STATUS-018",
            Kind::Valid,
            set(
                card_value(),
                "state",
                map(vec![
                    ("code", Value::from(STATE_IDLE)),
                    ("since_secs", Value::from(60u64)),
                ]),
            ),
            accepted(with_state(Some(60))),
        ),
        card(
            "MESH-STATUS-018",
            Kind::Boundary,
            set(
                card_value(),
                "state",
                map(vec![
                    ("code", Value::from(STATE_IDLE)),
                    ("since_secs", Value::from(u64::MAX)),
                ]),
            ),
            accepted(with_state(Some(u64::MAX))),
        ),
        card(
            "MESH-STATUS-019",
            Kind::Valid,
            set(
                card_value(),
                "state",
                map(vec![
                    ("code", Value::from(STATE_IDLE)),
                    ("mood", Value::from("fine")),
                ]),
            ),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-019",
            Kind::Valid,
            set(
                card_value(),
                "state",
                map(vec![
                    ("label", Value::from("idle")),
                    ("code", Value::from(STATE_IDLE)),
                ]),
            ),
            accepted(minimal_card()),
        ),
        card(
            "MESH-STATUS-020",
            Kind::Invalid,
            card_sub("repo", vec![]),
            malformed("`repo.name` is missing or blank"),
        ),
        card(
            "MESH-STATUS-020",
            Kind::Invalid,
            card_sub("repo", vec![("name", Value::Nil)]),
            malformed("`repo.name` is missing or blank"),
        ),
        card(
            "MESH-STATUS-020",
            Kind::Invalid,
            card_sub("repo", vec![("name", Value::from("  "))]),
            malformed("`repo.name` is missing or blank"),
        ),
        card(
            "MESH-STATUS-020",
            Kind::Invalid,
            card_sub("repo", vec![("name", Value::from("\u{200B}\u{1b}[2J"))]),
            malformed("`repo.name` is missing or blank"),
        ),
        card(
            "MESH-STATUS-020",
            Kind::Invalid,
            card_sub("repo", vec![("name", Value::from(7))]),
            malformed("`name` is not a string"),
        ),
        card(
            "MESH-STATUS-020",
            Kind::Invalid,
            card_sub("repo", vec![("name", Value::Binary(b"coyote".to_vec()))]),
            malformed("`name` is not a string"),
        ),
        card(
            "MESH-STATUS-020",
            Kind::Valid,
            card_sub("repo", vec![("name", Value::from("coyote"))]),
            accepted(with_repo("coyote", None)),
        ),
        card(
            "MESH-STATUS-020",
            Kind::Boundary,
            card_sub("repo", vec![("name", name(REPO_NAME_MAX_CHARS))]),
            accepted(with_repo(&text(REPO_NAME_MAX_CHARS), None)),
        ),
        card(
            "MESH-STATUS-020",
            Kind::Boundary,
            card_sub("repo", vec![("name", name(REPO_NAME_MAX_CHARS + 1))]),
            accepted(with_repo(&text(REPO_NAME_MAX_CHARS), None)),
        ),
        card(
            "MESH-STATUS-021",
            Kind::Invalid,
            card_sub(
                "repo",
                vec![("name", Value::from("coyote")), ("branch", Value::from(7))],
            ),
            malformed("`branch` is not a string"),
        ),
        card(
            "MESH-STATUS-021",
            Kind::Invalid,
            card_sub(
                "repo",
                vec![
                    ("name", Value::from("coyote")),
                    ("branch", Value::Binary(b"main".to_vec())),
                ],
            ),
            malformed("`branch` is not a string"),
        ),
        card(
            "MESH-STATUS-021",
            Kind::Valid,
            card_sub(
                "repo",
                vec![("name", Value::from("coyote")), ("branch", Value::Nil)],
            ),
            accepted(with_repo("coyote", None)),
        ),
        card(
            "MESH-STATUS-021",
            Kind::Valid,
            card_sub(
                "repo",
                vec![
                    ("name", Value::from("coyote")),
                    ("branch", Value::from("  ")),
                ],
            ),
            accepted(with_repo("coyote", None)),
        ),
        card(
            "MESH-STATUS-021",
            Kind::Valid,
            card_sub(
                "repo",
                vec![
                    ("name", Value::from("coyote")),
                    ("branch", Value::from("main")),
                ],
            ),
            accepted(with_repo("coyote", Some("main"))),
        ),
        card(
            "MESH-STATUS-021",
            Kind::Boundary,
            card_sub(
                "repo",
                vec![
                    ("name", Value::from("coyote")),
                    ("branch", name(BRANCH_MAX_CHARS)),
                ],
            ),
            accepted(with_repo("coyote", Some(text(BRANCH_MAX_CHARS).as_str()))),
        ),
        card(
            "MESH-STATUS-021",
            Kind::Boundary,
            card_sub(
                "repo",
                vec![
                    ("name", Value::from("coyote")),
                    ("branch", name(BRANCH_MAX_CHARS + 1)),
                ],
            ),
            accepted(with_repo("coyote", Some(text(BRANCH_MAX_CHARS).as_str()))),
        ),
        card(
            "MESH-STATUS-022",
            Kind::Valid,
            card_sub(
                "repo",
                vec![
                    ("name", Value::from("coyote")),
                    ("remote", Value::from("origin")),
                ],
            ),
            accepted(with_repo("coyote", None)),
        ),
        card(
            "MESH-STATUS-022",
            Kind::Valid,
            card_sub(
                "repo",
                vec![
                    ("path", Value::from("/home")),
                    ("name", Value::from("coyote")),
                ],
            ),
            accepted(with_repo("coyote", None)),
        ),
        card(
            "MESH-STATUS-023",
            Kind::Invalid,
            card_sub("plan", vec![]),
            malformed("`plan.title` is missing or blank"),
        ),
        card(
            "MESH-STATUS-023",
            Kind::Invalid,
            card_sub("plan", vec![("title", Value::Nil)]),
            malformed("`plan.title` is missing or blank"),
        ),
        card(
            "MESH-STATUS-023",
            Kind::Invalid,
            card_sub("plan", vec![("title", Value::from(" \t "))]),
            malformed("`plan.title` is missing or blank"),
        ),
        card(
            "MESH-STATUS-023",
            Kind::Invalid,
            card_sub("plan", vec![("title", Value::from(7))]),
            malformed("`title` is not a string"),
        ),
        card(
            "MESH-STATUS-023",
            Kind::Invalid,
            card_sub("plan", vec![("title", Value::Binary(b"Ship".to_vec()))]),
            malformed("`title` is not a string"),
        ),
        card(
            "MESH-STATUS-023",
            Kind::Valid,
            card_sub("plan", vec![("title", Value::from("Ship"))]),
            accepted(with_plan("Ship")),
        ),
        card(
            "MESH-STATUS-023",
            Kind::Boundary,
            card_sub("plan", vec![("title", name(PLAN_TITLE_MAX_CHARS))]),
            accepted(with_plan(&text(PLAN_TITLE_MAX_CHARS))),
        ),
        card(
            "MESH-STATUS-023",
            Kind::Boundary,
            card_sub("plan", vec![("title", name(PLAN_TITLE_MAX_CHARS + 1))]),
            accepted(with_plan(&text(PLAN_TITLE_MAX_CHARS))),
        ),
        card(
            "MESH-STATUS-024",
            Kind::Valid,
            card_sub(
                "plan",
                vec![("title", Value::from("Ship")), ("steps", Value::from(3))],
            ),
            accepted(with_plan("Ship")),
        ),
        card(
            "MESH-STATUS-024",
            Kind::Valid,
            card_sub(
                "plan",
                vec![
                    ("path", Value::from("plans/x.md")),
                    ("title", Value::from("Ship")),
                ],
            ),
            accepted(with_plan("Ship")),
        ),
        card(
            "MESH-STATUS-025",
            Kind::Invalid,
            todo_map(Some(Value::from(7)), Value::from(1u32), Value::from(2u32)),
            malformed("`goal` is not a string"),
        ),
        card(
            "MESH-STATUS-025",
            Kind::Invalid,
            todo_map(
                Some(Value::Binary(b"ship".to_vec())),
                Value::from(1u32),
                Value::from(2u32),
            ),
            malformed("`goal` is not a string"),
        ),
        card(
            "MESH-STATUS-025",
            Kind::Valid,
            todo_map(None, Value::from(1u32), Value::from(2u32)),
            accepted(with_todo(None, 1, 2)),
        ),
        card(
            "MESH-STATUS-025",
            Kind::Valid,
            todo_map(Some(Value::Nil), Value::from(1u32), Value::from(2u32)),
            accepted(with_todo(None, 1, 2)),
        ),
        card(
            "MESH-STATUS-025",
            Kind::Valid,
            todo_map(
                Some(Value::from("ship")),
                Value::from(1u32),
                Value::from(2u32),
            ),
            accepted(with_todo(Some("ship"), 1, 2)),
        ),
        card(
            "MESH-STATUS-025",
            Kind::Boundary,
            todo_map(
                Some(name(TODO_GOAL_MAX_CHARS)),
                Value::from(1u32),
                Value::from(2u32),
            ),
            accepted(with_todo(Some(text(TODO_GOAL_MAX_CHARS).as_str()), 1, 2)),
        ),
        card(
            "MESH-STATUS-025",
            Kind::Boundary,
            todo_map(
                Some(name(TODO_GOAL_MAX_CHARS + 1)),
                Value::from(1u32),
                Value::from(2u32),
            ),
            accepted(with_todo(Some(text(TODO_GOAL_MAX_CHARS).as_str()), 1, 2)),
        ),
        card(
            "MESH-STATUS-026",
            Kind::Invalid,
            card_sub("todo", vec![("total", Value::from(2u32))]),
            malformed("`done` is missing or not a 32-bit count"),
        ),
        card(
            "MESH-STATUS-026",
            Kind::Invalid,
            todo_map(None, Value::Nil, Value::from(2u32)),
            malformed("`done` is missing or not a 32-bit count"),
        ),
        card(
            "MESH-STATUS-026",
            Kind::Invalid,
            todo_map(None, Value::from(1u64 << 32), Value::from(2u32)),
            malformed("`done` is missing or not a 32-bit count"),
        ),
        card(
            "MESH-STATUS-026",
            Kind::Invalid,
            todo_map(None, Value::from("1"), Value::from(2u32)),
            malformed("`done` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-026",
            Kind::Invalid,
            todo_map(None, Value::from(-1), Value::from(2u32)),
            malformed("`done` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-026",
            Kind::Boundary,
            todo_map(None, Value::from(u32::MAX), Value::from(u32::MAX)),
            accepted(with_todo(None, u32::MAX, u32::MAX)),
        ),
        card(
            "MESH-STATUS-026",
            Kind::Boundary,
            todo_map(None, Value::from(0u32), Value::from(0u32)),
            accepted(with_todo(None, 0, 0)),
        ),
        card(
            "MESH-STATUS-027",
            Kind::Invalid,
            card_sub("todo", vec![("done", Value::from(1u32))]),
            malformed("`total` is missing or not a 32-bit count"),
        ),
        card(
            "MESH-STATUS-027",
            Kind::Invalid,
            todo_map(None, Value::from(1u32), Value::Nil),
            malformed("`total` is missing or not a 32-bit count"),
        ),
        card(
            "MESH-STATUS-027",
            Kind::Invalid,
            todo_map(None, Value::from(1u32), Value::from(1u64 << 32)),
            malformed("`total` is missing or not a 32-bit count"),
        ),
        card(
            "MESH-STATUS-027",
            Kind::Invalid,
            todo_map(None, Value::from(1u32), Value::from("2")),
            malformed("`total` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-027",
            Kind::Invalid,
            todo_map(None, Value::from(1u32), Value::from(-2)),
            malformed("`total` is not a non-negative integer"),
        ),
        card(
            "MESH-STATUS-027",
            Kind::Boundary,
            todo_map(None, Value::from(1u32), Value::from(u32::MAX)),
            accepted(with_todo(None, 1, u32::MAX)),
        ),
        card(
            "MESH-STATUS-027",
            Kind::Valid,
            todo_map(None, Value::from(5u32), Value::from(2u32)),
            accepted(with_todo(None, 5, 2)),
        ),
        card(
            "MESH-STATUS-028",
            Kind::Valid,
            card_sub(
                "todo",
                vec![
                    ("done", Value::from(1u32)),
                    ("total", Value::from(2u32)),
                    ("items", Value::Array(vec![])),
                ],
            ),
            accepted(with_todo(None, 1, 2)),
        ),
        card(
            "MESH-STATUS-028",
            Kind::Valid,
            card_sub(
                "todo",
                vec![
                    ("owner", Value::from("me")),
                    ("done", Value::from(1u32)),
                    ("total", Value::from(2u32)),
                ],
            ),
            accepted(with_todo(None, 1, 2)),
        ),
        card(
            "MESH-STATUS-029",
            Kind::Valid,
            set(card_value(), "state", state(Value::from(7u8))),
            accepted(StatusCard {
                state: CardState {
                    code: 7,
                    since_secs: None,
                },
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-029",
            Kind::Valid,
            set(card_value(), "state", state(Value::from(200u8))),
            accepted(StatusCard {
                state: CardState {
                    code: 200,
                    since_secs: None,
                },
                ..minimal_card()
            }),
        ),
        card(
            "MESH-STATUS-029",
            Kind::Boundary,
            set(card_value(), "state", state(Value::from(3u8))),
            accepted(StatusCard {
                state: CardState {
                    code: 3,
                    since_secs: None,
                },
                ..minimal_card()
            }),
        ),
        row(
            "MESH-STATUS-030",
            Kind::Boundary,
            Case::Card {
                value: set(
                    set(
                        set(card_value(), "served_at_secs", Value::from(u64::MAX)),
                        "state",
                        map(vec![
                            ("code", Value::from(STATE_WORKING)),
                            ("since_secs", Value::from(u64::MAX)),
                        ]),
                    ),
                    "snapshot_age_secs",
                    Value::from(u64::MAX),
                ),
                expect: accepted(StatusCard {
                    state: CardState {
                        code: STATE_WORKING,
                        since_secs: Some(u64::MAX),
                    },
                    snapshot_age_secs: Some(u64::MAX),
                    served_at_secs: u64::MAX,
                    ..minimal_card()
                }),
            },
        ),
        card(
            "MESH-STATUS-030",
            Kind::Valid,
            todo_map(None, Value::from(9u32), Value::from(1u32)),
            accepted(with_todo(None, 9, 1)),
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Section 10: the message body, the acknowledgement, the typed refusal and the LXMF form
// ---------------------------------------------------------------------------------------

fn message(id: &'static str, kind: Kind, value: Value, expect: BodyAction) -> Vector {
    row(id, kind, Case::MessageBody { value, expect })
}

fn refused(reason: &'static str) -> BodyAction {
    BodyAction::InvalidData(reason)
}

fn ack(id: &'static str, kind: Kind, value: Value, expect: bool) -> Vector {
    row(
        id,
        kind,
        Case::Ack {
            value,
            id: "m-1",
            expect,
        },
    )
}

fn lxmf_peer(id: &'static str, kind: Kind, message: InboundMessage, expect: PeerLxmf) -> Vector {
    row(id, kind, Case::LxmfPeer { message, expect })
}

fn peer_malformed(reason: &'static str) -> PeerLxmf {
    PeerLxmf::Malformed(reason)
}

fn outbound_row(
    id: &'static str,
    kind: Kind,
    content: &str,
    title: Option<&str>,
    in_reply_to: Option<&'static str>,
    fields: Option<serde_json::Value>,
    expect: OutboundAction,
) -> Vector {
    row(
        id,
        kind,
        Case::Outbound {
            kind: PeerKind::Message,
            content: content.to_string(),
            title: title.map(str::to_string),
            in_reply_to,
            fields,
            expect,
        },
    )
}

fn minted(content: &str, title: Option<&str>, in_reply_to: Option<&str>) -> OutboundAction {
    OutboundAction::Minted {
        content: content.to_string(),
        title: title.map(str::to_string),
        in_reply_to: in_reply_to.map(str::to_string),
    }
}

const V_ERR: &str = "v is missing or not the supported version";
const KIND_ERR: &str = "kind is missing or unknown";
const ID_ERR: &str = "id is missing, blank, too long or outside the id alphabet";
const IN_REPLY_TO_ERR: &str = "in_reply_to is not a message id";
const TITLE_ERR: &str = "title is not text or is too long";
const CONTENT_ERR: &str = "content is missing, not text or too long";
const FIELDS_ERR: &str = "fields is not a map";
const TS_ERR: &str = "ts is missing or not a finite number";

fn message_vectors() -> Vec<Vector> {
    let sixty_four = text(PEER_ID_MAX_CHARS);
    let all_alphabet = "aZ09_.:-";
    vec![
        message(
            "MESH-MSG-001",
            Kind::Invalid,
            without(body_value(), "v"),
            refused(V_ERR),
        ),
        message(
            "MESH-MSG-001",
            Kind::Invalid,
            set(body_value(), "v", Value::Nil),
            refused(V_ERR),
        ),
        message(
            "MESH-MSG-001",
            Kind::Invalid,
            set(body_value(), "v", Value::from(2u64)),
            refused(V_ERR),
        ),
        message(
            "MESH-MSG-001",
            Kind::Invalid,
            set(body_value(), "v", Value::from(0u64)),
            refused(V_ERR),
        ),
        message(
            "MESH-MSG-001",
            Kind::Invalid,
            set(body_value(), "v", Value::from("1")),
            refused(V_ERR),
        ),
        message(
            "MESH-MSG-001",
            Kind::Invalid,
            set(body_value(), "v", Value::F64(1.0)),
            refused(V_ERR),
        ),
        message(
            "MESH-MSG-001",
            Kind::Invalid,
            set(body_value(), "v", Value::from(-1)),
            refused(V_ERR),
        ),
        message(
            "MESH-MSG-001",
            Kind::Boundary,
            set(body_value(), "v", Value::from(1u64)),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-002",
            Kind::Invalid,
            without(body_value(), "kind"),
            refused(KIND_ERR),
        ),
        message(
            "MESH-MSG-002",
            Kind::Invalid,
            set(body_value(), "kind", Value::Nil),
            refused(KIND_ERR),
        ),
        message(
            "MESH-MSG-002",
            Kind::Invalid,
            set(body_value(), "kind", Value::from("shout")),
            refused(KIND_ERR),
        ),
        message(
            "MESH-MSG-002",
            Kind::Invalid,
            set(body_value(), "kind", Value::from("Message")),
            refused(KIND_ERR),
        ),
        message(
            "MESH-MSG-002",
            Kind::Invalid,
            set(body_value(), "kind", Value::from("message ")),
            refused(KIND_ERR),
        ),
        message(
            "MESH-MSG-002",
            Kind::Invalid,
            set(body_value(), "kind", Value::from(1)),
            refused(KIND_ERR),
        ),
        message(
            "MESH-MSG-002",
            Kind::Invalid,
            set(body_value(), "kind", Value::from("")),
            refused(KIND_ERR),
        ),
        message(
            "MESH-MSG-002",
            Kind::Valid,
            set(body_value(), "kind", Value::from("message")),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-002",
            Kind::Valid,
            set(body_value(), "kind", Value::from("ask")),
            accepted_body(PeerBody {
                kind: PeerKind::Ask,
                ..body()
            }),
        ),
        message(
            "MESH-MSG-002",
            Kind::Valid,
            set(body_value(), "kind", Value::from("reply")),
            accepted_body(PeerBody {
                kind: PeerKind::Reply,
                disposition: Some(Disposition::Answered),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-002",
            Kind::Valid,
            set(body_value(), "kind", Value::from("bulletin")),
            accepted_body(PeerBody {
                kind: PeerKind::Bulletin,
                ..body()
            }),
        ),
        message(
            "MESH-MSG-002",
            Kind::Valid,
            set(body_value(), "kind", Value::Binary(b"bulletin".to_vec())),
            accepted_body(PeerBody {
                kind: PeerKind::Bulletin,
                ..body()
            }),
        ),
        message(
            "MESH-MSG-003",
            Kind::Invalid,
            without(body_value(), "id"),
            refused(ID_ERR),
        ),
        message(
            "MESH-MSG-003",
            Kind::Invalid,
            set(body_value(), "id", Value::Nil),
            refused(ID_ERR),
        ),
        message(
            "MESH-MSG-003",
            Kind::Invalid,
            set(body_value(), "id", Value::from("")),
            refused(ID_ERR),
        ),
        message(
            "MESH-MSG-003",
            Kind::Invalid,
            set(body_value(), "id", Value::from(text(PEER_ID_MAX_CHARS + 1))),
            refused(ID_ERR),
        ),
        message(
            "MESH-MSG-003",
            Kind::Invalid,
            set(body_value(), "id", Value::from("has space")),
            refused(ID_ERR),
        ),
        message(
            "MESH-MSG-003",
            Kind::Invalid,
            set(body_value(), "id", Value::from("ünïcode")),
            refused(ID_ERR),
        ),
        message(
            "MESH-MSG-003",
            Kind::Invalid,
            set(body_value(), "id", Value::from("id/1")),
            refused(ID_ERR),
        ),
        message(
            "MESH-MSG-003",
            Kind::Invalid,
            set(body_value(), "id", Value::from(7)),
            refused(ID_ERR),
        ),
        message(
            "MESH-MSG-003",
            Kind::Boundary,
            set(body_value(), "id", Value::from(sixty_four.as_str())),
            accepted_body(PeerBody {
                id: sixty_four.clone(),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-003",
            Kind::Boundary,
            set(body_value(), "id", Value::from("a")),
            accepted_body(PeerBody {
                id: "a".to_string(),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-003",
            Kind::Valid,
            set(body_value(), "id", Value::from(all_alphabet)),
            accepted_body(PeerBody {
                id: all_alphabet.to_string(),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-003",
            Kind::Valid,
            set(
                body_value(),
                "id",
                Value::from("0123456789abcdef0123456789abcdef"),
            ),
            accepted_body(PeerBody {
                id: "0123456789abcdef0123456789abcdef".to_string(),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-004",
            Kind::Invalid,
            set(body_value(), "in_reply_to", Value::from("")),
            refused(IN_REPLY_TO_ERR),
        ),
        message(
            "MESH-MSG-004",
            Kind::Invalid,
            set(
                body_value(),
                "in_reply_to",
                Value::from(text(PEER_ID_MAX_CHARS + 1)),
            ),
            refused(IN_REPLY_TO_ERR),
        ),
        message(
            "MESH-MSG-004",
            Kind::Invalid,
            set(body_value(), "in_reply_to", Value::from("a b")),
            refused(IN_REPLY_TO_ERR),
        ),
        message(
            "MESH-MSG-004",
            Kind::Invalid,
            set(body_value(), "in_reply_to", Value::from(7)),
            refused(IN_REPLY_TO_ERR),
        ),
        message(
            "MESH-MSG-004",
            Kind::Invalid,
            set(body_value(), "in_reply_to", Value::Array(vec![])),
            refused(IN_REPLY_TO_ERR),
        ),
        message(
            "MESH-MSG-004",
            Kind::Valid,
            body_value(),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-004",
            Kind::Valid,
            set(body_value(), "in_reply_to", Value::Nil),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-004",
            Kind::Valid,
            set(body_value(), "in_reply_to", Value::from("m-0")),
            accepted_body(PeerBody {
                in_reply_to: Some("m-0".to_string()),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-004",
            Kind::Boundary,
            set(
                body_value(),
                "in_reply_to",
                Value::from(sixty_four.as_str()),
            ),
            accepted_body(PeerBody {
                in_reply_to: Some(sixty_four.clone()),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-005",
            Kind::Invalid,
            set(body_value(), "title", Value::from(7)),
            refused(TITLE_ERR),
        ),
        message(
            "MESH-MSG-005",
            Kind::Invalid,
            set(body_value(), "title", Value::Boolean(true)),
            refused(TITLE_ERR),
        ),
        message(
            "MESH-MSG-005",
            Kind::Invalid,
            set(
                body_value(),
                "title",
                Value::from(text(PEER_TITLE_MAX_CHARS + 1)),
            ),
            refused(TITLE_ERR),
        ),
        message(
            "MESH-MSG-005",
            Kind::Invalid,
            set(
                body_value(),
                "title",
                Value::from("é".repeat(PEER_TITLE_MAX_CHARS + 1)),
            ),
            refused(TITLE_ERR),
        ),
        message(
            "MESH-MSG-005",
            Kind::Valid,
            set(body_value(), "title", Value::Nil),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-005",
            Kind::Valid,
            set(body_value(), "title", Value::from("Re: hi")),
            accepted_body(PeerBody {
                title: Some("Re: hi".to_string()),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-005",
            Kind::Valid,
            set(body_value(), "title", Value::from("")),
            accepted_body(PeerBody {
                title: Some(String::new()),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-005",
            Kind::Boundary,
            set(
                body_value(),
                "title",
                Value::from(text(PEER_TITLE_MAX_CHARS)),
            ),
            accepted_body(PeerBody {
                title: Some(text(PEER_TITLE_MAX_CHARS)),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-005",
            Kind::Boundary,
            set(
                body_value(),
                "title",
                Value::from("é".repeat(PEER_TITLE_MAX_CHARS)),
            ),
            accepted_body(PeerBody {
                title: Some("é".repeat(PEER_TITLE_MAX_CHARS)),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-006",
            Kind::Invalid,
            without(body_value(), "content"),
            refused(CONTENT_ERR),
        ),
        message(
            "MESH-MSG-006",
            Kind::Invalid,
            set(body_value(), "content", Value::Nil),
            refused(CONTENT_ERR),
        ),
        message(
            "MESH-MSG-006",
            Kind::Invalid,
            set(body_value(), "content", Value::from(7)),
            refused(CONTENT_ERR),
        ),
        message(
            "MESH-MSG-006",
            Kind::Invalid,
            set(
                body_value(),
                "content",
                Value::Array(vec![Value::from("hi")]),
            ),
            refused(CONTENT_ERR),
        ),
        message(
            "MESH-MSG-006",
            Kind::Invalid,
            set(
                body_value(),
                "content",
                Value::from(text(PEER_CONTENT_MAX_CHARS + 1)),
            ),
            refused(CONTENT_ERR),
        ),
        message(
            "MESH-MSG-006",
            Kind::Boundary,
            set(
                body_value(),
                "content",
                Value::from(text(PEER_CONTENT_MAX_CHARS)),
            ),
            accepted_body(PeerBody {
                content: text(PEER_CONTENT_MAX_CHARS),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-006",
            Kind::Boundary,
            set(body_value(), "content", Value::from("")),
            accepted_body(PeerBody {
                content: String::new(),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-006",
            Kind::Valid,
            set(body_value(), "content", Value::Binary(b"hi".to_vec())),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-007",
            Kind::Invalid,
            set(body_value(), "fields", Value::from(7)),
            refused(FIELDS_ERR),
        ),
        message(
            "MESH-MSG-007",
            Kind::Invalid,
            set(body_value(), "fields", Value::from("{}")),
            refused(FIELDS_ERR),
        ),
        message(
            "MESH-MSG-007",
            Kind::Invalid,
            set(body_value(), "fields", Value::Array(vec![])),
            refused(FIELDS_ERR),
        ),
        message(
            "MESH-MSG-007",
            Kind::Invalid,
            set(body_value(), "fields", Value::Boolean(false)),
            refused(FIELDS_ERR),
        ),
        message(
            "MESH-MSG-007",
            Kind::Valid,
            set(body_value(), "fields", Value::Nil),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-007",
            Kind::Valid,
            set(body_value(), "fields", Value::Map(vec![])),
            accepted_body(PeerBody {
                fields: Some(serde_json::json!({})),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-007",
            Kind::Valid,
            set(
                body_value(),
                "fields",
                map(vec![
                    ("n", Value::from(1)),
                    (
                        "list",
                        Value::Array(vec![
                            Value::Boolean(true),
                            Value::Nil,
                            Value::F64(2.5),
                            Value::from("s"),
                        ]),
                    ),
                    ("neg", Value::from(-3)),
                ]),
            ),
            accepted_body(PeerBody {
                fields: Some(
                    serde_json::json!({ "n": 1, "list": [true, null, 2.5, "s"], "neg": -3 }),
                ),
                ..body()
            }),
        ),
        message(
            "MESH-MSG-008",
            Kind::Invalid,
            set(
                body_value(),
                "fields",
                nested_maps(PEER_FIELDS_MAX_DEPTH + 1),
            ),
            accepted_body(PeerBody {
                fields: None,
                ..body()
            }),
        ),
        message(
            "MESH-MSG-008",
            Kind::Invalid,
            set(
                body_value(),
                "fields",
                nested_maps(PEER_FIELDS_MAX_DEPTH + 5),
            ),
            accepted_body(PeerBody {
                fields: None,
                ..body()
            }),
        ),
        row(
            "MESH-MSG-008",
            Kind::Boundary,
            Case::Custom(|| {
                let deepest = from_r3_body(&set(
                    body_value(),
                    "fields",
                    nested_maps(PEER_FIELDS_MAX_DEPTH),
                ))?;
                ensure(deepest.fields.is_some(), "fields at depth 8 were dropped")?;
                let leaf = (0..PEER_FIELDS_MAX_DEPTH - 1)
                    .fold(Value::from(1), |inner, _| map(vec![("n", inner)]));
                let with_leaf = from_r3_body(&set(body_value(), "fields", leaf))?;
                ensure(with_leaf.fields.is_some(), "a leaf at depth 8 was dropped")
            }),
        ),
        row(
            "MESH-MSG-008",
            Kind::Invalid,
            Case::Custom(|| {
                let big = serde_json::json!({ "blob": text(PEER_FIELDS_MAX_BYTES) });
                let kept = PeerMessage::new(raw(Some(big), None, "hi"));
                same("oversize fields dropped", kept.fields, None)?;
                same("the message kept", kept.content, "hi".to_string())?;
                let fits = serde_json::json!({ "blob": text(PEER_FIELDS_MAX_BYTES - 11) });
                let kept = PeerMessage::new(raw(Some(fits.clone()), None, "hi"));
                same("fields at the cap kept", kept.fields, Some(fits))
            }),
        ),
        message(
            "MESH-MSG-009",
            Kind::Invalid,
            without(body_value(), "ts"),
            refused(TS_ERR),
        ),
        message(
            "MESH-MSG-009",
            Kind::Invalid,
            set(body_value(), "ts", Value::Nil),
            refused(TS_ERR),
        ),
        message(
            "MESH-MSG-009",
            Kind::Invalid,
            set(body_value(), "ts", Value::from("1.5")),
            refused(TS_ERR),
        ),
        message(
            "MESH-MSG-009",
            Kind::Invalid,
            set(body_value(), "ts", Value::Boolean(true)),
            refused(TS_ERR),
        ),
        message(
            "MESH-MSG-009",
            Kind::Invalid,
            set(body_value(), "ts", Value::F64(f64::NAN)),
            refused(TS_ERR),
        ),
        message(
            "MESH-MSG-009",
            Kind::Invalid,
            set(body_value(), "ts", Value::F64(f64::INFINITY)),
            refused(TS_ERR),
        ),
        message(
            "MESH-MSG-009",
            Kind::Invalid,
            set(body_value(), "ts", Value::F64(f64::NEG_INFINITY)),
            refused(TS_ERR),
        ),
        message(
            "MESH-MSG-009",
            Kind::Invalid,
            set(body_value(), "ts", Value::F32(f32::NAN)),
            refused(TS_ERR),
        ),
        message(
            "MESH-MSG-009",
            Kind::Valid,
            set(body_value(), "ts", Value::from(1_700_000_000u64)),
            accepted_body(PeerBody {
                timestamp: 1_700_000_000.0,
                ..body()
            }),
        ),
        message(
            "MESH-MSG-009",
            Kind::Valid,
            set(body_value(), "ts", Value::from(-1)),
            accepted_body(PeerBody {
                timestamp: -1.0,
                ..body()
            }),
        ),
        message(
            "MESH-MSG-009",
            Kind::Valid,
            set(body_value(), "ts", Value::F32(1.5)),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-009",
            Kind::Boundary,
            set(body_value(), "ts", Value::F64(f64::MAX)),
            accepted_body(PeerBody {
                timestamp: f64::MAX,
                ..body()
            }),
        ),
        message(
            "MESH-MSG-009",
            Kind::Boundary,
            set(body_value(), "ts", Value::F64(0.0)),
            accepted_body(PeerBody {
                timestamp: 0.0,
                ..body()
            }),
        ),
        message(
            "MESH-MSG-010",
            Kind::Valid,
            with(body_value(), "unknown", Value::from(1)),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-010",
            Kind::Valid,
            with(
                with(body_value(), "priority", Value::from("high")),
                "tags",
                Value::Array(vec![]),
            ),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-010",
            Kind::Valid,
            with(body_value(), "Kind", Value::from("shout")),
            accepted_body(body()),
        ),
        row(
            "MESH-MSG-011",
            Kind::Valid,
            Case::MessageBodyEncode {
                message: OutboundPeer::new(
                    PeerKind::Ask,
                    "q",
                    Some("t"),
                    Some("m-0"),
                    Some(serde_json::json!({ "k": "v" })),
                )
                .unwrap(),
                timestamp: 1.5,
                expect_keys: vec![
                    "v",
                    "kind",
                    "id",
                    "in_reply_to",
                    "title",
                    "content",
                    "fields",
                    "ts",
                ],
            },
        ),
        row(
            "MESH-MSG-011",
            Kind::Valid,
            Case::MessageBodyEncode {
                message: outbound(PeerKind::Bulletin, "all hands"),
                timestamp: 1_700_000_000.0,
                expect_keys: vec!["v", "kind", "id", "content", "ts"],
            },
        ),
        row(
            "MESH-MSG-011",
            Kind::Valid,
            Case::MessageBodyEncode {
                message: OutboundPeer::new(PeerKind::Reply, "a", None, Some("m-0"), None).unwrap(),
                timestamp: 3.0,
                expect_keys: vec!["v", "kind", "id", "in_reply_to", "content", "ts"],
            },
        ),
        row(
            "MESH-MSG-011",
            Kind::Valid,
            Case::Custom(|| {
                let value = to_r3_body(&outbound(PeerKind::Message, "hi"), 1.5);
                let entries = value.as_map().ok_or("not a map")?;
                same("v", &entries[0].1, &Value::from(1u64))?;
                same("ts", &entries[4].1, &Value::F64(1.5))
            }),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            Value::from(7),
            refused("the body is not a map"),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            Value::Nil,
            refused("the body is not a map"),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            Value::from("ping"),
            refused("the body is not a map"),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            Value::Array(vec![]),
            refused("the body is not a map"),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            Value::Map(vec![]),
            refused(V_ERR),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            set(without(body_value(), "kind"), "v", Value::from(2u64)),
            refused(V_ERR),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            set(without(body_value(), "id"), "kind", Value::from("shout")),
            refused(KIND_ERR),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            set(
                set(body_value(), "id", Value::from("")),
                "in_reply_to",
                Value::from(""),
            ),
            refused(ID_ERR),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            set(
                set(body_value(), "in_reply_to", Value::from(7)),
                "title",
                Value::from(7),
            ),
            refused(IN_REPLY_TO_ERR),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            set(without(body_value(), "content"), "title", Value::from(7)),
            refused(TITLE_ERR),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            set(without(body_value(), "content"), "fields", Value::from(7)),
            refused(CONTENT_ERR),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            set(without(body_value(), "ts"), "fields", Value::from(7)),
            refused(FIELDS_ERR),
        ),
        message(
            "MESH-MSG-012",
            Kind::Invalid,
            set(
                with(body_value(), "unknown", Value::from(1)),
                "ts",
                Value::Nil,
            ),
            refused(TS_ERR),
        ),
        message(
            "MESH-MSG-013",
            Kind::Valid,
            set(
                set(
                    set(body_value(), "in_reply_to", Value::Nil),
                    "title",
                    Value::Nil,
                ),
                "fields",
                Value::Nil,
            ),
            accepted_body(body()),
        ),
        message(
            "MESH-MSG-013",
            Kind::Invalid,
            set(body_value(), "v", Value::Nil),
            refused(V_ERR),
        ),
        message(
            "MESH-MSG-013",
            Kind::Invalid,
            set(body_value(), "kind", Value::Nil),
            refused(KIND_ERR),
        ),
        message(
            "MESH-MSG-013",
            Kind::Invalid,
            set(body_value(), "id", Value::Nil),
            refused(ID_ERR),
        ),
        message(
            "MESH-MSG-013",
            Kind::Invalid,
            set(body_value(), "content", Value::Nil),
            refused(CONTENT_ERR),
        ),
        message(
            "MESH-MSG-013",
            Kind::Invalid,
            set(body_value(), "ts", Value::Nil),
            refused(TS_ERR),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Valid,
            "hi",
            None,
            None,
            None,
            minted("hi", None, None),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Valid,
            "\u{1b}[2J hi\t",
            Some(" \u{1b}]0;x\u{7}Re: hi "),
            Some("m-0"),
            None,
            minted("hi", Some("Re: hi"), Some("m-0")),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Valid,
            "Al\u{202E}ex",
            Some("\u{200B}"),
            None,
            None,
            minted("Alex", None, None),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Valid,
            "  ",
            None,
            None,
            None,
            minted("", None, None),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Boundary,
            &text(PEER_CONTENT_MAX_CHARS),
            Some(&text(PEER_TITLE_MAX_CHARS)),
            None,
            None,
            minted(
                &text(PEER_CONTENT_MAX_CHARS),
                Some(&text(PEER_TITLE_MAX_CHARS)),
                None,
            ),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Boundary,
            &format!("{}\u{200B}", text(PEER_CONTENT_MAX_CHARS)),
            None,
            None,
            None,
            minted(&text(PEER_CONTENT_MAX_CHARS), None, None),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Invalid,
            &text(PEER_CONTENT_MAX_CHARS + 1),
            None,
            None,
            None,
            OutboundAction::Refused(SendError::ContentTooLong {
                chars: 4_001,
                max: 4_000,
            }),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Invalid,
            "hi",
            Some(&text(PEER_TITLE_MAX_CHARS + 1)),
            None,
            None,
            OutboundAction::Refused(SendError::TitleTooLong {
                chars: 121,
                max: 120,
            }),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Invalid,
            "hi",
            None,
            Some("not a wire id"),
            None,
            OutboundAction::Refused(SendError::InvalidFields("in_reply_to is not a message id")),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Boundary,
            "hi",
            None,
            Some(" \u{200B} "),
            None,
            minted("hi", None, None),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Invalid,
            "hi",
            None,
            None,
            Some((0..PEER_FIELDS_MAX_DEPTH + 1).fold(
                serde_json::json!(1),
                |inner, _| serde_json::json!({ "n": inner }),
            )),
            OutboundAction::Refused(SendError::InvalidFields("the fields nest too deeply")),
        ),
        outbound_row(
            "MESH-MSG-014",
            Kind::Invalid,
            "hi",
            None,
            None,
            Some(serde_json::json!({ "blob": text(PEER_FIELDS_MAX_BYTES) })),
            OutboundAction::Refused(SendError::InvalidFields(
                "the fields are too large once serialised",
            )),
        ),
        row(
            "MESH-MSG-014",
            Kind::Valid,
            Case::Custom(|| {
                let first = outbound(PeerKind::Message, "hi");
                let second = outbound(PeerKind::Message, "hi");
                ensure(first.id != second.id, "two messages minted the same id")?;
                let cleaned = OutboundPeer::new(
                    PeerKind::Message,
                    "hi",
                    None,
                    None,
                    Some(serde_json::json!({ "k\u{1b}[2Jey": "va\u{200B}lue" })),
                )
                .map_err(|err| err.to_string())?;
                same(
                    "fields cleaned",
                    cleaned.fields,
                    Some(serde_json::json!({ "key": "value" })),
                )
            }),
        ),
        ack("MESH-MSG-015", Kind::Valid, received_reply("m-1"), true),
        ack(
            "MESH-MSG-015",
            Kind::Invalid,
            without(received_reply("m-1"), "received"),
            false,
        ),
        ack(
            "MESH-MSG-015",
            Kind::Invalid,
            set(received_reply("m-1"), "received", Value::Boolean(false)),
            false,
        ),
        ack(
            "MESH-MSG-015",
            Kind::Invalid,
            set(received_reply("m-1"), "received", Value::from(1)),
            false,
        ),
        ack(
            "MESH-MSG-015",
            Kind::Invalid,
            set(received_reply("m-1"), "received", Value::from("true")),
            false,
        ),
        ack(
            "MESH-MSG-015",
            Kind::Invalid,
            set(received_reply("m-1"), "received", Value::Nil),
            false,
        ),
        ack("MESH-MSG-015", Kind::Invalid, Value::from(7), false),
        ack("MESH-MSG-015", Kind::Invalid, Value::Nil, false),
        ack(
            "MESH-MSG-015",
            Kind::Invalid,
            Value::Array(vec![Value::Boolean(true), Value::from("m-1")]),
            false,
        ),
        ack(
            "MESH-MSG-016",
            Kind::Invalid,
            without(received_reply("m-1"), "id"),
            false,
        ),
        ack(
            "MESH-MSG-016",
            Kind::Invalid,
            set(received_reply("m-1"), "id", Value::Binary(b"m-1".to_vec())),
            false,
        ),
        ack(
            "MESH-MSG-016",
            Kind::Invalid,
            set(received_reply("m-1"), "id", Value::from("m-2")),
            false,
        ),
        ack(
            "MESH-MSG-016",
            Kind::Invalid,
            set(received_reply("m-1"), "id", Value::from("M-1")),
            false,
        ),
        ack(
            "MESH-MSG-016",
            Kind::Invalid,
            set(received_reply("m-1"), "id", Value::from(1)),
            false,
        ),
        ack(
            "MESH-MSG-016",
            Kind::Invalid,
            set(received_reply("m-1"), "id", Value::Nil),
            false,
        ),
        ack("MESH-MSG-016", Kind::Valid, received_reply("m-1"), true),
        ack(
            "MESH-MSG-017",
            Kind::Valid,
            with(received_reply("m-1"), "extra", Value::from(1)),
            true,
        ),
        ack(
            "MESH-MSG-017",
            Kind::Valid,
            with(
                with(received_reply("m-1"), "received_at", Value::F64(1.5)),
                "note",
                Value::Nil,
            ),
            true,
        ),
        row(
            "MESH-MSG-037",
            Kind::Valid,
            Case::MessageBody {
                value: set(
                    set(
                        set(body_value(), "kind", Value::from("reply")),
                        "in_reply_to",
                        Value::from("m-0"),
                    ),
                    "fields",
                    map(vec![
                        ("refusal", Value::from("out_of_coffee")),
                        ("retry_after_secs", Value::from(60u64)),
                    ]),
                ),
                expect: accepted_body(PeerBody {
                    kind: PeerKind::Reply,
                    in_reply_to: Some("m-0".to_string()),
                    disposition: Some(Disposition::Answered),
                    fields: Some(
                        serde_json::json!({ "refusal": "out_of_coffee", "retry_after_secs": 60 }),
                    ),
                    ..body()
                }),
            },
        ),
        row(
            "MESH-MSG-037",
            Kind::Valid,
            Case::MessageBody {
                value: set(
                    set(
                        set(body_value(), "kind", Value::from("reply")),
                        "in_reply_to",
                        Value::from("m-0"),
                    ),
                    "fields",
                    map(vec![("retry_after_secs", Value::from(60u64))]),
                ),
                expect: accepted_body(PeerBody {
                    kind: PeerKind::Reply,
                    in_reply_to: Some("m-0".to_string()),
                    disposition: Some(Disposition::Answered),
                    fields: Some(serde_json::json!({ "retry_after_secs": 60 })),
                    ..body()
                }),
            },
        ),
        row(
            "MESH-MSG-038",
            Kind::Valid,
            Case::MessageBody {
                value: set(
                    set(
                        set(body_value(), "kind", Value::from("reply")),
                        "in_reply_to",
                        Value::from("m-0"),
                    ),
                    "fields",
                    map(vec![
                        ("refusal", Value::from("rate_limited")),
                        ("retry_after_secs", Value::from("soon")),
                    ]),
                ),
                expect: accepted_body(PeerBody {
                    kind: PeerKind::Reply,
                    in_reply_to: Some("m-0".to_string()),
                    disposition: Some(Disposition::Answered),
                    fields: Some(
                        serde_json::json!({ "refusal": "rate_limited", "retry_after_secs": "soon" }),
                    ),
                    ..body()
                }),
            },
        ),
        row(
            "MESH-MSG-038",
            Kind::Valid,
            Case::Custom(|| {
                let fields = |reason: RefusalReason, retry_after: Duration| {
                    PeerRefusal {
                        reason,
                        retry_after,
                    }
                    .fields()
                };
                same(
                    "whole seconds",
                    fields(RefusalReason::RateLimited, Duration::from_secs(90)),
                    serde_json::json!({ "refusal": "rate_limited", "retry_after_secs": 90 }),
                )?;
                same(
                    "ceiled",
                    fields(RefusalReason::TokenCeiling, Duration::from_millis(1_500)),
                    serde_json::json!({ "refusal": "token_ceiling", "retry_after_secs": 2 }),
                )?;
                same(
                    "at least one",
                    fields(RefusalReason::CostCeiling, Duration::ZERO),
                    serde_json::json!({ "refusal": "cost_ceiling", "retry_after_secs": 1 }),
                )?;
                same(
                    "PEER_RETRY_AFTER_CAPACITY",
                    PEER_RETRY_AFTER_CAPACITY,
                    Duration::from_secs(120),
                )?;
                same(
                    "capacity",
                    PeerRefusal::capacity(RefusalReason::EnvoyBusy).fields(),
                    serde_json::json!({ "refusal": "envoy_busy", "retry_after_secs": 120 }),
                )?;
                same(
                    "peer_concurrency",
                    PeerRefusal::capacity(RefusalReason::PeerConcurrency).fields(),
                    serde_json::json!({ "refusal": "peer_concurrency", "retry_after_secs": 120 }),
                )
            }),
        ),
        row(
            "MESH-MSG-039",
            Kind::Valid,
            Case::MessageBody {
                value: set(
                    set(
                        set(body_value(), "kind", Value::from("reply")),
                        "in_reply_to",
                        Value::from("m-0"),
                    ),
                    "fields",
                    map(vec![
                        ("refusal", Value::from("envoy_busy")),
                        ("retry_after_secs", Value::from(120u64)),
                        ("operator_note", Value::from("back soon")),
                    ]),
                ),
                expect: accepted_body(PeerBody {
                    kind: PeerKind::Reply,
                    in_reply_to: Some("m-0".to_string()),
                    disposition: Some(Disposition::Answered),
                    fields: Some(
                        serde_json::json!({ "refusal": "envoy_busy", "retry_after_secs": 120, "operator_note": "back soon" }),
                    ),
                    ..body()
                }),
            },
        ),
        lxmf_peer(
            "MESH-MSG-046",
            Kind::Invalid,
            peer_inbound(lxmf_fields(None, Some(peer_data()))),
            PeerLxmf::NotAPeer,
        ),
        lxmf_peer(
            "MESH-MSG-046",
            Kind::Invalid,
            peer_inbound(lxmf_fields(
                Some(Value::from(KNOCK_TYPE)),
                Some(peer_data()),
            )),
            PeerLxmf::NotAPeer,
        ),
        lxmf_peer(
            "MESH-MSG-046",
            Kind::Invalid,
            peer_inbound(lxmf_fields(
                Some(Value::from("scope.peer/2")),
                Some(peer_data()),
            )),
            PeerLxmf::NotAPeer,
        ),
        lxmf_peer(
            "MESH-MSG-046",
            Kind::Invalid,
            peer_inbound(lxmf_fields(Some(Value::from(1)), Some(peer_data()))),
            PeerLxmf::NotAPeer,
        ),
        lxmf_peer(
            "MESH-MSG-046",
            Kind::Invalid,
            peer_inbound(lxmf_fields(Some(Value::Nil), Some(peer_data()))),
            PeerLxmf::NotAPeer,
        ),
        lxmf_peer(
            "MESH-MSG-046",
            Kind::Invalid,
            inbound(None, Some(b"ping"), Some(b"hi")),
            PeerLxmf::NotAPeer,
        ),
        lxmf_peer(
            "MESH-MSG-046",
            Kind::Invalid,
            inbound(Some(Value::Array(vec![])), Some(b"ping"), Some(b"hi")),
            PeerLxmf::NotAPeer,
        ),
        lxmf_peer(
            "MESH-MSG-046",
            Kind::Valid,
            peer_inbound(peer_fields(peer_data())),
            peer(PeerKind::Message, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-047",
            Kind::Invalid,
            peer_inbound(lxmf_fields(Some(Value::from(PEER_MESSAGE_TYPE)), None)),
            peer_malformed("custom data is missing or not a map"),
        ),
        lxmf_peer(
            "MESH-MSG-047",
            Kind::Invalid,
            peer_inbound(peer_fields(Value::from(7))),
            peer_malformed("custom data is missing or not a map"),
        ),
        lxmf_peer(
            "MESH-MSG-047",
            Kind::Invalid,
            peer_inbound(peer_fields(Value::Array(vec![]))),
            peer_malformed("custom data is missing or not a map"),
        ),
        lxmf_peer(
            "MESH-MSG-047",
            Kind::Invalid,
            peer_inbound(peer_fields(Value::Nil)),
            peer_malformed("custom data is missing or not a map"),
        ),
        lxmf_peer(
            "MESH-MSG-048",
            Kind::Valid,
            peer_inbound(Value::Map(vec![
                (Value::from(1), Value::from("title field")),
                (
                    Value::from(FIELD_CUSTOM_TYPE),
                    Value::from(PEER_MESSAGE_TYPE),
                ),
                (Value::from(FIELD_CUSTOM_DATA), peer_data()),
                (Value::from(0xfd), Value::Map(vec![])),
            ])),
            peer(PeerKind::Message, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-049",
            Kind::Invalid,
            peer_inbound(peer_fields(without(peer_data(), "kind"))),
            peer_malformed("kind is missing or unknown"),
        ),
        lxmf_peer(
            "MESH-MSG-049",
            Kind::Invalid,
            peer_inbound(peer_fields(set(peer_data(), "kind", Value::Nil))),
            peer_malformed("kind is missing or unknown"),
        ),
        lxmf_peer(
            "MESH-MSG-049",
            Kind::Invalid,
            peer_inbound(peer_fields(set(peer_data(), "kind", Value::from("shout")))),
            peer_malformed("kind is missing or unknown"),
        ),
        lxmf_peer(
            "MESH-MSG-049",
            Kind::Invalid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "kind",
                Value::from("Message"),
            ))),
            peer_malformed("kind is missing or unknown"),
        ),
        lxmf_peer(
            "MESH-MSG-049",
            Kind::Invalid,
            peer_inbound(peer_fields(set(peer_data(), "kind", Value::from(1)))),
            peer_malformed("kind is missing or unknown"),
        ),
        lxmf_peer(
            "MESH-MSG-049",
            Kind::Valid,
            peer_inbound(peer_fields(set(peer_data(), "kind", Value::from("ask")))),
            peer(PeerKind::Ask, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-049",
            Kind::Valid,
            peer_inbound(peer_fields(set(peer_data(), "kind", Value::from("reply")))),
            peer(PeerKind::Reply, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-049",
            Kind::Valid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "kind",
                Value::from("bulletin"),
            ))),
            peer(PeerKind::Bulletin, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-050",
            Kind::Invalid,
            peer_inbound(peer_fields(without(peer_data(), "id"))),
            peer_malformed("id is missing or blank"),
        ),
        lxmf_peer(
            "MESH-MSG-050",
            Kind::Invalid,
            peer_inbound(peer_fields(set(peer_data(), "id", Value::Nil))),
            peer_malformed("id is missing or blank"),
        ),
        lxmf_peer(
            "MESH-MSG-050",
            Kind::Invalid,
            peer_inbound(peer_fields(set(peer_data(), "id", Value::from("")))),
            peer_malformed("id is missing or blank"),
        ),
        lxmf_peer(
            "MESH-MSG-050",
            Kind::Invalid,
            peer_inbound(peer_fields(set(peer_data(), "id", Value::from(7)))),
            peer_malformed("id is missing or blank"),
        ),
        lxmf_peer(
            "MESH-MSG-050",
            Kind::Invalid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "id",
                Value::from(text(PEER_ID_MAX_CHARS + 1)),
            ))),
            peer_malformed("id is too long or has characters outside the id alphabet"),
        ),
        lxmf_peer(
            "MESH-MSG-050",
            Kind::Invalid,
            peer_inbound(peer_fields(set(peer_data(), "id", Value::from("a b")))),
            peer_malformed("id is too long or has characters outside the id alphabet"),
        ),
        lxmf_peer(
            "MESH-MSG-050",
            Kind::Boundary,
            peer_inbound(peer_fields(set(
                peer_data(),
                "id",
                Value::from(sixty_four.as_str()),
            ))),
            peer(PeerKind::Message, &sixty_four, None),
        ),
        lxmf_peer(
            "MESH-MSG-050",
            Kind::Valid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "id",
                Value::from(all_alphabet),
            ))),
            peer(PeerKind::Message, all_alphabet, None),
        ),
        lxmf_peer(
            "MESH-MSG-051",
            Kind::Invalid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "in_reply_to",
                Value::from(""),
            ))),
            peer_malformed("in_reply_to is not a message id"),
        ),
        lxmf_peer(
            "MESH-MSG-051",
            Kind::Invalid,
            peer_inbound(peer_fields(set(peer_data(), "in_reply_to", Value::from(7)))),
            peer_malformed("in_reply_to is not a message id"),
        ),
        lxmf_peer(
            "MESH-MSG-051",
            Kind::Invalid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "in_reply_to",
                Value::from("a b"),
            ))),
            peer_malformed("in_reply_to is not a message id"),
        ),
        lxmf_peer(
            "MESH-MSG-051",
            Kind::Invalid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "in_reply_to",
                Value::from(text(PEER_ID_MAX_CHARS + 1)),
            ))),
            peer_malformed("in_reply_to is not a message id"),
        ),
        lxmf_peer(
            "MESH-MSG-051",
            Kind::Valid,
            peer_inbound(peer_fields(peer_data())),
            peer(PeerKind::Message, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-051",
            Kind::Valid,
            peer_inbound(peer_fields(set(peer_data(), "in_reply_to", Value::Nil))),
            peer(PeerKind::Message, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-051",
            Kind::Valid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "in_reply_to",
                Value::from("m-0"),
            ))),
            peer(PeerKind::Message, "m-1", Some("m-0")),
        ),
        lxmf_peer(
            "MESH-MSG-051",
            Kind::Boundary,
            peer_inbound(peer_fields(set(
                peer_data(),
                "in_reply_to",
                Value::from(sixty_four.as_str()),
            ))),
            peer(PeerKind::Message, "m-1", Some(&sixty_four)),
        ),
        lxmf_peer(
            "MESH-MSG-052",
            Kind::Invalid,
            peer_inbound(peer_fields(without(peer_data(), "name_hash"))),
            peer_malformed("name_hash is missing or not binary"),
        ),
        lxmf_peer(
            "MESH-MSG-052",
            Kind::Invalid,
            peer_inbound(peer_fields(set(peer_data(), "name_hash", Value::Nil))),
            peer_malformed("name_hash is missing or not binary"),
        ),
        lxmf_peer(
            "MESH-MSG-052",
            Kind::Invalid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "name_hash",
                Value::from(hex_lower(&ORIGIN)),
            ))),
            peer_malformed("name_hash is missing or not binary"),
        ),
        lxmf_peer(
            "MESH-MSG-052",
            Kind::Invalid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "name_hash",
                Value::Binary(vec![7; 9]),
            ))),
            peer_malformed("name_hash is not 10 bytes"),
        ),
        lxmf_peer(
            "MESH-MSG-052",
            Kind::Invalid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "name_hash",
                Value::Binary(vec![7; 11]),
            ))),
            peer_malformed("name_hash is not 10 bytes"),
        ),
        lxmf_peer(
            "MESH-MSG-052",
            Kind::Invalid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "name_hash",
                Value::Binary(vec![7; 16]),
            ))),
            peer_malformed("name_hash is not 10 bytes"),
        ),
        lxmf_peer(
            "MESH-MSG-052",
            Kind::Boundary,
            peer_inbound(peer_fields(peer_data())),
            peer(PeerKind::Message, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-053",
            Kind::Valid,
            peer_inbound(peer_fields(peer_data())),
            peer(PeerKind::Message, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-053",
            Kind::Valid,
            peer_inbound(peer_fields(set(peer_data(), "fields", Value::Nil))),
            peer(PeerKind::Message, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-053",
            Kind::Valid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "fields",
                Value::Map(vec![
                    (
                        Value::from("k"),
                        Value::Array(vec![
                            Value::Nil,
                            Value::Ext(1, vec![0xaa]),
                            Value::Boolean(true),
                            Value::from(1),
                            Value::from(-1),
                            Value::F64(1.5),
                            Value::F32(0.5),
                            Value::from("s"),
                            Value::Binary(vec![0xab, 0xcd]),
                        ]),
                    ),
                    (Value::from(7), Value::from("int key")),
                    (Value::from("nested"), map(vec![("m", Value::from(2))])),
                ]),
            ))),
            peer_with_fields(serde_json::json!({
                "k": [null, null, true, 1, -1, 1.5, 0.5, "s", "abcd"],
                "7": "int key",
                "nested": { "m": 2 },
            })),
        ),
        lxmf_peer(
            "MESH-MSG-053",
            Kind::Valid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "fields",
                unpacked(&[0x81, 0xa1, b'k', 0xa2, 0xff, b'h']),
            ))),
            peer_with_fields(serde_json::json!({ "k": "\u{FFFD}h" })),
        ),
        lxmf_peer(
            "MESH-MSG-053",
            Kind::Valid,
            peer_inbound(peer_fields(set(peer_data(), "fields", Value::from(7)))),
            peer_with_fields(serde_json::json!(7)),
        ),
        lxmf_peer(
            "MESH-MSG-053",
            Kind::Valid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "fields",
                Value::F64(f64::NAN),
            ))),
            peer_with_fields(serde_json::Value::Null),
        ),
        lxmf_peer(
            "MESH-MSG-054",
            Kind::Invalid,
            peer_inbound(peer_fields(set(
                peer_data(),
                "fields",
                nested_maps(PEER_FIELDS_MAX_DEPTH + 1),
            ))),
            peer(PeerKind::Message, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-054",
            Kind::Boundary,
            peer_inbound(peer_fields(set(
                peer_data(),
                "fields",
                nested_maps(PEER_FIELDS_MAX_DEPTH),
            ))),
            peer_with_fields((1..PEER_FIELDS_MAX_DEPTH).fold(
                serde_json::json!({}),
                |inner, _| serde_json::json!({ "n": inner }),
            )),
        ),
        row(
            "MESH-MSG-054",
            Kind::Invalid,
            Case::Custom(|| {
                let big = serde_json::json!({ "blob": text(PEER_FIELDS_MAX_BYTES) });
                let kept = PeerMessage::new(raw(Some(big), Some("ping"), "hi"));
                same(
                    "oversize fields dropped at the sanitising seam",
                    kept.fields,
                    None,
                )?;
                same("the message kept", kept.content, "hi".to_string())?;
                same("via", kept.via, PeerVia::StoreAndForward)
            }),
        ),
        lxmf_peer(
            "MESH-MSG-055",
            Kind::Valid,
            peer_inbound(peer_fields(with(peer_data(), "extra", Value::from(1)))),
            peer(PeerKind::Message, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-055",
            Kind::Valid,
            peer_inbound(peer_fields(with(
                peer_data(),
                "destination",
                Value::Binary(vec![0; 16]),
            ))),
            peer(PeerKind::Message, "m-1", None),
        ),
        lxmf_peer(
            "MESH-MSG-055",
            Kind::Valid,
            peer_inbound(peer_fields(with(peer_data(), "v", Value::from(2u64)))),
            peer(PeerKind::Message, "m-1", None),
        ),
        row(
            "MESH-MSG-056",
            Kind::Valid,
            Case::Custom(|| {
                let origin = OriginName(ORIGIN);
                let titled = peer_lxmf_message(
                    &OutboundPeer::new(PeerKind::Ask, "are you there", Some("ping"), None, None)
                        .unwrap(),
                    &origin,
                );
                same("title bytes", titled.title, Some(b"ping".to_vec()))?;
                same("content bytes", titled.content, b"are you there".to_vec())?;
                let untitled = peer_lxmf_message(&outbound(PeerKind::Message, "héllo"), &origin);
                same("absent title", untitled.title, None)?;
                same(
                    "utf-8 content",
                    untitled.content,
                    "héllo".as_bytes().to_vec(),
                )?;
                let Some(Value::Map(fields)) = untitled.fields else {
                    return Err("LXMF fields are not a map".to_string());
                };
                same(
                    "type tag",
                    &fields[0],
                    &(
                        Value::from(FIELD_CUSTOM_TYPE),
                        Value::from(PEER_MESSAGE_TYPE),
                    ),
                )?;
                same(
                    "custom data key",
                    fields[1].0.as_u64(),
                    Some(u64::from(FIELD_CUSTOM_DATA)),
                )
            }),
        ),
        lxmf_peer(
            "MESH-MSG-057",
            Kind::Valid,
            inbound(
                Some(peer_fields(peer_data())),
                Some(&[0xff, b't']),
                Some(&[0xfe, b'h', b'i']),
            ),
            peer_text(
                PeerKind::Message,
                "m-1",
                None,
                Some("\u{FFFD}t"),
                "\u{FFFD}hi",
                None,
            ),
        ),
        lxmf_peer(
            "MESH-MSG-057",
            Kind::Valid,
            inbound(Some(peer_fields(peer_data())), None, None),
            peer_text(PeerKind::Message, "m-1", None, None, "", None),
        ),
        row(
            "MESH-MSG-057",
            Kind::Boundary,
            Case::Custom(|| {
                let kept = PeerMessage::new(raw(
                    None,
                    Some(&text(PEER_TITLE_MAX_CHARS + 1)),
                    &text(PEER_CONTENT_MAX_CHARS + 1),
                ));
                same(
                    "title truncated",
                    kept.title,
                    Some(text(PEER_TITLE_MAX_CHARS)),
                )?;
                same(
                    "content truncated",
                    kept.content,
                    text(PEER_CONTENT_MAX_CHARS),
                )?;
                let cleaned =
                    PeerMessage::new(raw(None, Some("\u{1b}[2J ping\t"), " Al\u{202E}ex "));
                same("title cleaned", cleaned.title, Some("ping".to_string()))?;
                same("content cleaned", cleaned.content, "Alex".to_string())
            }),
        ),
        lxmf_peer(
            "MESH-PROP-038",
            Kind::Valid,
            knock_inbound(knock_fields(knock_data()), b"hi"),
            PeerLxmf::NotAPeer,
        ),
        lxmf_knock(
            "MESH-PROP-038",
            Kind::Valid,
            peer_inbound(peer_fields(peer_data())),
            KnockMessage::NotAKnock,
        ),
        lxmf_knock(
            "MESH-PROP-038",
            Kind::Valid,
            inbound(
                Some(Value::Map(vec![(Value::from(1), Value::from("plain"))])),
                None,
                Some(b"hi"),
            ),
            KnockMessage::NotAKnock,
        ),
        lxmf_peer(
            "MESH-PROP-038",
            Kind::Valid,
            inbound(
                Some(Value::Map(vec![(Value::from(1), Value::from("plain"))])),
                None,
                Some(b"hi"),
            ),
            PeerLxmf::NotAPeer,
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Section 11.1 and 11.4: the sender's envelope and the pure fetch stages
// ---------------------------------------------------------------------------------------

/// A signed message and the envelope `prepare_envelope` builds for it at a cost every mine
/// reaches in a handful of nonces.
struct Prepared {
    envelope: Vec<u8>,
    transient_id: [u8; 32],
    stamp_value: u32,
    recipient_delivery: AddressHash,
}

fn prepared(fields: Option<Value>) -> Result<Prepared, String> {
    let sender = LxmfIdentity::new_from_rand(OsRng);
    let recipient = PrivateIdentity::new_from_rand(OsRng);
    let node = propagation_node(1, 256);
    let message = OutboundMessage {
        title: Some(b"hello".to_vec()),
        content: b"hi".to_vec(),
        fields,
    };
    let envelope = block_on(prepare_envelope(
        &sender,
        recipient.as_identity(),
        &node,
        &message,
        CancellationToken::new(),
    ))
    .map_err(|err| err.to_string())?;
    Ok(Prepared {
        envelope: envelope.envelope,
        transient_id: envelope.transient_id,
        stamp_value: envelope.stamp_value,
        recipient_delivery: lxmf_delivery_hash(recipient.as_identity()),
    })
}

fn envelope_elements(envelope: &[u8]) -> Result<Vec<Value>, String> {
    match unpacked(envelope) {
        Value::Array(elements) => Ok(elements),
        other => Err(format!("the envelope is not an array: {other}")),
    }
}

fn signed(fields: Option<Value>, title: Option<&[u8]>) -> Result<WireMessage, PropagationError> {
    let sender = LxmfIdentity::new_from_rand(OsRng);
    let recipient = PrivateIdentity::new_from_rand(OsRng);
    build_signed_message(
        &sender,
        &lxmf_delivery_hash(recipient.as_identity()),
        &OutboundMessage {
            title: title.map(<[u8]>::to_vec),
            content: b"hi".to_vec(),
            fields,
        },
        1_700_000_000.0,
    )
}

fn prop_vectors() -> Vec<Vector> {
    vec![
        row(
            "MESH-PROP-001",
            Kind::Valid,
            Case::Custom(|| {
                let prepared = prepared(None)?;
                let elements = envelope_elements(&prepared.envelope)?;
                let Some(Value::Array(transients)) = elements.get(1) else {
                    return Err("no transient array".to_string());
                };
                let Some(Value::Binary(transient)) = transients.first() else {
                    return Err("no transient".to_string());
                };
                same(
                    "the transient is addressed to the recipient's delivery hash",
                    &transient[..16],
                    prepared.recipient_delivery.as_slice(),
                )
            }),
        ),
        row(
            "MESH-PROP-002",
            Kind::Valid,
            Case::Custom(|| {
                let wire = signed(None, None).map_err(|err| err.to_string())?;
                let payload = wire
                    .payload
                    .to_msgpack_without_stamp()
                    .map_err(|err| err.to_string())?;
                let mut expected = vec![0x94, 0xcb];
                expected.extend_from_slice(&1_700_000_000.0f64.to_be_bytes());
                expected.extend_from_slice(&[0xc4, 0x00, 0xc4, 0x02, b'h', b'i', 0x80]);
                same(
                    "[timestamp, title or empty, content, fields or empty map]",
                    payload,
                    expected,
                )
            }),
        ),
        row(
            "MESH-PROP-002",
            Kind::Valid,
            Case::Custom(|| {
                let wire = signed(None, None).map_err(|err| err.to_string())?;
                same("no stamp", wire.payload.stamp.is_none(), true)?;
                same(
                    "empty title",
                    wire.payload.title.as_deref().map(|bytes| bytes.to_vec()),
                    Some(Vec::new()),
                )?;
                same(
                    "content",
                    wire.payload.content.as_deref().map(|bytes| bytes.to_vec()),
                    Some(b"hi".to_vec()),
                )?;
                same(
                    "empty fields map",
                    wire.payload.fields.clone(),
                    Some(Value::Map(vec![])),
                )?;
                same("timestamp", wire.payload.timestamp, 1_700_000_000.0)?;
                let elements = envelope_elements(
                    &wire
                        .payload
                        .to_msgpack_without_stamp()
                        .map_err(|err| err.to_string())?,
                )?;
                same("four elements, no stamp", elements.len(), 4)?;
                same(
                    "timestamp is an f64",
                    matches!(elements[0], Value::F64(_)),
                    true,
                )?;
                let titled = signed(
                    Some(Value::Map(vec![(Value::from(1), Value::from("x"))])),
                    Some(b"t"),
                )
                .map_err(|err| err.to_string())?;
                same(
                    "title kept",
                    titled.payload.title.as_deref().map(|bytes| bytes.to_vec()),
                    Some(b"t".to_vec()),
                )?;
                same(
                    "fields kept",
                    titled.payload.fields,
                    Some(Value::Map(vec![(Value::from(1), Value::from("x"))])),
                )
            }),
        ),
        row(
            "MESH-PROP-003",
            Kind::Invalid,
            Case::Custom(|| {
                for (what, fields) in [
                    ("an integer", Value::from(7)),
                    ("a string", Value::from("fields")),
                    ("an array", Value::Array(vec![])),
                    ("nil", Value::Nil),
                    ("binary", Value::Binary(vec![0x80])),
                ] {
                    match signed(Some(fields), None) {
                        Err(PropagationError::Encode(_)) => {}
                        Err(other) => {
                            return Err(format!("{what}: refused as {other:?}, not Encode"));
                        }
                        Ok(_) => return Err(format!("{what} was encoded as fields")),
                    }
                }
                Ok(())
            }),
        ),
        row(
            "MESH-PROP-003",
            Kind::Valid,
            Case::Custom(|| {
                let wire = signed(Some(Value::Map(vec![])), None).map_err(|err| err.to_string())?;
                same(
                    "a map is encoded",
                    wire.payload.fields,
                    Some(Value::Map(vec![])),
                )
            }),
        ),
        row(
            "MESH-PROP-004",
            Kind::Valid,
            Case::Custom(|| {
                let sender = LxmfIdentity::new_from_rand(OsRng);
                let stranger = LxmfIdentity::new_from_rand(OsRng);
                let recipient = PrivateIdentity::new_from_rand(OsRng);
                let wire = build_signed_message(
                    &sender,
                    &lxmf_delivery_hash(recipient.as_identity()),
                    &OutboundMessage {
                        title: None,
                        content: b"hi".to_vec(),
                        fields: None,
                    },
                    1_700_000_000.0,
                )
                .map_err(|err| err.to_string())?;
                let unpacked = WireMessage::unpack(&wire.pack().map_err(|err| err.to_string())?)
                    .map_err(|err| err.to_string())?;
                same(
                    "verifies for the sender",
                    unpacked.verify(sender.as_identity()).ok(),
                    Some(true),
                )?;
                same(
                    "not for a stranger",
                    unpacked.verify(stranger.as_identity()).ok(),
                    Some(false),
                )
            }),
        ),
        row(
            "MESH-PROP-005",
            Kind::Valid,
            Case::Custom(|| {
                let prepared = prepared(None)?;
                let elements = envelope_elements(&prepared.envelope)?;
                let Some(Value::Array(transients)) = elements.get(1) else {
                    return Err("no transient array".to_string());
                };
                let Some(Value::Binary(element)) = transients.first() else {
                    return Err("no transient".to_string());
                };
                let lxmf_data = &element[..element.len() - PROPAGATION_STAMP_SIZE];
                same(
                    "destination(16) leads",
                    &lxmf_data[..16],
                    prepared.recipient_delivery.as_slice(),
                )?;
                ensure(
                    lxmf_data.len() > 16 + 32,
                    "nothing encrypted follows the destination",
                )?;
                same(
                    "H(lxmf_data) is the transient id",
                    Sha256::digest(lxmf_data).to_vec(),
                    prepared.transient_id.to_vec(),
                )
            }),
        ),
        row(
            "MESH-PROP-006",
            Kind::Valid,
            Case::Custom(|| {
                let prepared = prepared(None)?;
                same("transient id length", prepared.transient_id.len(), 32)?;
                let elements = envelope_elements(&prepared.envelope)?;
                let Some(Value::Array(transients)) = elements.get(1) else {
                    return Err("no transient array".to_string());
                };
                let Some(Value::Binary(element)) = transients.first() else {
                    return Err("no transient".to_string());
                };
                let lxmf_data = &element[..element.len() - PROPAGATION_STAMP_SIZE];
                same(
                    "H(lxmf_data)",
                    Sha256::digest(lxmf_data).to_vec(),
                    prepared.transient_id.to_vec(),
                )
            }),
        ),
        row(
            "MESH-PROP-007",
            Kind::Valid,
            Case::Custom(|| {
                let prepared = prepared(None)?;
                let elements = envelope_elements(&prepared.envelope)?;
                match elements.first() {
                    Some(Value::F64(timestamp)) => ensure(
                        timestamp.is_finite() && *timestamp > 1_700_000_000.0,
                        format!("timestamp {timestamp} is not Unix seconds"),
                    ),
                    other => Err(format!("[0] is {other:?}, not an f64")),
                }
            }),
        ),
        row(
            "MESH-PROP-008",
            Kind::Valid,
            Case::Custom(|| {
                let prepared = prepared(None)?;
                let elements = envelope_elements(&prepared.envelope)?;
                let Some(Value::Array(transients)) = elements.get(1) else {
                    return Err(format!("[1] is {:?}, not an array", elements.get(1)));
                };
                same("exactly one element", transients.len(), 1)?;
                let Value::Binary(element) = &transients[0] else {
                    return Err(format!("[1][0] is {}, not a bin", transients[0]));
                };
                let (lxmf_data, stamp) = element.split_at(element.len() - PROPAGATION_STAMP_SIZE);
                same("stamp length", stamp.len(), 32)?;
                same(
                    "transient precedes the stamp",
                    Sha256::digest(lxmf_data).to_vec(),
                    prepared.transient_id.to_vec(),
                )?;
                same(
                    "the stamp validates at the node's cost",
                    validate_propagation_stamp(element, 1),
                    Some(prepared.stamp_value),
                )
            }),
        ),
        row(
            "MESH-PROP-009",
            Kind::Valid,
            Case::Custom(|| {
                let prepared = prepared(None)?;
                let elements = envelope_elements(&prepared.envelope)?;
                same("exactly two elements", elements.len(), 2)?;
                let (mut cursor, len) = (
                    Cursor::new(prepared.envelope.as_slice()),
                    prepared.envelope.len(),
                );
                rmpv::decode::read_value(&mut cursor).map_err(|err| err.to_string())?;
                same("nothing follows the array", cursor.position(), len as u64)
            }),
        ),
        row(
            "MESH-PROP-011",
            Kind::Invalid,
            Case::Custom(|| {
                let sender = LxmfIdentity::new_from_rand(OsRng);
                let recipient = PrivateIdentity::new_from_rand(OsRng);
                let message = OutboundMessage {
                    title: None,
                    content: b"hi".to_vec(),
                    fields: None,
                };
                let outcome = block_on(prepare_envelope(
                    &sender,
                    recipient.as_identity(),
                    &propagation_node(MAX_ACCEPTED_STAMP_COST + 1, 256),
                    &message,
                    CancellationToken::new(),
                ))
                .map(|prepared| prepared.stamp_value);
                same(
                    "refused before mining",
                    outcome,
                    Err(PropagationError::StampCostAboveCeiling { cost: 27, max: 26 }),
                )
            }),
        ),
        row(
            "MESH-PROP-013",
            Kind::Invalid,
            Case::Custom(|| {
                let sender = LxmfIdentity::new_from_rand(OsRng);
                let recipient = PrivateIdentity::new_from_rand(OsRng);
                let message = OutboundMessage {
                    title: None,
                    content: b"hi".to_vec(),
                    fields: None,
                };
                let outcome = block_on(prepare_envelope(
                    &sender,
                    recipient.as_identity(),
                    &propagation_node(1, 0),
                    &message,
                    CancellationToken::new(),
                ))
                .map(|prepared| prepared.stamp_value);
                match outcome {
                    Err(PropagationError::Oversize { len, max }) => {
                        same("max is per_transfer_limit_kb * 1000", max, 0)?;
                        ensure(len > 0, "the envelope has no length")
                    }
                    other => Err(format!("{other:?}, expected Oversize")),
                }
            }),
        ),
        row(
            "MESH-PROP-028",
            Kind::Boundary,
            Case::Custom(|| {
                same("MIN_FETCHED_MESSAGE_BYTES", MIN_FETCHED_MESSAGE_BYTES, 112)?;
                same(
                    "MAX_FETCHED_MESSAGE_BYTES",
                    MAX_FETCHED_MESSAGE_BYTES,
                    131_072,
                )?;
                same(
                    "at the floor",
                    check_bounds(&[0; MIN_FETCHED_MESSAGE_BYTES]),
                    Ok(()),
                )?;
                same(
                    "at the ceiling",
                    check_bounds(&vec![0; MAX_FETCHED_MESSAGE_BYTES]),
                    Ok(()),
                )
            }),
        ),
        row(
            "MESH-PROP-028",
            Kind::Invalid,
            Case::Custom(|| {
                same(
                    "one under",
                    check_bounds(&[0; MIN_FETCHED_MESSAGE_BYTES - 1]),
                    Err(Discard::Undersize { len: 111 }),
                )?;
                same(
                    "empty",
                    check_bounds(&[]),
                    Err(Discard::Undersize { len: 0 }),
                )?;
                same(
                    "one over",
                    check_bounds(&vec![0; MAX_FETCHED_MESSAGE_BYTES + 1]),
                    Err(Discard::Oversize { len: 131_073 }),
                )
            }),
        ),
    ]
}

fn vectors() -> Vec<Vector> {
    [
        dest_vectors(),
        announce_vectors(),
        pn_vectors(),
        canon_vectors(),
        ext_vectors(),
        code_vectors(),
        time_vectors(),
        knock_vectors(),
        status_vectors(),
        message_vectors(),
        prop_vectors(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

// ---------------------------------------------------------------------------------------
// Tests: one per family
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn announce_vectors_decode_as_section_5_1_mandates() {
        run_family("Announce");
    }

    #[test]
    fn announce_encode_vectors_refuse_what_a_sender_must_not_emit() {
        run_family("AnnounceEncode");
    }

    #[test]
    fn announce_policy_vectors_withhold_the_display_name_as_section_5_2_mandates() {
        run_family("AnnouncePolicy");
    }

    #[test]
    fn propagation_node_announce_vectors_file_or_refuse_as_section_5_4_mandates() {
        run_family("PnAnnounce");
    }

    #[test]
    fn card_vectors_decode_as_section_9_mandates() {
        run_family("Card");
    }

    #[test]
    fn card_encode_vectors_pin_the_emission_order() {
        run_family("CardEncode");
    }

    #[test]
    fn message_body_vectors_decode_as_section_10_1_mandates() {
        run_family("MessageBody");
    }

    #[test]
    fn message_body_encode_vectors_pin_the_emission_order() {
        run_family("MessageBodyEncode");
    }

    #[test]
    fn outbound_vectors_mint_clean_and_refuse_as_section_10_1_mandates() {
        run_family("Outbound");
    }

    #[test]
    fn acknowledgement_vectors_are_read_only_for_their_id() {
        run_family("Ack");
    }

    #[test]
    fn knock_body_vectors_read_the_intro_as_section_8_1_mandates() {
        run_family("KnockBody");
    }

    #[test]
    fn knock_intro_vectors_clean_and_refuse_as_section_8_1_mandates() {
        run_family("KnockIntro");
    }

    #[test]
    fn lxmf_knock_vectors_decode_as_section_8_6_mandates() {
        run_family("LxmfKnock");
    }

    #[test]
    fn lxmf_peer_vectors_decode_as_section_10_8_mandates() {
        run_family("LxmfPeer");
    }

    #[test]
    fn text_vectors_clean_as_section_3_2_mandates() {
        run_family("Text");
    }

    #[test]
    fn hash_text_vectors_accept_only_32_hex_digits() {
        run_family("HashText");
    }

    #[test]
    fn trust_vectors_authorize_as_the_precedence_mandates() {
        run_family("Trust");
    }

    #[test]
    fn derivation_vectors_reproduce_section_4() {
        run_family("Derivation");
    }

    #[test]
    fn registry_vectors_pin_the_code_points_of_section_13() {
        run_family("Registry");
    }

    #[test]
    fn custom_vectors_hold() {
        run_family("Custom");
    }

    #[test]
    fn every_family_has_a_test_and_every_row_a_kind() {
        let families: std::collections::BTreeSet<&str> = vectors()
            .iter()
            .map(|vector| vector.case.family())
            .collect();
        let tested = [
            "Announce",
            "AnnounceEncode",
            "AnnouncePolicy",
            "PnAnnounce",
            "Card",
            "CardEncode",
            "MessageBody",
            "MessageBodyEncode",
            "Outbound",
            "Ack",
            "KnockBody",
            "KnockIntro",
            "LxmfKnock",
            "LxmfPeer",
            "Text",
            "HashText",
            "Trust",
            "Derivation",
            "Registry",
            "Custom",
        ];
        assert_eq!(
            families,
            tested.iter().copied().collect(),
            "a family without a test above"
        );
        assert!(listed().iter().any(|row| row.kind == Kind::Boundary));
    }
}
