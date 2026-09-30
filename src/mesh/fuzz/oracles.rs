//! The oracles behind the fuzz targets in `super`: structured generators built on
//! `arbitrary`, spec-derived predicates, reference models and round-trip laws for the
//! dispatch pipeline, the Envelope parser, the propagation body pipeline and the codec
//! family. Every check names the requirement or invariant it stands for in its failure
//! text; a check that could only fail on a panic is not an oracle and is not here.

use super::SplitMix;
use crate::mesh::announce::{
    ANNOUNCE_MAGIC, AnnounceAppData, MAX_DISPLAY_NAME_BYTES, is_control_or_invisible,
};
use crate::mesh::card::{STATUS_CARD_VERSION, StatusCard, StatusError};
use crate::mesh::knock::{KNOCK_TYPE, KnockMessage, decode_knock_message, intro_from_r3_body};
use crate::mesh::knocks::KNOCK_INTRO_MAX_CHARS;
use crate::mesh::message::{
    OutboundPeer, PEER_MESSAGE_TYPE, PEER_WIRE_VERSION, PeerKind, PeerLxmf, PeerMessage, PeerVia,
    decode_peer_lxmf, from_r3_body, to_r3_body,
};
use crate::mesh::pending::{
    PENDING_RECORD_VERSION, PENDING_TTL, PendingRecord, PendingState, PendingStore,
};
use crate::mesh::propagation::lxmf_delivery_hash;
use crate::mesh::propagation_fetch::{
    BodyOutcome, BodyPipeline, Discard, FetchStore, InboundMessage, InboundSink,
    MAX_FETCHED_MESSAGE_BYTES, MAX_UNKNOWN_SOURCE_DEFERRALS, MIN_FETCHED_MESSAGE_BYTES, SourceKeys,
    UNKNOWN_SOURCE_DEFERRAL_HORIZON,
};
use crate::mesh::protocol::{MESH_PROTOCOL_MIN_SUPPORTED, MESH_PROTOCOL_VERSION, VersionRefusal};
use crate::mesh::r3::{
    AdmittedRequest, DispatchError, Dispatcher, Envelope, EnvelopeError, Handler, InboundRequest,
    KNOCK_PATH, KnockEvent, KnockSink, MAX_R3_NESTING_DEPTH, MESSAGE_PATH, NAME_HASH_LEN, PathHash,
    R3Error, RefusalCode, Reply, RequestFrame, RequestHandler, RequestId, STATUS_PATH, SizeBranch,
};
use crate::mesh::test_support::{TempDir, TrustList};
use crate::mesh::trust::TrustStore;
use crate::mesh::{destination_address, display_text, hex_lower, parse_rfc3339, rfc3339_utc};

use arbitrary::{Arbitrary, Unstructured};
use async_trait::async_trait;
use lxmf_core::constants::{FIELD_CUSTOM_DATA, FIELD_CUSTOM_TYPE};
use lxmf_core::identity::PrivateIdentity as CorePrivateIdentity;
use lxmf_core::message::{Payload, WireMessage};
use rmpv::Value;
use rns_transport::destination::link::LinkId;
use rns_transport::hash::{ADDRESS_HASH_SIZE, AddressHash};
use rns_transport::identity::{Identity, PrivateIdentity};
use rns_transport::identity_bridge::{to_core_identity, to_transport_identity};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::runtime::Runtime;

/// The clock every fixture runs at, so a generated timestamp's age is reproducible.
const FIXED_NOW_SECS: u64 = 1_800_000_000;

// ---------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------

pub(super) fn packed(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, value).unwrap();
    bytes
}

/// The one msgpack value `bytes` hold within the frame decoder's nesting budget; `None`
/// when they do not decode or leave bytes over.
pub(super) fn unpack_whole(bytes: &[u8]) -> Option<Value> {
    let mut cursor = Cursor::new(bytes);
    let value = rmpv::decode::read_value_with_max_depth(&mut cursor, MAX_R3_NESTING_DEPTH).ok()?;
    (usize::try_from(cursor.position()).ok() == Some(bytes.len())).then_some(value)
}

/// Equality on the wire, so a NaN compares equal to itself.
fn same_value(a: &Value, b: &Value) -> bool {
    packed(a) == packed(b)
}

fn ensure(condition: bool, what: impl FnOnce() -> String) -> Result<(), String> {
    if condition { Ok(()) } else { Err(what()) }
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn fixed_now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(FIXED_NOW_SECS)
}

/// The first entry under a `str` key, a nil value reading as absent: how the card, the
/// message body and the LXMF custom data are read (MESH-CANON-012).
fn first<'a>(entries: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|(name, _)| name.as_str() == Some(key))
        .map(|(_, value)| value)
        .filter(|value| !value.is_nil())
}

/// MESH-CANON-013 for every emitted map and sub-map.
fn no_duplicate_keys(value: &Value) -> Result<(), String> {
    let Some(entries) = value.as_map() else {
        return Ok(());
    };
    for (index, (key, nested)) in entries.iter().enumerate() {
        ensure(
            !entries[..index].iter().any(|(earlier, _)| earlier == key),
            || format!("MESH-CANON-013: key {key} is emitted twice"),
        )?;
        ensure(!nested.is_nil(), || {
            format!("MESH-CANON-013: key {key} is emitted as nil")
        })?;
        no_duplicate_keys(nested)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------------------

/// `Some` seven times in eight, so keyed generators reach the deep branches far more often
/// than a coin flip would while still leaving every key out now and then.
#[derive(Debug)]
struct Likely<T>(Option<T>);

impl<'a, T: Arbitrary<'a>> Arbitrary<'a> for Likely<T> {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let present = u.ratio(7u8, 8u8)?;
        Ok(Self(present.then(|| T::arbitrary(u)).transpose()?))
    }
}

/// `Some` one time in eight: the odd value that should rarely displace the right one.
#[derive(Debug)]
struct Unlikely<T>(Option<T>);

impl<'a, T: Arbitrary<'a>> Arbitrary<'a> for Unlikely<T> {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let present = u.ratio(1u8, 8u8)?;
        Ok(Self(present.then(|| T::arbitrary(u)).transpose()?))
    }
}

const MAX_CHILDREN: usize = 8;
const MAX_DEPTH: usize = 12;

/// Any msgpack value; `rmpv::Value` has no `Arbitrary` impl of its own.
#[derive(Debug, Arbitrary)]
enum ArbValue {
    Nil,
    Bool(bool),
    U(u64),
    I(i64),
    F32(f32),
    F64(f64),
    Str(String),
    Bin(Vec<u8>),
    Arr(Vec<ArbValue>),
    Map(Vec<(ArbValue, ArbValue)>),
    Ext(i8, Vec<u8>),
}

impl ArbValue {
    /// Containers are cut to `MAX_CHILDREN` and emptied past `MAX_DEPTH`, so generation
    /// stays cheap whatever `arbitrary` nests.
    fn into_value(self) -> Value {
        self.into_value_at(0)
    }

    fn into_value_at(self, depth: usize) -> Value {
        match self {
            Self::Nil => Value::Nil,
            Self::Bool(flag) => Value::from(flag),
            Self::U(n) => Value::from(n),
            Self::I(n) => Value::from(n),
            Self::F32(f) => Value::F32(f),
            Self::F64(f) => Value::F64(f),
            Self::Str(text) => Value::from(text),
            Self::Bin(bytes) => Value::Binary(bytes),
            Self::Ext(tag, data) => Value::Ext(tag, data),
            Self::Arr(_) if depth >= MAX_DEPTH => Value::Array(Vec::new()),
            Self::Arr(items) => Value::Array(
                items
                    .into_iter()
                    .take(MAX_CHILDREN)
                    .map(|item| item.into_value_at(depth + 1))
                    .collect(),
            ),
            Self::Map(_) if depth >= MAX_DEPTH => Value::Map(Vec::new()),
            Self::Map(entries) => Value::Map(
                entries
                    .into_iter()
                    .take(MAX_CHILDREN)
                    .map(|(key, value)| {
                        (key.into_value_at(depth + 1), value.into_value_at(depth + 1))
                    })
                    .collect(),
            ),
        }
    }
}

/// A scalar that is sometimes the right type and sometimes anything at all.
#[derive(Debug, Arbitrary)]
enum Scalar {
    Text(String),
    Uint(u64),
    Byte(u8),
    Int(i64),
    Float(f64),
    Bin(Vec<u8>),
    Nil,
    Bool(bool),
    Any(ArbValue),
}

impl Scalar {
    fn into_value(self) -> Value {
        match self {
            Self::Text(text) => Value::from(text),
            Self::Uint(n) => Value::from(n),
            Self::Byte(n) => Value::from(n),
            Self::Int(n) => Value::from(n),
            Self::Float(f) => Value::F64(f),
            Self::Bin(bytes) => Value::Binary(bytes),
            Self::Nil => Value::Nil,
            Self::Bool(flag) => Value::from(flag),
            Self::Any(value) => value.into_value(),
        }
    }
}

/// A field that is usually what the schema wants and one time in eight some `Scalar`.
#[derive(Debug, Arbitrary)]
struct Field<T> {
    right: T,
    odd: Unlikely<Scalar>,
}

impl<T> Field<T> {
    fn into_value(self, right: impl FnOnce(T) -> Value) -> Value {
        match self.odd.0 {
            Some(odd) => odd.into_value(),
            None => right(self.right),
        }
    }
}

type TextField = Field<String>;
type UintField = Field<u64>;
type U32Field = Field<u32>;
type U16Field = Field<u16>;
type ByteField = Field<u8>;
/// The schema version `1` most of the time.
type Version1 = Field<()>;

fn text(field: TextField) -> Value {
    field.into_value(Value::from)
}

fn uint(field: UintField) -> Value {
    field.into_value(Value::from)
}

fn version1(field: Version1) -> Value {
    field.into_value(|()| Value::from(1u64))
}

/// Entries a map is padded with: unknown keys, non-string keys and duplicates of its own
/// keys, in an order the generator rotates so a duplicate lands before or after the
/// original.
#[derive(Debug, Arbitrary)]
enum Extra {
    Unknown(String, ArbValue),
    NonStr(ArbValue, ArbValue),
    Duplicate(u8, Scalar),
    Ext(i8, Vec<u8>),
}

#[derive(Debug, Arbitrary)]
struct Shape {
    extras: Vec<Extra>,
    rotate: u8,
}

impl Shape {
    /// `fields` with the absent ones dropped, the extras appended and the whole rotated.
    fn map(self, fields: Vec<(Value, Option<Value>)>) -> Value {
        let mut entries: Vec<(Value, Value)> = fields
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect();
        let originals = entries.len();
        for extra in self.extras.into_iter().take(MAX_CHILDREN) {
            match extra {
                Extra::Unknown(key, value) => entries.push((Value::from(key), value.into_value())),
                Extra::NonStr(key, value) => entries.push((key.into_value(), value.into_value())),
                Extra::Duplicate(at, value) if originals > 0 => {
                    let key = entries[usize::from(at) % originals].0.clone();
                    entries.push((key, value.into_value()));
                }
                Extra::Duplicate(..) => {}
                Extra::Ext(tag, data) => entries.push((Value::from("ext"), Value::Ext(tag, data))),
            }
        }
        if !entries.is_empty() {
            let by = usize::from(self.rotate) % entries.len();
            entries.rotate_left(by);
        }
        Value::Map(entries)
    }
}

fn key(name: &str) -> Value {
    Value::from(name)
}

fn present<T>(slot: Likely<T>, into: impl FnOnce(T) -> Value) -> Option<Value> {
    slot.0.map(into)
}

fn rare<T>(slot: Unlikely<T>, into: impl FnOnce(T) -> Value) -> Option<Value> {
    slot.0.map(into)
}

/// The map a generator built, or one time in eight some other value in its place.
fn or_replaced(map: Value, replace: Unlikely<ArbValue>) -> Value {
    replace.0.map_or(map, ArbValue::into_value)
}

// ---------------------------------------------------------------------------------------
// Envelope: generator and spec predicate (section 6.5)
// ---------------------------------------------------------------------------------------

const FIXED_ORIGIN: [u8; NAME_HASH_LEN] = [7; NAME_HASH_LEN];

#[derive(Debug, Arbitrary)]
enum OddVersion {
    Zero,
    Two,
    Max16,
    Over16,
    U64Max,
    Negative,
    Float,
    Str,
    Any(ArbValue),
}

#[derive(Debug, Arbitrary)]
struct VersionGen {
    odd: Unlikely<OddVersion>,
}

impl VersionGen {
    fn into_value(self) -> Value {
        match self.odd.0 {
            None => Value::from(u64::from(MESH_PROTOCOL_VERSION)),
            Some(OddVersion::Zero) => Value::from(0u64),
            Some(OddVersion::Two) => Value::from(2u64),
            Some(OddVersion::Max16) => Value::from(65_535u64),
            Some(OddVersion::Over16) => Value::from(65_536u64),
            Some(OddVersion::U64Max) => Value::from(u64::MAX),
            Some(OddVersion::Negative) => Value::from(-1i64),
            Some(OddVersion::Float) => Value::F64(1.0),
            Some(OddVersion::Str) => Value::from("1"),
            Some(OddVersion::Any(value)) => value.into_value(),
        }
    }
}

