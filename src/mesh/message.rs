//! Messages between trusted peers. Outbound, a message is one R3 request on `/message`
//! that the peer acknowledges by id; a peer that cannot be reached gets the message held
//! by an LXMF propagation node instead, typed `scope.peer/1`, until it next fetches.
//! Inbound, both routes end in the same `PeerSurface`: the handler behind `/message` and
//! the fetch path's `PeerRouting` decode, bound and sanitise what the peer sent, then hand
//! a `PeerMessage` over and answer at once. Nothing on either inbound path waits on the
//! model.

use crate::config::mesh_config::{DEFAULT_INLINE_MAX_BYTES, MAX_INLINE_FILE_TOTAL};
use crate::mesh::card::DISPLAY_NAME_MAX_CHARS;
use crate::mesh::events::MeshEvent;
use crate::mesh::inbox::InboxStaging;
use crate::mesh::limits::PeerRefusal;
use crate::mesh::node::MeshRuntime;
use crate::mesh::peers::PeerRecord;
use crate::mesh::propagation::{OutboundMessage, PropagationError, PropagationOptions};
use crate::mesh::propagation_fetch::{InboundMessage, InboundSink};
use crate::mesh::protocol::describe_version;
use crate::mesh::r3::{
    AdmittedRequest, DEFAULT_LINK_TIMEOUT, Handler, MESSAGE_PATH, NAME_HASH_LEN, OriginName,
    R3Error, RefusalCode, Reply, RequestOptions, redact_hashes, short,
};
use crate::mesh::trust::{
    Decision, IdentityStanding, KeyChangeOutcome, OriginVerdict, Rule, TrustStore,
};
use crate::mesh::wire_path::WirePath;
use crate::mesh::{canonical_hash, decode_hex, destination_address, display_text, hex_lower};
use crate::supervisor::mailbox::{Envelope, EnvelopePayload, Inbox};
use crate::supervisor::notification::{
    MESH_NOTIFICATION_QUEUE_CAPACITY, SystemNotification, mesh_events_dropped,
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use lxmf_core::constants::{FIELD_CUSTOM_DATA, FIELD_CUSTOM_TYPE};
use parking_lot::Mutex;
use rmpv::Value;
use rns_transport::destination::{DestinationDesc, DestinationName};
use rns_transport::hash::AddressHash;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The LXMF custom type a stored peer message carries, so a fetch can tell it from a
/// knock or a plain LXMF message before reading anything else. Versioned in the name.
pub(crate) const PEER_MESSAGE_TYPE: &str = "scope.peer/1";
/// The `v` every R3 `/message` body carries; a body with any other value is refused. It is
/// the body's own schema version, evolving under the one mesh protocol version the
/// envelope carries.
pub(crate) const PEER_WIRE_VERSION: u64 = 1;
pub(crate) const PEER_TITLE_MAX_CHARS: usize = 120;
pub(crate) const PEER_CONTENT_MAX_CHARS: usize = 4_000;
/// A message id is a 32-character uuid; the cap leaves room for another scheme without
/// letting a peer send a paragraph as one.
pub(crate) const PEER_ID_MAX_CHARS: usize = 64;
/// Ceiling on the serialised `fields` JSON; over it the fields are dropped and the
/// message kept, since the words are what the user asked for.
pub(crate) const PEER_FIELDS_MAX_BYTES: usize = 4_096;
pub(crate) const PEER_FIELDS_MAX_DEPTH: usize = 8;
pub(crate) const MAX_PARTS: usize = 8;
/// Ceiling on the msgpack-encoded `parts` array. With content, title, fields and every id
/// at their caps beside it, this is what keeps a message under the 128 KiB LXMF bound on
/// the store-and-forward route, with room for the LXMF header.
pub(crate) const MAX_PARTS_BYTES: usize = 104 * 1024;
const SHA256_MISMATCH: &str = "file part sha256 does not match its bytes";
const DATA_PART_RULE: &str = "data part is too large or nests too deeply";
/// Peer envelopes the inbox holds before the oldest is dropped; the loss is counted.
pub(crate) const PEER_INBOX_CAPACITY: usize = 64;
/// Ceiling on the direct attempt. The handler answers before anything slow happens, so a
/// peer that is up replies well inside this; a longer wait only delays the fallback.
pub(crate) const PEER_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const PEER_LINK_TIMEOUT: Duration = DEFAULT_LINK_TIMEOUT;
/// Recipients a broadcast sends to at once. Each open link costs the transport a handler
/// lock round; a trust list of dozens must not open dozens of links in one burst.
pub(crate) const BROADCAST_MAX_CONCURRENCY: usize = 4;
/// The one-line rendering a message earns on the terminal.
pub(crate) const PEER_LINE_MAX_CHARS: usize = 160;

/// The model's next call for a peer message, ask or bulletin it has not read yet.
pub(crate) const CHECK_INBOX_NEXT_ACTION: &str = "mesh__check_inbox";

/// The model's next call for the reply to question `id`.
pub(crate) fn collect_next_action(id: &str) -> String {
    format!("mesh__collect --id {id}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PeerKind {
    Message,
    Ask,
    Reply,
    Bulletin,
}

impl PeerKind {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Ask => "ask",
            Self::Reply => "reply",
            Self::Bulletin => "bulletin",
        }
    }

    fn from_wire_name(name: &str) -> Option<Self> {
        [Self::Message, Self::Ask, Self::Reply, Self::Bulletin]
            .into_iter()
            .find(|kind| kind.wire_name() == name)
    }

    pub(crate) fn verb(self) -> &'static str {
        match self {
            Self::Message => "says",
            Self::Ask => "asks",
            Self::Reply => "replies",
            Self::Bulletin => "announces",
        }
    }
}

impl fmt::Display for PeerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PeerVia {
    Direct,
    StoreAndForward,
}

/// What a reply says about the question it names. Only a `kind: reply` carries one; an
/// unknown wire value reads as `Answered`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disposition {
    #[default]
    Answered,
    /// The peer's human has been asked; the correlation stays open for a later answer.
    Escalated,
    Refused,
    BudgetExhausted,
}

impl Disposition {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Escalated => "escalated",
            Self::Refused => "refused",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }

    fn from_wire_name(name: &str) -> Option<Self> {
        [
            Self::Answered,
            Self::Escalated,
            Self::Refused,
            Self::BudgetExhausted,
        ]
        .into_iter()
        .find(|disposition| disposition.wire_name() == name)
    }
}

/// The configured ceilings a part is admitted under, the same on both routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PartLimits {
    pub inline_max_bytes: u64,
}

impl Default for PartLimits {
    fn default() -> Self {
        Self {
            inline_max_bytes: DEFAULT_INLINE_MAX_BYTES,
        }
    }
}

/// One `parts` element as it travels: hand-encoded, so an inline file's bytes exist only
/// here and in the staging inbox they are written to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RawPart {
    Text {
        text: String,
    },
    Data {
        data: serde_json::Value,
    },
    File {
        name: String,
        size: u64,
        sha256: [u8; 32],
        /// Inline bytes; a reference file carries `reference` instead, never both.
        bytes: Option<Vec<u8>>,
        reference: Option<String>,
    },
}

/// A part as the pending store, an inbox envelope and a tool result see it. Never the
/// bytes: an inline file is a path in the staging inbox by the time anyone reads this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Part {
    Text {
        text: String,
    },
    Data {
        data: serde_json::Value,
    },
    File {
        name: String,
        size: u64,
        /// Lower-hex.
        sha256: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        staged: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
}

/// One message as it lands, LXMF-shaped: who sent it, where it arrived, the words and the
/// routing. Every peer-supplied string has been capped and sanitised by `new`, so a
/// consumer may show or store any field as it is. Serde serves two readers: the pending
/// store, where a reply waits on disk inside a `PendingRecord` (a layout change bumps
/// `PENDING_RECORD_VERSION`), and `EnvelopePayload::Peer`, whose JSON `agent__check_inbox`
/// hands the model. Neither is the mesh wire. Like every on-disk shape it rejects unknown
/// fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PeerMessage {
    /// Lower-hex of the identity that signed the link or the stored message.
    pub source_identity: String,
    /// Lower-hex of the sending instance, derived from the origin it named and the
    /// identity it proved; never a hash the peer supplied outright.
    pub source_destination: String,
    /// Lower-hex of this node's destination that received it.
    pub destination: String,
    pub title: Option<String>,
    pub content: String,
    pub fields: Option<serde_json::Value>,
    /// The sender's clock, unix seconds; zero when it sent nothing finite.
    pub timestamp: f64,
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub kind: PeerKind,
    pub via: PeerVia,
    /// The conversation this message belongs to; `thread()` falls back to its own id.
    #[serde(default)]
    pub thread: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<Disposition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u32>,
    #[serde(default)]
    pub parts: Vec<Part>,
    /// Parts the sender attached that did not survive admission or staging.
    #[serde(default)]
    pub dropped_parts: u32,
}

/// `PeerMessage` before sanitising: what a codec read off the wire.
pub(crate) struct RawPeerMessage {
    pub source_identity: String,
    pub source_destination: String,
    pub destination: String,
    pub title: Option<String>,
    pub content: String,
    pub fields: Option<serde_json::Value>,
    pub timestamp: f64,
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub kind: PeerKind,
    pub via: PeerVia,
    pub thread: Option<String>,
    pub disposition: Option<Disposition>,
    pub retry_after: Option<u32>,
    pub parts: Vec<RawPart>,
    /// Parts the codec could not read at all: a `parts` that was not a list counts one.
    pub dropped_parts: u32,
}

impl PeerMessage {
    /// The one place peer text is cleaned: title, content and ids are capped and stripped
    /// of escapes and invisible characters, and `fields` loses every string leaf's
    /// escapes too or is dropped whole when it nests or weighs more than the caps allow.
    /// Parts are admitted under the default limits with nowhere to stage an inline file.
    pub(crate) fn new(raw: RawPeerMessage) -> Self {
        Self::new_with(raw, &PartLimits::default(), None)
    }

    /// `new` with the node's part limits and its staging inbox: an inline file is written
    /// there and kept as its path, dropped and counted when there is no inbox or the write
    /// fails. Runs on a blocking thread on the request path, since it touches the disk.
    pub(crate) fn new_with(
        raw: RawPeerMessage,
        limits: &PartLimits,
        staging: Option<&InboxStaging>,
    ) -> Self {
        let (admitted, dropped) = admit_parts(raw.parts, limits, &raw.source_destination);
        let mut dropped_parts = raw.dropped_parts.saturating_add(dropped);
        let mut parts = Vec::with_capacity(admitted.len());
        for part in admitted {
            match keep_part(part, &raw.source_destination, staging) {
                Some(part) => parts.push(part),
                None => dropped_parts += 1,
            }
        }
        Self {
            source_identity: raw.source_identity,
            source_destination: raw.source_destination,
            destination: raw.destination,
            title: raw
                .title
                .as_deref()
                .and_then(|title| display_text(title, PEER_TITLE_MAX_CHARS)),
            content: display_text(&raw.content, PEER_CONTENT_MAX_CHARS).unwrap_or_default(),
            fields: raw.fields.and_then(|fields| sanitize_fields(fields).ok()),
            timestamp: if raw.timestamp.is_finite() {
                raw.timestamp
            } else {
                0.0
            },
            message_id: display_text(&raw.message_id, PEER_ID_MAX_CHARS).unwrap_or_default(),
            in_reply_to: raw
                .in_reply_to
                .as_deref()
                .and_then(|id| display_text(id, PEER_ID_MAX_CHARS)),
            kind: raw.kind,
            via: raw.via,
            thread: raw
                .thread
                .as_deref()
                .and_then(|id| display_text(id, PEER_ID_MAX_CHARS)),
            disposition: raw.disposition,
            retry_after: raw.retry_after,
            parts,
            dropped_parts,
        }
    }

    /// The conversation this message belongs to: the `thread` it carried, else its own id.
    pub(crate) fn thread(&self) -> &str {
        self.thread.as_deref().unwrap_or(&self.message_id)
    }

    /// `Answered` unless the reply said otherwise; a non-reply never carries one.
    pub(crate) fn disposition(&self) -> Disposition {
        self.disposition.unwrap_or_default()
    }

    /// `"<name or dest8> says|asks|replies|announces: <words>"`, one terminal line. The
    /// display name is the peer table's, cleaned again here since it is peer text too;
    /// without one the sender is named by the short hash of its instance.
    pub(crate) fn summary_line(&self, display_name: Option<&str>) -> String {
        let who = display_name
            .and_then(|name| display_text(name, DISPLAY_NAME_MAX_CHARS))
            .unwrap_or_else(|| short(&self.source_destination).to_string());
        let words = if self.content.is_empty() {
            self.title.as_deref().unwrap_or("(no text)")
        } else {
            &self.content
        };
        let words = display_text(words, PEER_LINE_MAX_CHARS).unwrap_or_default();
        format!("{who} {}: {words}", self.kind.verb())
    }
}

/// `fields` as this node will keep or send it: every string cleaned, every object key
/// cleaned, refused when it nests past `PEER_FIELDS_MAX_DEPTH` or serialises past
/// `PEER_FIELDS_MAX_BYTES`.
fn sanitize_fields(fields: serde_json::Value) -> Result<serde_json::Value, &'static str> {
    let cleaned = clean_json(fields, 1).ok_or("the fields nest too deeply")?;
    let bytes = serde_json::to_vec(&cleaned).map_err(|_| "the fields cannot be serialised")?;
    if bytes.len() > PEER_FIELDS_MAX_BYTES {
        return Err("the fields are too large once serialised");
    }
    Ok(cleaned)
}

fn clean_json(value: serde_json::Value, depth: usize) -> Option<serde_json::Value> {
    if depth > PEER_FIELDS_MAX_DEPTH {
        return None;
    }
    Some(match value {
        serde_json::Value::String(text) => serde_json::Value::String(
            display_text(&text, PEER_FIELDS_MAX_BYTES).unwrap_or_default(),
        ),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(|item| clean_json(item, depth + 1))
                .collect::<Option<Vec<_>>>()?,
        ),
        serde_json::Value::Object(entries) => serde_json::Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| {
                    Some((
                        display_text(&key, PEER_FIELDS_MAX_BYTES).unwrap_or_default(),
                        clean_json(value, depth + 1)?,
                    ))
                })
                .collect::<Option<serde_json::Map<_, _>>>()?,
        ),
        other => other,
    })
}

/// A msgpack value as JSON, or `None` past `PEER_FIELDS_MAX_DEPTH`, so a peer cannot make
/// this node recurse to its heart's content. Binary becomes lower-hex, extensions become
/// null, and a non-string map key is rendered as msgpack prints it.
fn json_from_rmpv(value: &Value, depth: usize) -> Option<serde_json::Value> {
    if depth > PEER_FIELDS_MAX_DEPTH {
        return None;
    }
    Some(match value {
        Value::Nil | Value::Ext(..) => serde_json::Value::Null,
        Value::Boolean(flag) => serde_json::Value::Bool(*flag),
        Value::Integer(int) => match (int.as_i64(), int.as_u64()) {
            (Some(signed), _) => serde_json::Value::from(signed),
            (None, Some(unsigned)) => serde_json::Value::from(unsigned),
            (None, None) => serde_json::Value::Null,
        },
        Value::F32(float) => serde_json::Number::from_f64(f64::from(*float))
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Value::F64(float) => serde_json::Number::from_f64(*float)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Value::String(text) => {
            serde_json::Value::String(String::from_utf8_lossy(text.as_bytes()).into_owned())
        }
        Value::Binary(bytes) => serde_json::Value::String(hex_lower(bytes)),
        Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| json_from_rmpv(item, depth + 1))
                .collect::<Option<Vec<_>>>()?,
        ),
        Value::Map(entries) => serde_json::Value::Object(
            entries
                .iter()
                .map(|(key, value)| {
                    let key = match key {
                        Value::String(text) => {
                            String::from_utf8_lossy(text.as_bytes()).into_owned()
                        }
                        other => other.to_string(),
                    };
                    Some((key, json_from_rmpv(value, depth + 1)?))
                })
                .collect::<Option<serde_json::Map<_, _>>>()?,
        ),
    })
}

fn rmpv_from_json(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(flag) => Value::from(*flag),
        serde_json::Value::Number(number) => number
            .as_i64()
            .map(Value::from)
            .or_else(|| number.as_u64().map(Value::from))
            .or_else(|| number.as_f64().map(Value::from))
            .unwrap_or(Value::Nil),
        serde_json::Value::String(text) => Value::from(text.as_str()),
        serde_json::Value::Array(items) => Value::Array(items.iter().map(rmpv_from_json).collect()),
        serde_json::Value::Object(entries) => Value::Map(
            entries
                .iter()
                .map(|(key, value)| (Value::from(key.as_str()), rmpv_from_json(value)))
                .collect(),
        ),
    }
}

/// The msgpack size of `value`. The only error source is the writer, and a `Vec` never
/// fails to grow; should it, the value reads as past every cap.
pub(crate) fn packed_len(value: &Value) -> usize {
    let mut packed = Vec::new();
    match rmpv::encode::write_value(&mut packed, value) {
        Ok(()) => packed.len(),
        Err(_) => usize::MAX,
    }
}

/// `parts` as both routes carry it: a list of string-keyed maps, each with a `type`. A
/// file carries `sha256` as 32 binary bytes and either `bytes` (inline) or `ref: {path}`.
pub(crate) fn encode_parts(parts: &[RawPart]) -> Value {
    Value::Array(
        parts
            .iter()
            .map(|part| {
                Value::Map(match part {
                    RawPart::Text { text } => vec![
                        (Value::from("type"), Value::from("text")),
                        (Value::from("text"), Value::from(text.as_str())),
                    ],
                    RawPart::Data { data } => vec![
                        (Value::from("type"), Value::from("data")),
                        (Value::from("data"), rmpv_from_json(data)),
                    ],
                    RawPart::File {
                        name,
                        size,
                        sha256,
                        bytes,
                        reference,
                    } => {
                        let mut entries = vec![
                            (Value::from("type"), Value::from("file")),
                            (Value::from("name"), Value::from(name.as_str())),
                            (Value::from("size"), Value::from(*size)),
                            (Value::from("sha256"), Value::Binary(sha256.to_vec())),
                        ];
                        if let Some(bytes) = bytes {
                            entries.push((Value::from("bytes"), Value::Binary(bytes.clone())));
                        }
                        if let Some(path) = reference {
                            entries.push((
                                Value::from("ref"),
                                Value::Map(vec![(Value::from("path"), Value::from(path.as_str()))]),
                            ));
                        }
                        entries
                    }
                })
            })
            .collect(),
    )
}

/// Reads `parts`: the parts read, and how many were lost to the shape. A value that is
/// not a list reads as no parts with one dropped; an element that is not a map or names
/// no known `type` is skipped, so a part this build does not know never costs the
/// message its words; one of a known type whose fields do not decode is dropped and
/// counted, since the sender meant it and the receiver can tell.
fn decode_parts(value: &Value) -> (Vec<RawPart>, u32) {
    let Some(items) = value.as_array() else {
        return (Vec::new(), 1);
    };
    let mut parts = Vec::new();
    let mut dropped = 0u32;
    for item in items {
        let Some(entries) = item.as_map() else {
            continue;
        };
        let Some(kind) = entry(entries, "type").and_then(text_of) else {
            continue;
        };
        match decode_part(&kind, entries) {
            Some(Some(part)) => parts.push(part),
            Some(None) => dropped += 1,
            None => {}
        }
    }
    (parts, dropped)
}

/// `None` for a `type` this build does not know; `Some(None)` for a known type whose
/// fields do not decode.
fn decode_part(kind: &str, entries: &[(Value, Value)]) -> Option<Option<RawPart>> {
    Some(match kind {
        "text" => entry(entries, "text")
            .and_then(text_of)
            .map(|text| RawPart::Text { text }),
        "data" => entry_or_nil(entries, "data")
            .and_then(|data| json_from_rmpv(data, 1))
            .map(|data| RawPart::Data { data }),
        "file" => decode_file_part(entries),
        _ => return None,
    })
}

fn decode_file_part(entries: &[(Value, Value)]) -> Option<RawPart> {
    let name = entry(entries, "name").and_then(text_of)?;
    let size = entry(entries, "size").and_then(Value::as_u64)?;
    let Value::Binary(digest) = entry(entries, "sha256")? else {
        return None;
    };
    let sha256 = <[u8; 32]>::try_from(digest.as_slice()).ok()?;
    let bytes = match entry(entries, "bytes") {
        None => None,
        Some(Value::Binary(bytes)) => Some(bytes.clone()),
        Some(_) => return None,
    };
    let reference = match entry(entries, "ref") {
        None => None,
        Some(Value::Map(reference)) => Some(entry(reference, "path").and_then(text_of)?),
        Some(_) => return None,
    };
    if bytes.is_some() == reference.is_some() {
        return None;
    }
    Some(RawPart::File {
        name,
        size,
        sha256,
        bytes,
        reference,
    })
}

/// The rule `part` breaks, if any, given the inline bytes already admitted before it.
/// The sender refuses on the first rule; the receiver drops the part and keeps the
/// message. The text names the rule and nothing of the part, so it can be logged.
fn part_violation(part: &RawPart, limits: &PartLimits, inline_so_far: u64) -> Option<&'static str> {
    match part {
        RawPart::Text { text } => {
            (text.chars().count() > PEER_CONTENT_MAX_CHARS).then_some("text part is too long")
        }
        RawPart::Data { data } => sanitize_fields(data.clone())
            .is_err()
            .then_some(DATA_PART_RULE),
        RawPart::File {
            name,
            size,
            sha256,
            bytes,
            reference,
        } => {
            if WirePath::parse(name).is_err() {
                return Some("file part name is not a wire path");
            }
            if reference
                .as_deref()
                .is_some_and(|path| WirePath::parse(path).is_err())
            {
                return Some("file part ref is not a wire path");
            }
            let Some(bytes) = bytes else {
                return None;
            };
            if u64::try_from(bytes.len()) != Ok(*size) {
                return Some("file part size does not match its bytes");
            }
            if *size > limits.inline_max_bytes {
                return Some("file part is over the inline cap");
            }
            if inline_so_far.saturating_add(*size) > MAX_INLINE_FILE_TOTAL {
                return Some("file parts are over the per-message inline total");
            }
            if Sha256::digest(bytes).as_slice() != sha256 {
                return Some(SHA256_MISMATCH);
            }
            None
        }
    }
}

pub(crate) fn inline_size(part: &RawPart) -> u64 {
    match part {
        RawPart::File {
            size,
            bytes: Some(_),
            ..
        } => *size,
        _ => 0,
    }
}

/// The receiver's admission: the parts that pass, in order, and how many did not. A part
/// past `MAX_PARTS` or breaking a rule goes; then parts go from the tail until the encoded
/// list fits `MAX_PARTS_BYTES`. A hash mismatch is noted once per message.
pub(crate) fn admit_parts(
    parts: Vec<RawPart>,
    limits: &PartLimits,
    source_destination: &str,
) -> (Vec<RawPart>, u32) {
    let mut admitted = Vec::new();
    let mut dropped = 0u32;
    let mut inline_so_far = 0u64;
    let mut hash_mismatch = false;
    for (index, part) in parts.into_iter().enumerate() {
        if index >= MAX_PARTS {
            dropped += 1;
            continue;
        }
        match part_violation(&part, limits, inline_so_far) {
            Some(rule) => {
                hash_mismatch |= rule == SHA256_MISMATCH;
                dropped += 1;
            }
            None => {
                inline_so_far += inline_size(&part);
                admitted.push(part);
            }
        }
    }
    if hash_mismatch {
        debug!(
            "Mesh message part from instance {} dropped: {SHA256_MISMATCH}",
            short(source_destination)
        );
    }
    while !admitted.is_empty() && packed_len(&encode_parts(&admitted)) > MAX_PARTS_BYTES {
        admitted.pop();
        dropped += 1;
    }
    (admitted, dropped)
}

