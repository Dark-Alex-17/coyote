//! Messages between trusted peers. Outbound, a message is one R3 request on `/message`
//! that the peer acknowledges by id; a peer that cannot be reached gets the message held
//! by an LXMF propagation node instead, typed `coyote.peer/1`, until it next fetches.
//! Inbound, both routes end in the same `PeerSurface`: the handler behind `/message` and
//! the fetch path's `PeerRouting` decode, bound and sanitise what the peer sent, then hand
//! a `PeerMessage` over and answer at once. Nothing on either inbound path waits on the
//! model.

use crate::mesh::card::DISPLAY_NAME_MAX_CHARS;
use crate::mesh::node::MeshRuntime;
use crate::mesh::peers::PeerRecord;
use crate::mesh::propagation::{OutboundMessage, PropagationError, PropagationOptions};
use crate::mesh::propagation_fetch::{InboundMessage, InboundSink};
use crate::mesh::r3::{
    AdmittedRequest, DEFAULT_LINK_TIMEOUT, Handler, MESSAGE_PATH, NAME_HASH_LEN, OriginName,
    R3Error, RefusalCode, Reply, RequestOptions, short,
};
use crate::mesh::trust::{Decision, TrustStore};
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
use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The LXMF custom type a stored peer message carries, so a fetch can tell it from a
/// knock or a plain LXMF message before reading anything else. Versioned in the name.
pub(crate) const PEER_MESSAGE_TYPE: &str = "coyote.peer/1";
/// The `v` every R3 `/message` body carries; a body with any other value is refused.
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

    fn verb(self) -> &'static str {
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

/// One message as it lands, LXMF-shaped: who sent it, where it arrived, the words and the
/// routing. Every peer-supplied string has been capped and sanitised by `new`, so a
/// consumer may show or store any field as it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

impl PeerMessage {
    /// The one place peer text is cleaned: title, content and ids are capped and stripped
    /// of escapes and invisible characters, and `fields` loses every string leaf's
    /// escapes too or is dropped whole when it nests or weighs more than the caps allow.
    pub(crate) fn new(raw: RawPeerMessage) -> Self {
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
        }
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
}

impl OutboundPeer {
    pub(crate) fn new(
        kind: PeerKind,
        content: &str,
        title: Option<&str>,
        in_reply_to: Option<&str>,
        fields: Option<serde_json::Value>,
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
        Ok(Self {
            kind,
            id: uuid::Uuid::new_v4().simple().to_string(),
            in_reply_to,
            title,
            content,
            fields,
        })
    }
}