#[derive(Debug, Arbitrary)]
enum GoodNameHash {
    Fixed,
    Random([u8; NAME_HASH_LEN]),
}

#[derive(Debug, Arbitrary)]
enum OddNameHash {
    Nine([u8; 9]),
    Sixteen([u8; 16]),
    Str(String),
    Any(ArbValue),
}

#[derive(Debug, Arbitrary)]
struct NameHashGen {
    good: GoodNameHash,
    odd: Unlikely<OddNameHash>,
}

impl NameHashGen {
    fn into_value(self) -> Value {
        match (self.odd.0, self.good) {
            (None, GoodNameHash::Fixed) => Value::Binary(FIXED_ORIGIN.to_vec()),
            (None, GoodNameHash::Random(bytes)) => Value::Binary(bytes.to_vec()),
            (Some(OddNameHash::Nine(bytes)), _) => Value::Binary(bytes.to_vec()),
            (Some(OddNameHash::Sixteen(bytes)), _) => Value::Binary(bytes.to_vec()),
            (Some(OddNameHash::Str(text)), _) => Value::from(text),
            (Some(OddNameHash::Any(value)), _) => value.into_value(),
        }
    }
}

#[derive(Debug, Arbitrary)]
enum EnvelopeExtra {
    Unknown(String, ArbValue),
    NonStr(ArbValue, ArbValue),
    DuplicateVersion(VersionGen),
    DuplicateNameHash(NameHashGen),
    DuplicateBody(ArbValue),
    Ext(i8, Vec<u8>),
}

/// An Envelope map biased toward `v` = 1, a 10-byte `name_hash` (half the time
/// `FIXED_ORIGIN`) and a present `body`, with duplicates, unknown keys, non-string keys and
/// extension values mixed in; one time in eight not a map at all.
#[derive(Debug, Arbitrary)]
pub(super) struct EnvelopeGen {
    v: Likely<VersionGen>,
    name_hash: Likely<NameHashGen>,
    body: Likely<ArbValue>,
    extras: Vec<EnvelopeExtra>,
    rotate: u8,
    replace: Unlikely<ArbValue>,
}

impl EnvelopeGen {
    pub(super) fn into_value(self) -> Value {
        if let Some(other) = self.replace.0 {
            return other.into_value();
        }
        let mut entries = Vec::new();
        if let Some(v) = self.v.0 {
            entries.push((key("v"), v.into_value()));
        }
        if let Some(name_hash) = self.name_hash.0 {
            entries.push((key("name_hash"), name_hash.into_value()));
        }
        if let Some(body) = self.body.0 {
            entries.push((key("body"), body.into_value()));
        }
        for extra in self.extras.into_iter().take(MAX_CHILDREN) {
            entries.push(match extra {
                EnvelopeExtra::Unknown(name, value) => (Value::from(name), value.into_value()),
                EnvelopeExtra::NonStr(name, value) => (name.into_value(), value.into_value()),
                EnvelopeExtra::DuplicateVersion(v) => (key("v"), v.into_value()),
                EnvelopeExtra::DuplicateNameHash(hash) => (key("name_hash"), hash.into_value()),
                EnvelopeExtra::DuplicateBody(body) => (key("body"), body.into_value()),
                EnvelopeExtra::Ext(tag, data) => (key("ext"), Value::Ext(tag, data)),
            });
        }
        if !entries.is_empty() {
            let by = usize::from(self.rotate) % entries.len();
            entries.rotate_left(by);
        }
        Value::Map(entries)
    }
}

/// What section 6.5 says the receiver must make of an Envelope value.
#[derive(Debug)]
pub(super) enum Expect {
    Version(Option<u16>),
    Malformed,
    Ok {
        origin: [u8; NAME_HASH_LEN],
        body: Value,
    },
}

impl Expect {
    fn describe(&self) -> String {
        match self {
            Self::Version(found) => format!("version refusal with found={found:?}"),
            Self::Malformed => "malformed".to_string(),
            Self::Ok { origin, .. } => format!("an envelope from origin {}", hex_lower(origin)),
        }
    }

    /// The same verdict, bodies compared on the wire.
    fn agrees(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Version(a), Self::Version(b)) => a == b,
            (Self::Malformed, Self::Malformed) => true,
            (
                Self::Ok {
                    origin: a,
                    body: body_a,
                },
                Self::Ok {
                    origin: b,
                    body: body_b,
                },
            ) => a == b && same_value(body_a, body_b),
            _ => false,
        }
    }
}

/// The table of section 6.5 read as a predicate: MESH-ENV-021 (not a map), MESH-ENV-017
/// (unknown and non-`str` keys ignored), MESH-ENV-019 (last occurrence wins), MESH-ENV-014
/// and MESH-ENV-020 (`v` judged first: missing, not a `uint`, above 65535 or outside
/// `[1, 1]`), MESH-ENV-015 (`name_hash` a `bin` of 10), MESH-ENV-016 (`body` present).
pub(super) fn expect_envelope(value: &Value) -> Expect {
    let Value::Map(entries) = value else {
        return Expect::Malformed;
    };
    let mut version = None;
    let mut name_hash = None;
    let mut body = None;
    for (name, value) in entries {
        match name.as_str() {
            Some("v") => version = Some(value),
            Some("name_hash") => name_hash = Some(value),
            Some("body") => body = Some(value),
            _ => {}
        }
    }
    let found = version
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok());
    match found {
        Some(v) if (MESH_PROTOCOL_MIN_SUPPORTED..=MESH_PROTOCOL_VERSION).contains(&v) => {}
        found => return Expect::Version(found),
    }
    let Some(Value::Binary(bytes)) = name_hash else {
        return Expect::Malformed;
    };
    let Ok(origin) = <[u8; NAME_HASH_LEN]>::try_from(bytes.as_slice()) else {
        return Expect::Malformed;
    };
    match body {
        Some(body) => Expect::Ok {
            origin,
            body: body.clone(),
        },
        None => Expect::Malformed,
    }
}

fn describe_envelope_result(result: &Result<Envelope, EnvelopeError>) -> String {
    match result {
        Ok(envelope) => format!("an envelope from origin {}", hex_lower(&envelope.origin.0)),
        Err(err) => format!("{err:?}"),
    }
}

/// `Envelope::from_value` against the predicate, plus the canonical re-encoding and the
/// round trip when the value is an envelope.
fn check_envelope_value(value: &Value) -> Result<Expect, String> {
    let expected = expect_envelope(value);
    let observed = Envelope::from_value(value.clone());
    match (&expected, observed) {
        (Expect::Version(found), Err(EnvelopeError::UnsupportedVersion { found: seen })) => {
            ensure(*found == seen, || {
                format!(
                    "MESH-ENV-014: the refusal must name found={found:?}; the parser named {seen:?}"
                )
            })?;
        }
        (Expect::Malformed, Err(EnvelopeError::Malformed)) => {}
        (Expect::Ok { origin, body }, Ok(envelope)) => {
            ensure(
                envelope.version == MESH_PROTOCOL_VERSION
                    && envelope.origin.0 == *origin
                    && same_value(&envelope.body, body),
                || {
                    "MESH-ENV-019: the parsed envelope must carry v=1, the last name_hash and the last body"
                        .to_string()
                },
            )?;
            let canonical = Value::Map(vec![
                (key("v"), Value::from(1u64)),
                (key("name_hash"), Value::Binary(origin.to_vec())),
                (key("body"), body.clone()),
            ]);
            let reencoded = envelope.into_value();
            ensure(same_value(&reencoded, &canonical), || {
                format!(
                    "section 6.5: into_value must emit the canonical v, name_hash, body map; got {reencoded}"
                )
            })?;
            match Envelope::from_value(reencoded) {
                Ok(again) => ensure(
                    again.version == MESH_PROTOCOL_VERSION
                        && again.origin.0 == *origin
                        && same_value(&again.body, body),
                    || "round trip: from_value(into_value(e)) must equal e".to_string(),
                )?,
                Err(err) => {
                    return Err(format!(
                        "round trip: from_value(into_value(e)) refused its own output: {err:?}"
                    ));
                }
            }
        }
        (expected, observed) => {
            return Err(format!(
                "MESH-ENV-014..021: the spec predicate says {}, the parser said {}",
                expected.describe(),
                describe_envelope_result(&observed)
            ));
        }
    }
    Ok(expected)
}

/// A request frame around `data` as it would sit on the wire, time 0 and a zero path hash.
fn frame_around(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x93, 0xcb];
    out.extend_from_slice(&0f64.to_be_bytes());
    out.extend_from_slice(&[0xc4, 0x10]);
    out.extend_from_slice(&[0u8; ADDRESS_HASH_SIZE]);
    out.extend_from_slice(data);
    out
}

/// One corpus file or one encoded generator output: an envelope value on the wire.
pub(super) fn check_envelope_bytes(bytes: &[u8]) -> Result<(), String> {
    match unpack_whole(bytes) {
        Some(value) => check_envelope_value(&value).map(drop),
        None => ensure(RequestFrame::decode(&frame_around(bytes)).is_err(), || {
            "MESH-ENV-028: a frame whose data is not one whole msgpack value must not decode"
                .to_string()
        }),
    }
}

/// The in-memory value and its wire form must earn the same verdict.
pub(super) fn check_envelope_wire_and_value(bytes: &[u8], value: &Value) -> Result<(), String> {
    let in_memory = check_envelope_value(value)?;
    let Some(decoded) = unpack_whole(bytes) else {
        return Err("the encoder's own output must decode as one whole value".to_string());
    };
    let on_wire = check_envelope_value(&decoded)?;
    ensure(in_memory.agrees(&on_wire), || {
        format!(
            "the verdict must survive an encode/decode round trip: in memory {}, on the wire {}",
            in_memory.describe(),
            on_wire.describe()
        )
    })
}

// ---------------------------------------------------------------------------------------
// Dispatch: the authorization pipeline through the real dispatcher (section 6.6)
// ---------------------------------------------------------------------------------------

#[derive(Debug, Arbitrary)]
enum PathGen {
    Status,
    Message,
    Knock,
    Other(String),
    RawHash([u8; ADDRESS_HASH_SIZE]),
}

impl PathGen {
    fn into_hash(self) -> PathHash {
        match self {
            Self::Status => PathHash::of(STATUS_PATH),
            Self::Message => PathHash::of(MESSAGE_PATH),
            Self::Knock => PathHash::of(KNOCK_PATH),
            Self::Other(path) => PathHash::of(&path),
            Self::RawHash(bytes) => PathHash::from(bytes),
        }
    }
}

/// Wire-level damage to an otherwise well-formed frame.
#[derive(Debug, Arbitrary)]
enum Tamper {
    ArbitraryArray(Vec<ArbValue>),
    Raw(Vec<u8>),
    Truncate(u16),
    Trailing(Vec<u8>),
}

/// A request frame, well-formed seven times in eight, around a biased Envelope.
#[derive(Debug, Arbitrary)]
pub(super) struct FrameGen {
    time: f64,
    path: PathGen,
    data: EnvelopeGen,
    tamper: Unlikely<Tamper>,
}

impl FrameGen {
    pub(super) fn into_bytes(self) -> Vec<u8> {
        let frame = RequestFrame {
            time: self.time,
            path_hash: self.path.into_hash(),
            data: self.data.into_value(),
        };
        let bytes = frame.encode();
        match self.tamper.0 {
            None => bytes,
            Some(Tamper::ArbitraryArray(items)) => packed(&Value::Array(
                items
                    .into_iter()
                    .take(MAX_CHILDREN)
                    .map(ArbValue::into_value)
                    .collect(),
            )),
            Some(Tamper::Raw(raw)) => raw,
            Some(Tamper::Truncate(at)) => {
                let keep = usize::from(at) % bytes.len();
                bytes[..keep].to_vec()
            }
            Some(Tamper::Trailing(extra)) => [bytes, extra].concat(),
        }
    }
}

/// Section 6.1 as a predicate: one whole msgpack value that is an array of exactly three
/// elements, an `f64`, a `bin` of 16 and anything (MESH-ENV-001..004).
fn expect_frame(bytes: &[u8]) -> Option<(f64, [u8; ADDRESS_HASH_SIZE], Value)> {
    let Value::Array(elements) = unpack_whole(bytes)? else {
        return None;
    };
    let [time, hash, data] = <[Value; 3]>::try_from(elements).ok()?;
    let Value::F64(time) = time else {
        return None;
    };
    let Value::Binary(hash) = hash else {
        return None;
    };
    let hash = <[u8; ADDRESS_HASH_SIZE]>::try_from(hash.as_slice()).ok()?;
    Some((time, hash, data))
}