/// `part` as the receiver would keep it, so the sender's caps measure what travels: text
/// cleaned, data sanitised. Text that cleans to nothing is refused rather than sent for
/// the receiver to drop.
fn normalise_part(part: RawPart) -> Result<RawPart, &'static str> {
    Ok(match part {
        RawPart::Text { text } => RawPart::Text {
            text: display_text(&text, usize::MAX).ok_or("text part is blank")?,
        },
        RawPart::Data { data } => RawPart::Data {
            data: sanitize_fields(data).map_err(|_| DATA_PART_RULE)?,
        },
        file @ RawPart::File { .. } => file,
    })
}

/// An admitted part as the message keeps it, or `None` for one nothing can be kept of:
/// text that cleans to nothing, or inline bytes with no inbox to land in.
fn keep_part(
    part: RawPart,
    source_destination: &str,
    staging: Option<&InboxStaging>,
) -> Option<Part> {
    match part {
        RawPart::Text { text } => {
            display_text(&text, PEER_CONTENT_MAX_CHARS).map(|text| Part::Text { text })
        }
        RawPart::Data { data } => sanitize_fields(data).ok().map(|data| Part::Data { data }),
        RawPart::File {
            name,
            size,
            sha256,
            bytes: None,
            reference,
        } => Some(Part::File {
            name,
            size,
            sha256: hex_lower(&sha256),
            staged: None,
            reference,
        }),
        RawPart::File {
            name,
            size,
            sha256,
            bytes: Some(bytes),
            reference: _,
        } => {
            let Some(staging) = staging else {
                debug!("Mesh file part dropped: no staging inbox is attached");
                return None;
            };
            let rel = WirePath::parse(&name).ok()?;
            match staging.stage(source_destination, &rel, &sha256, &bytes) {
                Ok(staged) => Some(Part::File {
                    name,
                    size,
                    sha256: hex_lower(&sha256),
                    staged: Some(staged),
                    reference: None,
                }),
                Err(err) => {
                    warn!(
                        "Mesh file part from instance {} dropped: {}",
                        short(source_destination),
                        redact_hashes(&err.to_string())
                    );
                    None
                }
            }
        }
    }
}

/// A message this node sends. `new` cleans the text as the receiver will and refuses
/// what is still over a cap, so the sender is told rather than having its words cut.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OutboundPeer {
    pub kind: PeerKind,
    pub id: String,
    pub in_reply_to: Option<String>,
    pub title: Option<String>,
    pub content: String,
    pub fields: Option<serde_json::Value>,
    pub parts: Vec<RawPart>,
    pub thread: Option<String>,
    /// Sent only on a reply.
    pub disposition: Option<Disposition>,
    /// Sent only beside a disposition.
    pub retry_after: Option<u32>,
}

impl OutboundPeer {
    /// A message without parts; `with_parts` takes them, with the limits to check them
    /// against.
    pub(crate) fn new(
        kind: PeerKind,
        content: &str,
        title: Option<&str>,
        in_reply_to: Option<&str>,
        fields: Option<serde_json::Value>,
    ) -> Result<Self, SendError> {
        Self::with_parts(
            kind,
            content,
            title,
            in_reply_to,
            fields,
            Vec::new(),
            &PartLimits::default(),
        )
    }

    /// `new` with `parts`, refused whole on the first rule any part breaks: the receiver
    /// would drop that part, and the sender is better told than silently trimmed.
    pub(crate) fn with_parts(
        kind: PeerKind,
        content: &str,
        title: Option<&str>,
        in_reply_to: Option<&str>,
        fields: Option<serde_json::Value>,
        parts: Vec<RawPart>,
        limits: &PartLimits,
    ) -> Result<Self, SendError> {
        let content = display_text(content, usize::MAX).unwrap_or_default();
        let chars = content.chars().count();
        if chars > PEER_CONTENT_MAX_CHARS {
            return Err(SendError::ContentTooLong {
                chars,
                max: PEER_CONTENT_MAX_CHARS,
            });
        }
        let title = title.and_then(|title| display_text(title, usize::MAX));
        if let Some(title) = &title {
            let chars = title.chars().count();
            if chars > PEER_TITLE_MAX_CHARS {
                return Err(SendError::TitleTooLong {
                    chars,
                    max: PEER_TITLE_MAX_CHARS,
                });
            }
        }
        let in_reply_to = in_reply_to.and_then(|id| display_text(id, usize::MAX));
        if in_reply_to.as_deref().is_some_and(|id| !is_wire_id(id)) {
            return Err(SendError::InvalidFields("in_reply_to is not a message id"));
        }
        let fields = fields
            .map(sanitize_fields)
            .transpose()
            .map_err(SendError::InvalidFields)?;
        if parts.len() > MAX_PARTS {
            return Err(SendError::InvalidParts("too many parts"));
        }
        let parts = parts
            .into_iter()
            .map(normalise_part)
            .collect::<Result<Vec<_>, _>>()
            .map_err(SendError::InvalidParts)?;
        let mut inline_so_far = 0u64;
        for part in &parts {
            if let Some(rule) = part_violation(part, limits, inline_so_far) {
                return Err(SendError::InvalidParts(rule));
            }
            inline_so_far += inline_size(part);
        }
        if packed_len(&encode_parts(&parts)) > MAX_PARTS_BYTES {
            return Err(SendError::InvalidParts("parts are too large once encoded"));
        }
        Ok(Self {
            kind,
            id: uuid::Uuid::new_v4().simple().to_string(),
            in_reply_to,
            title,
            content,
            fields,
            parts,
            thread: None,
            disposition: None,
            retry_after: None,
        })
    }

    /// Names the conversation; `None` leaves the receiver to infer it (a root message is
    /// its own thread, a reply inherits the answered message's).
    pub(crate) fn with_thread(mut self, thread: Option<String>) -> Result<Self, SendError> {
        let thread = thread.and_then(|id| display_text(&id, usize::MAX));
        if thread.as_deref().is_some_and(|id| !is_wire_id(id)) {
            return Err(SendError::InvalidFields("thread is not a message id"));
        }
        self.thread = thread;
        Ok(self)
    }

    /// What this reply says about its question; the wire carries it on a reply only.
    pub(crate) fn with_disposition(
        mut self,
        disposition: Disposition,
        retry_after: Option<u32>,
    ) -> Self {
        self.disposition = Some(disposition);
        self.retry_after = retry_after;
        self
    }
}

/// The R3 `/message` body: a string-keyed map with `v`, `kind`, `id`, `content`, `ts`
/// and, when set, `in_reply_to`, `thread`, `title`, `fields`, a reply's `disposition` and
/// `retry_after`, and `parts`.
pub(crate) fn to_r3_body(message: &OutboundPeer, timestamp: f64) -> Value {
    let mut entries = vec![
        (Value::from("v"), Value::from(PEER_WIRE_VERSION)),
        (Value::from("kind"), Value::from(message.kind.wire_name())),
        (Value::from("id"), Value::from(message.id.as_str())),
    ];
    if let Some(in_reply_to) = &message.in_reply_to {
        entries.push((
            Value::from("in_reply_to"),
            Value::from(in_reply_to.as_str()),
        ));
    }
    if let Some(thread) = &message.thread {
        entries.push((Value::from("thread"), Value::from(thread.as_str())));
    }
    if let Some(title) = &message.title {
        entries.push((Value::from("title"), Value::from(title.as_str())));
    }
    entries.push((
        Value::from("content"),
        Value::from(message.content.as_str()),
    ));
    if let Some(fields) = &message.fields {
        entries.push((Value::from("fields"), rmpv_from_json(fields)));
    }
    entries.extend(reply_entries(message));
    if !message.parts.is_empty() {
        entries.push((Value::from("parts"), encode_parts(&message.parts)));
    }
    entries.push((Value::from("ts"), Value::F64(timestamp)));
    Value::Map(entries)
}

/// `disposition` and `retry_after`, on a reply that set one; the same on both routes.
fn reply_entries(message: &OutboundPeer) -> Vec<(Value, Value)> {
    let mut entries = Vec::new();
    if message.kind != PeerKind::Reply {
        return entries;
    }
    if let Some(disposition) = message.disposition {
        entries.push((
            Value::from("disposition"),
            Value::from(disposition.wire_name()),
        ));
        if let Some(retry_after) = message.retry_after {
            entries.push((Value::from("retry_after"), Value::from(retry_after)));
        }
    }
    entries
}

/// What a well-formed `/message` body carries, still the peer's own text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PeerBody {
    pub kind: PeerKind,
    pub id: String,
    pub in_reply_to: Option<String>,
    pub title: Option<String>,
    pub content: String,
    pub fields: Option<serde_json::Value>,
    pub timestamp: f64,
    pub thread: Option<String>,
    pub disposition: Option<Disposition>,
    pub retry_after: Option<u32>,
    pub parts: Vec<RawPart>,
    pub dropped_parts: u32,
}

/// A string or its bytes, since msgpack encoders differ on which they emit; bytes that
/// are not UTF-8 are read lossily.
fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(String::from_utf8_lossy(text.as_bytes()).into_owned()),
        Value::Binary(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

fn entry<'a>(entries: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|(name, _)| name.as_str() == Some(key))
        .map(|(_, value)| value)
        .filter(|value| !value.is_nil())
}

/// `entry` for a value that may legitimately be nil: a `data` part's JSON null.
fn entry_or_nil<'a>(entries: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|(name, _)| name.as_str() == Some(key))
        .map(|(_, value)| value)
}

/// A message id as a peer may send one: non-empty, at most `PEER_ID_MAX_CHARS`, drawn from
/// `[0-9A-Za-z_.:-]`. Our own ids are simple-form uuids; the alphabet leaves room for
/// another scheme while keeping an id out of the text a peer can choose freely, since
/// the id is echoed back in the acknowledgement and named in `in_reply_to`.
pub(crate) fn is_wire_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= PEER_ID_MAX_CHARS
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

fn kind_of(value: &Value) -> Option<PeerKind> {
    text_of(value).and_then(|name| PeerKind::from_wire_name(&name))
}

/// The optional keys beside the words, read the same way on both routes.
struct BodyExtras {
    thread: Option<String>,
    disposition: Option<Disposition>,
    retry_after: Option<u32>,
    parts: Vec<RawPart>,
    dropped_parts: u32,
}

/// Every key here is optional under `v: 1` and never refuses the body. A `thread` that is
/// not an id reads as absent, so the message is its own thread. A reply always has a
/// disposition, `Answered` when it names none or one this build does not know; nothing
/// else carries one, nor a `retry_after`. A `parts` that is not a list reads as no parts
/// with one dropped.
fn body_extras(entries: &[(Value, Value)], kind: PeerKind) -> BodyExtras {
    let thread = entry(entries, "thread")
        .and_then(text_of)
        .filter(|id| is_wire_id(id));
    let is_reply = kind == PeerKind::Reply;
    let disposition = is_reply.then(|| {
        entry(entries, "disposition")
            .and_then(text_of)
            .and_then(|name| Disposition::from_wire_name(&name))
            .unwrap_or_default()
    });
    let retry_after = is_reply
        .then(|| entry(entries, "retry_after").and_then(Value::as_u64))
        .flatten()
        .and_then(|secs| u32::try_from(secs).ok());
    let (parts, dropped_parts) = match entry(entries, "parts") {
        None => (Vec::new(), 0),
        Some(value) => decode_parts(value),
    };
    BodyExtras {
        thread,
        disposition,
        retry_after,
        parts,
        dropped_parts,
    }
}

/// Reads a `/message` body. Anything the sender's own `OutboundPeer::new` would have
/// refused is refused here too: a well-behaved peer never sends it, so it is not worth
/// truncating for. Fields that nest too deeply are dropped and the message kept.
pub(crate) fn from_r3_body(body: &Value) -> Result<PeerBody, &'static str> {
    let entries = body.as_map().ok_or("the body is not a map")?;
    if entry(entries, "v").and_then(Value::as_u64) != Some(PEER_WIRE_VERSION) {
        return Err("v is missing or not the supported version");
    }
    let kind = entry(entries, "kind")
        .and_then(kind_of)
        .ok_or("kind is missing or unknown")?;
    let id = entry(entries, "id")
        .and_then(text_of)
        .filter(|id| is_wire_id(id))
        .ok_or("id is missing, blank, too long or outside the id alphabet")?;
    let in_reply_to = match entry(entries, "in_reply_to") {
        None => None,
        Some(value) => Some(
            text_of(value)
                .filter(|id| is_wire_id(id))
                .ok_or("in_reply_to is not a message id")?,
        ),
    };
    let title = match entry(entries, "title") {
        None => None,
        Some(value) => Some(
            text_of(value)
                .filter(|title| title.chars().count() <= PEER_TITLE_MAX_CHARS)
                .ok_or("title is not text or is too long")?,
        ),
    };
    let content = entry(entries, "content")
        .and_then(text_of)
        .filter(|content| content.chars().count() <= PEER_CONTENT_MAX_CHARS)
        .ok_or("content is missing, not text or too long")?;
    let fields = match entry(entries, "fields") {
        None => None,
        Some(value @ Value::Map(_)) => json_from_rmpv(value, 1),
        Some(_) => return Err("fields is not a map"),
    };
    let timestamp = entry(entries, "ts")
        .and_then(Value::as_f64)
        .filter(|ts| ts.is_finite())
        .ok_or("ts is missing or not a finite number")?;
    let BodyExtras {
        thread,
        disposition,
        retry_after,
        parts,
        dropped_parts,
    } = body_extras(entries, kind);
    Ok(PeerBody {
        kind,
        id,
        in_reply_to,
        title,
        content,
        fields,
        timestamp,
        thread,
        disposition,
        retry_after,
        parts,
        dropped_parts,
    })
}