/// The R3 `/message` body: a string-keyed map with `v`, `kind`, `id`, `content`, `ts`
/// and, when set, `in_reply_to`, `title` and `fields`.
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
    entries.push((Value::from("ts"), Value::F64(timestamp)));
    Value::Map(entries)
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
    Ok(PeerBody {
        kind,
        id,
        in_reply_to,
        title,
        content,
        fields,
        timestamp,
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
    data.push((Value::from("name_hash"), Value::Binary(origin.0.to_vec())));
    if let Some(fields) = &message.fields {
        data.push((Value::from("fields"), rmpv_from_json(fields)));
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
    Peer {
        name_hash: [u8; NAME_HASH_LEN],
        kind: PeerKind,
        id: String,
        in_reply_to: Option<String>,
        title: Option<String>,
        content: String,
        fields: Option<serde_json::Value>,
    },
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
    let bytes_text = |bytes: &Option<Vec<u8>>| {
        bytes
            .as_deref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    };
    PeerLxmf::Peer {
        name_hash,
        kind,
        id,
        in_reply_to,
        title: bytes_text(&message.title),
        content: bytes_text(&message.content).unwrap_or_default(),
        fields: entry(data, "fields").and_then(|fields| json_from_rmpv(fields, 1)),
    }
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
    /// The peer answered with a refusal code; nothing is stored for a peer that said no.
    Refused(RefusalCode),
    /// The direct attempt failed for a reason that is not the peer being unreachable, so
    /// nothing was stored for it.
    Direct(R3Error),
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
            Self::Refused(RefusalCode::NoAccess) => write!(
                f,
                "The peer does not trust this instance and refused the message; `.mesh knock` asks it to"
            ),
            Self::Refused(code) => write!(f, "The peer refused the message: {code}"),
            Self::Direct(err) => write!(f, "The message could not be sent: {err}"),
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

fn unix_now() -> f64 {
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
        let not_trusted = || SendError::NotTrusted {
            destination: destination_hex.to_string(),
        };
        let destination = canonical_hash(destination_hex).ok_or_else(not_trusted)?;
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
            Err(err) => {
                debug!("Mesh {kind} {id} to {dest8} was not sent over the link: {err}");
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
            "Mesh {kind} {id} to {dest8} could not be delivered over a link ({unreachable}); storing it with propagation node {}",
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
        Ok(BroadcastOutcome {
            id: message.id.clone(),
            recipients,
        })
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
            Err(err @ SendError::Refused(_)) => RecipientOutcome::Refused {
                reason: err.to_string(),
            },
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

/// Where inbound peer messages land. Held weakly by the handler and the runtime: the
/// slot owns the runtime that owns both. `deliver_peer` may write the pending store, so
/// the request path runs it on a blocking thread; the fetch task calls it in place.
pub(crate) trait PeerSurface: Send + Sync {
    fn deliver_peer(&self, message: PeerMessage);
    /// Lower-hex of the destination this node receives on right now; `None` while off.
    fn local_destination(&self) -> Option<String>;
}

/// Serves `/message` to whoever the dispatcher has already let through: decodes and
/// bounds the body, hands the message to the surface and acknowledges by id. A surface
/// that is gone means the node is stopping, and a message nobody will read is left
/// unacknowledged so the sender falls back to storing it; so is one whose delivery
/// thread failed, since nothing says it landed.
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
                debug!("Mesh message from {id8} (instance {dest8}) on link {link} refused: {why}");
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
        // The sender checks the acknowledgement against the id it sent, so the raw wire
        // id is echoed; the alphabet check in `from_r3_body` has already bounded it.
        let id = body.id.clone();
        let message = PeerMessage::new(RawPeerMessage {
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
        });
        debug!(
            "Mesh {} {id} from {id8} (instance {dest8}) received on link {link}",
            message.kind
        );
        let delivered = tokio::task::spawn_blocking(move || surface.deliver_peer(message)).await;
        match delivered {
            Ok(()) => Reply::Value(received_reply(&id)),
            Err(err) => {
                warn!(
                    "Mesh message {id} from {id8} (instance {dest8}) was not delivered to the session: {err}; leaving it unacknowledged so the peer stores it instead"
                );
                Reply::Silent
            }
        }
    }
}

/// An `InboundSink` in front of another: fetched peer messages go to the surface once the
/// trust list allows the instance they name, everything else to `inner`. A payload typed
/// as a peer message is never a plain message, so a malformed or untrusted one is dropped
/// rather than forwarded.
pub(crate) struct PeerRouting<'a> {
    pub trust: &'a TrustStore,
    pub surface: Option<Arc<dyn PeerSurface>>,
    pub inner: &'a dyn InboundSink,
}

impl InboundSink for PeerRouting<'_> {
    fn deliver(&self, message: InboundMessage) {
        let id8 = short(&message.source_identity_hash);
        let (name_hash, kind, id, in_reply_to, title, content, fields) =
            match decode_peer_lxmf(&message) {
                PeerLxmf::NotAPeer => return self.inner.deliver(message),
                PeerLxmf::Malformed(why) => {
                    debug!("Propagated peer message from {id8} dropped: {why}");
                    return;
                }
                PeerLxmf::Peer {
                    name_hash,
                    kind,
                    id,
                    in_reply_to,
                    title,
                    content,
                    fields,
                } => (name_hash, kind, id, in_reply_to, title, content, fields),
            };
        let Ok(identity) = AddressHash::new_from_hex_string(&message.source_identity_hash) else {
            debug!("Propagated peer message from {id8} dropped: the signer's hash is malformed");
            return;
        };
        let source_destination = destination_address(&name_hash, &identity).to_hex_string();
        let dest8 = short(&source_destination).to_string();
        if self
            .trust
            .authorize(&message.source_identity_hash, &source_destination)
            .decision
            != Decision::Allow
        {
            debug!("Propagated {kind} from {id8} dropped: instance {dest8} is not trusted");
            return;
        }
        let Some(surface) = &self.surface else {
            debug!(
                "Propagated {kind} from {id8} (instance {dest8}) dropped: the session slot is gone"
            );
            return;
        };
        let peer = PeerMessage::new(RawPeerMessage {
            source_identity: message.source_identity_hash.clone(),
            source_destination,
            destination: surface.local_destination().unwrap_or_default(),
            title,
            content,
            fields,
            timestamp: message.timestamp,
            message_id: id,
            in_reply_to,
            kind,
            via: PeerVia::StoreAndForward,
        });
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
    use crate::mesh::knock::{KNOCK_TYPE, KnockIntro, knock_message};
    use crate::mesh::test_support::TrustList;
    use crate::supervisor::notification::MESH_EVENTS_DROPPED_EVENT;

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
            PeerLxmf::Peer {
                name_hash: origin.0,
                kind: PeerKind::Ask,
                id: message.id.clone(),
                in_reply_to: Some("prev".into()),
                title: Some("ping".into()),
                content: "are you there".into(),
                fields: Some(serde_json::json!({ "k": ["v"] })),
            }
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
            PeerLxmf::Peer {
                name_hash: [3; NAME_HASH_LEN],
                kind: PeerKind::Bulletin,
                id: "b-1".into(),
                in_reply_to: None,
                title: None,
                content: "\u{FFFD}hi".into(),
                fields: None,
            },
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
                .contains(".mesh knock")
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

    /// Records what the routing hands over, standing in for the slot.
    #[derive(Default)]
    struct RecordingSurface {
        delivered: Mutex<Vec<PeerMessage>>,
    }

    impl PeerSurface for RecordingSurface {
        fn deliver_peer(&self, message: PeerMessage) {
            self.delivered.lock().push(message);
        }

        fn local_destination(&self) -> Option<String> {
            Some(hash_of("local"))
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
}