/// How the trust list stands toward a tier's identity, from which the reference model
/// derives the verdict for any destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// Stage 3 or 4: no identity, or one that is unknown or blocked.
    Silent,
    /// `all_destinations`.
    Allow,
    /// Known without `all_destinations` and bound to nothing: every instance knocks, or is
    /// refused as identity changed when the instance is one the list binds to another identity.
    Knock,
    /// Bound to one destination, its own at `FIXED_ORIGIN`: that one is allowed and every
    /// other origin knocks. This tier never reaches identity changed, since the one origin
    /// that would trigger it derives the destination it is bound to.
    BoundTo(AddressHash),
    /// Trusted for all destinations but with one denied.
    DeniedAt(AddressHash),
}

struct Tier {
    label: &'static str,
    identity: Option<Identity>,
    gate: Gate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Allow,
    Knock,
    IdentityChanged,
    Refuse,
}

impl Gate {
    /// MESH-ENV-039: destination deny, identity block, destination allow, identity allow,
    /// identity changed, default closed. `FIXED_ORIGIN` is the one origin whose destination
    /// the list binds to another identity (`bound`), so a default-closed instance from it is
    /// refused as identity changed instead of knocking. `Silent` never gets here.
    fn verdict(self, origin: &[u8; NAME_HASH_LEN], destination: &AddressHash) -> Verdict {
        let closed = if *origin == FIXED_ORIGIN {
            Verdict::IdentityChanged
        } else {
            Verdict::Knock
        };
        match self {
            Self::Silent => unreachable!("silent tiers are judged before the trust verdict"),
            Self::Allow => Verdict::Allow,
            Self::Knock => closed,
            Self::BoundTo(bound) if bound == *destination => Verdict::Allow,
            Self::BoundTo(_) => Verdict::Knock,
            Self::DeniedAt(denied) if denied == *destination => Verdict::Refuse,
            Self::DeniedAt(_) => Verdict::Allow,
        }
    }
}

#[derive(Default)]
struct CountingHandler(AtomicUsize);

#[async_trait]
impl Handler for CountingHandler {
    async fn handle(&self, request: AdmittedRequest) -> Reply {
        self.0.fetch_add(1, Ordering::SeqCst);
        Reply::Value(request.body)
    }
}

struct Knock {
    identity_hash: String,
    destination_hash: String,
    name_hash: String,
    path_hash: PathHash,
    data: Option<Value>,
}

#[derive(Default)]
struct SpySink(Mutex<Vec<Knock>>);

impl KnockSink for SpySink {
    fn knock(&self, knock: KnockEvent) {
        self.0.lock().unwrap().push(Knock {
            identity_hash: knock.identity_hash,
            destination_hash: knock.destination_hash,
            name_hash: knock.name_hash,
            path_hash: knock.path_hash,
            data: knock.data,
        });
    }
}

/// One dispatcher over one trust list, built once per target run from the seed, and the
/// seven tiers every frame is run against.
pub(super) struct DispatchFixture {
    rt: Runtime,
    dispatcher: Dispatcher,
    knocks: Arc<SpySink>,
    handler: Arc<CountingHandler>,
    tiers: Vec<Tier>,
    link_id: LinkId,
    _tmp: TempDir,
}

impl DispatchFixture {
    pub(super) fn new(rng: &mut SplitMix) -> Self {
        let mut mint = || *PrivateIdentity::new_from_rand(&mut *rng).as_identity();
        let [trusted, known, blocked, bound, denied, unknown] =
            [mint(), mint(), mint(), mint(), mint(), mint()];
        let hex = |identity: &Identity| identity.address_hash.to_hex_string();
        let own = |identity: &Identity| destination_address(&FIXED_ORIGIN, &identity.address_hash);
        let list = TrustList::default()
            .identity(&hex(&trusted), true)
            .identity(&hex(&known), false)
            .block(&hex(&blocked))
            .destination(&own(&bound).to_hex_string(), &hex(&bound))
            .identity(&hex(&denied), true)
            .deny(&own(&denied).to_hex_string());
        let (trust, tmp) = list.open("fuzz-dispatch");
        let knocks = Arc::new(SpySink::default());
        let dispatcher = Dispatcher::new(trust, knocks.clone());
        let handler = Arc::new(CountingHandler::default());
        assert!(dispatcher.register(STATUS_PATH, handler.clone()).is_ok());
        let tier = |label, identity: Identity, gate| Tier {
            label,
            identity: Some(identity),
            gate,
        };
        let tiers = vec![
            tier("trusted", trusted, Gate::Allow),
            tier("known", known, Gate::Knock),
            tier("blocked", blocked, Gate::Silent),
            tier("bound", bound, Gate::BoundTo(own(&bound))),
            tier("denied", denied, Gate::DeniedAt(own(&denied))),
            tier("unknown", unknown, Gate::Silent),
            Tier {
                label: "anonymous",
                identity: None,
                gate: Gate::Silent,
            },
        ];
        Self {
            rt: runtime(),
            dispatcher,
            knocks,
            handler,
            tiers,
            link_id: LinkId::new_from_rand(&mut *rng),
            _tmp: tmp,
        }
    }
}

fn describe_reply(reply: &Reply) -> String {
    match reply {
        Reply::Value(value) => format!("Value({value})"),
        Reply::Code(code) => format!("Code({code})"),
        Reply::Silent => "Silent".to_string(),
    }
}

fn is_no_access(reply: &Reply) -> bool {
    matches!(reply, Reply::Code(code) if code.to_wire() == Value::from(0xf1u8))
}

/// The same frame bytes against every tier: decode as section 6.1 says, then the reply,
/// the knock and the handler count each tier's stage mandates.
pub(super) fn check_dispatch_bytes(fx: &DispatchFixture, bytes: &[u8]) -> Result<(), String> {
    let frame = match (RequestFrame::decode(bytes), expect_frame(bytes)) {
        (Err(_), None) => return Ok(()),
        (Ok(frame), Some((time, hash, data))) => {
            ensure(
                frame.time.to_bits() == time.to_bits()
                    && frame.path_hash == PathHash::from(hash)
                    && same_value(&frame.data, &data),
                || {
                    "MESH-ENV-001..003: the decoded frame must carry the three elements on the wire"
                        .to_string()
                },
            )?;
            frame
        }
        (Ok(frame), None) => {
            return Err(format!(
                "MESH-ENV-001..004: the frame decoder accepted bytes section 6.1 rejects: {frame:?}"
            ));
        }
        (Err(err), Some(_)) => {
            return Err(format!(
                "MESH-ENV-001..004: the frame decoder refused a well-formed frame: {err}"
            ));
        }
    };
    let expected = expect_envelope(&frame.data);
    for tier in &fx.tiers {
        check_tier(fx, tier, &frame, &expected)
            .map_err(|what| format!("{} tier: {what}", tier.label))?;
    }
    Ok(())
}