/// A peer message as a propagation node stores it: the words as LXMF title and content,
/// the type, routing and this node's origin name in the custom fields. The recipient
/// recomputes the sending destination from that name and the signer's identity, as the
/// dispatcher does for a link request, so a stored message can only ever name one of the
/// sender's own instances.
pub(crate) fn peer_lxmf_message(message: &OutboundPeer, origin: &OriginName) -> OutboundMessage {
    let mut data = vec![
        (Value::from("kind"), Value::from(message.kind.wire_name())),
        (Value::from("id"), Value::from(message.id.as_str())),
    ];
    if let Some(in_reply_to) = &message.in_reply_to {
        data.push((
            Value::from("in_reply_to"),
            Value::from(in_reply_to.as_str()),
        ));
    }
    if let Some(thread) = &message.thread {
        data.push((Value::from("thread"), Value::from(thread.as_str())));
    }
    data.push((Value::from("name_hash"), Value::Binary(origin.0.to_vec())));
    if let Some(fields) = &message.fields {
        data.push((Value::from("fields"), rmpv_from_json(fields)));
    }
    data.extend(reply_entries(message));
    if !message.parts.is_empty() {
        data.push((Value::from("parts"), encode_parts(&message.parts)));
    }
    OutboundMessage {
        title: message
            .title
            .as_ref()
            .map(|title| title.as_bytes().to_vec()),
        content: message.content.as_bytes().to_vec(),
        fields: Some(Value::Map(vec![
            (
                Value::from(FIELD_CUSTOM_TYPE),
                Value::from(PEER_MESSAGE_TYPE),
            ),
            (Value::from(FIELD_CUSTOM_DATA), Value::Map(data)),
        ])),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PeerLxmf {
    NotAPeer,
    /// Typed as a peer message but not laid out as one; never a plain message either.
    Malformed(&'static str),
    Peer(Box<LxmfPeer>),
}

/// What a peer message carries on the store-and-forward route, still the peer's own text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LxmfPeer {
    pub name_hash: [u8; NAME_HASH_LEN],
    pub kind: PeerKind,
    pub id: String,
    pub in_reply_to: Option<String>,
    pub title: Option<String>,
    pub content: String,
    pub fields: Option<serde_json::Value>,
    pub thread: Option<String>,
    pub disposition: Option<Disposition>,
    pub retry_after: Option<u32>,
    pub parts: Vec<RawPart>,
    pub dropped_parts: u32,
}

/// Reads a fetched message as a peer message. Text is returned as sent: the recipient
/// cannot refuse a stored message back to its sender, so `PeerMessage::new` caps it
/// rather than dropping the words.
pub(crate) fn decode_peer_lxmf(message: &InboundMessage) -> PeerLxmf {
    let Some(Value::Map(fields)) = &message.fields else {
        return PeerLxmf::NotAPeer;
    };
    let field = |key: u8| {
        fields
            .iter()
            .find(|(field, _)| field.as_u64() == Some(u64::from(key)))
            .map(|(_, value)| value)
    };
    let Some(kind) = field(FIELD_CUSTOM_TYPE) else {
        return PeerLxmf::NotAPeer;
    };
    let is_peer = match kind {
        Value::String(text) => text.as_str() == Some(PEER_MESSAGE_TYPE),
        Value::Binary(bytes) => bytes == PEER_MESSAGE_TYPE.as_bytes(),
        _ => false,
    };
    if !is_peer {
        return PeerLxmf::NotAPeer;
    }
    let Some(Value::Map(data)) = field(FIELD_CUSTOM_DATA) else {
        return PeerLxmf::Malformed("custom data is missing or not a map");
    };
    let Some(Value::Binary(bytes)) = entry(data, "name_hash") else {
        return PeerLxmf::Malformed("name_hash is missing or not binary");
    };
    let Ok(name_hash) = <[u8; NAME_HASH_LEN]>::try_from(bytes.as_slice()) else {
        return PeerLxmf::Malformed("name_hash is not 10 bytes");
    };
    let Some(kind) = entry(data, "kind").and_then(kind_of) else {
        return PeerLxmf::Malformed("kind is missing or unknown");
    };
    let Some(id) = entry(data, "id")
        .and_then(text_of)
        .filter(|id| !id.is_empty())
    else {
        return PeerLxmf::Malformed("id is missing or blank");
    };
    if !is_wire_id(&id) {
        return PeerLxmf::Malformed("id is too long or has characters outside the id alphabet");
    }
    let in_reply_to = match entry(data, "in_reply_to").map(text_of) {
        None => None,
        Some(Some(id)) if is_wire_id(&id) => Some(id),
        Some(_) => return PeerLxmf::Malformed("in_reply_to is not a message id"),
    };
    let BodyExtras {
        thread,
        disposition,
        retry_after,
        parts,
        dropped_parts,
    } = body_extras(data, kind);
    let bytes_text = |bytes: &Option<Vec<u8>>| {
        bytes
            .as_deref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    };
    PeerLxmf::Peer(Box::new(LxmfPeer {
        name_hash,
        kind,
        id,
        in_reply_to,
        title: bytes_text(&message.title),
        content: bytes_text(&message.content).unwrap_or_default(),
        fields: entry(data, "fields").and_then(|fields| json_from_rmpv(fields, 1)),
        thread,
        disposition,
        retry_after,
        parts,
        dropped_parts,
    }))
}

/// The handler's answer once the message is in the inbox.
pub(crate) fn received_reply(id: &str) -> Value {
    Value::Map(vec![
        (Value::from("received"), Value::from(true)),
        (Value::from("id"), Value::from(id)),
    ])
}

/// `true` only for `received_reply(id)`: an echo, a dispatch error or an acknowledgement
/// of some other id all count as not delivered.
pub(crate) fn is_received_reply(value: &Value, id: &str) -> bool {
    let Some(entries) = value.as_map() else {
        return false;
    };
    entry(entries, "received").and_then(Value::as_bool) == Some(true)
        && entry(entries, "id").and_then(Value::as_str) == Some(id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendError {
    /// Unknown to the peer table, known but untrusted, denied or blocked: one text for
    /// all four, since telling them apart tells the model nothing it can act on.
    NotTrusted {
        destination: String,
    },
    /// Trusted, but its identity has not announced since this node started, so there is
    /// no description to link to.
    UnknownDestination {
        destination: String,
    },
    NotRunning,
    ContentTooLong {
        chars: usize,
        max: usize,
    },
    TitleTooLong {
        chars: usize,
        max: usize,
    },
    InvalidFields(&'static str),
    /// A part breaks a rule the receiver would drop it for; the text names the rule.
    InvalidParts(&'static str),
    /// The peer answered with a refusal code; nothing is stored for a peer that said no.
    Refused(RefusalCode),
    /// The direct attempt failed for a reason that is not the peer being unreachable, so
    /// nothing was stored for it.
    Direct(R3Error),
    /// The peer speaks a protocol this Coyote does not, on record from its announce or said
    /// over the link. Nothing is stored: a held copy would meet the same peer.
    IncompatibleVersion {
        destination: String,
        found: Option<u16>,
        min: u16,
        max: u16,
    },
    NoPropagationNode,
    Propagation(PropagationError),
    /// The peer answered, but not with the acknowledgement for this id.
    NotAcknowledged,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTrusted { destination } => write!(
                f,
                "Destination {destination} is not trusted for messaging. Trust it first with `.mesh trust {destination}`; `.mesh peers` lists what this Coyote has heard from."
            ),
            Self::UnknownDestination { destination } => write!(
                f,
                "Destination {destination} is trusted but has not announced since this node started, so there is no path to it yet. Wait for its next announce; `.mesh peers` shows when it was last heard."
            ),
            Self::NotRunning => write!(
                f,
                "The mesh node has been stopped; run `.mesh on` to start it again"
            ),
            Self::ContentTooLong { chars, max } => write!(
                f,
                "The message is {chars} characters, above the {max}-character limit"
            ),
            Self::TitleTooLong { chars, max } => write!(
                f,
                "The title is {chars} characters, above the {max}-character limit"
            ),
            Self::InvalidFields(reason) => write!(
                f,
                "The message fields were refused: {reason} (at most {PEER_FIELDS_MAX_DEPTH} levels deep and {PEER_FIELDS_MAX_BYTES} bytes serialised)"
            ),
            Self::InvalidParts(reason) => write!(
                f,
                "The message parts were refused: {reason} (at most {MAX_PARTS} parts, {MAX_PARTS_BYTES} bytes encoded, {MAX_INLINE_FILE_TOTAL} inline file bytes)"
            ),
            Self::Refused(RefusalCode::NoAccess) => write!(
                f,
                "The peer does not trust this instance and refused the message; `.mesh knock <destination>` asks it to"
            ),
            Self::Refused(code) => write!(f, "The peer refused the message: {code}"),
            Self::Direct(err) => write!(f, "The message could not be sent: {err}"),
            Self::IncompatibleVersion {
                destination,
                found,
                min,
                max,
            } => write!(
                f,
                "Destination {destination} and this Coyote speak incompatible mesh protocol versions: version {} was refused by the side that supports {min}..={max}. One of the two needs upgrading before they can talk; the peer listing names which.",
                describe_version(*found)
            ),
            Self::NoPropagationNode => write!(
                f,
                "The peer is unreachable and no propagation node is known yet to hold the message for it. Run `.mesh peers` to see which nodes this Coyote has heard from."
            ),
            Self::Propagation(err) => write!(
                f,
                "The peer is unreachable and the message could not be stored: {err}"
            ),
            Self::NotAcknowledged => write!(
                f,
                "The peer answered but did not acknowledge the message; it may be running an older Coyote that serves nothing at {MESSAGE_PATH}"
            ),
        }
    }
}

impl std::error::Error for SendError {}

impl SendError {
    /// The variant as a stable snake_case token, for hooks.
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::NotTrusted { .. } => "not_trusted",
            Self::UnknownDestination { .. } => "unknown_destination",
            Self::NotRunning => "not_running",
            Self::ContentTooLong { .. } => "content_too_long",
            Self::TitleTooLong { .. } => "title_too_long",
            Self::InvalidFields(_) => "invalid_fields",
            Self::InvalidParts(_) => "invalid_parts",
            Self::Refused(_) => "refused",
            Self::Direct(_) => "direct",
            Self::IncompatibleVersion { .. } => "incompatible_version",
            Self::NoPropagationNode => "no_propagation_node",
            Self::Propagation(_) => "propagation",
            Self::NotAcknowledged => "not_acknowledged",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SendOutcome {
    pub id: String,
    pub via: PeerVia,
}

/// Timeouts for one send: the direct attempt, then the fallback post.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PeerSendOptions {
    pub request: RequestOptions,
    pub propagation: PropagationOptions,
}

impl Default for PeerSendOptions {
    fn default() -> Self {
        Self {
            request: RequestOptions {
                request_timeout: PEER_REQUEST_TIMEOUT,
                link_timeout: PEER_LINK_TIMEOUT,
            },
            propagation: PropagationOptions::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub(crate) enum RecipientOutcome {
    Delivered,
    StoreAndForward,
    Unreachable { reason: String },
    Refused { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RecipientReport {
    pub destination: String,
    pub display_name: Option<String>,
    #[serde(flatten)]
    pub outcome: RecipientOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct BroadcastOutcome {
    pub id: String,
    pub recipients: Vec<RecipientReport>,
}

/// A broadcast's recipients by outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BulletinTally {
    pub recipients: usize,
    pub delivered: usize,
    pub stored: usize,
    pub unreachable: usize,
    pub refused: usize,
}

impl BroadcastOutcome {
    pub(crate) fn tally(&self) -> BulletinTally {
        let mut tally = BulletinTally {
            recipients: self.recipients.len(),
            ..BulletinTally::default()
        };
        for report in &self.recipients {
            match report.outcome {
                RecipientOutcome::Delivered => tally.delivered += 1,
                RecipientOutcome::StoreAndForward => tally.stored += 1,
                RecipientOutcome::Unreachable { .. } => tally.unreachable += 1,
                RecipientOutcome::Refused { .. } => tally.refused += 1,
            }
        }
        tally
    }
}

pub(crate) fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

impl MeshRuntime {
    /// Sends `message` to the trusted instance at `destination_hex`. The link is tried
    /// first; a peer that cannot be reached gets the message held by a propagation node
    /// until it next fetches.
    pub(crate) async fn send_peer(
        &self,
        destination_hex: &str,
        message: &OutboundPeer,
    ) -> Result<SendOutcome, SendError> {
        self.send_peer_with(destination_hex, message, PeerSendOptions::default())
            .await
    }

    /// `send_peer` with its timeouts chosen. Trust is checked before anything touches
    /// the wire, and a destination the peer table has never heard is as untrusted as one
    /// the list refuses. Only the peer-unreachable errors fall back to store-and-forward:
    /// a refusal of any code means the peer heard and said no, and storing a message it
    /// has refused would deliver it behind its back. Posts to the propagation node queue
    /// behind `posting`, since `propagate` needs one caller per node at a time.
    pub(crate) async fn send_peer_with(
        &self,
        destination_hex: &str,
        message: &OutboundPeer,
        options: PeerSendOptions,
    ) -> Result<SendOutcome, SendError> {
        let outcome = self
            .send_peer_inner(destination_hex, message, options)
            .await;
        // A bulletin's fan-out reports once, through `mesh.bulletin.sent`.
        if message.kind != PeerKind::Bulletin {
            let destination = canonical_hash(destination_hex);
            self.hooks().fire(match &outcome {
                Ok(sent) => MeshEvent::MessageSent {
                    kind: message.kind,
                    id: message.id.clone(),
                    destination,
                    via: sent.via,
                },
                Err(err) => MeshEvent::MessageFailed {
                    kind: message.kind,
                    id: message.id.clone(),
                    destination,
                    class: err.class(),
                    error: err.to_string(),
                },
            });
        }
        outcome
    }

    async fn send_peer_inner(
        &self,
        destination_hex: &str,
        message: &OutboundPeer,
        options: PeerSendOptions,
    ) -> Result<SendOutcome, SendError> {
        // The error text names the destination, so a value that is not a hash is
        // cleaned before it can carry anything into a hook or the model's view.
        let Some(destination) = canonical_hash(destination_hex) else {
            return Err(SendError::NotTrusted {
                destination: display_text(destination_hex, 64).unwrap_or_default(),
            });
        };
        let not_trusted = || SendError::NotTrusted {
            destination: destination.clone(),
        };
        let peer = self.peers().get(&destination).ok_or_else(not_trusted)?;
        if self
            .trust()
            .authorize(&peer.identity_hash, &destination)
            .decision
            != Decision::Allow
        {
            return Err(not_trusted());
        }
        let desc = self
            .resolve_destination(&destination)
            .await
            .ok_or_else(|| SendError::UnknownDestination {
                destination: destination.clone(),
            })?;
        let dest8 = short(&destination);
        let (kind, id) = (message.kind, message.id.as_str());
        let unreachable = match self
            .request(
                &desc,
                MESSAGE_PATH,
                to_r3_body(message, unix_now()),
                options.request,
            )
            .await
        {
            Ok(outcome) if is_received_reply(&outcome.value, id) => {
                debug!("Mesh {kind} {id} to {dest8} was delivered over a link");
                return Ok(SendOutcome {
                    id: message.id.clone(),
                    via: PeerVia::Direct,
                });
            }
            Ok(_) => {
                debug!("Mesh {kind} {id} to {dest8} was answered but not acknowledged");
                return Err(SendError::NotAcknowledged);
            }
            Err(err @ (R3Error::Timeout { .. } | R3Error::LinkFailed(_) | R3Error::LinkClosed)) => {
                err
            }
            Err(R3Error::Refused(code)) => {
                debug!("Mesh {kind} {id} to {dest8} was refused: {code}");
                return Err(SendError::Refused(code));
            }
            Err(R3Error::NotRunning | R3Error::Shutdown) => return Err(SendError::NotRunning),
            Err(R3Error::UnsupportedVersion { found, min, max }) => {
                return Err(SendError::IncompatibleVersion {
                    destination: destination.clone(),
                    found,
                    min,
                    max,
                });
            }
            Err(err) => {
                debug!(
                    "Mesh {kind} {id} to {dest8} was not sent over the link: {}",
                    redact_hashes(&err.to_string())
                );
                return Err(SendError::Direct(err));
            }
        };
        // Selected before queueing behind another post: a sender with no node to fall
        // back on is told so at once rather than after someone else's transfer.
        let node = self
            .propagation_nodes()
            .select_for_posting()
            .map_err(|_| SendError::NoPropagationNode)?;
        let node_hex = node.destination.address_hash.to_hex_string();
        debug!(
            "Mesh {kind} {id} to {dest8} could not be delivered over a link ({}); storing it with propagation node {}",
            redact_hashes(&unreachable.to_string()),
            short(&node_hex)
        );
        self.post_to_node(
            &desc.identity,
            &node,
            |origin| peer_lxmf_message(message, origin),
            &options.propagation,
        )
        .await
        .map_err(|err| match err {
            PropagationError::Cancelled
            | PropagationError::Link(R3Error::Shutdown | R3Error::NotRunning) => {
                SendError::NotRunning
            }
            other => SendError::Propagation(other),
        })?;
        Ok(SendOutcome {
            id: message.id.clone(),
            via: PeerVia::StoreAndForward,
        })
    }

    /// Sends `message` to every trusted instance this node has a path to, a few at a
    /// time. Each recipient's outcome is reported on its own, a node that stops mid-way
    /// included: the recipients not yet reached read as unreachable, so the ones that
    /// were are still reported. The only error is the node having stopped before the
    /// fan-out starts.
    pub(crate) async fn broadcast(
        &self,
        message: &OutboundPeer,
    ) -> Result<BroadcastOutcome, SendError> {
        self.broadcast_with(message, PeerSendOptions::default())
            .await
    }

    pub(crate) async fn broadcast_with(
        &self,
        message: &OutboundPeer,
        options: PeerSendOptions,
    ) -> Result<BroadcastOutcome, SendError> {
        let transport = self.transport_handle().await.ok_or(SendError::NotRunning)?;
        let trust = self.trust();
        let mut recipients = Vec::new();
        for peer in self.peers().snapshot() {
            if trust
                .authorize(&peer.identity_hash, &peer.destination_hash)
                .decision
                != Decision::Allow
            {
                continue;
            }
            let Ok(hash) = AddressHash::new_from_hex_string(&peer.destination_hash) else {
                continue;
            };
            if transport.has_path(&hash).await {
                recipients.push(peer);
            }
        }
        drop(transport);
        let mut recipients: Vec<RecipientReport> = stream::iter(recipients)
            .map(|peer| self.report_send(peer, message, options))
            .buffer_unordered(BROADCAST_MAX_CONCURRENCY)
            .collect()
            .await;
        recipients.sort_by(|a, b| a.destination.cmp(&b.destination));
        let outcome = BroadcastOutcome {
            id: message.id.clone(),
            recipients,
        };
        self.hooks().fire(MeshEvent::BulletinSent {
            id: outcome.id.clone(),
            tally: outcome.tally(),
        });
        Ok(outcome)
    }

    /// One broadcast recipient's send as a report.
    async fn report_send(
        &self,
        peer: PeerRecord,
        message: &OutboundPeer,
        options: PeerSendOptions,
    ) -> RecipientReport {
        let outcome = match self
            .send_peer_with(&peer.destination_hash, message, options)
            .await
        {
            Ok(SendOutcome {
                via: PeerVia::Direct,
                ..
            }) => RecipientOutcome::Delivered,
            Ok(SendOutcome {
                via: PeerVia::StoreAndForward,
                ..
            }) => RecipientOutcome::StoreAndForward,
            Err(err @ (SendError::Refused(_) | SendError::IncompatibleVersion { .. })) => {
                RecipientOutcome::Refused {
                    reason: err.to_string(),
                }
            }
            Err(err) => RecipientOutcome::Unreachable {
                reason: err.to_string(),
            },
        };
        RecipientReport {
            destination: peer.destination_hash,
            display_name: peer
                .display_name
                .as_deref()
                .and_then(|name| display_text(name, DISPLAY_NAME_MAX_CHARS)),
            outcome,
        }
    }

    /// The description a request to `destination_hex` links to: the identity the transport
    /// learned from its announce and the name hash the peer table kept. `None` when either
    /// is missing, which is the case until the peer announces again after this node
    /// started, or when the two do not derive the destination, since a peer record that
    /// does not is not that instance's.
    pub(crate) async fn resolve_destination(
        &self,
        destination_hex: &str,
    ) -> Option<DestinationDesc> {
        let destination = canonical_hash(destination_hex)?;
        let address_hash = AddressHash::new_from_hex_string(&destination).ok()?;
        let name_hash: [u8; NAME_HASH_LEN] =
            decode_hex(&self.peers().get(&destination)?.name_hash)?
                .try_into()
                .ok()?;
        let identity = self
            .transport_handle()
            .await?
            .destination_identity(&address_hash)
            .await?;
        if destination_address(&name_hash, &identity.address_hash) != address_hash {
            return None;
        }
        Some(DestinationDesc {
            identity,
            address_hash,
            name: DestinationName::new_from_hash_slice(&name_hash),
        })
    }

    /// Whether the transport knows a route to `destination_hex` right now.
    pub(crate) async fn path_known(&self, destination_hex: &str) -> bool {
        let Some(hash) = canonical_hash(destination_hex)
            .and_then(|hex| AddressHash::new_from_hex_string(&hex).ok())
        else {
            return false;
        };
        match self.transport_handle().await {
            Some(transport) => transport.has_path(&hash).await,
            None => false,
        }
    }
}

/// One inbound message put to the surface for admission: who signed it, the instance a
/// refusal would be answered to, its id for the correlation, its kind, the question it
/// replies to if it is itself a reply (a refusal never answers one), the thread a refusal
/// inherits, and the path it took.
pub(crate) struct PeerAdmission<'a> {
    pub source_identity: &'a str,
    pub source_destination: &'a str,
    pub message_id: &'a str,
    pub kind: PeerKind,
    pub in_reply_to: Option<&'a str>,
    pub thread: Option<&'a str>,
    pub disposition: Option<Disposition>,
    pub via: PeerVia,
}

/// Where inbound peer messages land. Held weakly by the handler and the runtime: the
/// slot owns the runtime that owns both. `deliver_peer` may write the pending store, so
/// the request path runs it on a blocking thread; the fetch task calls it in place.
pub(crate) trait PeerSurface: Send + Sync {
    /// Counts one message from the sender against its hourly limit, and on a link first
    /// asks whether a message the envoy would run could run now: a sender with a run in
    /// flight, an already-spent token or cost window, or a full envoy queue is refused
    /// uncounted. `Err` means the message must not reach the envoy: a link caller
    /// refuses it, a store-and-forward caller files it with `file_peer`.
    fn admit_peer_message(&self, request: &PeerAdmission) -> Result<(), PeerRefusal>;
    fn deliver_peer(&self, message: PeerMessage);
    /// The inbox path only, for a message admission refused: the human still sees it,
    /// the envoy never does.
    fn file_peer(&self, message: PeerMessage);
    /// Lower-hex of the destination this node receives on right now; `None` while off.
    fn local_destination(&self) -> Option<String>;
    fn part_limits(&self) -> PartLimits {
        PartLimits::default()
    }
    /// Where an inline file part is written; `None` drops every inline file, counted.
    fn inbox_staging(&self) -> Option<InboxStaging> {
        None
    }
}

/// Serves `/message` to whoever the dispatcher has already let through: decodes and
/// bounds the body, hands the message to the surface and acknowledges by id. A sender
/// over its hourly limit, or one whose message or question the envoy could not run now
/// (a run of its already in flight, its token or cost window spent, the envoy queue
/// full), is refused with `Throttled` before anything is delivered, so it never earns
/// an acknowledgement and nothing is filed. A surface that is gone means the node is
/// stopping, and a message nobody will read is left unacknowledged so the sender falls
/// back to storing it; so is one whose delivery thread failed, since nothing says it
/// landed.
pub(crate) struct PeerMessageHandler {
    surface: Weak<dyn PeerSurface>,
}

impl PeerMessageHandler {
    pub(crate) fn new(surface: Weak<dyn PeerSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Handler for PeerMessageHandler {
    async fn handle(&self, request: AdmittedRequest) -> Reply {
        let identity_hex = request.identity.address_hash.to_hex_string();
        let destination_hex = request.destination_hash.to_hex_string();
        let (id8, dest8) = (short(&identity_hex), short(&destination_hex));
        let link = request.link_id.to_hex_string();
        let body = match from_r3_body(&request.body) {
            Ok(body) => body,
            Err(why) => {
                debug!(
                    "Mesh message from {id8} (instance {dest8}) on link {link} refused: {}",
                    redact_hashes(why)
                );
                return Reply::Code(RefusalCode::InvalidData);
            }
        };
        let Some(surface) = self.surface.upgrade() else {
            debug!(
                "Mesh {} from {id8} (instance {dest8}) on link {link} dropped: the session slot behind the provider is gone",
                body.kind
            );
            return Reply::Silent;
        };
        let admission = PeerAdmission {
            source_identity: &identity_hex,
            source_destination: &destination_hex,
            message_id: &body.id,
            kind: body.kind,
            in_reply_to: body.in_reply_to.as_deref(),
            thread: body.thread.as_deref(),
            disposition: body.disposition,
            via: PeerVia::Direct,
        };
        if let Err(refusal) = surface.admit_peer_message(&admission) {
            debug!(
                "Mesh {} {} from {id8} (instance {dest8}) on link {link} refused with Throttled: {} (retry after {} s)",
                body.kind,
                body.id,
                refusal.reason.as_str(),
                refusal.retry_after.as_secs()
            );
            return Reply::Code(RefusalCode::Throttled);
        }
        // The sender checks the acknowledgement against the id it sent, so the raw wire
        // id is echoed; the alphabet check in `from_r3_body` has already bounded it.
        let id = body.id.clone();
        let kind = body.kind;
        let raw = RawPeerMessage {
            source_identity: identity_hex.clone(),
            source_destination: destination_hex.clone(),
            destination: surface.local_destination().unwrap_or_default(),
            title: body.title,
            content: body.content,
            fields: body.fields,
            timestamp: body.timestamp,
            message_id: body.id,
            in_reply_to: body.in_reply_to,
            kind: body.kind,
            via: PeerVia::Direct,
            thread: body.thread,
            disposition: body.disposition,
            retry_after: body.retry_after,
            parts: body.parts,
            dropped_parts: body.dropped_parts,
        };
        debug!("Mesh {kind} {id} from {id8} (instance {dest8}) received on link {link}");
        // Staging an inline file writes to disk, so the sanitising runs off the request
        // loop with the delivery.
        let delivered = tokio::task::spawn_blocking(move || {
            let message = PeerMessage::new_with(
                raw,
                &surface.part_limits(),
                surface.inbox_staging().as_ref(),
            );
            surface.deliver_peer(message)
        })
        .await;
        match delivered {
            Ok(()) => Reply::Value(received_reply(&id)),
            Err(err) => {
                warn!(
                    "Mesh message {id} from {id8} (instance {dest8}) was not delivered to the session: {}; leaving it unacknowledged so the peer stores it instead",
                    redact_hashes(&err.to_string())
                );
                Reply::Silent
            }
        }
    }
}

/// An `InboundSink` in front of another: fetched peer messages go to the surface once the
/// trust list allows the instance they name, everything else to `inner`. A payload typed
/// as a peer message is never a plain message, so a malformed or untrusted one is dropped
/// rather than forwarded. As on the link and knock paths, an unknown or blocked signer is
/// dropped before any record is consulted; past that gate the verdict is
/// `authorize_origin`'s, so a message naming an instance bound to another identity marks
/// the record and tells the human as a link request would, whichever way it is judged.
///
/// A sender over its hourly limit still has its message filed in the inbox, without an
/// envoy run. Store-and-forward has no link to carry a refusal code back on, so the
/// surface answers the first refusal of each reason in the hour with a typed reply and
/// prints its line for the person at the keyboard.
pub(crate) struct PeerRouting<'a> {
    pub trust: &'a TrustStore,
    pub surface: Option<Arc<dyn PeerSurface>>,
    pub inner: &'a dyn InboundSink,
}

impl InboundSink for PeerRouting<'_> {
    fn deliver(&self, message: InboundMessage) {
        let id8 = short(&message.source_identity_hash);
        // The sending instance and the local destination are filled in below, once the
        // signer's identity has been read and the surface found.
        let (name_hash, mut raw) = match decode_peer_lxmf(&message) {
            PeerLxmf::NotAPeer => return self.inner.deliver(message),
            PeerLxmf::Malformed(why) => {
                debug!(
                    "Propagated peer message from {id8} dropped: {}",
                    redact_hashes(why)
                );
                return;
            }
            PeerLxmf::Peer(peer) => {
                let peer = *peer;
                (
                    peer.name_hash,
                    RawPeerMessage {
                        source_identity: message.source_identity_hash.clone(),
                        source_destination: String::new(),
                        destination: String::new(),
                        title: peer.title,
                        content: peer.content,
                        fields: peer.fields,
                        timestamp: message.timestamp,
                        message_id: peer.id,
                        in_reply_to: peer.in_reply_to,
                        kind: peer.kind,
                        via: PeerVia::StoreAndForward,
                        thread: peer.thread,
                        disposition: peer.disposition,
                        retry_after: peer.retry_after,
                        parts: peer.parts,
                        dropped_parts: peer.dropped_parts,
                    },
                )
            }
        };
        let kind = raw.kind;
        let Ok(identity) = AddressHash::new_from_hex_string(&message.source_identity_hash) else {
            debug!("Propagated peer message from {id8} dropped: the signer's hash is malformed");
            return;
        };
        let standing = match self.trust.identity_standing(&message.source_identity_hash) {
            IdentityStanding::Trusted { .. } => None,
            IdentityStanding::Unknown => Some("unknown"),
            IdentityStanding::Blocked => Some("blocked"),
        };
        if let Some(standing) = standing {
            debug!("Propagated {kind} from {id8} dropped: {standing} identity");
            return;
        }
        let OriginVerdict {
            verdict,
            destination,
            collisions,
        } = self.trust.authorize_origin(&identity, &name_hash);
        raw.source_destination = destination.to_hex_string();
        let dest8 = short(&raw.source_destination).to_string();
        if !collisions.is_empty() || verdict.rule == Rule::IdentityChanged {
            let outcome = match verdict.decision {
                Decision::Allow => KeyChangeOutcome::Served,
                Decision::Refuse => KeyChangeOutcome::Refused,
            };
            self.trust.note_key_change(
                &message.source_identity_hash,
                &hex_lower(&name_hash),
                outcome,
                SystemTime::now(),
            );
        }
        if verdict.decision != Decision::Allow {
            debug!("Propagated {kind} from {id8} dropped: instance {dest8} is not trusted");
            return;
        }
        let Some(surface) = &self.surface else {
            debug!(
                "Propagated {kind} from {id8} (instance {dest8}) dropped: the session slot is gone"
            );
            return;
        };
        let admission = PeerAdmission {
            source_identity: &message.source_identity_hash,
            source_destination: &raw.source_destination,
            message_id: &raw.message_id,
            kind,
            in_reply_to: raw.in_reply_to.as_deref(),
            thread: raw.thread.as_deref(),
            disposition: raw.disposition,
            via: PeerVia::StoreAndForward,
        };
        let admitted = surface.admit_peer_message(&admission);
        raw.destination = surface.local_destination().unwrap_or_default();
        let staging = match admitted {
            Ok(()) => surface.inbox_staging(),
            Err(_) => None,
        };
        let peer = PeerMessage::new_with(raw, &surface.part_limits(), staging.as_ref());
        if let Err(refusal) = admitted {
            debug!(
                "Propagated {kind} {} from {id8} (instance {dest8}) over its limit: {}; filed in the inbox without an envoy run and without staging its inline files, the surface answers the first refusal of the hour",
                peer.message_id,
                refusal.reason.as_str()
            );
            return surface.file_peer(peer);
        }
        debug!(
            "Propagated {kind} {} from {id8} (instance {dest8}) received",
            peer.message_id
        );
        surface.deliver_peer(peer);
    }
}

/// The peer channel of an `Inbox`: at `PEER_INBOX_CAPACITY` the oldest peer envelope
/// makes room and the loss is counted until the next drain reports it.
#[derive(Default)]
pub(crate) struct PeerInbox {
    inbox: Inbox,
    dropped: Mutex<usize>,
}

impl PeerInbox {
    pub(crate) fn deliver(&self, message: PeerMessage) {
        let envelope = Envelope {
            from: message.source_destination.clone(),
            to: message.destination.clone(),
            payload: EnvelopePayload::Peer(Box::new(message)),
            timestamp: chrono::Utc::now(),
        };
        if self
            .inbox
            .deliver_bounded(envelope, PEER_INBOX_CAPACITY)
            .is_some()
        {
            let mut dropped = self.dropped.lock();
            *dropped += 1;
            debug!(
                "Mesh peer inbox is at its {PEER_INBOX_CAPACITY}-message cap; dropped the oldest message ({} dropped since the last drain)",
                *dropped
            );
        }
    }

    /// Everything waiting, in the inbox's priority order, and how many peer messages
    /// were dropped since the last drain.
    pub(crate) fn drain(&self) -> (Vec<Envelope>, usize) {
        let dropped = std::mem::take(&mut *self.dropped.lock());
        (self.inbox.drain(), dropped)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inbox.len()
    }
}

#[derive(Default)]
struct NotesState {
    notes: VecDeque<SystemNotification>,
    dropped: usize,
}

/// Notes for the model waiting for its next tool batch, capped like the notification
/// queue's mesh channel: past the cap the oldest goes and `take` says how many did.
#[derive(Default)]
pub(crate) struct ModelNotes {
    state: Mutex<NotesState>,
}

impl ModelNotes {
    pub(crate) fn push(&self, note: SystemNotification) {
        let mut state = self.state.lock();
        if state.notes.len() >= MESH_NOTIFICATION_QUEUE_CAPACITY {
            state.notes.pop_front();
            state.dropped += 1;
        }
        state.notes.push_back(note);
    }

    /// Everything waiting, oldest first, followed by one summary of what was dropped
    /// since the last take, when anything was.
    pub(crate) fn take(&self) -> Vec<SystemNotification> {
        let mut state = self.state.lock();
        let mut notes: Vec<SystemNotification> = state.notes.drain(..).collect();
        let dropped = std::mem::take(&mut state.dropped);
        if dropped > 0 {
            notes.push(mesh_events_dropped(
                dropped,
                "mesh-slot",
                "before this tool batch",
            ));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::events::env_value;
    use crate::mesh::knock::{
        KNOCK_TYPE, KnockIntro, KnockSurface, RecordingSurface as KeyChangeSurface, knock_message,
    };
    use crate::mesh::limits::RefusalReason;
    use crate::mesh::r3::{PathHash, RequestId, SizeBranch};
    use crate::mesh::test_support::{TempDir, TrustList};
    use crate::supervisor::notification::MESH_EVENTS_DROPPED_EVENT;

    use rand_core::OsRng;
    use rns_transport::destination::link::LinkId;
    use rns_transport::identity::PrivateIdentity;
    use std::io::Cursor;

    fn hash_of(seed: &str) -> String {
        let mut bytes = [0u8; 16];
        for (slot, byte) in bytes.iter_mut().zip(seed.bytes()) {
            *slot = byte;
        }
        hex_lower(&bytes)
    }

    fn raw(content: &str) -> RawPeerMessage {
        RawPeerMessage {
            source_identity: hash_of("identity"),
            source_destination: hash_of("instance"),
            destination: hash_of("me"),
            title: None,
            content: content.to_string(),
            fields: None,
            timestamp: 1_700_000_000.0,
            message_id: "id-1".to_string(),
            in_reply_to: None,
            kind: PeerKind::Message,
            via: PeerVia::Direct,
            thread: None,
            disposition: None,
            retry_after: None,
            parts: Vec::new(),
            dropped_parts: 0,
        }
    }

    fn inbound(
        fields: Option<Value>,
        title: Option<Vec<u8>>,
        content: Option<Vec<u8>>,
        source: &str,
    ) -> InboundMessage {
        InboundMessage {
            transient_id: [1u8; 32],
            message_id: [2u8; 32],
            source_identity_hash: source.to_string(),
            source_delivery_hash: hash_of("delivery"),
            timestamp: 1_700_000_000.0,
            title,
            content,
            fields,
            stamp_value: None,
        }
    }

    fn outbound(kind: PeerKind, content: &str) -> OutboundPeer {
        OutboundPeer::new(kind, content, None, None, None).unwrap()
    }

    fn round_trip(value: &Value) -> Value {
        let mut packed = Vec::new();
        rmpv::encode::write_value(&mut packed, value).unwrap();
        rmpv::decode::read_value(&mut Cursor::new(packed)).unwrap()
    }

    #[test]
    fn peer_message_new_caps_and_sanitises_every_peer_string() {
        let deep = (0..PEER_FIELDS_MAX_DEPTH + 1).fold(
            serde_json::json!(1),
            |inner, _| serde_json::json!({ "n": inner }),
        );
        let message = PeerMessage::new(RawPeerMessage {
            title: Some(format!(
                "\u{1b}[31mT\u{1b}[0m{}",
                "t".repeat(PEER_TITLE_MAX_CHARS)
            )),
            content: format!("hi\u{202E}\r\nthere{}", "x".repeat(PEER_CONTENT_MAX_CHARS)),
            fields: Some(deep.clone()),
            message_id: format!("\u{1b}[2J{}", "i".repeat(PEER_ID_MAX_CHARS + 5)),
            in_reply_to: Some("\u{7}".to_string()),
            ..raw("")
        });

        assert_eq!(
            message.title.as_deref().unwrap().chars().count(),
            PEER_TITLE_MAX_CHARS
        );
        assert!(message.title.as_deref().unwrap().starts_with("Tttt"));
        assert!(
            message.content.starts_with("hi  there"),
            "{:?}",
            message.content
        );
        assert_eq!(message.content.chars().count(), PEER_CONTENT_MAX_CHARS);
        assert!(!message.content.contains('\u{202E}'));
        assert_eq!(message.message_id, "i".repeat(PEER_ID_MAX_CHARS));
        assert_eq!(
            message.in_reply_to, None,
            "an id that cleans to nothing is none"
        );
        assert_eq!(
            message.fields, None,
            "too deep is dropped, the message kept"
        );

        let large = serde_json::json!({ "blob": "b".repeat(PEER_FIELDS_MAX_BYTES) });
        assert_eq!(
            PeerMessage::new(RawPeerMessage {
                fields: Some(large),
                ..raw("x")
            })
            .fields,
            None
        );

        let hostile = serde_json::json!({
            "k\u{1b}[1mkey": ["\u{1b}[31mred\u{1b}[0m", { "n": "a\u{200B}b" }],
            "": 7,
            "ok": true,
        });
        let cleaned = PeerMessage::new(RawPeerMessage {
            fields: Some(hostile),
            ..raw("x")
        })
        .fields
        .unwrap();
        assert_eq!(
            cleaned,
            serde_json::json!({ "kkey": ["red", { "n": "ab" }], "": 7, "ok": true })
        );

        let at_depth = (0..PEER_FIELDS_MAX_DEPTH - 1).fold(
            serde_json::json!("leaf"),
            |inner, _| serde_json::json!({ "n": inner }),
        );
        assert!(
            PeerMessage::new(RawPeerMessage {
                fields: Some(at_depth.clone()),
                ..raw("x")
            })
            .fields
            .is_some()
        );

        let blank = PeerMessage::new(raw("  \u{1b}[2J \n"));
        assert_eq!(blank.content, "");
        assert_eq!(
            blank.summary_line(None),
            format!("{} says: (no text)", &hash_of("instance")[..8])
        );

        for bad_clock in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let message = PeerMessage::new(RawPeerMessage {
                timestamp: bad_clock,
                ..raw("x")
            });
            assert_eq!(message.timestamp, 0.0, "{bad_clock}");
        }
    }

    #[test]
    fn summary_line_names_the_peer_and_keeps_to_one_capped_line() {
        let message = PeerMessage::new(RawPeerMessage {
            kind: PeerKind::Ask,
            content: format!("first line\nsecond {}", "w".repeat(PEER_LINE_MAX_CHARS)),
            ..raw("")
        });
        let line = message.summary_line(Some("Bea\u{1b}[2Jtrice"));
        assert!(
            line.starts_with("Beatrice asks: first line second w"),
            "{line}"
        );
        assert!(!line.contains('\n'));
        assert_eq!(
            line.chars().count(),
            "Beatrice asks: ".len() + PEER_LINE_MAX_CHARS
        );

        for (kind, verb) in [
            (PeerKind::Message, "says"),
            (PeerKind::Reply, "replies"),
            (PeerKind::Bulletin, "announces"),
        ] {
            let message = PeerMessage::new(RawPeerMessage { kind, ..raw("yo") });
            assert_eq!(
                message.summary_line(Some("  ")),
                format!("{} {verb}: yo", &hash_of("instance")[..8])
            );
        }
        let titled = PeerMessage::new(RawPeerMessage {
            title: Some("Subject".into()),
            ..raw("")
        });
        assert_eq!(
            titled.summary_line(None),
            format!("{} says: Subject", &hash_of("instance")[..8])
        );
    }

    #[test]
    fn outbound_refuses_over_long_text_and_bad_fields_and_mints_an_id() {
        let err = OutboundPeer::new(
            PeerKind::Message,
            &"x".repeat(PEER_CONTENT_MAX_CHARS + 1),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SendError::ContentTooLong {
                chars: PEER_CONTENT_MAX_CHARS + 1,
                max: PEER_CONTENT_MAX_CHARS
            }
        );
        assert!(err.to_string().contains("4001"), "{err}");
        let err = OutboundPeer::new(
            PeerKind::Message,
            "hi",
            Some(&"t".repeat(PEER_TITLE_MAX_CHARS + 1)),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SendError::TitleTooLong {
                chars: PEER_TITLE_MAX_CHARS + 1,
                max: PEER_TITLE_MAX_CHARS
            }
        );
        let too_large = serde_json::json!({ "blob": "b".repeat(PEER_FIELDS_MAX_BYTES) });
        assert!(matches!(
            OutboundPeer::new(PeerKind::Message, "hi", None, None, Some(too_large)).unwrap_err(),
            SendError::InvalidFields(_)
        ));
        assert!(matches!(
            OutboundPeer::new(
                PeerKind::Reply,
                "hi",
                None,
                Some(&"i".repeat(PEER_ID_MAX_CHARS + 1)),
                None
            )
            .unwrap_err(),
            SendError::InvalidFields(_)
        ));

        let cleaned_under_cap = format!(
            "{}{}",
            "\u{200B}".repeat(50),
            "y".repeat(PEER_CONTENT_MAX_CHARS)
        );
        let message = OutboundPeer::new(
            PeerKind::Ask,
            &cleaned_under_cap,
            Some(" \u{1b}[1mTitle\u{1b}[0m "),
            Some(" reply-to "),
            Some(serde_json::json!({ "k": "v\u{1b}[0m" })),
        )
        .unwrap();
        assert_eq!(message.content, "y".repeat(PEER_CONTENT_MAX_CHARS));
        assert_eq!(message.title.as_deref(), Some("Title"));
        assert_eq!(message.in_reply_to.as_deref(), Some("reply-to"));
        assert_eq!(message.fields, Some(serde_json::json!({ "k": "v" })));
        assert_eq!(message.id.len(), 32);
        assert!(message.id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(message.id, outbound(PeerKind::Ask, "again").id);
        assert_eq!(
            OutboundPeer::new(PeerKind::Message, "x", Some("  "), None, None)
                .unwrap()
                .title,
            None
        );
    }

    #[test]
    fn r3_body_round_trips_and_rejects_malformed() {
        let message = OutboundPeer::new(
            PeerKind::Reply,
            "the answer",
            Some("Re: question"),
            Some("q-1"),
            Some(serde_json::json!({ "n": 1, "list": [true, null, 2.5, "s"], "neg": -3 })),
        )
        .unwrap();
        let body = round_trip(&to_r3_body(&message, 1_700_000_000.5));
        let decoded = from_r3_body(&body).unwrap();
        assert_eq!(
            decoded,
            PeerBody {
                kind: PeerKind::Reply,
                id: message.id.clone(),
                in_reply_to: Some("q-1".into()),
                title: Some("Re: question".into()),
                content: "the answer".into(),
                fields: message.fields.clone(),
                timestamp: 1_700_000_000.5,
                thread: None,
                disposition: Some(Disposition::Answered),
                retry_after: None,
                parts: Vec::new(),
                dropped_parts: 0,
            }
        );
        let bare =
            from_r3_body(&to_r3_body(&outbound(PeerKind::Bulletin, "all hands"), 1.0)).unwrap();
        assert_eq!(bare.in_reply_to, None);
        assert_eq!(bare.title, None);
        assert_eq!(bare.fields, None);

        fn good(edit: impl FnOnce(&mut Vec<(Value, Value)>)) -> Value {
            let Value::Map(mut entries) = to_r3_body(&outbound(PeerKind::Message, "hi"), 1.0)
            else {
                unreachable!()
            };
            edit(&mut entries);
            Value::Map(entries)
        }
        fn set(entries: &mut Vec<(Value, Value)>, key: &str, value: Value) {
            entries.retain(|(k, _)| k.as_str() != Some(key));
            entries.push((Value::from(key), value));
        }
        fn drop_key(entries: &mut Vec<(Value, Value)>, key: &str) {
            entries.retain(|(k, _)| k.as_str() != Some(key));
        }
        for (what, body, why) in [
            ("not a map", Value::from("ping"), "the body is not a map"),
            ("nil", Value::Nil, "the body is not a map"),
            (
                "no v",
                good(|e| drop_key(e, "v")),
                "v is missing or not the supported version",
            ),
            (
                "wrong v",
                good(|e| set(e, "v", Value::from(2))),
                "v is missing or not the supported version",
            ),
            (
                "bad kind",
                good(|e| set(e, "kind", Value::from("shout"))),
                "kind is missing or unknown",
            ),
            (
                "no id",
                good(|e| drop_key(e, "id")),
                "id is missing, blank, too long or outside the id alphabet",
            ),
            (
                "blank id",
                good(|e| set(e, "id", Value::from(""))),
                "id is missing, blank, too long or outside the id alphabet",
            ),
            (
                "long id",
                good(|e| set(e, "id", Value::from("i".repeat(PEER_ID_MAX_CHARS + 1)))),
                "id is missing, blank, too long or outside the id alphabet",
            ),
            (
                "id with a space",
                good(|e| set(e, "id", Value::from("two words"))),
                "id is missing, blank, too long or outside the id alphabet",
            ),
            (
                "id with a quote",
                good(|e| set(e, "id", Value::from("q\"1"))),
                "id is missing, blank, too long or outside the id alphabet",
            ),
            (
                "non-ASCII id",
                good(|e| set(e, "id", Value::from("idé"))),
                "id is missing, blank, too long or outside the id alphabet",
            ),
            (
                "long in_reply_to",
                good(|e| {
                    set(
                        e,
                        "in_reply_to",
                        Value::from("i".repeat(PEER_ID_MAX_CHARS + 1)),
                    )
                }),
                "in_reply_to is not a message id",
            ),
            (
                "in_reply_to with a space",
                good(|e| set(e, "in_reply_to", Value::from("q 1"))),
                "in_reply_to is not a message id",
            ),
            (
                "long title",
                good(|e| {
                    set(
                        e,
                        "title",
                        Value::from("t".repeat(PEER_TITLE_MAX_CHARS + 1)),
                    )
                }),
                "title is not text or is too long",
            ),
            (
                "no content",
                good(|e| drop_key(e, "content")),
                "content is missing, not text or too long",
            ),
            (
                "long content",
                good(|e| {
                    set(
                        e,
                        "content",
                        Value::from("c".repeat(PEER_CONTENT_MAX_CHARS + 1)),
                    )
                }),
                "content is missing, not text or too long",
            ),
            (
                "fields not a map",
                good(|e| set(e, "fields", Value::from(3))),
                "fields is not a map",
            ),
            (
                "no ts",
                good(|e| drop_key(e, "ts")),
                "ts is missing or not a finite number",
            ),
            (
                "nan ts",
                good(|e| set(e, "ts", Value::F64(f64::NAN))),
                "ts is missing or not a finite number",
            ),
        ] {
            assert_eq!(from_r3_body(&body), Err(why), "{what}");
        }

        let binary_text = good(|e| {
            set(e, "id", Value::Binary(b"bin-id".to_vec()));
            set(e, "kind", Value::Binary(b"bulletin".to_vec()));
            set(e, "content", Value::Binary(vec![0xff, b'h', b'i']));
            set(e, "ts", Value::from(1_700_000_000u64));
        });
        let decoded = from_r3_body(&binary_text).unwrap();
        assert_eq!(decoded.id, "bin-id");
        assert_eq!(decoded.kind, PeerKind::Bulletin, "a binary kind reads too");
        assert_eq!(decoded.content, "\u{FFFD}hi");
        assert_eq!(decoded.timestamp, 1_700_000_000.0);

        let unknown_key = good(|e| e.push((Value::from("unknown"), Value::from(1))));
        assert_eq!(
            from_r3_body(&unknown_key).map(|body| body.content),
            Ok("hi".to_string()),
            "an unknown key is ignored"
        );

        let deep = (0..PEER_FIELDS_MAX_DEPTH + 1).fold(Value::from(1), |inner, _| {
            Value::Map(vec![(Value::from("n"), inner)])
        });
        let decoded = from_r3_body(&good(|e| set(e, "fields", deep.clone()))).unwrap();
        assert_eq!(
            decoded.fields, None,
            "fields too deep are dropped, the message kept"
        );
        let odd = good(|e| {
            set(
                e,
                "fields",
                Value::Map(vec![
                    (Value::from(7), Value::Binary(vec![0xab, 0xcd])),
                    (Value::from("f"), Value::F32(1.5)),
                    (Value::from("nil"), Value::Nil),
                ]),
            )
        });
        assert_eq!(
            from_r3_body(&odd).unwrap().fields,
            Some(serde_json::json!({ "7": "abcd", "f": 1.5, "nil": null }))
        );
    }

    #[test]
    fn a_wire_id_is_our_uuid_or_another_short_ascii_token_and_nothing_else() {
        let minted = OutboundPeer::new(PeerKind::Message, "hi", None, None, None)
            .unwrap()
            .id;
        assert_eq!(minted.len(), 32);
        for id in [
            minted.as_str(),
            "Abc-1.2:x",
            "a",
            &"z".repeat(PEER_ID_MAX_CHARS),
        ] {
            assert!(is_wire_id(id), "{id:?}");
        }
        for id in [
            "",
            " ",
            "two words",
            "q\"1",
            "q'1",
            "idé",
            "id\u{1b}[2J",
            "a/b",
            "{}",
            &"z".repeat(PEER_ID_MAX_CHARS + 1),
        ] {
            assert!(!is_wire_id(id), "{id:?}");
        }
        assert!(matches!(
            OutboundPeer::new(PeerKind::Reply, "hi", None, Some("q 1"), None).unwrap_err(),
            SendError::InvalidFields("in_reply_to is not a message id")
        ));
        assert_eq!(
            OutboundPeer::new(PeerKind::Reply, "hi", None, Some(" Abc-1.2:x "), None)
                .unwrap()
                .in_reply_to
                .as_deref(),
            Some("Abc-1.2:x"),
            "the sender's own id is trimmed, then checked"
        );
    }

    #[test]
    fn peer_lxmf_round_trips_and_a_knock_is_not_a_peer() {
        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let message = OutboundPeer::new(
            PeerKind::Ask,
            "are you there",
            Some("ping"),
            Some("prev"),
            Some(serde_json::json!({ "k": ["v"] })),
        )
        .unwrap();
        let stored = peer_lxmf_message(&message, &origin);
        assert_eq!(stored.title.as_deref(), Some(b"ping".as_slice()));
        assert_eq!(stored.content, b"are you there".to_vec());
        assert_eq!(FIELD_CUSTOM_TYPE, 0xFB);
        let Some(Value::Map(fields)) = &stored.fields else {
            panic!("typed fields");
        };
        assert_eq!(
            fields[0],
            (Value::from(0xFBu8), Value::from(PEER_MESSAGE_TYPE))
        );

        let decoded = decode_peer_lxmf(&inbound(
            Some(round_trip(stored.fields.as_ref().unwrap())),
            stored.title.clone(),
            Some(stored.content.clone()),
            &hash_of("signer"),
        ));
        assert_eq!(
            decoded,
            PeerLxmf::Peer(Box::new(LxmfPeer {
                name_hash: origin.0,
                kind: PeerKind::Ask,
                id: message.id.clone(),
                in_reply_to: Some("prev".into()),
                title: Some("ping".into()),
                content: "are you there".into(),
                fields: Some(serde_json::json!({ "k": ["v"] })),
                thread: None,
                disposition: None,
                retry_after: None,
                parts: Vec::new(),
                dropped_parts: 0,
            }))
        );

        let knock = knock_message(&KnockIntro::new("hi").unwrap(), &origin);
        assert_eq!(
            decode_peer_lxmf(&inbound(
                knock.fields,
                None,
                Some(knock.content),
                &hash_of("s")
            )),
            PeerLxmf::NotAPeer
        );
        let typed = |kind: Value| Value::Map(vec![(Value::from(FIELD_CUSTOM_TYPE), kind)]);
        for fields in [
            None,
            Some(Value::Map(vec![])),
            Some(Value::from("fields")),
            Some(typed(Value::from(KNOCK_TYPE))),
            Some(typed(Value::from(3))),
        ] {
            assert_eq!(
                decode_peer_lxmf(&inbound(fields.clone(), None, None, &hash_of("s"))),
                PeerLxmf::NotAPeer,
                "{fields:?}"
            );
        }

        let with_data = |data: Value| {
            Value::Map(vec![
                (
                    Value::from(FIELD_CUSTOM_TYPE),
                    Value::Binary(PEER_MESSAGE_TYPE.as_bytes().to_vec()),
                ),
                (Value::from(FIELD_CUSTOM_DATA), data),
            ])
        };
        let name_hash = (
            Value::from("name_hash"),
            Value::Binary(vec![3; NAME_HASH_LEN]),
        );
        for (data, why) in [
            (Value::Nil, "custom data is missing or not a map"),
            (Value::Map(vec![]), "name_hash is missing or not binary"),
            (
                Value::Map(vec![(Value::from("name_hash"), Value::Binary(vec![1; 9]))]),
                "name_hash is not 10 bytes",
            ),
            (
                Value::Map(vec![name_hash.clone()]),
                "kind is missing or unknown",
            ),
            (
                Value::Map(vec![
                    name_hash.clone(),
                    (Value::from("kind"), Value::from("shout")),
                ]),
                "kind is missing or unknown",
            ),
            (
                Value::Map(vec![
                    name_hash.clone(),
                    (Value::from("kind"), Value::from("message")),
                ]),
                "id is missing or blank",
            ),
            (
                Value::Map(vec![
                    name_hash.clone(),
                    (Value::from("kind"), Value::from("message")),
                    (Value::from("id"), Value::from("two words")),
                ]),
                "id is too long or has characters outside the id alphabet",
            ),
            (
                Value::Map(vec![
                    name_hash.clone(),
                    (Value::from("kind"), Value::from("message")),
                    (
                        Value::from("id"),
                        Value::from("i".repeat(PEER_ID_MAX_CHARS + 1)),
                    ),
                ]),
                "id is too long or has characters outside the id alphabet",
            ),
            (
                Value::Map(vec![
                    name_hash.clone(),
                    (Value::from("kind"), Value::from("reply")),
                    (Value::from("id"), Value::from("r-1")),
                    (Value::from("in_reply_to"), Value::from("idé")),
                ]),
                "in_reply_to is not a message id",
            ),
            (
                Value::Map(vec![
                    name_hash.clone(),
                    (Value::from("kind"), Value::from("reply")),
                    (Value::from("id"), Value::from("r-1")),
                    (Value::from("in_reply_to"), Value::from(7)),
                ]),
                "in_reply_to is not a message id",
            ),
        ] {
            assert_eq!(
                decode_peer_lxmf(&inbound(Some(with_data(data)), None, None, &hash_of("s"))),
                PeerLxmf::Malformed(why)
            );
        }
        assert_eq!(
            decode_peer_lxmf(&inbound(
                Some(typed(Value::from(PEER_MESSAGE_TYPE))),
                None,
                None,
                &hash_of("s")
            )),
            PeerLxmf::Malformed("custom data is missing or not a map")
        );

        let minimal = with_data(Value::Map(vec![
            name_hash,
            (Value::from("kind"), Value::Binary(b"bulletin".to_vec())),
            (Value::from("id"), Value::from("b-1")),
        ]));
        assert_eq!(
            decode_peer_lxmf(&inbound(
                Some(minimal),
                None,
                Some(vec![0xff, b'h', b'i']),
                &hash_of("s")
            )),
            PeerLxmf::Peer(Box::new(LxmfPeer {
                name_hash: [3; NAME_HASH_LEN],
                kind: PeerKind::Bulletin,
                id: "b-1".into(),
                in_reply_to: None,
                title: None,
                content: "\u{FFFD}hi".into(),
                fields: None,
                thread: None,
                disposition: None,
                retry_after: None,
                parts: Vec::new(),
                dropped_parts: 0,
            })),
            "invalid UTF-8 is read lossily, never refused"
        );
    }

    #[test]
    fn received_reply_is_recognised_only_for_its_id() {
        let reply = round_trip(&received_reply("abc"));
        assert!(is_received_reply(&reply, "abc"));
        assert!(!is_received_reply(&reply, "abd"));
        assert!(!is_received_reply(&Value::Nil, "abc"));
        assert!(!is_received_reply(&Value::from("abc"), "abc"));
        assert!(!is_received_reply(
            &Value::Map(vec![
                (Value::from("received"), Value::from(false)),
                (Value::from("id"), Value::from("abc"))
            ]),
            "abc"
        ));
        assert!(!is_received_reply(
            &Value::Map(vec![(Value::from("id"), Value::from("abc"))]),
            "abc"
        ));
        assert!(!is_received_reply(
            &crate::mesh::r3::DispatchError::NoProvider {
                path: MESSAGE_PATH.to_string()
            }
            .to_value(),
            "abc"
        ));
    }

    #[test]
    fn peer_inbox_evicts_the_oldest_peer_at_capacity_and_counts_it() {
        let inbox = PeerInbox::default();
        inbox.inbox.deliver(Envelope {
            from: "local".into(),
            to: "me".into(),
            payload: EnvelopePayload::Text {
                content: "keep me".into(),
            },
            timestamp: chrono::Utc::now(),
        });
        for n in 0..PEER_INBOX_CAPACITY + 2 {
            inbox.deliver(PeerMessage::new(RawPeerMessage {
                message_id: format!("m{n}"),
                ..raw("hello")
            }));
        }

        assert_eq!(inbox.len(), PEER_INBOX_CAPACITY + 1);
        let (envelopes, dropped) = inbox.drain();
        assert_eq!(
            dropped, 2,
            "the local envelope takes no peer slot, so two peers went"
        );
        assert_eq!(envelopes.len(), PEER_INBOX_CAPACITY + 1);
        let ids: Vec<&str> = envelopes
            .iter()
            .filter_map(|e| match &e.payload {
                EnvelopePayload::Peer(message) => Some(message.message_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids[0], "m2", "the oldest peer envelopes went first");
        assert_eq!(ids.len(), PEER_INBOX_CAPACITY);
        assert!(
            matches!(
                envelopes.last().unwrap().payload,
                EnvelopePayload::Text { .. }
            ),
            "peers sort before text"
        );
        assert_eq!(envelopes[0].from, hash_of("instance"));
        assert_eq!(envelopes[0].to, hash_of("me"));
        let (rest, dropped) = inbox.drain();
        assert!(rest.is_empty());
        assert_eq!(dropped, 0, "the count resets on drain");
        assert_eq!(inbox.len(), 0);
    }

    #[test]
    fn model_notes_cap_evicts_the_oldest_and_take_appends_one_summary() {
        let notes = ModelNotes::default();
        for n in 0..MESH_NOTIFICATION_QUEUE_CAPACITY + 2 {
            notes.push(crate::supervisor::notification::mesh_notification(
                "peer_message",
                &format!("m{n}"),
                "mesh",
                true,
                "mesh__check_inbox".into(),
            ));
        }

        let taken = notes.take();
        assert_eq!(taken.len(), MESH_NOTIFICATION_QUEUE_CAPACITY + 1);
        assert_eq!(taken[0].id, "m2");
        let summary = taken.last().unwrap();
        assert_eq!(summary.event, MESH_EVENTS_DROPPED_EVENT);
        assert_eq!(summary.tool_or_agent, "mesh-slot");
        assert_eq!(
            summary.next_action,
            "2 older mesh events were dropped before this tool batch"
        );
        assert!(notes.take().is_empty(), "the count resets on take");
    }

    #[test]
    fn send_error_texts_name_the_remedies() {
        let text = SendError::NotTrusted {
            destination: hash_of("d"),
        }
        .to_string();
        assert!(
            text.contains(&format!(".mesh trust {}", hash_of("d"))),
            "{text}"
        );
        assert!(text.contains(".mesh peers"), "{text}");
        assert!(
            SendError::NoPropagationNode
                .to_string()
                .contains(".mesh peers")
        );
        assert!(SendError::NotRunning.to_string().contains(".mesh on"));
        assert!(
            SendError::Refused(RefusalCode::NoAccess)
                .to_string()
                .contains(".mesh knock <destination>")
        );
        assert!(
            SendError::Refused(RefusalCode::Throttled)
                .to_string()
                .contains("throttled")
        );
        assert!(
            SendError::NotAcknowledged
                .to_string()
                .contains(MESSAGE_PATH)
        );
        assert!(
            SendError::InvalidFields("too deep")
                .to_string()
                .contains("too deep")
        );
    }

    #[test]
    fn send_error_classes_are_stable_snake_case_tokens() {
        let destination = hash_of("d");
        let table = [
            (
                SendError::NotTrusted {
                    destination: destination.clone(),
                },
                "not_trusted",
            ),
            (
                SendError::UnknownDestination {
                    destination: destination.clone(),
                },
                "unknown_destination",
            ),
            (SendError::NotRunning, "not_running"),
            (
                SendError::ContentTooLong { chars: 2, max: 1 },
                "content_too_long",
            ),
            (
                SendError::TitleTooLong { chars: 2, max: 1 },
                "title_too_long",
            ),
            (SendError::InvalidFields("too deep"), "invalid_fields"),
            (SendError::Refused(RefusalCode::NoAccess), "refused"),
            (SendError::Direct(R3Error::LinkClosed), "direct"),
            (
                SendError::IncompatibleVersion {
                    destination,
                    found: Some(9),
                    min: 1,
                    max: 2,
                },
                "incompatible_version",
            ),
            (SendError::NoPropagationNode, "no_propagation_node"),
            (
                SendError::Propagation(PropagationError::Cancelled),
                "propagation",
            ),
            (SendError::NotAcknowledged, "not_acknowledged"),
        ];

        for (error, class) in table {
            assert_eq!(error.class(), class, "{error:?}");
            assert!(
                class.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{class}"
            );
        }
    }

    #[test]
    fn bulletin_tally_counts_each_outcome_and_the_sent_event_carries_the_counts() {
        let report = |seed: &str, outcome: RecipientOutcome| RecipientReport {
            destination: hash_of(seed),
            display_name: None,
            outcome,
        };
        let outcome = BroadcastOutcome {
            id: "b-1".to_string(),
            recipients: vec![
                report("one", RecipientOutcome::Delivered),
                report("two", RecipientOutcome::StoreAndForward),
                report("three", RecipientOutcome::StoreAndForward),
                report(
                    "four",
                    RecipientOutcome::Unreachable {
                        reason: "timed out".to_string(),
                    },
                ),
                report(
                    "five",
                    RecipientOutcome::Refused {
                        reason: "no access".to_string(),
                    },
                ),
            ],
        };

        let tally = outcome.tally();

        assert_eq!(
            tally,
            BulletinTally {
                recipients: 5,
                delivered: 1,
                stored: 2,
                unreachable: 1,
                refused: 1,
            }
        );
        let envs = MeshEvent::BulletinSent {
            id: outcome.id.clone(),
            tally,
        }
        .envs();
        assert_eq!(env_value(&envs, "COYOTE_MESH_MESSAGE_ID"), Some("b-1"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_RECIPIENTS"), Some("5"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_DELIVERED"), Some("1"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_STORED"), Some("2"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_UNREACHABLE"), Some("1"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_REFUSED"), Some("1"));
    }

    #[test]
    fn incompatible_version_text_names_neither_side_as_the_refuser() {
        let incompatible = |found: Option<u16>| {
            SendError::IncompatibleVersion {
                destination: hash_of("d"),
                found,
                min: 1,
                max: 1,
            }
            .to_string()
        };
        assert_eq!(
            incompatible(Some(2)),
            format!(
                "Destination {} and this Coyote speak incompatible mesh protocol versions: version 2 was refused by the side that supports 1..=1. One of the two needs upgrading before they can talk; the peer listing names which.",
                hash_of("d")
            )
        );
        assert_eq!(
            incompatible(None),
            format!(
                "Destination {} and this Coyote speak incompatible mesh protocol versions: version none was refused by the side that supports 1..=1. One of the two needs upgrading before they can talk; the peer listing names which.",
                hash_of("d")
            )
        );
    }

    #[test]
    fn recipient_reports_serialise_with_a_flat_outcome() {
        let report = RecipientReport {
            destination: hash_of("d"),
            display_name: Some("Bea".into()),
            outcome: RecipientOutcome::Unreachable {
                reason: "no path".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({ "destination": hash_of("d"), "display_name": "Bea", "outcome": "unreachable", "reason": "no path" })
        );
        let delivered = RecipientReport {
            outcome: RecipientOutcome::Delivered,
            ..report
        };
        assert_eq!(
            serde_json::to_value(&delivered).unwrap()["outcome"],
            "delivered"
        );
        let message = PeerMessage::new(raw("hi"));
        let json = serde_json::to_value(&Envelope {
            from: "a".into(),
            to: "b".into(),
            payload: EnvelopePayload::Peer(Box::new(message.clone())),
            timestamp: chrono::Utc::now(),
        })
        .unwrap();
        assert_eq!(json["payload"]["type"], "peer");
        assert_eq!(json["payload"]["kind"], "message");
        assert_eq!(json["payload"]["via"], "direct");
        let back: Envelope = serde_json::from_value(json).unwrap();
        assert!(matches!(back.payload, EnvelopePayload::Peer(m) if *m == message));
    }

    /// Records what the routing asks and hands over, standing in for the slot; admits
    /// every sender until `admit_up_to` messages have been offered, whoever sent them,
    /// and stages inline files under `staging_root` when one is set.
    struct RecordingSurface {
        delivered: Mutex<Vec<PeerMessage>>,
        filed: Mutex<Vec<PeerMessage>>,
        offered: Mutex<Vec<(String, String, String, PeerVia)>>,
        admit_up_to: usize,
        staging_root: Option<PathBuf>,
    }

    impl Default for RecordingSurface {
        fn default() -> Self {
            Self::admitting(usize::MAX)
        }
    }

    impl RecordingSurface {
        fn admitting(admit_up_to: usize) -> Self {
            Self {
                delivered: Mutex::new(Vec::new()),
                filed: Mutex::new(Vec::new()),
                offered: Mutex::new(Vec::new()),
                admit_up_to,
                staging_root: None,
            }
        }

        fn with_staging(mut self, root: PathBuf) -> Self {
            self.staging_root = Some(root);
            self
        }

        fn offered(&self) -> usize {
            self.offered.lock().len()
        }
    }

    impl PeerSurface for RecordingSurface {
        fn admit_peer_message(&self, request: &PeerAdmission) -> Result<(), PeerRefusal> {
            let mut offered = self.offered.lock();
            offered.push((
                request.source_identity.to_string(),
                request.source_destination.to_string(),
                request.message_id.to_string(),
                request.via,
            ));
            if offered.len() > self.admit_up_to {
                return Err(PeerRefusal {
                    reason: RefusalReason::RateLimited,
                    retry_after: Duration::from_secs(1),
                });
            }
            Ok(())
        }

        fn deliver_peer(&self, message: PeerMessage) {
            self.delivered.lock().push(message);
        }

        fn file_peer(&self, message: PeerMessage) {
            self.filed.lock().push(message);
        }

        fn local_destination(&self) -> Option<String> {
            Some(hash_of("local"))
        }

        fn inbox_staging(&self) -> Option<InboxStaging> {
            self.staging_root.clone().map(InboxStaging::new)
        }
    }

    #[derive(Default)]
    struct CountingSink {
        messages: Mutex<Vec<InboundMessage>>,
    }

    impl InboundSink for CountingSink {
        fn deliver(&self, message: InboundMessage) {
            self.messages.lock().push(message);
        }
    }

    #[test]
    fn peer_routing_recomputes_the_source_destination_and_drops_untrusted() {
        crate::testing::install_log_collector();
        let identity = hash_of("id-peer-route");
        let origin = OriginName([6u8; NAME_HASH_LEN]);
        let destination = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&identity).unwrap(),
        )
        .to_hex_string();
        let other_origin = OriginName([9u8; NAME_HASH_LEN]);
        let (trust, _tmp) = TrustList::default()
            .destination(&destination, &identity)
            .open("peer-routing");
        let surface = Arc::new(RecordingSurface::default());
        let inner = CountingSink::default();
        let routing = PeerRouting {
            trust: &trust,
            surface: Some(surface.clone() as Arc<dyn PeerSurface>),
            inner: &inner,
        };
        let message =
            OutboundPeer::new(PeerKind::Message, "stored\u{1b}[2J words", None, None, None)
                .unwrap();

        let stored = peer_lxmf_message(&message, &origin);
        routing.deliver(inbound(
            stored.fields.clone(),
            None,
            Some(stored.content.clone()),
            &identity,
        ));
        let from_untrusted_instance = peer_lxmf_message(&message, &other_origin);
        routing.deliver(inbound(
            from_untrusted_instance.fields.clone(),
            None,
            Some(from_untrusted_instance.content.clone()),
            &identity,
        ));
        routing.deliver(inbound(
            stored.fields.clone(),
            None,
            Some(stored.content.clone()),
            &hash_of("stranger"),
        ));
        routing.deliver(inbound(
            Some(Value::Map(vec![(
                Value::from(FIELD_CUSTOM_TYPE),
                Value::from(PEER_MESSAGE_TYPE),
            )])),
            None,
            None,
            &identity,
        ));
        routing.deliver(inbound(
            Some(Value::Map(vec![])),
            None,
            Some(b"plain lxmf".to_vec()),
            &identity,
        ));

        let delivered = surface.delivered.lock();
        assert_eq!(
            delivered.len(),
            1,
            "only the trusted instance's message lands"
        );
        assert_eq!(delivered[0].source_identity, identity);
        assert_eq!(delivered[0].source_destination, destination);
        assert_eq!(delivered[0].destination, hash_of("local"));
        assert_eq!(delivered[0].via, PeerVia::StoreAndForward);
        assert_eq!(delivered[0].message_id, message.id);
        assert_eq!(delivered[0].content, "stored words");
        drop(delivered);
        assert_eq!(
            inner.messages.lock().len(),
            1,
            "only the plain message reaches the inner sink"
        );
        let logs = crate::testing::debug_snapshot();
        assert!(
            logs.iter().any(|line| line
                == &format!(
                    "Propagated message from {} dropped: instance {} is not trusted",
                    &identity[..8],
                    &destination_address(
                        &other_origin.0,
                        &AddressHash::new_from_hex_string(&identity).unwrap()
                    )
                    .to_hex_string()[..8]
                )),
            "{logs:#?}"
        );
        assert!(logs.iter().any(|line| line
            == &format!(
                "Propagated peer message from {} dropped: custom data is missing or not a map",
                &identity[..8]
            )));
        assert!(
            !logs
                .iter()
                .any(|line| line.contains(&identity) || line.contains(&destination)),
            "the routing logs short hashes only"
        );

        let gone = PeerRouting {
            trust: &trust,
            surface: None,
            inner: &inner,
        };
        gone.deliver(inbound(
            stored.fields.clone(),
            None,
            Some(stored.content),
            &identity,
        ));
        assert_eq!(surface.delivered.lock().len(), 1);
        assert_eq!(
            inner.messages.lock().len(),
            1,
            "a peer message never falls through to the inner sink"
        );
    }

    /// A trust list holding `identity` for all destinations and binding `origin` to
    /// another identity, with a key-change recorder attached; the store's protection flag
    /// is left at its default.
    fn colliding_routing(
        tag: &str,
        identity: &str,
        origin: &OriginName,
    ) -> (Arc<TrustStore>, Arc<KeyChangeSurface>, String, TempDir) {
        let bound_to = hash_of("id-bound-elsewhere");
        let bound = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&bound_to).unwrap(),
        )
        .to_hex_string();
        let (trust, tmp) = TrustList::default()
            .identity(identity, true)
            .destination(&bound, &bound_to)
            .open(tag);
        let surface = Arc::new(KeyChangeSurface::default());
        trust.attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        (trust, surface, bound, tmp)
    }

    fn mark_on(trust: &TrustStore, bound: &str) -> Option<String> {
        trust
            .records()
            .into_iter()
            .find(|record| record.hash == bound)
            .and_then(|record| record.key_changed)
            .map(|mark| mark.seen_identity)
    }

    /// A stored message from an identity trusted for all destinations, naming an instance
    /// bound to another identity, is judged as its link request would be: with collision
    /// protection off it is delivered, the bound record is marked with the sender as the
    /// identity seen, and the human gets one warning.
    #[test]
    fn a_stored_message_from_a_trusted_for_all_identity_over_a_colliding_record_is_delivered_with_a_warning()
     {
        let identity = hash_of("id-rotated-stored");
        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let (trust, notes, bound, _tmp) =
            colliding_routing("peer-routing-collision-served", &identity, &origin);
        let surface = Arc::new(RecordingSurface::default());
        let inner = CountingSink::default();
        let routing = PeerRouting {
            trust: &trust,
            surface: Some(surface.clone() as Arc<dyn PeerSurface>),
            inner: &inner,
        };
        let stored = peer_lxmf_message(&outbound(PeerKind::Message, "stored"), &origin);

        routing.deliver(inbound(
            stored.fields.clone(),
            None,
            Some(stored.content.clone()),
            &identity,
        ));

        let delivered = surface.delivered.lock();
        assert_eq!(delivered.len(), 1, "{delivered:#?}");
        assert_eq!(delivered[0].source_identity, identity);
        assert_eq!(delivered[0].via, PeerVia::StoreAndForward);
        drop(delivered);
        assert_eq!(mark_on(&trust, &bound), Some(identity.clone()));
        let texts = notes.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("warning: "), "{}", texts[0]);
        assert!(texts[0].contains("is served"), "{}", texts[0]);
    }

    /// The same message under collision protection is dropped as any untrusted instance's
    /// is, the record is marked all the same, and the human gets the error.
    #[test]
    fn collision_protection_refuses_a_stored_message_over_a_colliding_record_with_an_error() {
        crate::testing::install_log_collector();
        let identity = hash_of("id-rotated-refused");
        let origin = OriginName([8u8; NAME_HASH_LEN]);
        let (trust, notes, bound, _tmp) =
            colliding_routing("peer-routing-collision-refused", &identity, &origin);
        trust.set_collision_protection(true);
        let surface = Arc::new(RecordingSurface::default());
        let inner = CountingSink::default();
        let routing = PeerRouting {
            trust: &trust,
            surface: Some(surface.clone() as Arc<dyn PeerSurface>),
            inner: &inner,
        };
        let stored = peer_lxmf_message(&outbound(PeerKind::Message, "stored"), &origin);

        routing.deliver(inbound(
            stored.fields.clone(),
            None,
            Some(stored.content.clone()),
            &identity,
        ));

        assert!(surface.delivered.lock().is_empty());
        assert_eq!(surface.offered(), 0, "admission is never asked");
        assert!(inner.messages.lock().is_empty());
        let dest8 = &destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&identity).unwrap(),
        )
        .to_hex_string()[..8];
        let expected = format!(
            "Propagated message from {} dropped: instance {dest8} is not trusted",
            &identity[..8]
        );
        let logs = crate::testing::debug_snapshot();
        assert!(logs.contains(&expected), "{logs:#?}");
        assert_eq!(mark_on(&trust, &bound), Some(identity.clone()));
        let texts = notes.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("error: "), "{}", texts[0]);
        assert!(texts[0].contains("is refused"), "{}", texts[0]);
    }

    /// Usage probe: the store-and-forward message path judges a presence-only rotation as
    /// the link does. The sender is trusted for all destinations and names an instance no
    /// record carries but the peer table holds under another identity trusted for all
    /// destinations. Under `mesh.collision_protection` the message is dropped as any
    /// untrusted instance's is, admission is never asked, the human gets one `error:` line
    /// naming both identities, nothing is marked (there is no record to mark) and
    /// `trust.yaml` keeps its bytes; with protection off the same message is delivered and
    /// the path adds no line of its own (the warning for that mode is the announce path's).
    #[test]
    fn usage_probe_a_stored_message_over_an_instance_heard_under_another_trusted_for_all_identity_is_refused_under_protection()
     {
        crate::testing::install_log_collector();
        let sender = hash_of("id-sender-now");
        let earlier = hash_of("id-heard-earlier");
        let origin = OriginName([0x5b_u8; NAME_HASH_LEN]);
        let (trust, tmp) = TrustList::default()
            .identity(&sender, true)
            .identity(&earlier, true)
            .open("peer-routing-presence-collision-protected");
        let notes = Arc::new(KeyChangeSurface::default());
        trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
        let peers = Arc::new(
            crate::mesh::peers::PeerTable::load(tmp.path.join("peers.json"), SystemTime::now())
                .unwrap(),
        );
        trust.attach_presence(
            Arc::downgrade(&peers) as Weak<dyn crate::mesh::trust::InstancePresence>
        );
        let earlier_destination = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&earlier).unwrap(),
        )
        .to_hex_string();
        peers.observe(
            crate::mesh::peers::PeerSighting {
                destination_hash: earlier_destination,
                identity_hash: earlier.clone(),
                name_hash: hex_lower(&origin.0),
                display_name: None,
                protocol_version: 1,
                hops: 1,
            },
            SystemTime::now(),
        );
        let sender_destination = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&sender).unwrap(),
        )
        .to_hex_string();
        let before = std::fs::read(trust.path()).unwrap();
        trust.set_collision_protection(true);
        let (surface, inner) = recorders();

        deliver_stored(&trust, &surface, &inner, &sender, &origin, "first");

        assert!(surface.delivered.lock().is_empty());
        assert_eq!(surface.offered(), 0, "admission is never asked");
        assert!(inner.messages.lock().is_empty());
        let expected = format!(
            "Propagated message from {} dropped: instance {} is not trusted",
            &sender[..8],
            &sender_destination[..8]
        );
        let logs = crate::testing::debug_snapshot();
        assert!(logs.contains(&expected), "{logs:#?}");
        assert!(
            trust
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            trust.records()
        );
        assert_eq!(std::fs::read(trust.path()).unwrap(), before);
        let texts = notes.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        let text = &texts[0];
        assert!(text.starts_with("error: "), "{text}");
        assert!(text.contains(&earlier), "{text}");
        assert!(text.contains(&sender), "{text}");
        assert!(text.contains("is refused when it asks"), "{text}");
        assert!(text.contains("nothing is marked"), "{text}");
        assert!(
            text.contains(&format!(".mesh trust {sender_destination}")),
            "{text}"
        );

        deliver_stored(&trust, &surface, &inner, &sender, &origin, "second");
        assert!(
            surface.delivered.lock().is_empty(),
            "the refusal holds per message"
        );
        assert_eq!(notes.texts().len(), 1, "the line is earned once");

        trust.set_collision_protection(false);
        deliver_stored(&trust, &surface, &inner, &sender, &origin, "third");
        let delivered = surface.delivered.lock();
        assert_eq!(delivered.len(), 1, "{delivered:#?}");
        assert_eq!(delivered[0].content, "third");
        assert_eq!(delivered[0].source_identity, sender);
        drop(delivered);
        assert_eq!(
            notes.texts().len(),
            1,
            "protection off: the message path adds no line of its own: {:#?}",
            notes.texts()
        );
        assert!(
            trust
                .records()
                .iter()
                .all(|record| record.key_changed.is_none())
        );
        assert_eq!(std::fs::read(trust.path()).unwrap(), before);
    }

    /// Recorders for what a routing hands the surface and the inner sink.
    fn recorders() -> (Arc<RecordingSurface>, Arc<CountingSink>) {
        (
            Arc::new(RecordingSurface::default()),
            Arc::new(CountingSink::default()),
        )
    }

    fn deliver_stored(
        trust: &TrustStore,
        surface: &Arc<RecordingSurface>,
        inner: &Arc<CountingSink>,
        identity: &str,
        origin: &OriginName,
        content: &str,
    ) {
        let routing = PeerRouting {
            trust,
            surface: Some(surface.clone() as Arc<dyn PeerSurface>),
            inner: inner.as_ref(),
        };
        let stored = peer_lxmf_message(&outbound(PeerKind::Message, content), origin);
        routing.deliver(inbound(
            stored.fields.clone(),
            None,
            Some(stored.content.clone()),
            identity,
        ));
    }

    /// The mark is first-wins: a second stored message from the same rotated identity over
    /// the already-marked record is delivered like the first and the human is not told
    /// again.
    #[test]
    fn usage_probe_a_second_stored_message_over_a_marked_record_is_delivered_and_warns_no_more() {
        let identity = hash_of("id-rotated-twice");
        let origin = OriginName([9u8; NAME_HASH_LEN]);
        let (trust, notes, bound, _tmp) =
            colliding_routing("peer-routing-collision-twice", &identity, &origin);
        let (surface, inner) = recorders();

        deliver_stored(&trust, &surface, &inner, &identity, &origin, "first");
        deliver_stored(&trust, &surface, &inner, &identity, &origin, "second");

        let delivered = surface.delivered.lock();
        assert_eq!(delivered.len(), 2, "{delivered:#?}");
        assert_eq!(delivered[0].content, "first");
        assert_eq!(delivered[1].content, "second");
        drop(delivered);
        assert!(inner.messages.lock().is_empty());
        assert_eq!(mark_on(&trust, &bound), Some(identity.clone()));
        let texts = notes.texts();
        assert_eq!(texts.len(), 1, "one line per record: {texts:#?}");
        assert!(texts[0].starts_with("warning: "), "{}", texts[0]);
    }

    /// An explicit destination allow admits in either mode: the sender's own recomputed
    /// destination is on the list, so the old record it supersedes is no collision — the
    /// message is delivered under protection too, nothing is marked and nobody is told.
    #[test]
    fn usage_probe_an_explicit_destination_allow_admits_a_stored_message_under_protection_without_a_mark()
     {
        for protection in [false, true] {
            let identity = hash_of("id-rotated-allowed");
            let origin = OriginName([10u8; NAME_HASH_LEN]);
            let bound_to = hash_of("id-bound-elsewhere");
            let old = destination_address(
                &origin.0,
                &AddressHash::new_from_hex_string(&bound_to).unwrap(),
            )
            .to_hex_string();
            let new = destination_address(
                &origin.0,
                &AddressHash::new_from_hex_string(&identity).unwrap(),
            )
            .to_hex_string();
            let (trust, _tmp) = TrustList::default()
                .destination(&new, &identity)
                .destination(&old, &bound_to)
                .open("peer-routing-collision-allowed");
            trust.set_collision_protection(protection);
            let notes = Arc::new(KeyChangeSurface::default());
            trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
            let (surface, inner) = recorders();

            deliver_stored(&trust, &surface, &inner, &identity, &origin, "allowed");

            let delivered = surface.delivered.lock();
            assert_eq!(
                delivered.len(),
                1,
                "protection {protection}: {delivered:#?}"
            );
            assert_eq!(delivered[0].source_destination, new);
            drop(delivered);
            assert_eq!(mark_on(&trust, &old), None, "protection {protection}");
            assert_eq!(mark_on(&trust, &new), None, "protection {protection}");
            assert!(
                notes.texts().is_empty(),
                "protection {protection}: {:#?}",
                notes.texts()
            );
        }
    }

    /// A blocked identity is silence before any record is touched: its stored message over
    /// a colliding record is dropped, the record stays clean and the human hears nothing.
    #[test]
    fn usage_probe_a_blocked_identitys_stored_message_over_a_colliding_record_marks_nothing() {
        let identity = hash_of("id-rotated-blocked");
        let origin = OriginName([11u8; NAME_HASH_LEN]);
        let bound_to = hash_of("id-bound-elsewhere");
        let bound = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&bound_to).unwrap(),
        )
        .to_hex_string();
        let (trust, tmp) = TrustList::default()
            .block(&identity)
            .destination(&bound, &bound_to)
            .open("peer-routing-collision-blocked");
        let notes = Arc::new(KeyChangeSurface::default());
        trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
        let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");
        let before = std::fs::read(&trust_path).unwrap();
        let (surface, inner) = recorders();

        deliver_stored(&trust, &surface, &inner, &identity, &origin, "blocked");

        assert!(surface.delivered.lock().is_empty());
        assert_eq!(surface.offered(), 0);
        assert!(inner.messages.lock().is_empty());
        assert_eq!(mark_on(&trust, &bound), None);
        assert!(notes.texts().is_empty(), "{:#?}", notes.texts());
        assert_eq!(
            std::fs::read(&trust_path).unwrap(),
            before,
            "trust.yaml untouched"
        );
    }

    /// A stranger (no record at all) is silenced before the destination tier on the
    /// store-and-forward path: its stored message over a colliding record is dropped and
    /// the path neither marks nor tells — the announce is where a stranger's collision is
    /// caught.
    #[test]
    fn usage_probe_a_strangers_stored_message_over_a_colliding_record_is_silent() {
        let identity = hash_of("id-rotated-stranger");
        let origin = OriginName([12u8; NAME_HASH_LEN]);
        let bound_to = hash_of("id-bound-elsewhere");
        let bound = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&bound_to).unwrap(),
        )
        .to_hex_string();
        let (trust, _tmp) = TrustList::default()
            .destination(&bound, &bound_to)
            .open("peer-routing-collision-stranger");
        let notes = Arc::new(KeyChangeSurface::default());
        trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
        let (surface, inner) = recorders();

        deliver_stored(&trust, &surface, &inner, &identity, &origin, "stranger");

        assert!(surface.delivered.lock().is_empty());
        assert_eq!(surface.offered(), 0);
        assert!(inner.messages.lock().is_empty());
        assert_eq!(
            mark_on(&trust, &bound),
            None,
            "a stranger's stored message marked the record; surfaced: {:#?}",
            notes.texts()
        );
        assert!(notes.texts().is_empty(), "{:#?}", notes.texts());
    }

    /// No collision, no write: a trusted sender whose instance is on no record leaves
    /// `trust.yaml` byte for byte and surfaces nothing.
    #[test]
    fn usage_probe_a_non_colliding_stored_message_writes_no_trust_file_and_tells_nobody() {
        let identity = hash_of("id-plain-sender");
        let origin = OriginName([13u8; NAME_HASH_LEN]);
        let (trust, tmp) = TrustList::default()
            .identity(&identity, true)
            .open("peer-routing-no-collision");
        let notes = Arc::new(KeyChangeSurface::default());
        trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
        let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");
        let before = std::fs::read(&trust_path).unwrap();
        let (surface, inner) = recorders();

        deliver_stored(&trust, &surface, &inner, &identity, &origin, "plain");

        assert_eq!(surface.delivered.lock().len(), 1);
        assert!(notes.texts().is_empty(), "{:#?}", notes.texts());
        assert_eq!(
            std::fs::read(&trust_path).unwrap(),
            before,
            "trust.yaml untouched"
        );
    }

    /// Deny wins first, and a collision is marked in every case: a sender whose own
    /// recomputed destination is denied, naming an instance bound to another identity, has
    /// its stored message dropped, the bound record marked and one error surfaced.
    #[test]
    fn usage_probe_a_denied_senders_stored_message_over_a_colliding_record_is_dropped_marked_and_an_error()
     {
        let identity = hash_of("id-rotated-denied");
        let origin = OriginName([14u8; NAME_HASH_LEN]);
        let bound_to = hash_of("id-bound-elsewhere");
        let bound = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&bound_to).unwrap(),
        )
        .to_hex_string();
        let denied = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&identity).unwrap(),
        )
        .to_hex_string();
        let (trust, _tmp) = TrustList::default()
            .identity(&identity, true)
            .deny(&denied)
            .destination(&bound, &bound_to)
            .open("peer-routing-collision-denied");
        let notes = Arc::new(KeyChangeSurface::default());
        trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
        let (surface, inner) = recorders();

        deliver_stored(&trust, &surface, &inner, &identity, &origin, "denied");

        assert!(surface.delivered.lock().is_empty());
        assert_eq!(surface.offered(), 0);
        assert!(inner.messages.lock().is_empty());
        assert_eq!(mark_on(&trust, &bound), Some(identity.clone()));
        let texts = notes.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("error: "), "{}", texts[0]);
        assert!(texts[0].contains("is refused"), "{}", texts[0]);
    }

    /// The standing gate sits before the protection flag: a stranger's stored message over
    /// a colliding record is dropped in either mode, nothing is marked, `trust.yaml` is
    /// byte for byte, the human hears nothing, and the drop is logged as an unknown
    /// identity rather than an untrusted instance.
    #[test]
    fn usage_probe_a_strangers_stored_message_is_silent_in_either_mode_and_leaves_trust_yaml_alone()
    {
        crate::testing::install_log_collector();
        for protection in [false, true] {
            let identity = hash_of("id-rotated-stranger-2");
            let origin = OriginName([15u8; NAME_HASH_LEN]);
            let bound_to = hash_of("id-bound-elsewhere");
            let bound = destination_address(
                &origin.0,
                &AddressHash::new_from_hex_string(&bound_to).unwrap(),
            )
            .to_hex_string();
            let (trust, tmp) = TrustList::default()
                .destination(&bound, &bound_to)
                .open("peer-routing-collision-stranger-modes");
            trust.set_collision_protection(protection);
            let notes = Arc::new(KeyChangeSurface::default());
            trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
            let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");
            let before = std::fs::read(&trust_path).unwrap();
            let (surface, inner) = recorders();

            deliver_stored(&trust, &surface, &inner, &identity, &origin, "stranger");

            assert!(
                surface.delivered.lock().is_empty(),
                "protection {protection}"
            );
            assert_eq!(surface.offered(), 0, "protection {protection}");
            assert!(inner.messages.lock().is_empty(), "protection {protection}");
            assert_eq!(mark_on(&trust, &bound), None, "protection {protection}");
            assert!(
                notes.texts().is_empty(),
                "protection {protection}: {:#?}",
                notes.texts()
            );
            assert_eq!(
                std::fs::read(&trust_path).unwrap(),
                before,
                "protection {protection}: trust.yaml untouched"
            );
            let expected = format!(
                "Propagated message from {} dropped: unknown identity",
                &identity[..8]
            );
            let logs = crate::testing::debug_snapshot();
            assert!(
                logs.contains(&expected),
                "protection {protection}: {logs:#?}"
            );
        }
    }

    /// The store-and-forward twin of the link path's identity-changed refusal: an identity
    /// trusted for its own instance only, whose stored message names an instance bound to
    /// another identity, passes the standing gate, is refused by the verdict, marks the
    /// bound record with itself as the identity seen and earns the human one error — in
    /// either mode, since protection only widens what an all-destinations identity is
    /// refused. Its own grant is untouched: the next stored message under its own instance
    /// is delivered and the human is not told again.
    #[test]
    fn usage_probe_a_destination_tier_senders_stored_message_naming_a_foreign_instance_is_refused_marked_and_an_error()
     {
        for protection in [false, true] {
            let sender = hash_of("id-standing-elsewhere");
            let shared = OriginName([16u8; NAME_HASH_LEN]);
            let own = OriginName([17u8; NAME_HASH_LEN]);
            let bound_to = hash_of("id-bound-elsewhere");
            let bound = destination_address(
                &shared.0,
                &AddressHash::new_from_hex_string(&bound_to).unwrap(),
            )
            .to_hex_string();
            let own_destination =
                destination_address(&own.0, &AddressHash::new_from_hex_string(&sender).unwrap())
                    .to_hex_string();
            let (trust, _tmp) = TrustList::default()
                .destination(&bound, &bound_to)
                .destination(&own_destination, &sender)
                .open("peer-routing-collision-standing");
            trust.set_collision_protection(protection);
            let notes = Arc::new(KeyChangeSurface::default());
            trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
            let (surface, inner) = recorders();

            deliver_stored(&trust, &surface, &inner, &sender, &shared, "foreign");

            assert!(
                surface.delivered.lock().is_empty(),
                "protection {protection}"
            );
            assert_eq!(surface.offered(), 0, "protection {protection}");
            assert!(inner.messages.lock().is_empty(), "protection {protection}");
            assert_eq!(
                mark_on(&trust, &bound),
                Some(sender.clone()),
                "protection {protection}: the bound record is marked with the sender"
            );
            assert_eq!(
                mark_on(&trust, &own_destination),
                None,
                "protection {protection}: the sender's own record is not marked"
            );
            let texts = notes.texts();
            assert_eq!(texts.len(), 1, "protection {protection}: {texts:#?}");
            assert!(texts[0].starts_with("error: "), "{}", texts[0]);
            assert!(texts[0].contains("is refused"), "{}", texts[0]);

            deliver_stored(&trust, &surface, &inner, &sender, &own, "mine");

            let delivered = surface.delivered.lock();
            assert_eq!(
                delivered.len(),
                1,
                "protection {protection}: {delivered:#?}"
            );
            assert_eq!(delivered[0].content, "mine");
            assert_eq!(delivered[0].source_destination, own_destination);
            drop(delivered);
            assert_eq!(
                notes.texts().len(),
                1,
                "protection {protection}: no second line: {:#?}",
                notes.texts()
            );
        }
    }

    /// A session-only grant is standing too: the standing gate on the store-and-forward
    /// path lets a peer trusted with `.mesh trust --session` through, its stored message is
    /// delivered under its own instance, and the file the session never wrote to stays
    /// byte for byte.
    #[test]
    fn usage_probe_a_session_trusted_senders_stored_message_passes_the_standing_gate() {
        use crate::mesh::peers::{PeerSighting, PeerTable};
        use crate::mesh::trust::{KnockProof, LiveMesh};

        struct Heard(Arc<PeerTable>);
        impl LiveMesh for Heard {
            fn peers(&self) -> Option<Arc<PeerTable>> {
                Some(self.0.clone())
            }
            fn knock(&self, _destination_hash: &str, _now: SystemTime) -> Option<KnockProof> {
                None
            }
        }

        let key = PrivateIdentity::new_from_rand(OsRng);
        let identity = key.address_hash().to_hex_string();
        let origin = OriginName([18u8; NAME_HASH_LEN]);
        let destination = destination_address(&origin.0, key.address_hash()).to_hex_string();
        let (trust, tmp) = TrustList::default().open("peer-routing-session-standing");
        let notes = Arc::new(KeyChangeSurface::default());
        trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_100);
        let peers = Arc::new(PeerTable::load(tmp.path.join("peers.json"), now).unwrap());
        peers.observe(
            PeerSighting {
                destination_hash: destination.clone(),
                identity_hash: identity.clone(),
                name_hash: hex_lower(&origin.0),
                display_name: Some("Bob".to_string()),
                protocol_version: 1,
                hops: 1,
            },
            now,
        );
        let (surface, inner) = recorders();

        deliver_stored(&trust, &surface, &inner, &identity, &origin, "before");
        assert!(
            surface.delivered.lock().is_empty(),
            "a stranger until the session grant"
        );

        let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");
        let before = std::fs::read(&trust_path).unwrap();
        trust
            .trust_destination_for_session(&Heard(peers), &destination, now)
            .unwrap();
        assert_eq!(std::fs::read(&trust_path).unwrap(), before);

        deliver_stored(&trust, &surface, &inner, &identity, &origin, "after");

        let delivered = surface.delivered.lock();
        assert_eq!(delivered.len(), 1, "{delivered:#?}");
        assert_eq!(delivered[0].content, "after");
        assert_eq!(delivered[0].source_destination, destination);
        assert_eq!(delivered[0].via, PeerVia::StoreAndForward);
        drop(delivered);
        assert!(notes.texts().is_empty(), "{:#?}", notes.texts());
        assert_eq!(
            std::fs::read(&trust_path).unwrap(),
            before,
            "a session grant never reaches the file"
        );
    }

    /// Admission is asked before a message is delivered on either path, with the
    /// sender, its instance, the id and the path, so a refused sender never reaches
    /// `deliver_peer`: over the link it hears `Throttled` instead of the
    /// acknowledgement and nothing is filed; off a propagation node the message is
    /// filed with `file_peer` and the surface owes the sender the typed reply.
    #[tokio::test]
    async fn both_inbound_paths_ask_admission_before_delivering() {
        let identity = PrivateIdentity::new_from_rand(OsRng);
        let identity_hex = identity.address_hash().to_hex_string();
        let origin = OriginName([6u8; NAME_HASH_LEN]);
        let destination = destination_address(&origin.0, identity.address_hash()).to_hex_string();
        let surface = Arc::new(RecordingSurface::admitting(1));
        let handler = PeerMessageHandler::new(Arc::downgrade(&surface) as Weak<dyn PeerSurface>);
        let request = |content: &str| {
            let out = OutboundPeer::new(PeerKind::Ask, content, None, None, None).unwrap();
            (
                out.id.clone(),
                AdmittedRequest {
                    link_id: LinkId::new_from_rand(OsRng),
                    identity: *identity.as_identity(),
                    destination_hash: AddressHash::new_from_hex_string(&destination).unwrap(),
                    request_id: RequestId::from([1u8; 16]),
                    path_hash: PathHash::of(MESSAGE_PATH),
                    requested_at: 1_700_000_000.0,
                    body: to_r3_body(&out, 1_700_000_000.0),
                    branch: SizeBranch::Packet,
                },
            )
        };

        let (first_id, first) = request("first");
        match handler.handle(first).await {
            Reply::Value(value) | Reply::Settled { value, .. } => {
                assert!(is_received_reply(&value, &first_id), "{value}")
            }
            Reply::Code(code) => panic!("the first message is refused: {code:?}"),
            Reply::Silent => panic!("the first message is not acknowledged"),
        }
        let (second_id, second) = request("second");
        assert!(matches!(
            handler.handle(second).await,
            Reply::Code(RefusalCode::Throttled)
        ));
        assert_eq!(
            *surface.offered.lock(),
            [
                (
                    identity_hex.clone(),
                    destination.clone(),
                    first_id,
                    PeerVia::Direct
                ),
                (
                    identity_hex.clone(),
                    destination.clone(),
                    second_id,
                    PeerVia::Direct
                ),
            ]
        );
        assert_eq!(surface.delivered.lock().len(), 1);
        assert_eq!(surface.delivered.lock()[0].content, "first");
        assert!(surface.filed.lock().is_empty());

        let (trust, _tmp) = TrustList::default()
            .destination(&destination, &identity_hex)
            .open("peer-routing-admission");
        let inner = CountingSink::default();
        let routing = PeerRouting {
            trust: &trust,
            surface: Some(surface.clone() as Arc<dyn PeerSurface>),
            inner: &inner,
        };
        let out = outbound(PeerKind::Message, "stored");
        let stored = peer_lxmf_message(&out, &origin);
        routing.deliver(inbound(
            stored.fields.clone(),
            None,
            Some(stored.content.clone()),
            &identity_hex,
        ));
        assert_eq!(surface.offered(), 3);
        assert_eq!(
            surface.offered.lock()[2],
            (
                identity_hex.clone(),
                destination.clone(),
                out.id.clone(),
                PeerVia::StoreAndForward
            )
        );
        assert_eq!(
            surface.delivered.lock().len(),
            1,
            "a refused propagated message never reaches deliver_peer"
        );
        let filed = surface.filed.lock();
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].message_id, out.id);
        assert_eq!(filed[0].content, "stored");
        assert_eq!(filed[0].via, PeerVia::StoreAndForward);
        drop(filed);
        assert!(inner.messages.lock().is_empty());

        let fresh = Arc::new(RecordingSurface::admitting(1));
        let routing = PeerRouting {
            trust: &trust,
            surface: Some(fresh.clone() as Arc<dyn PeerSurface>),
            inner: &inner,
        };
        for _ in 0..2 {
            routing.deliver(inbound(
                stored.fields.clone(),
                None,
                Some(stored.content.clone()),
                &identity_hex,
            ));
        }
        assert_eq!(fresh.offered(), 2);
        assert_eq!(fresh.delivered.lock().len(), 1);
        assert_eq!(fresh.filed.lock().len(), 1);
    }

    /// A propagated message from a sender over its limit is filed, but its inline
    /// files never touch the staging inbox: the throttle is consulted before any peer
    /// bytes land on disk, and the dropped part is counted. The same message from an
    /// admitted sender is staged, so the fixture is shown to route staging at all.
    #[test]
    fn a_throttled_propagated_message_is_filed_without_staging_its_inline_files() {
        let tmp = TempDir::new("message-throttled-staging");
        let identity = PrivateIdentity::new_from_rand(OsRng);
        let identity_hex = identity.address_hash().to_hex_string();
        let origin = OriginName([6u8; NAME_HASH_LEN]);
        let destination = destination_address(&origin.0, identity.address_hash()).to_hex_string();
        let (trust, _trust_tmp) = TrustList::default()
            .destination(&destination, &identity_hex)
            .open("peer-routing-throttled-staging");
        let inner = CountingSink::default();
        let out = OutboundPeer::with_parts(
            PeerKind::Message,
            "stored",
            None,
            None,
            None,
            vec![inline_file("docs/notes.md", b"# notes\n".to_vec())],
            &PartLimits::default(),
        )
        .unwrap();
        let stored = peer_lxmf_message(&out, &origin);
        let deliver = |surface: &Arc<RecordingSurface>| {
            PeerRouting {
                trust: &trust,
                surface: Some(surface.clone() as Arc<dyn PeerSurface>),
                inner: &inner,
            }
            .deliver(inbound(
                stored.fields.clone(),
                None,
                Some(stored.content.clone()),
                &identity_hex,
            ));
        };

        let root = tmp.path.join("inbox");
        let throttled = Arc::new(RecordingSurface::admitting(0).with_staging(root.clone()));
        deliver(&throttled);
        assert!(throttled.delivered.lock().is_empty());
        let filed = throttled.filed.lock();
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].message_id, out.id);
        assert!(filed[0].parts.is_empty());
        assert_eq!(filed[0].dropped_parts, 1);
        drop(filed);
        assert!(!root.exists(), "{}", root.display());

        let admitted = Arc::new(RecordingSurface::admitting(1).with_staging(root.clone()));
        deliver(&admitted);
        let delivered = admitted.delivered.lock();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].dropped_parts, 0);
        assert!(matches!(
            delivered[0].parts.as_slice(),
            [Part::File {
                staged: Some(staged),
                ..
            }] if staged.starts_with(dunce::canonicalize(&root).unwrap())
        ));
    }

    fn pack(value: &Value) -> Vec<u8> {
        let mut packed = Vec::new();
        rmpv::encode::write_value(&mut packed, value).unwrap();
        packed
    }

    fn text_part(text: &str) -> RawPart {
        RawPart::Text {
            text: text.to_string(),
        }
    }

    fn inline_file(name: &str, bytes: Vec<u8>) -> RawPart {
        RawPart::File {
            name: name.to_string(),
            size: bytes.len() as u64,
            sha256: Sha256::digest(&bytes).into(),
            bytes: Some(bytes),
            reference: None,
        }
    }

    fn with_parts(parts: Vec<RawPart>) -> RawPeerMessage {
        RawPeerMessage {
            parts,
            ..raw("hello")
        }
    }

    fn staging(tmp: &TempDir) -> InboxStaging {
        InboxStaging::new(tmp.path.join("inbox"))
    }

    fn body_entries(kind: PeerKind, edit: impl FnOnce(&mut Vec<(Value, Value)>)) -> Value {
        let Value::Map(mut entries) = to_r3_body(&outbound(kind, "hi"), 1.0) else {
            unreachable!()
        };
        edit(&mut entries);
        Value::Map(entries)
    }

    fn set_entry(entries: &mut Vec<(Value, Value)>, key: &str, value: Value) {
        entries.retain(|(k, _)| k.as_str() != Some(key));
        entries.push((Value::from(key), value));
    }

    /// `body_entries` for the LXMF route: the fetched message with its custom data edited.
    fn lxmf_data(kind: PeerKind, edit: impl FnOnce(&mut Vec<(Value, Value)>)) -> InboundMessage {
        let stored = peer_lxmf_message(&outbound(kind, "hi"), &OriginName([7u8; NAME_HASH_LEN]));
        let Some(Value::Map(mut fields)) = stored.fields else {
            unreachable!()
        };
        let Some((_, Value::Map(data))) = fields
            .iter_mut()
            .find(|(key, _)| key.as_u64() == Some(u64::from(FIELD_CUSTOM_DATA)))
        else {
            unreachable!()
        };
        edit(data);
        inbound(
            Some(round_trip(&Value::Map(fields))),
            stored.title,
            Some(stored.content),
            &hash_of("s"),
        )
    }

    #[test]
    fn an_unknown_part_type_is_skipped_and_the_message_still_lands_with_its_content() {
        let body = body_entries(PeerKind::Message, |entries| {
            set_entry(
                entries,
                "parts",
                Value::Array(vec![
                    Value::Map(vec![
                        (Value::from("type"), Value::from("sticker")),
                        (Value::from("id"), Value::from(7)),
                    ]),
                    Value::from("not a map"),
                    Value::Map(vec![
                        (Value::from("type"), Value::from("text")),
                        (Value::from("text"), Value::from("kept")),
                    ]),
                ]),
            );
        });
        let decoded = from_r3_body(&round_trip(&body)).unwrap();
        assert_eq!(decoded.parts, vec![text_part("kept")]);
        let message = PeerMessage::new(RawPeerMessage {
            parts: decoded.parts,
            ..raw(&decoded.content)
        });
        assert_eq!(message.content, "hi");
        assert_eq!(
            message.parts,
            vec![Part::Text {
                text: "kept".into()
            }]
        );
        assert_eq!(message.dropped_parts, 0);
    }

    #[test]
    fn a_known_part_that_does_not_decode_is_dropped_and_counted_on_the_wire() {
        let too_deep = (0..PEER_FIELDS_MAX_DEPTH + 1).fold(Value::from(1), |inner, _| {
            Value::Map(vec![(Value::from("n"), inner)])
        });
        let body = body_entries(PeerKind::Message, |entries| {
            set_entry(
                entries,
                "parts",
                Value::Array(vec![
                    Value::Map(vec![
                        (Value::from("type"), Value::from("data")),
                        (Value::from("data"), too_deep.clone()),
                    ]),
                    Value::Map(vec![(Value::from("type"), Value::from("text"))]),
                    Value::Map(vec![
                        (Value::from("type"), Value::from("file")),
                        (Value::from("name"), Value::from("a.bin")),
                    ]),
                    Value::Map(vec![(Value::from("type"), Value::from("sticker"))]),
                ]),
            );
        });
        let decoded = from_r3_body(&round_trip(&body)).unwrap();
        assert_eq!(decoded.parts, vec![]);
        assert_eq!(
            decoded.dropped_parts, 3,
            "each known type that does not decode counts; the unknown one is skipped"
        );
        let message = PeerMessage::new(RawPeerMessage {
            dropped_parts: decoded.dropped_parts,
            ..raw(&decoded.content)
        });
        assert_eq!(message.content, "hi");
        assert_eq!(message.dropped_parts, 3);
    }

    #[test]
    fn a_ninth_part_is_dropped_and_counted() {
        let parts = (0..MAX_PARTS + 1)
            .map(|n| text_part(&format!("part {n}")))
            .collect();
        let message = PeerMessage::new(with_parts(parts));
        assert_eq!(message.parts.len(), MAX_PARTS);
        assert_eq!(message.dropped_parts, 1);
        assert_eq!(
            message.parts.last(),
            Some(&Part::Text {
                text: "part 7".into()
            })
        );
    }

    #[test]
    fn a_text_part_over_the_content_cap_is_dropped() {
        let message = PeerMessage::new(with_parts(vec![
            text_part(&"x".repeat(PEER_CONTENT_MAX_CHARS + 1)),
            text_part("fits"),
        ]));
        assert_eq!(
            message.parts,
            vec![Part::Text {
                text: "fits".into()
            }]
        );
        assert_eq!(message.dropped_parts, 1);
    }

    #[test]
    fn a_data_part_over_the_fields_cap_is_dropped() {
        let message = PeerMessage::new(with_parts(vec![
            RawPart::Data {
                data: serde_json::json!({ "blob": "b".repeat(PEER_FIELDS_MAX_BYTES) }),
            },
            RawPart::Data {
                data: serde_json::json!({ "n": 1 }),
            },
        ]));
        assert_eq!(
            message.parts,
            vec![Part::Data {
                data: serde_json::json!({ "n": 1 })
            }]
        );
        assert_eq!(message.dropped_parts, 1);
    }

    #[test]
    fn an_inline_file_over_inline_max_bytes_is_dropped() {
        let tmp = TempDir::new("message-inline-cap");
        let limits = PartLimits {
            inline_max_bytes: 16,
        };
        let message = PeerMessage::new_with(
            with_parts(vec![
                inline_file("big.bin", vec![1; 17]),
                inline_file("small.bin", vec![2; 16]),
            ]),
            &limits,
            Some(&staging(&tmp)),
        );
        assert_eq!(message.dropped_parts, 1);
        assert!(
            matches!(message.parts.as_slice(), [Part::File { name, .. }] if name == "small.bin"),
            "{:?}",
            message.parts
        );
    }

    #[test]
    fn inline_files_past_the_per_message_total_are_dropped_from_the_second() {
        let tmp = TempDir::new("message-inline-total");
        let half = usize::try_from(MAX_INLINE_FILE_TOTAL / 2 + 1).unwrap();
        let limits = PartLimits {
            inline_max_bytes: MAX_INLINE_FILE_TOTAL,
        };
        let message = PeerMessage::new_with(
            with_parts(vec![
                inline_file("first.bin", vec![1; half]),
                inline_file("second.bin", vec![2; half]),
            ]),
            &limits,
            Some(&staging(&tmp)),
        );
        assert_eq!(message.dropped_parts, 1);
        assert!(
            matches!(message.parts.as_slice(), [Part::File { name, .. }] if name == "first.bin"),
            "{:?}",
            message.parts
        );
    }

    #[test]
    fn a_file_part_whose_sha256_does_not_match_is_dropped_and_the_message_kept() {
        let tmp = TempDir::new("message-sha-mismatch");
        let RawPart::File {
            name, size, bytes, ..
        } = inline_file("a.bin", b"hello".to_vec())
        else {
            unreachable!()
        };
        let message = PeerMessage::new_with(
            with_parts(vec![RawPart::File {
                name,
                size,
                sha256: [0; 32],
                bytes,
                reference: None,
            }]),
            &PartLimits::default(),
            Some(&staging(&tmp)),
        );
        assert_eq!(message.content, "hello");
        assert!(message.parts.is_empty());
        assert_eq!(message.dropped_parts, 1);
    }

    #[test]
    fn a_file_part_named_with_dot_dot_is_dropped() {
        let tmp = TempDir::new("message-dot-dot");
        let message = PeerMessage::new_with(
            with_parts(vec![inline_file("../../.bashrc", b"evil".to_vec())]),
            &PartLimits::default(),
            Some(&staging(&tmp)),
        );
        assert!(message.parts.is_empty());
        assert_eq!(message.dropped_parts, 1);
        assert!(!tmp.path.join("inbox").exists());
    }

    #[test]
    fn an_inline_file_is_staged_under_the_peer_directory_and_the_part_carries_the_path() {
        let tmp = TempDir::new("message-staged");
        let bytes = b"# notes\n".to_vec();
        let raw = with_parts(vec![inline_file("docs/notes.md", bytes.clone())]);
        let peer_dir = raw.source_destination.to_lowercase();
        let message = PeerMessage::new_with(raw, &PartLimits::default(), Some(&staging(&tmp)));
        assert_eq!(message.dropped_parts, 0);
        let [
            Part::File {
                name,
                size,
                sha256,
                staged: Some(staged),
                reference: None,
            },
        ] = message.parts.as_slice()
        else {
            panic!("{:?}", message.parts);
        };
        assert_eq!(name, "docs/notes.md");
        assert_eq!(*size, bytes.len() as u64);
        assert_eq!(*sha256, hex_lower(&Sha256::digest(&bytes)));
        assert!(staged.is_absolute());
        let root = dunce::canonicalize(tmp.path.join("inbox")).unwrap();
        assert_eq!(*staged, root.join(peer_dir).join("docs").join("notes.md"));
        assert_eq!(std::fs::read(staged).unwrap(), bytes);
    }

    #[test]
    fn an_inline_file_with_no_staging_inbox_is_dropped_and_counted() {
        let message = PeerMessage::new_with(
            with_parts(vec![inline_file("a.bin", b"hello".to_vec())]),
            &PartLimits::default(),
            None,
        );
        assert!(message.parts.is_empty());
        assert_eq!(message.dropped_parts, 1);
    }

    #[test]
    fn a_reference_file_part_is_kept_with_its_ref_path() {
        let message = PeerMessage::new(with_parts(vec![RawPart::File {
            name: "report.pdf".into(),
            size: 1 << 30,
            sha256: [9; 32],
            bytes: None,
            reference: Some("shared/report.pdf".into()),
        }]));
        assert_eq!(message.dropped_parts, 0);
        assert_eq!(
            message.parts,
            vec![Part::File {
                name: "report.pdf".into(),
                size: 1 << 30,
                sha256: hex_lower(&[9; 32]),
                staged: None,
                reference: Some("shared/report.pdf".into()),
            }]
        );
    }

    #[test]
    fn parts_that_is_not_a_list_reads_as_no_parts_with_one_dropped_on_both_routes() {
        let body = body_entries(PeerKind::Message, |entries| {
            set_entry(entries, "parts", Value::Map(vec![]));
        });
        let decoded = from_r3_body(&round_trip(&body)).unwrap();
        assert_eq!(decoded.content, "hi");
        assert!(decoded.parts.is_empty());
        assert_eq!(decoded.dropped_parts, 1);

        let PeerLxmf::Peer(peer) = decode_peer_lxmf(&lxmf_data(PeerKind::Message, |data| {
            set_entry(data, "parts", Value::from("not a list"));
        })) else {
            panic!("a peer message");
        };
        assert_eq!(peer.content, "hi");
        assert!(peer.parts.is_empty());
        assert_eq!(peer.dropped_parts, 1);

        let message = PeerMessage::new(RawPeerMessage {
            dropped_parts: decoded.dropped_parts,
            ..raw(&decoded.content)
        });
        assert_eq!(message.content, "hi");
        assert_eq!(
            message.dropped_parts, 1,
            "the codec's drop reaches the count"
        );
    }

    #[test]
    fn a_thread_that_is_not_a_wire_id_reads_as_absent_so_the_message_is_its_own_thread() {
        for bad in [
            Value::from("has a space"),
            Value::from(""),
            Value::from("t".repeat(PEER_ID_MAX_CHARS + 1)),
            Value::from(7),
            Value::Map(vec![]),
        ] {
            let body = body_entries(PeerKind::Message, |entries| {
                set_entry(entries, "thread", bad.clone());
            });
            let decoded = from_r3_body(&round_trip(&body)).unwrap();
            assert_eq!(decoded.thread, None, "{bad:?}");

            let PeerLxmf::Peer(peer) = decode_peer_lxmf(&lxmf_data(PeerKind::Message, |data| {
                set_entry(data, "thread", bad.clone());
            })) else {
                panic!("{bad:?}: a peer message");
            };
            assert_eq!(peer.thread, None, "{bad:?}");

            let message = PeerMessage::new(RawPeerMessage {
                message_id: decoded.id.clone(),
                thread: decoded.thread,
                ..raw(&decoded.content)
            });
            assert_eq!(message.thread(), decoded.id, "{bad:?}");
        }
    }

    #[test]
    fn an_unknown_disposition_on_a_reply_reads_as_answered() {
        let body = body_entries(PeerKind::Reply, |entries| {
            set_entry(entries, "in_reply_to", Value::from("q-1"));
            set_entry(entries, "disposition", Value::from("shrugged"));
        });
        let decoded = from_r3_body(&body).unwrap();
        assert_eq!(decoded.disposition, Some(Disposition::Answered));
    }

    #[test]
    fn a_disposition_on_a_non_reply_is_ignored() {
        let body = body_entries(PeerKind::Ask, |entries| {
            set_entry(entries, "disposition", Value::from("refused"));
            set_entry(entries, "retry_after", Value::from(30));
        });
        let decoded = from_r3_body(&body).unwrap();
        assert_eq!(decoded.disposition, None);
        assert_eq!(decoded.retry_after, None);
    }

    #[test]
    fn a_retry_after_past_u32_reads_as_none() {
        let body = body_entries(PeerKind::Reply, |entries| {
            set_entry(entries, "in_reply_to", Value::from("q-1"));
            set_entry(entries, "disposition", Value::from("refused"));
            set_entry(entries, "retry_after", Value::from(u64::from(u32::MAX) + 1));
        });
        let decoded = from_r3_body(&body).unwrap();
        assert_eq!(decoded.disposition, Some(Disposition::Refused));
        assert_eq!(decoded.retry_after, None);
    }

    #[test]
    fn a_staged_part_serialises_its_path_and_never_bytes() {
        let tmp = TempDir::new("message-staged-json");
        let message = PeerMessage::new_with(
            with_parts(vec![inline_file("a.bin", b"secret bytes".to_vec())]),
            &PartLimits::default(),
            Some(&staging(&tmp)),
        );
        assert_eq!(message.parts.len(), 1);
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("\"staged\""), "{json}");
        assert!(!json.contains("\"bytes\""), "{json}");
        assert!(!json.contains("secret bytes"), "{json}");
    }

    #[test]
    fn a_wire_reader_that_predates_parts_sees_a_plain_v1_body() {
        let out = OutboundPeer::with_parts(
            PeerKind::Reply,
            "the answer",
            None,
            Some("q-1"),
            None,
            vec![text_part("aside")],
            &PartLimits::default(),
        )
        .unwrap()
        .with_thread(Some("t-1".into()))
        .unwrap()
        .with_disposition(Disposition::Refused, Some(60));
        let Value::Map(mut entries) = to_r3_body(&out, 1.0) else {
            unreachable!()
        };
        for key in ["parts", "thread", "disposition", "retry_after"] {
            assert!(
                entries.iter().any(|(k, _)| k.as_str() == Some(key)),
                "{key} is on the wire"
            );
            entries.retain(|(k, _)| k.as_str() != Some(key));
        }
        let decoded = from_r3_body(&round_trip(&Value::Map(entries))).unwrap();
        assert_eq!(decoded.id, out.id);
        assert_eq!(decoded.content, "the answer");
        assert_eq!(decoded.in_reply_to.as_deref(), Some("q-1"));
        assert_eq!(decoded.thread, None);
        assert_eq!(decoded.disposition, Some(Disposition::Answered));
        assert_eq!(decoded.retry_after, None);
        assert!(decoded.parts.is_empty());
    }

    /// A reply with every optional key set, for the key-order checks on both routes.
    fn every_optional_key_reply() -> OutboundPeer {
        OutboundPeer::with_parts(
            PeerKind::Reply,
            "the answer",
            Some("Re: question"),
            Some("q-1"),
            Some(serde_json::json!({ "n": 1 })),
            vec![text_part("aside")],
            &PartLimits::default(),
        )
        .unwrap()
        .with_thread(Some("t-1".into()))
        .unwrap()
        .with_disposition(Disposition::Refused, Some(60))
    }

    /// An ask with every optional key set, including the disposition and retry_after the
    /// sender's kind guard keeps off a non-reply.
    fn every_optional_key_ask() -> OutboundPeer {
        OutboundPeer::with_parts(
            PeerKind::Ask,
            "a question",
            Some("Question"),
            None,
            Some(serde_json::json!({ "n": 1 })),
            vec![text_part("aside")],
            &PartLimits::default(),
        )
        .unwrap()
        .with_thread(Some("t-1".into()))
        .unwrap()
        .with_disposition(Disposition::Refused, Some(60))
    }

    fn lxmf_custom_data_keys(message: &OutboundPeer) -> Vec<String> {
        let stored = peer_lxmf_message(message, &OriginName([7u8; NAME_HASH_LEN]));
        let Some(Value::Map(fields)) = stored.fields else {
            unreachable!()
        };
        let Some((_, Value::Map(data))) = fields
            .iter()
            .find(|(key, _)| key.as_u64() == Some(u64::from(FIELD_CUSTOM_DATA)))
        else {
            unreachable!()
        };
        data.iter()
            .map(|(k, _)| k.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_reply_with_every_optional_key_is_emitted_in_the_specified_order() {
        fn keys(message: &OutboundPeer) -> Vec<String> {
            let Value::Map(entries) = to_r3_body(message, 1.0) else {
                unreachable!()
            };
            entries
                .iter()
                .map(|(k, _)| k.as_str().unwrap().to_string())
                .collect()
        }

        assert_eq!(
            keys(&every_optional_key_reply()),
            [
                "v",
                "kind",
                "id",
                "in_reply_to",
                "thread",
                "title",
                "content",
                "fields",
                "disposition",
                "retry_after",
                "parts",
                "ts",
            ]
        );
        assert_eq!(
            keys(&every_optional_key_ask()),
            [
                "v", "kind", "id", "thread", "title", "content", "fields", "parts", "ts",
            ]
        );
    }

    #[test]
    fn a_reply_with_every_optional_key_rides_lxmf_custom_data_in_the_specified_order() {
        assert_eq!(
            lxmf_custom_data_keys(&every_optional_key_reply()),
            [
                "kind",
                "id",
                "in_reply_to",
                "thread",
                "name_hash",
                "fields",
                "disposition",
                "retry_after",
                "parts",
            ]
        );
        assert_eq!(
            lxmf_custom_data_keys(&every_optional_key_ask()),
            ["kind", "id", "thread", "name_hash", "fields", "parts"]
        );
    }

    /// Every cap at once: with content, title, both ids, fields and parts of all four
    /// shapes (text, data, inline file, reference file) at their caps together, the message
    /// still fits under both receivers' bounds and every field survives encode→decode
    /// byte-for-byte on both routes.
    #[test]
    fn a_message_at_every_cap_fits_under_both_receiver_bounds_on_both_routes() {
        const TEXT_PARTS: usize = 4;
        let ts = 1_700_000_000.5;
        let content = "\u{10000}".repeat(PEER_CONTENT_MAX_CHARS);
        let title = "\u{10000}".repeat(PEER_TITLE_MAX_CHARS);
        let in_reply_to = "a".repeat(PEER_ID_MAX_CHARS);
        let thread = "b".repeat(PEER_ID_MAX_CHARS);
        let fields_overhead = serde_json::to_vec(&serde_json::json!({ "k": "" }))
            .unwrap()
            .len();
        let fields =
            serde_json::json!({ "k": "x".repeat(PEER_FIELDS_MAX_BYTES - fields_overhead) });
        assert_eq!(
            serde_json::to_vec(&fields).unwrap().len(),
            PEER_FIELDS_MAX_BYTES
        );
        let name = "n".repeat(1_024);
        let large = usize::try_from(DEFAULT_INLINE_MAX_BYTES).unwrap();
        let small = usize::try_from(MAX_INLINE_FILE_TOTAL).unwrap() - large;
        let fixed = vec![
            inline_file(&name, vec![0xAB; large]),
            inline_file(&name, vec![0xCD; small]),
            RawPart::Data {
                data: fields.clone(),
            },
            RawPart::File {
                name: "report.pdf".into(),
                size: u64::MAX,
                sha256: [0xEE; 32],
                bytes: None,
                reference: Some(name.clone()),
            },
        ];
        let assemble = |texts: &[String]| {
            let mut parts = fixed.clone();
            parts.extend(texts.iter().map(|text| text_part(text)));
            parts
        };

        // Pad the text parts until the encoded list lands exactly on the cap; a str
        // length prefix grows with its text, so the measure→pad loop runs to a fixpoint.
        let mut texts = vec![String::new(); TEXT_PARTS];
        for _ in 0..16 {
            let len = packed_len(&encode_parts(&assemble(&texts)));
            if len == MAX_PARTS_BYTES {
                break;
            }
            if len < MAX_PARTS_BYTES {
                let short = MAX_PARTS_BYTES - len;
                for (index, text) in texts.iter_mut().enumerate() {
                    let share = short / TEXT_PARTS + usize::from(index < short % TEXT_PARTS);
                    text.push_str(&"t".repeat(share));
                }
            } else {
                let over = len - MAX_PARTS_BYTES;
                let keep = texts[0].len() - over;
                texts[0].truncate(keep);
            }
        }
        let parts = assemble(&texts);
        assert_eq!(packed_len(&encode_parts(&parts)), MAX_PARTS_BYTES);
        assert!(
            texts
                .iter()
                .all(|text| text.chars().count() <= PEER_CONTENT_MAX_CHARS)
        );
        assert_eq!(parts.len(), MAX_PARTS);

        let build = |parts: Vec<RawPart>| {
            OutboundPeer::with_parts(
                PeerKind::Reply,
                &content,
                Some(&title),
                Some(&in_reply_to),
                Some(fields.clone()),
                parts,
                &PartLimits::default(),
            )
            .and_then(|out| out.with_thread(Some(thread.clone())))
            .map(|out| out.with_disposition(Disposition::Refused, Some(u32::MAX)))
        };
        let out = build(parts.clone()).unwrap();
        assert_eq!(out.content, content);
        assert_eq!(out.title.as_deref(), Some(title.as_str()));
        assert_eq!(out.fields, Some(fields.clone()));
        assert_eq!(out.parts, parts);

        let mut over = texts.clone();
        over[0].push('t');
        assert!(matches!(
            build(assemble(&over)),
            Err(SendError::InvalidParts(_))
        ));

        let body = to_r3_body(&out, ts);
        let r3_total = packed_len(&Value::Array(vec![
            Value::F64(ts),
            Value::Binary(vec![0; 16]),
            Value::Map(vec![
                (Value::from("name_hash"), Value::Binary(vec![0; 10])),
                (Value::from("body"), body.clone()),
            ]),
        ]));
        assert!(
            r3_total < crate::mesh::r3::MAX_R3_PAYLOAD_BYTES,
            "R3 request at every cap packs to {r3_total} bytes, bound {}",
            crate::mesh::r3::MAX_R3_PAYLOAD_BYTES
        );

        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let stored = peer_lxmf_message(&out, &origin);
        let lxmf_total = packed_len(&Value::Array(vec![
            Value::F64(ts),
            Value::Binary(stored.title.clone().unwrap()),
            Value::Binary(stored.content.clone()),
            stored.fields.clone().unwrap(),
        ])) + 16
            + 16
            + 64;
        assert!(
            lxmf_total < crate::mesh::propagation_fetch::MAX_FETCHED_MESSAGE_BYTES,
            "LXMF message at every cap is {lxmf_total} bytes with its header, bound {}",
            crate::mesh::propagation_fetch::MAX_FETCHED_MESSAGE_BYTES
        );

        let decoded = from_r3_body(&round_trip(&body)).unwrap();
        assert_eq!(
            decoded,
            PeerBody {
                kind: PeerKind::Reply,
                id: out.id.clone(),
                in_reply_to: Some(in_reply_to.clone()),
                title: Some(title.clone()),
                content: content.clone(),
                fields: Some(fields.clone()),
                timestamp: ts,
                thread: Some(thread.clone()),
                disposition: Some(Disposition::Refused),
                retry_after: Some(u32::MAX),
                parts: parts.clone(),
                dropped_parts: 0,
            }
        );
        let rebuilt = OutboundPeer {
            kind: decoded.kind,
            id: decoded.id,
            in_reply_to: decoded.in_reply_to,
            title: decoded.title,
            content: decoded.content,
            fields: decoded.fields,
            parts: decoded.parts,
            thread: decoded.thread,
            disposition: decoded.disposition,
            retry_after: decoded.retry_after,
        };
        assert_eq!(pack(&to_r3_body(&rebuilt, ts)), pack(&body));

        let PeerLxmf::Peer(peer) = decode_peer_lxmf(&inbound(
            Some(round_trip(stored.fields.as_ref().unwrap())),
            stored.title.clone(),
            Some(stored.content.clone()),
            &hash_of("signer"),
        )) else {
            panic!("a peer message");
        };
        assert_eq!(
            *peer,
            LxmfPeer {
                name_hash: origin.0,
                kind: PeerKind::Reply,
                id: out.id.clone(),
                in_reply_to: Some(in_reply_to.clone()),
                title: Some(title.clone()),
                content: content.clone(),
                fields: Some(fields.clone()),
                thread: Some(thread.clone()),
                disposition: Some(Disposition::Refused),
                retry_after: Some(u32::MAX),
                parts: parts.clone(),
                dropped_parts: 0,
            }
        );
        let peer = *peer;
        let rebuilt = OutboundPeer {
            kind: peer.kind,
            id: peer.id,
            in_reply_to: peer.in_reply_to,
            title: peer.title,
            content: peer.content,
            fields: peer.fields,
            parts: peer.parts,
            thread: peer.thread,
            disposition: peer.disposition,
            retry_after: peer.retry_after,
        };
        let restored = peer_lxmf_message(&rebuilt, &origin);
        assert_eq!(restored.title, stored.title);
        assert_eq!(restored.content, stored.content);
        assert_eq!(
            pack(restored.fields.as_ref().unwrap()),
            pack(stored.fields.as_ref().unwrap())
        );
    }

    // ---- usage-probe tests: spec-first patterns not pinned above ----

    /// Aggregate cap on the receive side: the receiver drops parts from the TAIL until the
    /// msgpack-encoded `parts` list fits `MAX_PARTS_BYTES`, counting each; the message and
    /// its `content` land. Eight text parts each at the per-part cap pass every per-part
    /// rule yet encode past the aggregate cap, so only the aggregate rule can bite.
    #[test]
    fn a_parts_list_over_the_encoded_cap_sheds_trailing_parts_and_the_sender_refuses_it() {
        let parts: Vec<RawPart> = (0..MAX_PARTS)
            .map(|n| {
                text_part(&format!(
                    "{n}{}",
                    "\u{10000}".repeat(PEER_CONTENT_MAX_CHARS - 1)
                ))
            })
            .collect();
        assert!(
            parts
                .iter()
                .all(|part| part_violation(part, &PartLimits::default(), 0).is_none()),
            "every part passes its own rule"
        );
        let encoded = packed_len(&encode_parts(&parts));
        assert!(encoded > MAX_PARTS_BYTES, "{encoded} must exceed the cap");

        let message = PeerMessage::new(with_parts(parts.clone()));

        assert_eq!(message.content, "hello", "the message still lands");
        assert!(!message.parts.is_empty(), "something is kept");
        assert!(message.parts.len() < MAX_PARTS, "something is dropped");
        assert_eq!(
            message.dropped_parts as usize,
            MAX_PARTS - message.parts.len(),
            "each dropped part is counted"
        );
        let kept: Vec<RawPart> = parts[..message.parts.len()].to_vec();
        let expected: Vec<Part> = kept
            .iter()
            .map(|part| match part {
                RawPart::Text { text } => Part::Text { text: text.clone() },
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            message.parts, expected,
            "the kept parts are the leading ones, in order"
        );
        assert!(packed_len(&encode_parts(&kept)) <= MAX_PARTS_BYTES);
        assert!(
            packed_len(&encode_parts(&parts[..message.parts.len() + 1])) > MAX_PARTS_BYTES,
            "one more part would not have fit"
        );

        let err = OutboundPeer::with_parts(
            PeerKind::Message,
            "hello",
            None,
            None,
            None,
            parts,
            &PartLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(err, SendError::InvalidParts(_)), "{err:?}");
        let text = err.to_string();
        assert!(text.contains(&MAX_PARTS_BYTES.to_string()), "{text}");
        assert!(text.contains(&MAX_PARTS.to_string()), "{text}");
    }

    /// Plan table: a `data` part is capped "as `fields`" — 4 096 B serialised AND depth 8.
    /// The byte half is pinned above; this is the depth half, on both sides.
    #[test]
    fn a_data_part_nesting_past_the_depth_cap_is_dropped_and_the_sender_refuses_it() {
        let too_deep = (0..PEER_FIELDS_MAX_DEPTH + 1).fold(
            serde_json::json!(1),
            |inner, _| serde_json::json!({ "d": inner }),
        );
        let at_depth = (0..PEER_FIELDS_MAX_DEPTH - 1).fold(
            serde_json::json!(1),
            |inner, _| serde_json::json!({ "d": inner }),
        );
        assert!(
            sanitize_fields(at_depth.clone()).is_ok(),
            "the control nests to the cap and is admitted"
        );

        let message = PeerMessage::new(with_parts(vec![
            RawPart::Data {
                data: too_deep.clone(),
            },
            RawPart::Data {
                data: at_depth.clone(),
            },
        ]));
        assert_eq!(message.content, "hello");
        assert_eq!(message.parts, vec![Part::Data { data: at_depth }]);
        assert_eq!(message.dropped_parts, 1);

        let err = OutboundPeer::with_parts(
            PeerKind::Message,
            "hello",
            None,
            None,
            None,
            vec![RawPart::Data { data: too_deep }],
            &PartLimits::default(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            SendError::InvalidParts("data part is too large or nests too deeply")
        );
    }

    /// Hostile part names: the four spec'd negative names (`../../.bashrc`, `C:\x`, NUL,
    /// non-NFC) plus the hardened grammar's reserved-name and absolute forms. On receipt
    /// the part is dropped and counted BEFORE the inbox root exists and nothing lands in
    /// cwd; on send the same name is refused outright, so the receiver never sees it.
    #[test]
    fn every_spec_negative_file_name_drops_the_part_before_the_inbox_or_cwd_is_touched() {
        let names: &[&str] = &[
            "../../.bashrc",
            "C:\\x",
            "a\0b",
            "e\u{301}.txt",
            "CON.md",
            "/etc/passwd",
            "docs\\notes.md",
            "docs/./notes.md",
            "docs//notes.md",
        ];
        for name in names {
            let tmp = TempDir::new("message-negative-name");
            let safe_cwd_probe =
                !name.contains('\0') && !name.starts_with("..") && !name.starts_with('/');
            let existed_before = safe_cwd_probe && std::path::Path::new(name).exists();

            let message = PeerMessage::new_with(
                with_parts(vec![inline_file(name, b"payload".to_vec())]),
                &PartLimits::default(),
                Some(&staging(&tmp)),
            );

            assert_eq!(message.content, "hello", "{name:?}: the message lands");
            assert!(message.parts.is_empty(), "{name:?}: {:?}", message.parts);
            assert_eq!(message.dropped_parts, 1, "{name:?}");
            assert!(
                !tmp.path.join("inbox").exists(),
                "{name:?}: the inbox root was created"
            );
            if safe_cwd_probe {
                assert_eq!(
                    std::path::Path::new(name).exists(),
                    existed_before,
                    "{name:?}: cwd was touched"
                );
            }

            let err = OutboundPeer::with_parts(
                PeerKind::Message,
                "hello",
                None,
                None,
                None,
                vec![inline_file(name, b"payload".to_vec())],
                &PartLimits::default(),
            )
            .unwrap_err();
            assert_eq!(
                err,
                SendError::InvalidParts("file part name is not a wire path"),
                "{name:?}"
            );
        }
    }

    /// Plan table (reference `file`): `ref.path` is a wire path too, since it is what the
    /// receiver will later put on a `/fetch`. Both sides apply the one grammar.
    #[test]
    fn a_reference_part_whose_ref_path_breaks_the_grammar_is_dropped_and_refused() {
        let reference = |path: &str| RawPart::File {
            name: "report.pdf".into(),
            size: 10,
            sha256: [9; 32],
            bytes: None,
            reference: Some(path.into()),
        };
        for bad in ["../secret", "/etc/passwd", "C:\\share\\x", "a\0b"] {
            let message = PeerMessage::new(with_parts(vec![reference(bad), reference("ok/a.pdf")]));
            assert_eq!(message.dropped_parts, 1, "{bad:?}");
            assert!(
                matches!(
                    message.parts.as_slice(),
                    [Part::File { reference: Some(path), staged: None, .. }] if path == "ok/a.pdf"
                ),
                "{bad:?}: {:?}",
                message.parts
            );
            let err = OutboundPeer::with_parts(
                PeerKind::Message,
                "hello",
                None,
                None,
                None,
                vec![reference(bad)],
                &PartLimits::default(),
            )
            .unwrap_err();
            assert_eq!(
                err,
                SendError::InvalidParts("file part ref is not a wire path"),
                "{bad:?}"
            );
        }
    }

    /// Per-part rules, sender half: `OutboundPeer` refuses on EVERY rule the receiver would
    /// drop a part for, not only the encoded total. One case per rule; each `Err` names the
    /// rule and nothing of the part.
    #[test]
    fn the_sender_refuses_each_part_rule_the_receiver_would_drop() {
        let limits = PartLimits {
            inline_max_bytes: 16,
        };
        let send = |parts: Vec<RawPart>, limits: &PartLimits| {
            OutboundPeer::with_parts(PeerKind::Message, "hello", None, None, None, parts, limits)
        };

        let nine = (0..MAX_PARTS + 1)
            .map(|n| text_part(&n.to_string()))
            .collect();
        assert_eq!(
            send(nine, &limits).unwrap_err(),
            SendError::InvalidParts("too many parts")
        );

        assert_eq!(
            send(
                vec![text_part(&"x".repeat(PEER_CONTENT_MAX_CHARS + 1))],
                &limits
            )
            .unwrap_err(),
            SendError::InvalidParts("text part is too long")
        );

        assert_eq!(
            send(vec![inline_file("big.bin", vec![1; 17])], &limits).unwrap_err(),
            SendError::InvalidParts("file part is over the inline cap")
        );

        let wide = PartLimits {
            inline_max_bytes: MAX_INLINE_FILE_TOTAL,
        };
        let half = usize::try_from(MAX_INLINE_FILE_TOTAL / 2 + 1).unwrap();
        assert_eq!(
            send(
                vec![
                    inline_file("a.bin", vec![1; half]),
                    inline_file("b.bin", vec![2; half])
                ],
                &wide
            )
            .unwrap_err(),
            SendError::InvalidParts("file parts are over the per-message inline total")
        );

        let RawPart::File {
            name, size, bytes, ..
        } = inline_file("a.bin", b"hello".to_vec())
        else {
            unreachable!()
        };
        let err = send(
            vec![RawPart::File {
                name,
                size,
                sha256: [0; 32],
                bytes,
                reference: None,
            }],
            &limits,
        )
        .unwrap_err();
        assert_eq!(err, SendError::InvalidParts(SHA256_MISMATCH));
        assert!(!err.to_string().contains("a.bin"), "{err}");

        let wide_data = serde_json::json!({ "k": "x".repeat(PEER_FIELDS_MAX_BYTES) });
        assert_eq!(
            send(vec![RawPart::Data { data: wide_data }], &limits).unwrap_err(),
            SendError::InvalidParts(DATA_PART_RULE)
        );
        let too_deep = (0..PEER_FIELDS_MAX_DEPTH + 1).fold(
            serde_json::json!(1),
            |inner, _| serde_json::json!({ "n": inner }),
        );
        assert_eq!(
            send(vec![RawPart::Data { data: too_deep }], &limits).unwrap_err(),
            SendError::InvalidParts(DATA_PART_RULE)
        );

        for blank in ["", "   ", "\u{1b}[2J", "\u{200B}"] {
            assert_eq!(
                send(vec![text_part(blank)], &limits).unwrap_err(),
                SendError::InvalidParts("text part is blank"),
                "{blank:?}"
            );
        }

        let over_encoded = (0..MAX_PARTS)
            .map(|n| {
                text_part(&format!(
                    "{n}{}",
                    "\u{10000}".repeat(PEER_CONTENT_MAX_CHARS - 1)
                ))
            })
            .collect();
        assert_eq!(
            send(over_encoded, &limits).unwrap_err(),
            SendError::InvalidParts("parts are too large once encoded")
        );

        // The positive control: the same shapes inside every cap are accepted, stored as
        // the receiver would keep them.
        let ok = send(
            vec![
                text_part("  fits\u{1b}[2J "),
                inline_file("a.bin", vec![1; 16]),
                RawPart::Data {
                    data: serde_json::json!({ "n": 1, " k\u{200B}": "\u{1b}[31mred " }),
                },
            ],
            &limits,
        )
        .unwrap();
        assert_eq!(ok.parts[0], text_part("fits"));
        assert_eq!(
            ok.parts[2],
            RawPart::Data {
                data: serde_json::json!({ "n": 1, "k": "red" })
            }
        );
        assert_eq!(ok.parts.len(), 3);
    }

    /// Round trip at working sizes: text, `data` (a map and a bare JSON null), an
    /// inline file and a reference file go through both routes as the same `RawPart`s and
    /// re-encode byte-for-byte.
    #[test]
    fn every_part_shape_round_trips_on_both_routes_byte_for_byte() {
        let parts = vec![
            text_part("first"),
            RawPart::Data {
                data: serde_json::json!({ "n": [1, 2.5, "s", null], "neg": -3 }),
            },
            RawPart::Data {
                data: serde_json::Value::Null,
            },
            inline_file("docs/notes.md", b"# notes\n".to_vec()),
            RawPart::File {
                name: "report.pdf".into(),
                size: 1 << 20,
                sha256: [0xEE; 32],
                bytes: None,
                reference: Some("shared/2024/report.pdf".into()),
            },
        ];
        let out = OutboundPeer::with_parts(
            PeerKind::Message,
            "hello",
            None,
            None,
            None,
            parts.clone(),
            &PartLimits::default(),
        )
        .unwrap();
        let ts = 1_700_000_000.5;

        let body = to_r3_body(&out, ts);
        let decoded = from_r3_body(&round_trip(&body)).unwrap();
        assert_eq!(decoded.parts, parts);
        assert_eq!(decoded.dropped_parts, 0);
        let rebuilt = OutboundPeer {
            parts: decoded.parts,
            ..out.clone()
        };
        assert_eq!(pack(&to_r3_body(&rebuilt, ts)), pack(&body));

        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let stored = peer_lxmf_message(&out, &origin);
        let PeerLxmf::Peer(peer) = decode_peer_lxmf(&inbound(
            Some(round_trip(stored.fields.as_ref().unwrap())),
            stored.title.clone(),
            Some(stored.content.clone()),
            &hash_of("signer"),
        )) else {
            panic!("a peer message");
        };
        assert_eq!(peer.parts, parts);
        assert_eq!(peer.dropped_parts, 0);
        let rebuilt = OutboundPeer {
            parts: peer.parts,
            ..out
        };
        assert_eq!(
            pack(
                peer_lxmf_message(&rebuilt, &origin)
                    .fields
                    .as_ref()
                    .unwrap()
            ),
            pack(stored.fields.as_ref().unwrap())
        );
    }

    /// Usage probe, normalisation before the caps: the sender normalises `text` and `data`
    /// parts BEFORE the caps, so a message whose parts are over the char, fields and
    /// encoded-size caps only by invisibles the receiver strips anyway is accepted — and
    /// what the sender puts on the wire is a fixed point for the receiver on both routes:
    /// nothing dropped, every part kept as sent.
    #[test]
    fn usage_probe_parts_over_a_cap_only_by_invisibles_are_accepted_and_kept_by_the_receiver() {
        let clean_text = "x".repeat(PEER_CONTENT_MAX_CHARS);
        let padded_text = format!("{clean_text}{}", "\u{1b}[2J".repeat(4_000));
        let clean_value = "x".repeat(PEER_FIELDS_MAX_BYTES - 64);
        let padded_value = format!("{clean_value}{}", "\u{200B}".repeat(PEER_FIELDS_MAX_BYTES));

        let mut parts: Vec<RawPart> = (0..MAX_PARTS - 1)
            .map(|_| text_part(&padded_text))
            .collect();
        parts.push(RawPart::Data {
            data: serde_json::json!({ "k": padded_value }),
        });
        assert!(
            padded_text.chars().count() > PEER_CONTENT_MAX_CHARS
                && padded_text.len() * (MAX_PARTS - 1) > MAX_PARTS_BYTES
                && padded_value.len() > PEER_FIELDS_MAX_BYTES,
            "raw, every cap is exceeded"
        );

        let out = OutboundPeer::with_parts(
            PeerKind::Message,
            "hello",
            None,
            None,
            None,
            parts,
            &PartLimits::default(),
        )
        .expect("over the caps only by invisibles");
        assert_eq!(out.parts.len(), MAX_PARTS);
        assert_eq!(out.parts[0], text_part(&clean_text));
        assert_eq!(
            out.parts[MAX_PARTS - 1],
            RawPart::Data {
                data: serde_json::json!({ "k": clean_value })
            }
        );

        let body = to_r3_body(&out, 1.0);
        let decoded = from_r3_body(&round_trip(&body)).unwrap();
        assert_eq!(decoded.dropped_parts, 0);
        assert_eq!(decoded.parts, out.parts);
        let message = PeerMessage::new(RawPeerMessage {
            parts: decoded.parts,
            ..raw(&decoded.content)
        });
        assert_eq!(message.dropped_parts, 0, "{:?}", message.parts.len());
        assert_eq!(message.parts.len(), MAX_PARTS);
        assert_eq!(
            message.parts[0],
            Part::Text {
                text: clean_text.clone()
            }
        );
        assert_eq!(
            message.parts[MAX_PARTS - 1],
            Part::Data {
                data: serde_json::json!({ "k": clean_value })
            }
        );

        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let stored = peer_lxmf_message(&out, &origin);
        let PeerLxmf::Peer(peer) = decode_peer_lxmf(&inbound(
            Some(round_trip(stored.fields.as_ref().unwrap())),
            stored.title.clone(),
            Some(stored.content.clone()),
            &hash_of("signer"),
        )) else {
            panic!("a peer message");
        };
        assert_eq!(peer.dropped_parts, 0);
        assert_eq!(peer.parts, out.parts);
        let landed = PeerMessage::new(RawPeerMessage {
            parts: peer.parts,
            ..raw(&peer.content)
        });
        assert_eq!(landed.dropped_parts, 0);
        assert_eq!(landed.parts, message.parts);
    }

    /// The staging path is keyed by the sending peer, so two peers sending a file under
    /// the same name each get their own copy under their own `<peer-dest32>` directory,
    /// neither suffixed and neither overwriting the other.
    #[test]
    fn two_peers_sending_the_same_file_name_land_in_separate_directories() {
        let tmp = TempDir::new("message-two-peers");
        let inbox = staging(&tmp);
        let mut from_a = with_parts(vec![inline_file("docs/a.md", b"from a".to_vec())]);
        from_a.source_destination = hash_of("alpha-peer");
        let mut from_b = with_parts(vec![inline_file("docs/a.md", b"from b".to_vec())]);
        from_b.source_destination = hash_of("bravo-peer");

        let a = PeerMessage::new_with(from_a, &PartLimits::default(), Some(&inbox));
        let b = PeerMessage::new_with(from_b, &PartLimits::default(), Some(&inbox));

        assert_eq!((a.dropped_parts, b.dropped_parts), (0, 0));
        let staged_of = |message: &PeerMessage| match message.parts.as_slice() {
            [
                Part::File {
                    staged: Some(path), ..
                },
            ] => path.clone(),
            other => panic!("{other:?}"),
        };
        let (path_a, path_b) = (staged_of(&a), staged_of(&b));
        let root = dunce::canonicalize(tmp.path.join("inbox")).unwrap();
        assert_eq!(
            path_a,
            root.join(hash_of("alpha-peer")).join("docs").join("a.md")
        );
        assert_eq!(
            path_b,
            root.join(hash_of("bravo-peer")).join("docs").join("a.md")
        );
        assert_eq!(std::fs::read(&path_a).unwrap(), b"from a");
        assert_eq!(std::fs::read(&path_b).unwrap(), b"from b");
    }

    /// Usage probe, staging collision rule (hard-link publish, never
    /// overwrite): a second message from the same peer under a taken name lands beside
    /// the first as `<stem>-<sha8><ext>`; when that sibling name is also taken by other
    /// bytes the part is dropped and counted, the message still lands with its content
    /// and neither file on disk changes.
    #[test]
    fn a_colliding_file_part_is_dropped_and_counted_while_the_message_and_earlier_files_stay() {
        let tmp = TempDir::new("message-collision");
        let inbox = staging(&tmp);
        let limits = PartLimits::default();
        let b_sha: [u8; 32] = Sha256::digest(b"bytes b").into();
        let suffixed = format!("a-{}.md", hex_lower(&b_sha[..4]));

        let first = PeerMessage::new_with(
            with_parts(vec![inline_file("a.md", b"bytes a".to_vec())]),
            &limits,
            Some(&inbox),
        );
        let second = PeerMessage::new_with(
            with_parts(vec![inline_file("a.md", b"bytes b".to_vec())]),
            &limits,
            Some(&inbox),
        );
        let squatter = PeerMessage::new_with(
            with_parts(vec![inline_file(&suffixed, b"bytes c".to_vec())]),
            &limits,
            Some(&inbox),
        );
        let third = PeerMessage::new_with(
            RawPeerMessage {
                message_id: "id-3".to_string(),
                ..with_parts(vec![inline_file("a.md", b"bytes b".to_vec())])
            },
            &limits,
            Some(&inbox),
        );

        let staged_of = |message: &PeerMessage| match message.parts.as_slice() {
            [
                Part::File {
                    staged: Some(path), ..
                },
            ] => path.clone(),
            other => panic!("{other:?}"),
        };
        let path_a = staged_of(&first);
        let path_b = staged_of(&second);
        assert_eq!(
            path_b,
            path_a.with_file_name(&suffixed),
            "{}",
            path_b.display()
        );
        assert_eq!(std::fs::read(&path_b).unwrap(), b"bytes b");
        // The squatter's name is the suffixed slot a later "bytes c" would otherwise take
        // beside `a.md`; its own staging reuses the existing identical path only when the
        // bytes match, which they do not here, so it lands under its own hash suffix.
        let path_c = staged_of(&squatter);
        assert_ne!(path_c, path_b);
        assert_eq!(std::fs::read(&path_c).unwrap(), b"bytes c");
        // Identical bytes under a taken name reuse the earlier path with no new file.
        assert_eq!(staged_of(&third), path_b);
        assert_eq!(third.dropped_parts, 0);

        // Now a genuine collision: both `a.md` and its `-<sha8>` sibling hold other bytes.
        let d_sha: [u8; 32] = Sha256::digest(b"bytes d").into();
        let d_suffixed = format!("a-{}.md", hex_lower(&d_sha[..4]));
        let blocker = PeerMessage::new_with(
            with_parts(vec![inline_file(&d_suffixed, b"blocker".to_vec())]),
            &limits,
            Some(&inbox),
        );
        assert_eq!(blocker.dropped_parts, 0);
        let collided = PeerMessage::new_with(
            RawPeerMessage {
                message_id: "id-5".to_string(),
                ..with_parts(vec![inline_file("a.md", b"bytes d".to_vec())])
            },
            &limits,
            Some(&inbox),
        );
        assert_eq!(collided.content, "hello");
        assert_eq!(collided.message_id, "id-5");
        assert!(collided.parts.is_empty(), "{:?}", collided.parts);
        assert_eq!(collided.dropped_parts, 1);
        assert_eq!(std::fs::read(&path_a).unwrap(), b"bytes a");
        assert_eq!(
            std::fs::read(path_a.with_file_name(&d_suffixed)).unwrap(),
            b"blocker"
        );
        let mut names: Vec<_> = std::fs::read_dir(path_a.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert!(
            names.iter().all(|name| !name.starts_with(".tmp-")),
            "{names:?}"
        );
        assert_eq!(names.len(), 4, "{names:?}");
    }
}