fn check_tier(
    fx: &DispatchFixture,
    tier: &Tier,
    frame: &RequestFrame,
    expected: &Expect,
) -> Result<(), String> {
    let handled_before = fx.handler.0.load(Ordering::SeqCst);
    fx.knocks.0.lock().unwrap().clear();
    let request = InboundRequest {
        link_id: fx.link_id,
        identity: tier.identity,
        request_id: RequestId::from([1u8; ADDRESS_HASH_SIZE]),
        path_hash: frame.path_hash,
        requested_at: frame.time,
        data: frame.data.clone(),
        branch: SizeBranch::Packet,
    };
    let reply = fx
        .rt
        .block_on(RequestHandler::handle(&fx.dispatcher, request));
    let handled = fx.handler.0.load(Ordering::SeqCst) - handled_before;
    let knocks = std::mem::take(&mut *fx.knocks.0.lock().unwrap());
    let (Some(identity), false) = (tier.identity, tier.gate == Gate::Silent) else {
        ensure(matches!(reply, Reply::Silent), || {
            format!(
                "(c) MESH-ENV-026/027: an anonymous, unknown or blocked requester hears silence; got {}",
                describe_reply(&reply)
            )
        })?;
        ensure(handled == 0, || {
            "(c) unauthenticated or unadmitted input never reaches a handler".to_string()
        })?;
        return ensure(knocks.is_empty(), || {
            "MESH-ENV-026/027: silence files no knock".to_string()
        });
    };
    match expected {
        Expect::Version(found) => {
            let Reply::Value(value) = &reply else {
                return Err(format!(
                    "MESH-ENV-029: a version refusal value was due, got {}",
                    describe_reply(&reply)
                ));
            };
            ensure(
                VersionRefusal::from_value(value) == Some(VersionRefusal::current(*found)),
                || {
                    format!(
                        "MESH-ENV-029: the refusal must name found={found:?} and this node's window; got {value}"
                    )
                },
            )?;
            ensure(knocks.is_empty(), || {
                "MESH-ENV-014: a version refusal never files a knock".to_string()
            })?;
            ensure(handled == 0, || {
                "MESH-ENV-029: a version refusal never reaches a handler".to_string()
            })
        }
        Expect::Malformed => {
            ensure(is_no_access(&reply), || {
                format!(
                    "MESH-ENV-030: a malformed envelope earns NoAccess, got {}",
                    describe_reply(&reply)
                )
            })?;
            ensure(knocks.is_empty(), || {
                "MESH-ENV-015: a malformed envelope never files a knock".to_string()
            })?;
            ensure(handled == 0, || {
                "MESH-ENV-030: a malformed envelope never reaches a handler".to_string()
            })
        }
        Expect::Ok { origin, body } => {
            let destination = destination_address(origin, &identity.address_hash);
            match tier.gate.verdict(origin, &destination) {
                Verdict::Refuse => {
                    ensure(is_no_access(&reply), || {
                        format!(
                            "MESH-ENV-033: a denied destination earns NoAccess, got {}",
                            describe_reply(&reply)
                        )
                    })?;
                    ensure(knocks.is_empty(), || {
                        "MESH-ENV-033: a denied destination files no knock".to_string()
                    })?;
                    ensure(handled == 0, || {
                        "MESH-ENV-033: a denied destination never reaches a handler".to_string()
                    })
                }
                Verdict::IdentityChanged => {
                    ensure(is_no_access(&reply), || {
                        format!(
                            "MESH-ENV-033/050: identity changed earns NoAccess, got {}",
                            describe_reply(&reply)
                        )
                    })?;
                    ensure(knocks.is_empty(), || {
                        "MESH-ENV-050: identity changed files no knock".to_string()
                    })?;
                    ensure(handled == 0, || {
                        "MESH-ENV-033: identity changed never reaches a handler".to_string()
                    })
                }
                Verdict::Knock => {
                    ensure(is_no_access(&reply), || {
                        format!(
                            "MESH-ENV-032: default closed earns NoAccess after the knock, got {}",
                            describe_reply(&reply)
                        )
                    })?;
                    ensure(handled == 0, || {
                        "MESH-ENV-032: default closed never reaches a handler".to_string()
                    })?;
                    ensure(knocks.len() == 1, || {
                        format!(
                            "MESH-ENV-032: default closed files exactly one knock, filed {}",
                            knocks.len()
                        )
                    })?;
                    let knock = &knocks[0];
                    ensure(
                        knock.identity_hash == identity.address_hash.to_hex_string()
                            && knock.destination_hash == destination.to_hex_string()
                            && knock.name_hash == hex_lower(origin)
                            && knock.path_hash == frame.path_hash,
                        || {
                            "MESH-KNOCK: the knock names the proven identity, the derived destination, the origin and the path".to_string()
                        },
                    )?;
                    let is_knock_path = frame.path_hash == PathHash::of(KNOCK_PATH);
                    ensure(knock.data.is_some() == is_knock_path, || {
                        "MESH-ENV-032: the knock retains the body only when the path is /knock"
                            .to_string()
                    })?;
                    match &knock.data {
                        Some(data) => ensure(same_value(data, body), || {
                            "MESH-ENV-032: the retained body is the envelope's body".to_string()
                        }),
                        None => Ok(()),
                    }
                }
                Verdict::Allow => {
                    ensure(knocks.is_empty(), || {
                        "MESH-ENV-034..036: an allowed request files no knock".to_string()
                    })?;
                    let Reply::Value(value) = &reply else {
                        return Err(format!(
                            "MESH-ENV-034..036: an allowed request is answered with a value, got {}",
                            describe_reply(&reply)
                        ));
                    };
                    let path_hash = frame.path_hash;
                    if path_hash == PathHash::of(STATUS_PATH) {
                        ensure(handled == 1, || {
                            format!(
                                "MESH-ENV-036: the provided path runs its handler exactly once, ran {handled}"
                            )
                        })?;
                        return ensure(same_value(value, body), || {
                            "MESH-ENV-036: the handler's reply (the echoed body) is answered verbatim".to_string()
                        });
                    }
                    ensure(handled == 0, || {
                        "(c) only /status has a provider here; no other path may run it".to_string()
                    })?;
                    if path_hash == PathHash::of(KNOCK_PATH) {
                        ensure(value.is_nil(), || {
                            format!(
                                "MESH-ENV-036: the dispatcher's own /knock handler answers nil to a trusted instance, got {value}"
                            )
                        })
                    } else if path_hash == PathHash::of(MESSAGE_PATH) {
                        ensure(
                            DispatchError::from_value(value)
                                == Some(DispatchError::NoProvider {
                                    path: MESSAGE_PATH.to_string(),
                                }),
                            || {
                                format!(
                                    "MESH-ENV-035: a known path without a provider earns the no_provider map, got {value}"
                                )
                            },
                        )
                    } else {
                        ensure(
                            DispatchError::from_value(value)
                                == Some(DispatchError::UnknownPath {
                                    path_hash: path_hash.to_hex_string(),
                                }),
                            || {
                                format!(
                                    "MESH-ENV-034: an unknown path earns the unknown_path map naming its hash, got {value}"
                                )
                            },
                        )
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Receipt: the propagation body pipeline against a reference model (section 11.4)
// ---------------------------------------------------------------------------------------

/// Long enough for a seal, its replay and its re-encryption to meet in one sequence; a
/// debug-build seal and verify cost about a millisecond each, so this bounds the budget.
const MAX_EVENTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Arbitrary)]
enum Signer {
    /// Trusted.
    A,
    /// Blocked.
    B,
    /// Known to nobody.
    C,
    /// Trusted; signs bodies that claim another source.
    F,
}

/// The oversize bound has no random event: a body above `MAX_FETCHED_MESSAGE_BYTES` costs
/// two debug-build SHA-256 passes over 128 KiB, and the corpus replays one every run.
#[derive(Debug, Arbitrary)]
enum Event {
    Garbage(Vec<u8>),
    Undersize(u8),
    WrongDestination {
        signer: Signer,
        content: Vec<u8>,
    },
    Honest {
        signer: Signer,
        content: Vec<u8>,
        known: bool,
    },
    Forged {
        signer: Signer,
        claimed: Signer,
        content: Vec<u8>,
        known: bool,
    },
    Flipped {
        signer: Signer,
        content: Vec<u8>,
        at: u16,
    },
    /// The exact bytes of an earlier event again.
    Replay(u8),
    /// An earlier event's signed message sealed again under a fresh ephemeral key: a new
    /// transient id carrying the same message id.
    Reencrypt(u8),
    /// Moves the bench clock forward by up to twice the deferral horizon, so a deferred
    /// body's budget can be spent within one sequence.
    Advance(u32),
}

impl Event {
    fn advance(secs: u32) -> Duration {
        let horizon = UNKNOWN_SOURCE_DEFERRAL_HORIZON.as_secs();
        Duration::from_secs(u64::from(secs) % (2 * horizon + 1))
    }
}

pub(super) struct Sequence(Vec<Event>);

impl<'a> Arbitrary<'a> for Sequence {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let len = u.int_in_range(1..=MAX_EVENTS)?;
        (0..len)
            .map(|_| Event::arbitrary(u))
            .collect::<arbitrary::Result<Vec<_>>>()
            .map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignerStanding {
    Trusted,
    Blocked,
    Unknown,
}

struct SignerKeys {
    core: CorePrivateIdentity,
    identity: Identity,
    delivery: AddressHash,
    standing: SignerStanding,
}

#[derive(Default)]
struct MutableKeys {
    known: Mutex<HashMap<AddressHash, Identity>>,
    calls: AtomicUsize,
}

impl MutableKeys {
    fn set_known(&self, signer: &SignerKeys, known: bool) {
        let mut map = self.known.lock().unwrap();
        if known {
            map.insert(signer.delivery, signer.identity);
        } else {
            map.remove(&signer.delivery);
        }
    }

    fn is_known(&self, source: &AddressHash) -> bool {
        self.known.lock().unwrap().contains_key(source)
    }
}

#[async_trait]
impl SourceKeys for MutableKeys {
    async fn identity_for(&self, source: &AddressHash) -> Result<Option<Identity>, R3Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.known.lock().unwrap().get(source).copied())
    }
}

#[derive(Default)]
struct CountingSink {
    delivered: Mutex<Vec<[u8; 32]>>,
}

impl InboundSink for CountingSink {
    fn deliver(&self, message: InboundMessage) {
        self.delivered.lock().unwrap().push(message.message_id);
    }
}

/// A signed message and the ground truth the pipeline must rediscover about it.
#[derive(Clone)]
struct Sealed {
    wire: WireMessage,
    message_id: [u8; 32],
    source: AddressHash,
    signature_valid: bool,
    /// Of the claimed source, whose key the pipeline verifies against.
    standing: SignerStanding,
}

#[derive(Clone)]
struct Built {
    bytes: Vec<u8>,
    prefix_ok: bool,
    /// The bytes are an untampered seal of `sealed`.
    decrypts: bool,
    /// A flipped byte: `Undecryptable` is expected, `BadSignature` tolerated.
    tampered: bool,
    sealed: Option<Sealed>,
}

/// MESH-PROP-034's per-body deferral record, as `FetchStore::defer` keeps it.
#[derive(Clone, Copy)]
struct Sighting {
    attempts: u8,
    first_seen: SystemTime,
}

/// What the spec's dedup and deferral state must be after each body.
#[derive(Default)]
struct Model {
    recorded: BTreeSet<[u8; 32]>,
    delivered: BTreeSet<[u8; 32]>,
    deferrals: BTreeMap<[u8; 32], Sighting>,
    delivered_log: Vec<[u8; 32]>,
}

impl Model {
    /// The "Recorded" column of 11.4 plus MESH-PROP-034's deferral count and first sighting.
    fn apply(
        &mut self,
        transient: [u8; 32],
        outcome: &BodyOutcome,
        message_id: Option<[u8; 32]>,
        now: SystemTime,
    ) {
        match outcome {
            BodyOutcome::Delivered => {
                self.record(transient);
                let id = message_id.expect("a delivered body was sealed here");
                self.delivered.insert(id);
                self.delivered_log.push(id);
            }
            BodyOutcome::Deferred { attempts } => {
                self.deferrals
                    .entry(transient)
                    .or_insert(Sighting {
                        attempts: 0,
                        first_seen: now,
                    })
                    .attempts = *attempts;
            }
            BodyOutcome::Discarded(
                Discard::Oversize { .. }
                | Discard::Undersize { .. }
                | Discard::Duplicate
                | Discard::UnknownSource,
            ) => {}
            BodyOutcome::Discarded(_) => self.record(transient),
        }
    }

    fn record(&mut self, transient: [u8; 32]) {
        self.deferrals.remove(&transient);
        self.recorded.insert(transient);
    }
}

/// The stage of 11.4 at which a body stopped; `Delivered` when none did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Bounds,
    TransientDedup,
    Prefix,
    Decrypt,
    MessageDedup,
    SourceKey,
    Signature,
    Standing,
    Delivered,
}

struct Bench {
    keys: MutableKeys,
    sink: CountingSink,
    store: FetchStore,
    model: Model,
    now: SystemTime,
    delivered_outcomes: usize,
}

/// One recipient, four signers and one trust list, minted once per target run.
pub(super) struct ReceiptFixture {
    rt: Runtime,
    recipient: CorePrivateIdentity,
    recipient_identity: Identity,
    delivery: AddressHash,
    signers: [SignerKeys; 4],
    trust: Arc<TrustStore>,
    now: SystemTime,
    stores: AtomicU64,
    seal_rng: Mutex<SplitMix>,
    tmp: TempDir,
}

impl ReceiptFixture {
    pub(super) fn new(rng: &mut SplitMix) -> Self {
        let recipient = CorePrivateIdentity::new_from_rand(&mut *rng);
        let recipient_identity = to_transport_identity(recipient.as_identity());
        let mut keys = |standing| {
            let core = CorePrivateIdentity::new_from_rand(&mut *rng);
            let identity = to_transport_identity(core.as_identity());
            SignerKeys {
                delivery: lxmf_delivery_hash(&identity),
                core,
                identity,
                standing,
            }
        };
        let signers = [
            keys(SignerStanding::Trusted),
            keys(SignerStanding::Blocked),
            keys(SignerStanding::Unknown),
            keys(SignerStanding::Trusted),
        ];
        let hex = |signer: &SignerKeys| signer.identity.address_hash.to_hex_string();
        let (trust, tmp) = TrustList::default()
            .identity(&hex(&signers[0]), true)
            .identity(&hex(&signers[3]), true)
            .block(&hex(&signers[1]))
            .open("fuzz-receipt");
        Self {
            rt: runtime(),
            delivery: lxmf_delivery_hash(&recipient_identity),
            recipient,
            recipient_identity,
            signers,
            trust,
            now: fixed_now(),
            stores: AtomicU64::new(0),
            seal_rng: Mutex::new(rng.fork()),
            tmp,
        }
    }

    fn signer(&self, signer: Signer) -> &SignerKeys {
        &self.signers[signer as usize]
    }

    fn bench(&self) -> Bench {
        let n = self.stores.fetch_add(1, Ordering::SeqCst);
        let store =
            FetchStore::load(self.tmp.path.join(format!("store-{n}.json")), self.now).unwrap();
        Bench {
            keys: MutableKeys::default(),
            sink: CountingSink::default(),
            store,
            model: Model::default(),
            now: self.now,
            delivered_outcomes: 0,
        }
    }

    /// An unsigned message to the recipient naming `claimed` as its source.
    fn wire_to(&self, claimed: &AddressHash, content: &[u8]) -> WireMessage {
        let mut source = [0u8; 16];
        source.copy_from_slice(claimed.as_slice());
        let mut destination = [0u8; 16];
        destination.copy_from_slice(self.delivery.as_slice());
        WireMessage::new(
            destination,
            source,
            Payload::new(
                1_700_000_000.5,
                Some(content.to_vec()),
                Some(b"hello".to_vec()),
                Some(Value::Map(vec![])),
                None,
            ),
        )
    }

    /// `wire` as a node serves it: `destination hash || encrypted message`, under an
    /// ephemeral key drawn from the fixture's seeded stream.
    fn seal(&self, wire: &WireMessage) -> Vec<u8> {
        let rng = self.seal_rng.lock().unwrap().fork();
        wire.pack_propagation_transient_with_rng(&to_core_identity(&self.recipient_identity), rng)
            .unwrap()
            .0
    }

    fn opaque(&self, bytes: Vec<u8>) -> Built {
        let prefix_ok = bytes.len() >= ADDRESS_HASH_SIZE
            && bytes[..ADDRESS_HASH_SIZE] == *self.delivery.as_slice();
        Built {
            bytes,
            prefix_ok,
            decrypts: false,
            tampered: false,
            sealed: None,
        }
    }

    /// A body signed by `signer` that names `claimed` as its source.
    fn signed(&self, signer: Signer, claimed: Signer, content: &[u8]) -> Built {
        let signer = self.signer(signer);
        let claimed = self.signer(claimed);
        let mut wire = self.wire_to(&claimed.delivery, content);
        wire.sign(&signer.core).unwrap();
        let message_id = wire.try_message_id().unwrap();
        let bytes = self.seal(&wire);
        Built {
            bytes,
            prefix_ok: true,
            decrypts: true,
            tampered: false,
            sealed: Some(Sealed {
                wire,
                message_id,
                source: claimed.delivery,
                signature_valid: signer.delivery == claimed.delivery,
                standing: claimed.standing,
            }),
        }
    }

    /// The body for `event`, with the key table set as the event says; `None` for a clock
    /// advance or a reference to an event that does not exist or cannot be re-sealed.
    fn build(&self, event: &Event, built: &[Built], keys: &MutableKeys) -> Option<Built> {
        Some(match event {
            Event::Garbage(bytes) => self.opaque(bytes.clone()),
            Event::Undersize(n) => {
                self.opaque(vec![0x5a; usize::from(*n) % MIN_FETCHED_MESSAGE_BYTES])
            }
            Event::WrongDestination { signer, content } => {
                let mut body = self.signed(*signer, *signer, content);
                body.bytes[0] ^= 0xff;
                body.prefix_ok = false;
                body.decrypts = false;
                body
            }
            Event::Honest {
                signer,
                content,
                known,
            } => {
                keys.set_known(self.signer(*signer), *known);
                self.signed(*signer, *signer, content)
            }
            Event::Forged {
                signer,
                claimed,
                content,
                known,
            } => {
                keys.set_known(self.signer(*claimed), *known);
                self.signed(*signer, *claimed, content)
            }
            Event::Flipped {
                signer,
                content,
                at,
            } => {
                let mut body = self.signed(*signer, *signer, content);
                let at = usize::from(*at) % body.bytes.len();
                body.bytes[at] ^= 0x01;
                body.prefix_ok = at >= ADDRESS_HASH_SIZE;
                body.decrypts = false;
                body.tampered = true;
                body
            }
            Event::Replay(i) => {
                let earlier = built.get(usize::from(*i) % built.len().max(1))?;
                earlier.clone()
            }
            Event::Reencrypt(i) => {
                let earlier = built.get(usize::from(*i) % built.len().max(1))?;
                let sealed = earlier.sealed.clone()?;
                Built {
                    bytes: self.seal(&sealed.wire),
                    prefix_ok: true,
                    decrypts: true,
                    tampered: false,
                    sealed: Some(sealed),
                }
            }
            Event::Advance(_) => return None,
        })
    }

    /// The first failing stage of 11.4 for `built` given the model's state at `now`.
    fn predict(
        &self,
        model: &Model,
        built: &Built,
        transient: &[u8; 32],
        source_known: bool,
        now: SystemTime,
    ) -> (BodyOutcome, Stage) {
        let len = built.bytes.len();
        if len > MAX_FETCHED_MESSAGE_BYTES {
            return (
                BodyOutcome::Discarded(Discard::Oversize { len }),
                Stage::Bounds,
            );
        }
        if len < MIN_FETCHED_MESSAGE_BYTES {
            return (
                BodyOutcome::Discarded(Discard::Undersize { len }),
                Stage::Bounds,
            );
        }
        if model.recorded.contains(transient) {
            return (
                BodyOutcome::Discarded(Discard::Duplicate),
                Stage::TransientDedup,
            );
        }
        if !built.prefix_ok {
            return (
                BodyOutcome::Discarded(Discard::Undecryptable(
                    "destination is not ours".to_string(),
                )),
                Stage::Prefix,
            );
        }
        let sealed = match &built.sealed {
            Some(sealed) if built.decrypts => sealed,
            _ => {
                return (
                    BodyOutcome::Discarded(Discard::Undecryptable(String::new())),
                    Stage::Decrypt,
                );
            }
        };
        if model.delivered.contains(&sealed.message_id) {
            return (
                BodyOutcome::Discarded(Discard::Duplicate),
                Stage::MessageDedup,
            );
        }
        if !source_known {
            let earlier = model.deferrals.get(transient).copied();
            if let Some(Sighting {
                attempts,
                first_seen,
            }) = earlier
                && attempts >= MAX_UNKNOWN_SOURCE_DEFERRALS
                && now.duration_since(first_seen).unwrap_or_default()
                    >= UNKNOWN_SOURCE_DEFERRAL_HORIZON
            {
                return (
                    BodyOutcome::Discarded(Discard::UnknownSourceBudgetSpent),
                    Stage::SourceKey,
                );
            }
            return (
                BodyOutcome::Deferred {
                    attempts: earlier.map_or(0, |s| s.attempts).saturating_add(1),
                },
                Stage::SourceKey,
            );
        }
        if !sealed.signature_valid {
            return (
                BodyOutcome::Discarded(Discard::BadSignature),
                Stage::Signature,
            );
        }
        match sealed.standing {
            SignerStanding::Unknown => (
                BodyOutcome::Discarded(Discard::UntrustedSource),
                Stage::Standing,
            ),
            SignerStanding::Blocked => (
                BodyOutcome::Discarded(Discard::BlockedSource),
                Stage::Standing,
            ),
            SignerStanding::Trusted => (BodyOutcome::Delivered, Stage::Delivered),
        }
    }

    /// Runs one body through the real pipeline and holds the outcome, the collaborator
    /// counters and the store to the model; returns what the pipeline said.
    fn run_event(&self, bench: &mut Bench, built: &Built) -> Result<BodyOutcome, String> {
        let transient: [u8; 32] = Sha256::digest(&built.bytes).into();
        let source_known = built
            .sealed
            .as_ref()
            .is_some_and(|sealed| bench.keys.is_known(&sealed.source));
        let (mut expected, mut stage) =
            self.predict(&bench.model, built, &transient, source_known, bench.now);
        let calls_before = bench.keys.calls.load(Ordering::SeqCst);
        let deliveries_before = bench.sink.delivered.lock().unwrap().len();
        let pipeline = BodyPipeline {
            recipient: &self.recipient,
            delivery: self.delivery,
            keys: &bench.keys,
            trust: &self.trust,
            sink: &bench.sink,
            node: "fuzz",
        };
        let observed = self
            .rt
            .block_on(pipeline.process(&built.bytes, &mut bench.store, bench.now))
            .map_err(|err| format!("process returned a transport error for a local body: {err}"))?;
        if built.tampered && observed == BodyOutcome::Discarded(Discard::BadSignature) {
            expected = observed.clone();
            stage = Stage::Signature;
        }
        let key_lookups = bench.keys.calls.load(Ordering::SeqCst) - calls_before;
        let deliveries = bench.sink.delivered.lock().unwrap().len() - deliveries_before;
        let matches = match (&expected, &observed) {
            (
                BodyOutcome::Discarded(Discard::Undecryptable(_)),
                BodyOutcome::Discarded(Discard::Undecryptable(_)),
            ) if stage == Stage::Decrypt => true,
            _ => expected == observed,
        };
        ensure(matches, || {
            let invariant = if observed == BodyOutcome::Delivered {
                "(ii) Delivered implies every prior stage passed"
            } else {
                "(i) the outcome is the first failing stage of 11.4"
            };
            format!("{invariant}: expected {expected:?} at stage {stage:?}, observed {observed:?}")
        })?;
        ensure(
            key_lookups == usize::from(stage >= Stage::SourceKey),
            || {
                format!(
                    "(iii) the key lookup runs exactly when stages 1-5 passed and never after a discard before it: stage {stage:?}, lookups {key_lookups}"
                )
            },
        )?;
        ensure(
            deliveries == usize::from(observed == BodyOutcome::Delivered),
            || {
                format!(
                    "(iii) the sink is reached by a delivered body once and by no discard or deferral: {observed:?}, deliveries {deliveries}"
                )
            },
        )?;
        if observed == BodyOutcome::Delivered {
            bench.delivered_outcomes += 1;
        }
        let message_id = built.sealed.as_ref().map(|sealed| sealed.message_id);
        bench
            .model
            .apply(transient, &observed, message_id, bench.now);
        ensure(
            bench.store.contains(&transient) == bench.model.recorded.contains(&transient),
            || {
                format!(
                    "MESH-PROP-028..037 Recorded column: after {observed:?} the transient id must be recorded={}",
                    bench.model.recorded.contains(&transient)
                )
            },
        )?;
        if let Some(message_id) = message_id {
            ensure(
                bench.store.was_delivered(&message_id)
                    == bench.model.delivered.contains(&message_id),
                || {
                    format!(
                        "MESH-PROP-032: the delivered set holds a message id exactly once it was delivered; after {observed:?} expected {}",
                        bench.model.delivered.contains(&message_id)
                    )
                },
            )?;
        }
        Ok(observed)
    }
}

/// The kind byte a `receipt/` corpus file starts with: one body, replayed as a `Garbage`
/// event on a fresh bench.
pub(super) const RECEIPT_BODY: u8 = 0x00;
/// The kind byte for the `Unstructured` bytes a whole `Sequence` is shaped from, which is
/// what a random-iteration violation writes.
pub(super) const RECEIPT_SEQUENCE: u8 = 0x01;

pub(super) fn check_receipt_file(fx: &ReceiptFixture, bytes: &[u8]) -> Result<(), String> {
    match bytes.split_first() {
        Some((&RECEIPT_BODY, body)) => check_receipt_body(fx, body),
        Some((&RECEIPT_SEQUENCE, rest)) => {
            let sequence =
                Sequence::arbitrary_take_rest(Unstructured::new(rest)).map_err(|err| {
                    format!("the bytes after the kind byte must shape a Sequence: {err}")
                })?;
            check_receipt_sequence(fx, sequence)
        }
        Some((other, _)) => Err(format!("unknown receipt corpus kind {other:#04x}")),
        None => Err("a receipt corpus file starts with its kind byte".to_string()),
    }
}

fn check_receipt_body(fx: &ReceiptFixture, bytes: &[u8]) -> Result<(), String> {
    let mut bench = fx.bench();
    let built = fx.opaque(bytes.to_vec());
    fx.run_event(&mut bench, &built).map(drop)
}

/// Every body of `sequence` through `bench` in order; the outcomes of the bodies that were
/// built, since a clock advance and a dangling reference build none.
fn run_sequence(
    fx: &ReceiptFixture,
    bench: &mut Bench,
    sequence: &Sequence,
) -> Result<Vec<BodyOutcome>, String> {
    let mut built: Vec<Built> = Vec::new();
    let mut outcomes = Vec::new();
    for event in &sequence.0 {
        if let Event::Advance(secs) = event {
            bench.now += Event::advance(*secs);
            continue;
        }
        let Some(body) = fx.build(event, &built, &bench.keys) else {
            continue;
        };
        let outcome = fx
            .run_event(bench, &body)
            .map_err(|what| format!("event {event:?}: {what}"))?;
        outcomes.push(outcome);
        built.push(body);
    }
    Ok(outcomes)
}

pub(super) fn check_receipt_sequence(
    fx: &ReceiptFixture,
    sequence: Sequence,
) -> Result<(), String> {
    let mut bench = fx.bench();
    run_sequence(fx, &mut bench, &sequence)?;
    let delivered = bench.sink.delivered.lock().unwrap();
    ensure(
        delivered.len() == bench.delivered_outcomes && *delivered == bench.model.delivered_log,
        || {
            format!(
                "(iv) the sink count equals the number of Delivered outcomes over the sequence: sink {}, delivered {}",
                delivered.len(),
                bench.delivered_outcomes
            )
        },
    )
}

/// MESH-PROP-034 end to end: a body whose source has no key is deferred on three sightings
/// and on a fourth still short of the heartbeat, discarded on the first sighting a
/// heartbeat after the first, and a duplicate from then on. The oracle's prediction and the
/// pipeline's verdict must agree at every step, so this also proves the model's budget leg
/// is reachable.
pub(super) fn check_receipt_deferral_budget(fx: &ReceiptFixture) -> Result<(), String> {
    let horizon = u32::try_from(UNKNOWN_SOURCE_DEFERRAL_HORIZON.as_secs()).unwrap();
    let sequence = Sequence(vec![
        Event::Honest {
            signer: Signer::C,
            content: b"no key for me yet".to_vec(),
            known: false,
        },
        Event::Replay(0),
        Event::Replay(0),
        Event::Advance(horizon - 1),
        Event::Replay(0),
        Event::Advance(1),
        Event::Replay(0),
        Event::Replay(0),
    ]);
    let mut bench = fx.bench();
    let outcomes = run_sequence(fx, &mut bench, &sequence)?;
    let expected = [
        BodyOutcome::Deferred { attempts: 1 },
        BodyOutcome::Deferred { attempts: 2 },
        BodyOutcome::Deferred { attempts: 3 },
        BodyOutcome::Deferred { attempts: 4 },
        BodyOutcome::Discarded(Discard::UnknownSourceBudgetSpent),
        BodyOutcome::Discarded(Discard::Duplicate),
    ];
    ensure(outcomes == expected, || {
        format!(
            "three sightings and a heartbeat spend the budget: expected {expected:?}, got {outcomes:?}"
        )
    })
}

// ---------------------------------------------------------------------------------------
// Codecs: every refusal is one the spec names; every accepted value re-encodes stably
// ---------------------------------------------------------------------------------------

pub(super) const TAG_CARD: u8 = 0x01;
pub(super) const TAG_BODY: u8 = 0x02;
const TAG_INTRO: u8 = 0x03;
const TAG_ANNOUNCE: u8 = 0x04;
const TAG_PEER_FIELDS: u8 = 0x05;
const TAG_KNOCK_FIELDS: u8 = 0x06;
pub(super) const TAG_REFUSAL_CODE: u8 = 0x07;
const TAG_VERSION_REFUSAL: u8 = 0x08;
const TAG_DISPATCH_ERROR: u8 = 0x09;
pub(super) const TAG_PENDING: u8 = 0x0a;
/// Every tag, for the README table check.
pub(super) const CODEC_TAGS: [u8; 10] = [
    TAG_CARD,
    TAG_BODY,
    TAG_INTRO,
    TAG_ANNOUNCE,
    TAG_PEER_FIELDS,
    TAG_KNOCK_FIELDS,
    TAG_REFUSAL_CODE,
    TAG_VERSION_REFUSAL,
    TAG_DISPATCH_ERROR,
    TAG_PENDING,
];

pub(super) struct CodecFixture {
    tmp: TempDir,
    now: SystemTime,
}

impl CodecFixture {
    pub(super) fn new() -> Self {
        Self {
            tmp: TempDir::new("fuzz-codecs"),
            now: fixed_now(),
        }
    }
}

fn tagged(tag: u8, payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(tag);
    out.extend_from_slice(&payload);
    out
}

/// One input per sub-target, the jsonl loader one time in eight since it touches the disk.
#[derive(Debug, Arbitrary)]
pub(super) struct CodecBundle {
    card: CardGen,
    body: BodyGen,
    intro: IntroGen,
    announce: AnnounceGen,
    peer_fields: FieldsGen,
    knock_fields: FieldsGen,
    refusal_code: CodeGen,
    version_refusal: VersionRefusalGen,
    dispatch_error: DispatchErrorGen,
    pending: Unlikely<JsonlGen>,
}

impl CodecBundle {
    pub(super) fn into_tagged_inputs(self) -> Vec<Vec<u8>> {
        let mut inputs = vec![
            tagged(TAG_CARD, packed(&self.card.into_value())),
            tagged(TAG_BODY, packed(&self.body.into_value())),
            tagged(TAG_INTRO, packed(&self.intro.into_value())),
            tagged(TAG_ANNOUNCE, self.announce.into_bytes()),
            tagged(TAG_PEER_FIELDS, packed(&self.peer_fields.into_value())),
            tagged(TAG_KNOCK_FIELDS, packed(&self.knock_fields.into_value())),
            tagged(TAG_REFUSAL_CODE, packed(&self.refusal_code.into_value())),
            tagged(
                TAG_VERSION_REFUSAL,
                packed(&self.version_refusal.into_value()),
            ),
            tagged(
                TAG_DISPATCH_ERROR,
                packed(&self.dispatch_error.into_value()),
            ),
        ];
        if let Some(pending) = self.pending.0 {
            inputs.push(tagged(TAG_PENDING, pending.into_text().into_bytes()));
        }
        inputs
    }
}

/// Random inputs are `packed` values under the depth clamp, so they always decode; only a
/// damaged corpus file reaches the `None` arm, and that is a violation, not a pass.
fn with_value(payload: &[u8], check: fn(&Value) -> Result<(), String>) -> Result<(), String> {
    match unpack_whole(payload) {
        Some(value) => check(&value),
        None => Err(
            "a codec payload must be exactly one msgpack value within the nesting budget; this one did not decode"
                .to_string(),
        ),
    }
}

/// A tag byte selecting the sub-target, then that decoder's wire bytes.
pub(super) fn check_codec_bytes(fx: &CodecFixture, bytes: &[u8]) -> Result<(), String> {
    let Some((tag, payload)) = bytes.split_first() else {
        return Err("a codec input starts with its sub-target tag byte".to_string());
    };
    match *tag {
        TAG_CARD => with_value(payload, check_card),
        TAG_BODY => with_value(payload, check_body),
        TAG_INTRO => with_value(payload, check_intro),
        TAG_ANNOUNCE => check_announce(payload),
        TAG_PEER_FIELDS => with_value(payload, check_peer_fields),
        TAG_KNOCK_FIELDS => with_value(payload, check_knock_fields),
        TAG_REFUSAL_CODE => with_value(payload, check_refusal_code),
        TAG_VERSION_REFUSAL => with_value(payload, check_version_refusal),
        TAG_DISPATCH_ERROR => with_value(payload, check_dispatch_error),
        TAG_PENDING => check_pending(fx, payload),
        other => Err(format!("unknown codec tag {other:#04x}")),
    }
}

// --- status card (section 9) -----------------------------------------------------------

#[derive(Debug, Arbitrary)]
struct StateGen {
    code: Likely<ByteField>,
    since_secs: Unlikely<UintField>,
    shape: Shape,
    replace: Unlikely<Scalar>,
}

#[derive(Debug, Arbitrary)]
struct RepoGen {
    name: Likely<TextField>,
    branch: Unlikely<TextField>,
    shape: Shape,
    replace: Unlikely<Scalar>,
}

#[derive(Debug, Arbitrary)]
struct PlanGen {
    title: Likely<TextField>,
    shape: Shape,
    replace: Unlikely<Scalar>,
}

#[derive(Debug, Arbitrary)]
struct TodoGen {
    goal: Unlikely<TextField>,
    done: Likely<U32Field>,
    total: Likely<U32Field>,
    shape: Shape,
    replace: Unlikely<Scalar>,
}

fn sub_map(map: Value, replace: Unlikely<Scalar>) -> Value {
    replace.0.map_or(map, Scalar::into_value)
}

#[derive(Debug, Arbitrary)]
struct CardGen {
    v: Likely<Version1>,
    display_name: Unlikely<TextField>,
    objective: Unlikely<TextField>,
    state: Likely<StateGen>,
    repo: Unlikely<RepoGen>,
    plan: Unlikely<PlanGen>,
    todo: Unlikely<TodoGen>,
    snapshot_age_secs: Unlikely<UintField>,
    served_at_secs: Likely<UintField>,
    shape: Shape,
    replace: Unlikely<ArbValue>,
}

impl CardGen {
    fn into_value(self) -> Value {
        let state = |state: StateGen| {
            let map = state.shape.map(vec![
                (
                    key("code"),
                    present(state.code, |code| code.into_value(Value::from)),
                ),
                (key("since_secs"), rare(state.since_secs, uint)),
            ]);
            sub_map(map, state.replace)
        };
        let repo = |repo: RepoGen| {
            let map = repo.shape.map(vec![
                (key("name"), present(repo.name, text)),
                (key("branch"), rare(repo.branch, text)),
            ]);
            sub_map(map, repo.replace)
        };
        let plan = |plan: PlanGen| {
            let map = plan
                .shape
                .map(vec![(key("title"), present(plan.title, text))]);
            sub_map(map, plan.replace)
        };
        let todo = |todo: TodoGen| {
            let map = todo.shape.map(vec![
                (key("goal"), rare(todo.goal, text)),
                (
                    key("done"),
                    present(todo.done, |n| n.into_value(Value::from)),
                ),
                (
                    key("total"),
                    present(todo.total, |n| n.into_value(Value::from)),
                ),
            ]);
            sub_map(map, todo.replace)
        };
        let map = self.shape.map(vec![
            (key("v"), present(self.v, version1)),
            (key("display_name"), rare(self.display_name, text)),
            (key("objective"), rare(self.objective, text)),
            (key("state"), present(self.state, state)),
            (key("repo"), rare(self.repo, repo)),
            (key("plan"), rare(self.plan, plan)),
            (key("todo"), rare(self.todo, todo)),
            (key("snapshot_age_secs"), rare(self.snapshot_age_secs, uint)),
            (key("served_at_secs"), present(self.served_at_secs, uint)),
        ]);
        or_replaced(map, self.replace)
    }
}

#[derive(Debug)]
enum CardClass {
    Malformed,
    Unsupported(u64),
    /// `v` = 1 in a map: a card or a `Malformed` refusal of some later field.
    Readable,
}

/// MESH-STATUS: not a map, or the first `v` missing, nil, not a `uint` or 0, is malformed;
/// `v` above `STATUS_CARD_VERSION` is the version refusal (MESH-CANON-012: first wins).
fn expect_card(value: &Value) -> CardClass {
    let Some(entries) = value.as_map() else {
        return CardClass::Malformed;
    };
    match first(entries, "v").and_then(Value::as_u64) {
        None | Some(0) => CardClass::Malformed,
        Some(found) if found > STATUS_CARD_VERSION => CardClass::Unsupported(found),
        Some(_) => CardClass::Readable,
    }
}

fn check_card(value: &Value) -> Result<(), String> {
    let observed = StatusCard::from_value(value);
    match (expect_card(value), &observed) {
        (CardClass::Malformed, Err(StatusError::Malformed(_))) => Ok(()),
        (
            CardClass::Unsupported(found),
            Err(StatusError::UnsupportedVersion {
                found: seen,
                supported,
            }),
        ) => ensure(*seen == found && *supported == STATUS_CARD_VERSION, || {
            format!(
                "MESH-STATUS: the version refusal names found={found} supported={STATUS_CARD_VERSION}, got {observed:?}"
            )
        }),
        (CardClass::Readable, Err(StatusError::Malformed(_))) => Ok(()),
        (CardClass::Readable, Ok(card)) => {
            let reencoded = card.to_value();
            ensure(
                StatusCard::from_value(&reencoded) == Ok(card.clone()),
                || {
                    format!(
                        "MESH-STATUS: decode(encode(card)) must equal card; card {card:?}, re-read {:?}",
                        StatusCard::from_value(&reencoded)
                    )
                },
            )?;
            ensure(packed(&reencoded) == packed(&card.to_value()), || {
                "MESH-CANON: encoding a card twice must give the same bytes".to_string()
            })?;
            no_duplicate_keys(&reencoded)
        }
        (class, observed) => Err(format!(
            "MESH-STATUS-002..004: the predicate says {class:?}, the decoder says {observed:?}"
        )),
    }
}

// --- /message body (section 10) --------------------------------------------------------

const BODY_REASONS: [&str; 9] = [
    "the body is not a map",
    "v is missing or not the supported version",
    "kind is missing or unknown",
    "id is missing, blank, too long or outside the id alphabet",
    "in_reply_to is not a message id",
    "title is not text or is too long",
    "content is missing, not text or too long",
    "fields is not a map",
    "ts is missing or not a finite number",
];

const KINDS: [&str; 4] = ["message", "ask", "reply", "bulletin"];

#[derive(Debug, Arbitrary)]
struct KindGen {
    known: u8,
    as_bin: bool,
    odd: Unlikely<Scalar>,
}

impl KindGen {
    fn into_value(self) -> Value {
        if let Some(odd) = self.odd.0 {
            return odd.into_value();
        }
        let name = KINDS[usize::from(self.known) % KINDS.len()];
        if self.as_bin {
            Value::Binary(name.as_bytes().to_vec())
        } else {
            Value::from(name)
        }
    }
}

#[derive(Debug, Arbitrary)]
struct IdGen {
    raw: String,
    len: u8,
    alphabet_only: bool,
    as_bin: bool,
    odd: Unlikely<Scalar>,
}

impl IdGen {
    fn into_value(self) -> Value {
        if let Some(odd) = self.odd.0 {
            return odd.into_value();
        }
        let mut id: String = if self.alphabet_only {
            self.raw
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
                .collect()
        } else {
            self.raw
        };
        if id.is_empty() {
            id.push_str("id");
        }
        let cap = usize::from(self.len) % 80;
        if let Some((cut, _)) = id.char_indices().nth(cap) {
            id.truncate(cut);
        }
        if self.as_bin {
            Value::Binary(id.into_bytes())
        } else {
            Value::from(id)
        }
    }
}

#[derive(Debug, Arbitrary)]
struct TsGen {
    secs: f64,
    odd: Unlikely<Scalar>,
}

impl TsGen {
    fn into_value(self) -> Value {
        self.odd.0.map_or(Value::F64(self.secs), Scalar::into_value)
    }
}

#[derive(Debug, Arbitrary)]
enum FieldsValue {
    Map(Vec<(String, ArbValue)>),
    /// Maps nested `n % 16` deep, straddling `PEER_FIELDS_MAX_DEPTH`.
    Deep(u8),
    Any(ArbValue),
}

impl FieldsValue {
    fn into_value(self) -> Value {
        match self {
            Self::Map(entries) => Value::Map(
                entries
                    .into_iter()
                    .take(MAX_CHILDREN)
                    .map(|(name, value)| (Value::from(name), value.into_value()))
                    .collect(),
            ),
            Self::Deep(n) => (0..usize::from(n) % 16).fold(Value::from(1u64), |inner, _| {
                Value::Map(vec![(key("k"), inner)])
            }),
            Self::Any(value) => value.into_value(),
        }
    }
}

#[derive(Debug, Arbitrary)]
struct BodyGen {
    v: Likely<Version1>,
    kind: Likely<KindGen>,
    id: Likely<IdGen>,
    in_reply_to: Unlikely<IdGen>,
    title: Unlikely<TextField>,
    content: Likely<TextField>,
    fields: Unlikely<FieldsValue>,
    ts: Likely<TsGen>,
    shape: Shape,
    replace: Unlikely<ArbValue>,
}

impl BodyGen {
    fn into_value(self) -> Value {
        let map = self.shape.map(vec![
            (key("v"), present(self.v, version1)),
            (key("kind"), present(self.kind, KindGen::into_value)),
            (key("id"), present(self.id, IdGen::into_value)),
            (
                key("in_reply_to"),
                rare(self.in_reply_to, IdGen::into_value),
            ),
            (key("title"), rare(self.title, text)),
            (key("content"), present(self.content, text)),
            (key("fields"), rare(self.fields, FieldsValue::into_value)),
            (key("ts"), present(self.ts, TsGen::into_value)),
        ]);
        or_replaced(map, self.replace)
    }
}

/// MESH-MSG-012: not a map first, then the first `v` (MESH-CANON-012) not equal to 1.
fn expect_body_gate(value: &Value) -> Option<&'static str> {
    let Some(entries) = value.as_map() else {
        return Some(BODY_REASONS[0]);
    };
    (first(entries, "v").and_then(Value::as_u64) != Some(PEER_WIRE_VERSION))
        .then_some(BODY_REASONS[1])
}

fn check_body(value: &Value) -> Result<(), String> {
    let gate = expect_body_gate(value);
    match from_r3_body(value) {
        Err(reason) => {
            ensure(BODY_REASONS.contains(&reason), || {
                format!("MESH-MSG-012: `{reason}` is not one of the refusals section 10 names")
            })?;
            match gate {
                Some(expected) => ensure(reason == expected, || {
                    format!(
                        "MESH-MSG-012: the first failure decides; expected `{expected}`, got `{reason}`"
                    )
                }),
                None => Ok(()),
            }
        }
        Ok(body) => {
            ensure(gate.is_none(), || {
                format!(
                    "MESH-MSG-012: a body that is not a map or not version 1 must be refused, got {body:?}"
                )
            })?;
            let peer = OutboundPeer {
                kind: body.kind,
                id: body.id.clone(),
                in_reply_to: body.in_reply_to.clone(),
                title: body.title.clone(),
                content: body.content.clone(),
                fields: body.fields.clone(),
            };
            let again = from_r3_body(&to_r3_body(&peer, body.timestamp));
            ensure(again.as_ref() == Ok(&body), || {
                format!(
                    "MESH-MSG round trip: from_r3_body(to_r3_body(body)) must equal body; got {again:?} for {body:?}"
                )
            })
        }
    }
}

// --- /knock body (section 8) -----------------------------------------------------------

#[derive(Debug, Arbitrary)]
struct IntroGen {
    intro: Likely<TextField>,
    shape: Shape,
    replace: Unlikely<ArbValue>,
}

impl IntroGen {
    fn into_value(self) -> Value {
        let map = self
            .shape
            .map(vec![(key("intro"), present(self.intro, text))]);
        or_replaced(map, self.replace)
    }
}

fn check_intro(value: &Value) -> Result<(), String> {
    let observed = intro_from_r3_body(Some(value));
    let expected = value
        .as_map()
        .and_then(|entries| {
            entries
                .iter()
                .find(|(name, _)| name.as_str() == Some("intro"))
                .and_then(|(_, intro)| intro.as_str())
        })
        .and_then(|intro| display_text(intro, KNOCK_INTRO_MAX_CHARS));
    ensure(observed == expected, || {
        format!(
            "MESH-KNOCK-001/MESH-CANON-012: the intro is the first `str` `intro` of a map, cleaned, else none; expected {expected:?}, got {observed:?}"
        )
    })?;
    let Some(intro) = observed else {
        return Ok(());
    };
    ensure(intro.chars().count() <= KNOCK_INTRO_MAX_CHARS, || {
        format!("MESH-KNOCK-003: a read intro is at most {KNOCK_INTRO_MAX_CHARS} characters")
    })?;
    ensure(
        display_text(&intro, KNOCK_INTRO_MAX_CHARS).as_deref() == Some(intro.as_str()),
        || "section 3.2: display_text is idempotent on an intro it produced".to_string(),
    )
}

// --- announce app_data (section 5.1) ---------------------------------------------------

#[derive(Debug, Arbitrary)]
enum NameGen {
    Text(String),
    Bytes(Vec<u8>),
    /// `64 + n % 4` ASCII bytes, straddling the cap.
    Long(u8),
    Empty,
    /// Valid text with one control or format character spliced in.
    Control(String, u8),
}

#[derive(Debug, Arbitrary)]
struct AnnounceGen {
    magic: Likely<()>,
    version: u16,
    name: NameGen,
}

impl AnnounceGen {
    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::new();
        match self.magic.0 {
            Some(()) => out.extend_from_slice(&ANNOUNCE_MAGIC),
            None => out.extend_from_slice(b"COYX"),
        }
        out.extend_from_slice(&self.version.to_be_bytes());
        match self.name {
            NameGen::Text(name) => out.extend_from_slice(name.as_bytes()),
            NameGen::Bytes(bytes) => out.extend_from_slice(&bytes),
            NameGen::Long(n) => out.extend(std::iter::repeat_n(
                b'a',
                MAX_DISPLAY_NAME_BYTES + usize::from(n) % 4,
            )),
            NameGen::Empty => {}
            NameGen::Control(name, which) => {
                let controls = ['\u{7}', '\u{200B}', '\u{202E}', '\u{2028}', '\u{FEFF}'];
                let mut name = name;
                name.push(controls[usize::from(which) % controls.len()]);
                out.extend_from_slice(name.as_bytes());
            }
        }
        out
    }
}

/// MESH-ANN-001 (length and magic), MESH-ANN-002 (big-endian version), MESH-ANN-003
/// (name at most 64 bytes, UTF-8, no control or invisible character), MESH-ANN-004 (empty
/// name is no name), MESH-ANN-005 (every byte from 6 on is the name).
fn expect_announce(bytes: &[u8]) -> Option<(u16, Option<&str>)> {
    if bytes.len() < ANNOUNCE_MAGIC.len() + 2 || bytes[..ANNOUNCE_MAGIC.len()] != ANNOUNCE_MAGIC {
        return None;
    }
    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    let name = &bytes[6..];
    if name.len() > MAX_DISPLAY_NAME_BYTES {
        return None;
    }
    let name = std::str::from_utf8(name).ok()?;
    if name.chars().any(is_control_or_invisible) {
        return None;
    }
    Some((version, (!name.is_empty()).then_some(name)))
}

fn check_announce(bytes: &[u8]) -> Result<(), String> {
    let observed = AnnounceAppData::decode(bytes);
    let expected = expect_announce(bytes);
    match (&expected, &observed) {
        (None, None) => Ok(()),
        (Some((version, name)), Some(decoded)) => {
            ensure(
                decoded.version == *version && decoded.display_name.as_deref() == *name,
                || {
                    format!(
                        "MESH-ANN-002..005: expected version {version} name {name:?}, decoded {decoded:?}"
                    )
                },
            )?;
            let reencoded = decoded.encode().map_err(|err| {
                format!("MESH-ANN-006: encode refused a name decode accepted: {err}")
            })?;
            ensure(reencoded == bytes, || {
                "MESH-ANN-005: encode(decode(bytes)) must be the same bytes".to_string()
            })
        }
        _ => Err(format!(
            "MESH-ANN-001..004: the predicate says {expected:?}, the decoder says {observed:?}"
        )),
    }
}

// --- LXMF custom fields (sections 8.6 and 10.8) ----------------------------------------

const PEER_REASONS: [&str; 7] = [
    "custom data is missing or not a map",
    "name_hash is missing or not binary",
    "name_hash is not 10 bytes",
    "kind is missing or unknown",
    "id is missing or blank",
    "id is too long or has characters outside the id alphabet",
    "in_reply_to is not a message id",
];

const KNOCK_REASONS: [&str; 3] = [
    "custom data is missing or not a map",
    "name_hash is missing or not binary",
    "name_hash is not 10 bytes",
];

#[derive(Debug, Arbitrary)]
enum TypeTag {
    PeerStr,
    PeerBin,
    KnockStr,
    KnockBin,
    Other(Scalar),
}

impl TypeTag {
    fn into_value(self) -> Value {
        match self {
            Self::PeerStr => Value::from(PEER_MESSAGE_TYPE),
            Self::PeerBin => Value::Binary(PEER_MESSAGE_TYPE.as_bytes().to_vec()),
            Self::KnockStr => Value::from(KNOCK_TYPE),
            Self::KnockBin => Value::Binary(KNOCK_TYPE.as_bytes().to_vec()),
            Self::Other(odd) => odd.into_value(),
        }
    }
}

#[derive(Debug, Arbitrary)]
struct DataGen {
    name_hash: Likely<NameHashGen>,
    kind: Unlikely<KindGen>,
    id: Unlikely<IdGen>,
    in_reply_to: Unlikely<IdGen>,
    fields: Unlikely<FieldsValue>,
    shape: Shape,
    replace: Unlikely<Scalar>,
}

/// An LXMF `fields` map: the custom type under `0xFB`, the custom data under `0xFC`.
#[derive(Debug, Arbitrary)]
struct FieldsGen {
    tag: Likely<TypeTag>,
    data: Likely<DataGen>,
    shape: Shape,
    replace: Unlikely<ArbValue>,
}

impl FieldsGen {
    fn into_value(self) -> Value {
        let data = |data: DataGen| {
            let map = data.shape.map(vec![
                (
                    key("name_hash"),
                    present(data.name_hash, NameHashGen::into_value),
                ),
                (key("kind"), rare(data.kind, KindGen::into_value)),
                (key("id"), rare(data.id, IdGen::into_value)),
                (
                    key("in_reply_to"),
                    rare(data.in_reply_to, IdGen::into_value),
                ),
                (key("fields"), rare(data.fields, FieldsValue::into_value)),
            ]);
            sub_map(map, data.replace)
        };
        let map = self.shape.map(vec![
            (
                Value::from(FIELD_CUSTOM_TYPE),
                present(self.tag, TypeTag::into_value),
            ),
            (Value::from(FIELD_CUSTOM_DATA), present(self.data, data)),
        ]);
        or_replaced(map, self.replace)
    }
}

/// `None` when the fields do not carry `tag` under `0xFB` as `str` or `bin` (MESH-PROP-038
/// typing); otherwise the 10-byte `name_hash` of the first `0xFC` map, when there is one
/// (MESH-KNOCK-023, MESH-MSG-052).
fn expect_typed(fields: &Value, tag: &str) -> Option<Option<[u8; NAME_HASH_LEN]>> {
    let entries = fields.as_map()?;
    let field = |number: u8| {
        entries
            .iter()
            .find(|(name, _)| name.as_u64() == Some(u64::from(number)))
            .map(|(_, value)| value)
    };
    let typed = match field(FIELD_CUSTOM_TYPE)? {
        Value::String(text) => text.as_str() == Some(tag),
        Value::Binary(bytes) => bytes == tag.as_bytes(),
        _ => false,
    };
    if !typed {
        return None;
    }
    let name_hash = field(FIELD_CUSTOM_DATA)
        .and_then(Value::as_map)
        .and_then(|data| {
            data.iter()
                .find(|(name, _)| name.as_str() == Some("name_hash"))
                .map(|(_, value)| value)
        })
        .and_then(|value| match value {
            Value::Binary(bytes) => <[u8; NAME_HASH_LEN]>::try_from(bytes.as_slice()).ok(),
            _ => None,
        });
    Some(name_hash)
}

fn inbound_with(fields: Value) -> InboundMessage {
    InboundMessage {
        transient_id: [1u8; 32],
        message_id: [2u8; 32],
        source_identity_hash: "0a".repeat(ADDRESS_HASH_SIZE),
        source_delivery_hash: "0b".repeat(ADDRESS_HASH_SIZE),
        timestamp: 1_700_000_000.0,
        title: None,
        content: Some(b"hello".to_vec()),
        fields: Some(fields),
        stamp_value: None,
    }
}

fn check_peer_fields(fields: &Value) -> Result<(), String> {
    let observed = decode_peer_lxmf(&inbound_with(fields.clone()));
    match (expect_typed(fields, PEER_MESSAGE_TYPE), &observed) {
        (None, PeerLxmf::NotAPeer) => Ok(()),
        (Some(_), PeerLxmf::Malformed(reason)) => ensure(PEER_REASONS.contains(reason), || {
            format!("MESH-MSG-049..052: `{reason}` is not a refusal section 10.8 names")
        }),
        (Some(Some(expected)), PeerLxmf::Peer { name_hash, .. }) => {
            ensure(*name_hash == expected, || {
                "MESH-MSG-052: the peer's name_hash is the 10-byte bin under the custom data"
                    .to_string()
            })
        }
        (expected, observed) => Err(format!(
            "MESH-PROP-038/MESH-MSG-052: typed={expected:?} by the predicate, decoded as {observed:?}"
        )),
    }
}

fn check_knock_fields(fields: &Value) -> Result<(), String> {
    let observed = decode_knock_message(&inbound_with(fields.clone()));
    match (expect_typed(fields, KNOCK_TYPE), &observed) {
        (None, KnockMessage::NotAKnock) => Ok(()),
        (Some(_), KnockMessage::Malformed(reason)) => {
            ensure(KNOCK_REASONS.contains(reason), || {
                format!("MESH-KNOCK-022/023: `{reason}` is not a refusal section 8.6 names")
            })
        }
        (Some(Some(expected)), KnockMessage::Knock { name_hash, .. }) => {
            ensure(*name_hash == expected, || {
                "MESH-KNOCK-023: the knock's name_hash is the 10-byte bin under the custom data"
                    .to_string()
            })
        }
        (expected, observed) => Err(format!(
            "MESH-PROP-038/MESH-KNOCK-023: typed={expected:?} by the predicate, decoded as {observed:?}"
        )),
    }
}

// --- client-side decoders: refusal code, version refusal, dispatch error ---------------

const REFUSAL_CODES: [u64; 8] = [0xf0, 0xf1, 0xf3, 0xf4, 0xf5, 0xf6, 0xfd, 0xfe];

#[derive(Debug, Arbitrary)]
enum CodeGen {
    Known(u8),
    Byte(u8),
    Uint(u64),
    Any(ArbValue),
}

impl CodeGen {
    fn into_value(self) -> Value {
        match self {
            Self::Known(i) => Value::from(REFUSAL_CODES[usize::from(i) % REFUSAL_CODES.len()]),
            Self::Byte(n) => Value::from(n),
            Self::Uint(n) => Value::from(n),
            Self::Any(value) => value.into_value(),
        }
    }
}

fn check_refusal_code(value: &Value) -> Result<(), String> {
    let observed = RefusalCode::from_wire(value);
    let expected = value.as_u64().filter(|code| REFUSAL_CODES.contains(code));
    match (expected, observed) {
        (None, None) => Ok(()),
        (Some(code), Some(refusal)) => ensure(
            refusal.to_wire() == Value::from(code)
                && RefusalCode::from_wire(&refusal.to_wire()) == Some(refusal),
            || {
                format!(
                    "MESH-ENV-044: to_wire must give back the bare uint {code:#x} and read as the same code"
                )
            },
        ),
        _ => Err(format!(
            "MESH-ENV-046/047 and MESH-EXT-004: a uint among the eight codes and nothing else is a refusal; {value} read as {observed:?}"
        )),
    }
}

#[derive(Debug, Arbitrary)]
enum RefusalTag {
    Right,
    Other(Scalar),
}

#[derive(Debug, Arbitrary)]
enum FoundGen {
    Nil,
    Version(u16),
    Odd(Scalar),
}

#[derive(Debug, Arbitrary)]
struct VersionRefusalGen {
    refusal: Likely<RefusalTag>,
    found: Likely<FoundGen>,
    min: Likely<U16Field>,
    max: Likely<U16Field>,
    shape: Shape,
    replace: Unlikely<ArbValue>,
}

impl VersionRefusalGen {
    fn into_value(self) -> Value {
        let map = self.shape.map(vec![
            (
                key("refusal"),
                present(self.refusal, |tag| match tag {
                    RefusalTag::Right => Value::from("unsupported_version"),
                    RefusalTag::Other(odd) => odd.into_value(),
                }),
            ),
            (
                key("found"),
                present(self.found, |found| match found {
                    FoundGen::Nil => Value::Nil,
                    FoundGen::Version(v) => Value::from(v),
                    FoundGen::Odd(odd) => odd.into_value(),
                }),
            ),
            (key("min"), present(self.min, |n| n.into_value(Value::from))),
            (key("max"), present(self.max, |n| n.into_value(Value::from))),
        ]);
        or_replaced(map, self.replace)
    }
}

fn u16_of(value: &Value) -> Option<u16> {
    u16::try_from(value.as_u64()?).ok()
}

/// Section 7 as a predicate: a map whose first `refusal` is the string, first `found` is
/// nil or a `uint` fitting u16 (MESH-VER-007), first `min` and `max` `uint`s fitting u16
/// (MESH-VER-008, MESH-VER-009); anything looser is a body (MESH-VER-006).
fn expect_version_refusal(value: &Value) -> Option<VersionRefusal> {
    let entries = value.as_map()?;
    let field = |name: &str| {
        entries
            .iter()
            .find(|(name_value, _)| name_value.as_str() == Some(name))
            .map(|(_, value)| value)
    };
    if field("refusal")?.as_str()? != "unsupported_version" {
        return None;
    }
    let found = match field("found")? {
        Value::Nil => None,
        other => Some(u16_of(other)?),
    };
    Some(VersionRefusal {
        found,
        min: u16_of(field("min")?)?,
        max: u16_of(field("max")?)?,
    })
}

fn check_version_refusal(value: &Value) -> Result<(), String> {
    let observed = VersionRefusal::from_value(value);
    let expected = expect_version_refusal(value);
    ensure(observed == expected, || {
        format!("MESH-VER-006..009: the predicate reads {expected:?}, the decoder {observed:?}")
    })?;
    match observed {
        Some(refusal) => ensure(
            VersionRefusal::from_value(&refusal.to_value()) == Some(refusal.clone()),
            || {
                format!(
                    "section 7 round trip: from_value(to_value(r)) must equal r for {refusal:?}"
                )
            },
        ),
        None => Ok(()),
    }
}

#[derive(Debug, Arbitrary)]
enum ErrorTag {
    UnknownPath,
    NoProvider,
    Other(Scalar),
}

#[derive(Debug, Arbitrary)]
enum PathHashGen {
    Hex([u8; ADDRESS_HASH_SIZE]),
    Upper([u8; ADDRESS_HASH_SIZE]),
    Short([u8; ADDRESS_HASH_SIZE - 1]),
    Long([u8; ADDRESS_HASH_SIZE + 1]),
    Odd(Scalar),
}

#[derive(Debug, Arbitrary)]
enum PathTextGen {
    Status,
    Message,
    Knock,
    Other(String),
    Odd(Scalar),
}

#[derive(Debug, Arbitrary)]
struct DispatchErrorGen {
    error: Likely<ErrorTag>,
    path_hash: Likely<PathHashGen>,
    path: Likely<PathTextGen>,
    shape: Shape,
    replace: Unlikely<ArbValue>,
}

impl DispatchErrorGen {
    fn into_value(self) -> Value {
        let map = self.shape.map(vec![
            (
                key("error"),
                present(self.error, |tag| match tag {
                    ErrorTag::UnknownPath => Value::from("unknown_path"),
                    ErrorTag::NoProvider => Value::from("no_provider"),
                    ErrorTag::Other(odd) => odd.into_value(),
                }),
            ),
            (
                key("path_hash"),
                present(self.path_hash, |hash| match hash {
                    PathHashGen::Hex(bytes) => Value::from(hex_lower(&bytes)),
                    PathHashGen::Upper(bytes) => {
                        Value::from(hex_lower(&bytes).to_ascii_uppercase())
                    }
                    PathHashGen::Short(bytes) => Value::from(hex_lower(&bytes)),
                    PathHashGen::Long(bytes) => Value::from(hex_lower(&bytes)),
                    PathHashGen::Odd(odd) => odd.into_value(),
                }),
            ),
            (
                key("path"),
                present(self.path, |path| match path {
                    PathTextGen::Status => Value::from(STATUS_PATH),
                    PathTextGen::Message => Value::from(MESSAGE_PATH),
                    PathTextGen::Knock => Value::from(KNOCK_PATH),
                    PathTextGen::Other(path) => Value::from(path),
                    PathTextGen::Odd(odd) => odd.into_value(),
                }),
            ),
        ]);
        or_replaced(map, self.replace)
    }
}

/// MESH-ENV-040 (`error` a `str` naming one of the two kinds), MESH-ENV-041 (`path_hash`
/// exactly 32 hex digits), MESH-ENV-042 (`path` one of the three known paths); first
/// occurrence wins (MESH-CANON-012).
fn expect_dispatch_error(value: &Value) -> Option<DispatchError> {
    let entries = value.as_map()?;
    let field = |name: &str| {
        entries
            .iter()
            .find(|(name_value, _)| name_value.as_str() == Some(name))
            .and_then(|(_, value)| value.as_str())
    };
    match field("error")? {
        "unknown_path" => {
            let path_hash = field("path_hash")?;
            let well_formed = path_hash.len() == 2 * ADDRESS_HASH_SIZE
                && path_hash.bytes().all(|b| b.is_ascii_hexdigit());
            well_formed.then(|| DispatchError::UnknownPath {
                path_hash: path_hash.to_string(),
            })
        }
        "no_provider" => {
            let path = field("path")?;
            [KNOCK_PATH, STATUS_PATH, MESSAGE_PATH]
                .contains(&path)
                .then(|| DispatchError::NoProvider {
                    path: path.to_string(),
                })
        }
        _ => None,
    }
}

fn check_dispatch_error(value: &Value) -> Result<(), String> {
    let observed = DispatchError::from_value(value);
    let expected = expect_dispatch_error(value);
    ensure(observed == expected, || {
        format!("MESH-ENV-040..042: the predicate reads {expected:?}, the decoder {observed:?}")
    })?;
    match observed {
        Some(error) => ensure(
            DispatchError::from_value(&error.to_value()) == Some(error.clone()),
            || {
                format!(
                    "section 6.7 round trip: from_value(to_value(e)) must equal e for {error:?}"
                )
            },
        ),
        None => Ok(()),
    }
}

// --- pending question store, jsonl (section 10.5) --------------------------------------

const MAX_LINES: usize = 24;

#[derive(Debug, Arbitrary)]
struct RecordGen {
    id: u32,
    question: String,
    /// Seconds before `FIXED_NOW_SECS`, modulo twice the TTL so half the records have expired.
    age_secs: u32,
    timeout_secs: u16,
    answered: bool,
    with_reply: bool,
}

impl RecordGen {
    fn record(self) -> PendingRecord {
        let age = u64::from(self.age_secs) % (2 * PENDING_TTL.as_secs());
        let sent_at = UNIX_EPOCH + Duration::from_secs(FIXED_NOW_SECS - age);
        let id = format!("q{:08x}", self.id);
        let reply = self.with_reply.then(|| PeerMessage {
            source_identity: "0a".repeat(ADDRESS_HASH_SIZE),
            source_destination: "0b".repeat(ADDRESS_HASH_SIZE),
            destination: "0c".repeat(ADDRESS_HASH_SIZE),
            title: None,
            content: "answer".to_string(),
            fields: None,
            timestamp: 1_700_000_000.0,
            message_id: format!("r{:08x}", self.id),
            in_reply_to: Some(id.clone()),
            kind: PeerKind::Reply,
            via: PeerVia::Direct,
        });
        PendingRecord {
            version: PENDING_RECORD_VERSION,
            id,
            peer_destination: "0b".repeat(ADDRESS_HASH_SIZE),
            peer_identity: "0a".repeat(ADDRESS_HASH_SIZE),
            question: self.question.chars().take(40).collect(),
            sent_at: rfc3339_utc(sent_at),
            timeout_at: rfc3339_utc(sent_at + Duration::from_secs(u64::from(self.timeout_secs))),
            state: if self.answered {
                PendingState::Answered
            } else {
                PendingState::Open
            },
            reply,
        }
    }
}

#[derive(Debug, Arbitrary)]
enum LineGen {
    Record(RecordGen),
    Blank,
    Garbage(String),
    Version(RecordGen, u8),
    Stamp(RecordGen, String),
    Missing(RecordGen, u8),
    Unknown(RecordGen, String),
}

impl LineGen {
    fn into_line(self) -> String {
        let json = |record: RecordGen| serde_json::to_value(record.record()).unwrap();
        let object = match self {
            Self::Record(record) => json(record),
            Self::Blank => return "  ".to_string(),
            Self::Garbage(text) => return text.replace(['\n', '\r'], " "),
            Self::Version(record, version) => {
                let mut object = json(record);
                object["version"] = serde_json::Value::from(version);
                object
            }
            Self::Stamp(record, stamp) => {
                let mut object = json(record);
                object["sent_at"] = serde_json::Value::from(stamp);
                object
            }
            Self::Missing(record, which) => {
                let mut object = json(record);
                if let Some(map) = object.as_object_mut() {
                    let keys: Vec<String> = map.keys().cloned().collect();
                    if let Some(name) = keys.get(usize::from(which) % keys.len().max(1)) {
                        map.remove(name);
                    }
                }
                object
            }
            Self::Unknown(record, name) => {
                let mut object = json(record);
                object[name] = serde_json::Value::from(42);
                object
            }
        };
        serde_json::to_string(&object).unwrap()
    }
}

#[derive(Debug, Arbitrary)]
struct JsonlGen {
    lines: Vec<LineGen>,
}

impl JsonlGen {
    fn into_text(self) -> String {
        let mut text = String::new();
        for line in self.lines.into_iter().take(MAX_LINES) {
            text.push_str(&line.into_line());
            text.push('\n');
        }
        text
    }
}

/// The file rule of `read_jsonl`: every non-blank line is a version-1 pending record with
/// an RFC 3339 `sent_at`, or the whole file is refused naming the first line that is not.
fn expect_pending(text: &str) -> Result<Vec<PendingRecord>, usize> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str::<PendingRecord>(line)
                .ok()
                .filter(|record| {
                    record.version == PENDING_RECORD_VERSION
                        && parse_rfc3339(&record.sent_at).is_some()
                })
                .ok_or(index + 1)
        })
        .collect()
}

fn check_pending(fx: &CodecFixture, payload: &[u8]) -> Result<(), String> {
    let text = String::from_utf8_lossy(payload);
    let store = PendingStore::new(&fx.tmp.path, "fuzz");
    fs::create_dir_all(store.path().parent().unwrap()).unwrap();
    fs::write(store.path(), text.as_bytes()).unwrap();
    let observed = store.load_pending(fx.now);
    match (expect_pending(&text), observed) {
        (Err(line), Err(err)) => ensure(
            format!("{err:#}").contains(&format!("line {line} ")),
            || format!("the refusal must name the first bad line ({line}): {err:#}"),
        ),
        (Ok(records), Ok(loaded)) => ensure(
            loaded.len() <= records.len()
                && loaded.iter().all(|record| {
                    record.version == PENDING_RECORD_VERSION
                        && (record.state == PendingState::Open || record.reply.is_some())
                        && records.contains(record)
                }),
            || {
                format!(
                    "load_pending returns a subset of the file's version-1 records, each open or carrying its reply; file {}, loaded {}",
                    records.len(),
                    loaded.len()
                )
            },
        ),
        (Err(line), Ok(loaded)) => Err(format!(
            "the loader accepted a file whose line {line} is not a version-1 pending record with an RFC 3339 sent_at; loaded {}",
            loaded.len()
        )),
        (Ok(records), Err(err)) => Err(format!(
            "the loader refused a file of {} valid records: {err:#}",
            records.len()
        )),
    }
}
