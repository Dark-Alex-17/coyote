//! Vectors for the request layer: the Envelope, the dispatcher's stages, refusal codes and
//! the version refusal, run in-process against this crate's decoders and `Dispatcher`.
//!
//! Every row carries a descriptive name and the receiver action expected of it. A row
//! written faithfully from the spec that the code does not honour is kept as written and
//! flagged with `known_divergence`; the executor prints such a row instead of asserting
//! it, and fails when the flag goes stale.

use super::{Kind, Listed};
use crate::mesh::knock::{KnockSurface, RecordingSurface};
use crate::mesh::peers::{PEER_TTL, PeerSighting, PeerTable};
use crate::mesh::protocol::{
    MESH_PROTOCOL_MIN_SUPPORTED, MESH_PROTOCOL_VERSION, VersionRefusal, protocol_supported,
};
use crate::mesh::request::{
    ACCESS_PATH, Admission, DispatchError, Dispatcher, Envelope, EnvelopeError, FETCH_PATH,
    HANDLER_TIMEOUT, Handler, InboundRequest, KNOCK_PATH, KnockEvent, KnockSink, LIST_PATH,
    MAX_CONCURRENT_INBOUND_REQUESTS, MAX_REQUEST_PAYLOAD_BYTES, MESSAGE_PATH, NAME_HASH_LEN,
    OriginName, PathHash, RefusalCode, Reply, RequestHandler, RequestId, ResponseFrame,
    STATUS_PATH,
};
use crate::mesh::test_support::{TempDir, TrustList};
use crate::mesh::trust::{Decision, InstancePresence, KeyChangeOutcome, Rule, TrustStore, Verdict};
use crate::mesh::{destination_address, hex_lower};

use async_trait::async_trait;
use rmpv::Value;
use rns_transport::destination::link::LinkId;
use rns_transport::hash::AddressHash;
use rns_transport::identity::PrivateIdentity;
use rns_transport::rand_core::OsRng;
use std::fmt::Debug;
use std::future::Future;
use std::io::Cursor;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One named input and the receiver action expected of it.
struct Vector {
    name: &'static str,
    kind: Kind,
    case: Case,
    /// Set when the row is faithful to the spec and the code does not honour it; the
    /// executor prints the observed behaviour instead of asserting. Never set to make a
    /// row pass.
    known_divergence: Option<&'static str>,
}

/// The input and expectation of a row, one variant per decoder under test. The variant
/// name is the family label `all_listed` carries for the row.
enum Case {
    /// `Envelope::from_value`.
    EnvelopeDecode {
        value: Value,
        expect: EnvelopeAction,
    },
    /// `Envelope::new(..).into_value()`, the sender's side of the Envelope, compared value
    /// for value so key order is checked.
    EnvelopeEncode {
        origin: OriginName,
        body: Value,
        expect: Value,
    },
    /// `RequestHandler::handle` on a `Dispatcher`, the dispatcher's stages, with the
    /// knock the sink must (or must not) see.
    Dispatch {
        path: &'static str,
        requester: Requester,
        trust: TrustShape,
        provider: Provider,
        data: Value,
        expect: Answer,
        knock: Knock,
    },
    /// `VersionRefusal::from_value`.
    VersionRefusalDecode {
        value: Value,
        expect: Option<VersionRefusal>,
    },
    /// `VersionRefusal::current(found).to_value()`, compared value for value so key order
    /// is checked.
    VersionRefusalEncode { found: Option<u16>, expect: Value },
    /// `DispatchError::from_value`.
    DispatchErrorDecode {
        value: Value,
        expect: Option<DispatchError>,
    },
    /// `RefusalCode::from_wire`; the expectation is the wire form of the code
    /// read, so no code is named here.
    RefusalCodeDecode { value: Value, expect: Option<Value> },
    /// A check with no single decoder seam: constants, precedence tables, byte-level checks.
    Custom(fn() -> Result<(), String>),
}

impl Case {
    fn family(&self) -> &'static str {
        match self {
            Case::EnvelopeDecode { .. } => "EnvelopeDecode",
            Case::EnvelopeEncode { .. } => "EnvelopeEncode",
            Case::Dispatch { .. } => "Dispatch",
            Case::VersionRefusalDecode { .. } => "VersionRefusalDecode",
            Case::VersionRefusalEncode { .. } => "VersionRefusalEncode",
            Case::DispatchErrorDecode { .. } => "DispatchErrorDecode",
            Case::RefusalCodeDecode { .. } => "RefusalCodeDecode",
            Case::Custom(_) => "Custom",
        }
    }
}

enum EnvelopeAction {
    Accepted {
        version: u16,
        origin: OriginName,
        body: Value,
    },
    Refused(EnvelopeError),
}

enum Requester {
    Anonymous,
    Identified,
}

/// Builds the trust list for a row from the requester's identity hash and the destination
/// hash derived for it; both are only known once the identity is minted.
type TrustShape = fn(identity: &str, destination: &str) -> TrustList;

enum Provider {
    None,
    Echo(&'static str),
    Silent(&'static str),
}

/// `Reply` with the refusal code in its wire form, so rows compare without naming a code.
#[derive(Debug, PartialEq)]
enum Answer {
    Value(Value),
    Code(Value),
    Silent,
}

#[derive(Debug, PartialEq)]
enum Knock {
    None,
    Filed { with_data: bool },
}

pub(super) fn listed() -> Vec<Listed> {
    vectors()
        .iter()
        .map(|vector| Listed {
            name: vector.name,
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
        Case::EnvelopeDecode { value, expect } => {
            match (Envelope::from_value(value.clone()), expect) {
                (
                    Ok(envelope),
                    EnvelopeAction::Accepted {
                        version,
                        origin,
                        body,
                    },
                ) => {
                    same("version", envelope.version, *version)?;
                    same("origin", envelope.origin, *origin)?;
                    same("body", &envelope.body, body)
                }
                (Ok(envelope), EnvelopeAction::Refused(error)) => Err(format!(
                    "expected {error:?}, observed an accepted envelope with body {:?}",
                    envelope.body
                )),
                (Err(observed), EnvelopeAction::Refused(error)) => {
                    same("error", observed, error.clone())
                }
                (Err(observed), EnvelopeAction::Accepted { .. }) => Err(format!(
                    "expected an accepted envelope, observed {observed:?}"
                )),
            }
        }
        Case::EnvelopeEncode {
            origin,
            body,
            expect,
        } => same(
            "encoded envelope",
            Envelope::new(*origin, body.clone()).into_value(),
            expect.clone(),
        ),
        Case::Dispatch {
            path,
            requester,
            trust,
            provider,
            data,
            expect,
            knock,
        } => block_on(run_dispatch(
            path,
            requester,
            *trust,
            provider,
            data.clone(),
            expect,
            knock,
        )),
        Case::VersionRefusalDecode { value, expect } => same(
            "version refusal",
            VersionRefusal::from_value(value),
            expect.clone(),
        ),
        Case::VersionRefusalEncode { found, expect } => same(
            "encoded version refusal",
            VersionRefusal::current(*found).to_value(),
            expect.clone(),
        ),
        Case::DispatchErrorDecode { value, expect } => same(
            "dispatch error",
            DispatchError::from_value(value),
            expect.clone(),
        ),
        Case::RefusalCodeDecode { value, expect } => same(
            "refusal code",
            RefusalCode::from_wire(value).map(RefusalCode::to_wire),
            expect.clone(),
        ),
        Case::Custom(check) => check(),
    }
}

async fn run_dispatch(
    path: &str,
    requester: &Requester,
    trust: TrustShape,
    provider: &Provider,
    data: Value,
    expect: &Answer,
    knock: &Knock,
) -> Result<(), String> {
    let identity = *PrivateIdentity::new_from_rand(OsRng).as_identity();
    let identity_hex = identity.address_hash.to_hex_string();
    let destination_hex = destination_address(&ORIGIN, &identity.address_hash).to_hex_string();
    let (store, _tmp) = trust(&identity_hex, &destination_hex).open("conformance-env");
    let sink = Arc::new(SpySink::default());
    let dispatcher = Dispatcher::new(store, sink.clone());
    match provider {
        Provider::None => {}
        Provider::Echo(at) => {
            dispatcher
                .register(at, Arc::new(Scripted::Echo))
                .map_err(|error| format!("register {at}: {error:?}"))?;
        }
        Provider::Silent(at) => {
            dispatcher
                .register(at, Arc::new(Scripted::Silent))
                .map_err(|error| format!("register {at}: {error:?}"))?;
        }
    }
    let identity = match requester {
        Requester::Anonymous => None,
        Requester::Identified => Some(identity),
    };
    let request = InboundRequest {
        link_id: LinkId::new_from_rand(OsRng),
        identity,
        request_id: RequestId::from([1u8; 16]),
        path_hash: PathHash::of(path),
        data,
    };
    let observed = match RequestHandler::handle(&dispatcher, request).await {
        Reply::Value(value) | Reply::Settled { value, .. } => Answer::Value(value),
        Reply::Code(code) => Answer::Code(code.to_wire()),
        Reply::Silent => Answer::Silent,
    };
    same("reply", &observed, expect)?;
    let knocks = sink.0.lock().unwrap();
    match knock {
        Knock::None => ensure(
            knocks.is_empty(),
            format!("expected no knock, observed {} knock(s)", knocks.len()),
        ),
        Knock::Filed { with_data } => {
            let [record] = knocks.as_slice() else {
                return Err(format!("expected one knock, observed {}", knocks.len()));
            };
            same(
                "knock",
                record,
                &KnockRecord {
                    path_hash: PathHash::of(path),
                    name_hash: hex_lower(&ORIGIN),
                    destination_hash: destination_hex,
                    identity_hash: identity_hex,
                    with_data: *with_data,
                },
            )
        }
    }
}

// ---------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------

const ORIGIN: [u8; NAME_HASH_LEN] = [7; NAME_HASH_LEN];
const UNKNOWN_PATH: &str = "/nowhere";

fn row(name: &'static str, kind: Kind, case: Case) -> Vector {
    Vector {
        name,
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

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// The refusal every dispatcher refusal carries, read off its wire byte so the code is
/// named at its one production site only.
fn no_access() -> RefusalCode {
    RefusalCode::from_wire(&Value::from(0xf1u8)).unwrap()
}

fn envelope_value() -> Value {
    Envelope::new(OriginName(ORIGIN), Value::from("body")).into_value()
}

#[derive(Debug, PartialEq)]
struct KnockRecord {
    path_hash: PathHash,
    name_hash: String,
    destination_hash: String,
    identity_hash: String,
    with_data: bool,
}

#[derive(Default)]
struct SpySink(Mutex<Vec<KnockRecord>>);

impl KnockSink for SpySink {
    fn knock(&self, knock: KnockEvent) {
        self.0.lock().unwrap().push(KnockRecord {
            path_hash: knock.path_hash,
            name_hash: knock.name_hash,
            destination_hash: knock.destination_hash,
            identity_hash: knock.identity_hash,
            with_data: knock.data.is_some(),
        });
    }
}

enum Scripted {
    Echo,
    Silent,
}

#[async_trait]
impl Handler for Scripted {
    async fn handle(&self, request: crate::mesh::request::AdmittedRequest) -> Reply {
        match self {
            Scripted::Echo => Reply::Value(request.body),
            Scripted::Silent => Reply::Silent,
        }
    }
}

fn vectors() -> Vec<Vector> {
    let mut all = Vec::new();
    all.extend(envelope_vectors());
    all.extend(dispatch_vectors());
    all.extend(custom_vectors());
    all.extend(version_refusal_vectors());
    all.extend(dispatch_error_vectors());
    all.extend(refusal_code_vectors());
    all
}

// ---------------------------------------------------------------------------------------
// The Envelope
// ---------------------------------------------------------------------------------------

fn accepted(body: Value) -> EnvelopeAction {
    EnvelopeAction::Accepted {
        version: MESH_PROTOCOL_VERSION,
        origin: OriginName(ORIGIN),
        body,
    }
}

fn unsupported(found: Option<u16>) -> EnvelopeAction {
    EnvelopeAction::Refused(EnvelopeError::UnsupportedVersion { found })
}

fn malformed() -> EnvelopeAction {
    EnvelopeAction::Refused(EnvelopeError::Malformed)
}

fn origin_bin(len: usize) -> Value {
    Value::Binary(vec![7; len])
}

fn envelope(name: &'static str, kind: Kind, value: Value, expect: EnvelopeAction) -> Vector {
    row(name, kind, Case::EnvelopeDecode { value, expect })
}

fn encode(name: &'static str, kind: Kind, body: Value, expect: Value) -> Vector {
    row(
        name,
        kind,
        Case::EnvelopeEncode {
            origin: OriginName(ORIGIN),
            body,
            expect,
        },
    )
}

fn envelope_vectors() -> Vec<Vector> {
    let body = Value::from("body");
    let expected = map(vec![
        ("v", Value::from(1u64)),
        ("name_hash", origin_bin(10)),
        ("body", body.clone()),
    ]);
    let Value::Map(baseline) = envelope_value() else {
        unreachable!("an envelope encodes as a map")
    };
    let numeric_key = Value::Map([vec![(Value::from(1u64), Value::Nil)], baseline].concat());
    vec![
        envelope(
            "envelope-round-trips",
            Kind::Valid,
            envelope_value(),
            accepted(body.clone()),
        ),
        envelope(
            "envelope-with-v-1-is-accepted",
            Kind::Boundary,
            set(envelope_value(), "v", Value::from(1u64)),
            accepted(body.clone()),
        ),
        envelope(
            "envelope-without-v-is-unsupported",
            Kind::Invalid,
            without(envelope_value(), "v"),
            unsupported(None),
        ),
        envelope(
            "envelope-v-as-string-is-unsupported",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from("1")),
            unsupported(None),
        ),
        envelope(
            "envelope-v-as-float-is-unsupported",
            Kind::Invalid,
            set(envelope_value(), "v", Value::F64(1.0)),
            unsupported(None),
        ),
        envelope(
            "envelope-v-as-nil-is-unsupported",
            Kind::Invalid,
            set(envelope_value(), "v", Value::Nil),
            unsupported(None),
        ),
        envelope(
            "envelope-v-past-u16-is-unsupported",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(65536u64)),
            unsupported(None),
        ),
        envelope(
            "envelope-v-0-is-unsupported",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(0u64)),
            unsupported(Some(0)),
        ),
        envelope(
            "envelope-v-2-is-unsupported",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(2u64)),
            unsupported(Some(2)),
        ),
        envelope(
            "envelope-name-hash-of-10-bytes-is-accepted",
            Kind::Boundary,
            set(envelope_value(), "name_hash", origin_bin(10)),
            accepted(body.clone()),
        ),
        envelope(
            "envelope-without-name-hash-is-malformed",
            Kind::Invalid,
            without(envelope_value(), "name_hash"),
            malformed(),
        ),
        envelope(
            "envelope-name-hash-as-hex-string-is-malformed",
            Kind::Invalid,
            set(
                envelope_value(),
                "name_hash",
                Value::from(hex_lower(&ORIGIN)),
            ),
            malformed(),
        ),
        envelope(
            "envelope-name-hash-as-nil-is-malformed",
            Kind::Invalid,
            set(envelope_value(), "name_hash", Value::Nil),
            malformed(),
        ),
        envelope(
            "envelope-name-hash-as-array-is-malformed",
            Kind::Invalid,
            set(
                envelope_value(),
                "name_hash",
                Value::Array(vec![Value::from(7u8); 10]),
            ),
            malformed(),
        ),
        envelope(
            "envelope-name-hash-of-9-bytes-is-malformed",
            Kind::Invalid,
            set(envelope_value(), "name_hash", origin_bin(9)),
            malformed(),
        ),
        envelope(
            "envelope-name-hash-of-11-bytes-is-malformed",
            Kind::Invalid,
            set(envelope_value(), "name_hash", origin_bin(11)),
            malformed(),
        ),
        envelope(
            "envelope-empty-name-hash-is-malformed",
            Kind::Invalid,
            set(envelope_value(), "name_hash", origin_bin(0)),
            malformed(),
        ),
        envelope(
            "envelope-name-hash-of-16-bytes-is-malformed",
            Kind::Invalid,
            set(envelope_value(), "name_hash", origin_bin(16)),
            malformed(),
        ),
        envelope(
            "envelope-with-nil-body-is-accepted",
            Kind::Valid,
            set(envelope_value(), "body", Value::Nil),
            accepted(Value::Nil),
        ),
        envelope(
            "envelope-with-empty-map-body-is-accepted",
            Kind::Valid,
            set(envelope_value(), "body", map(vec![])),
            accepted(map(vec![])),
        ),
        envelope(
            "envelope-without-body-is-malformed",
            Kind::Invalid,
            without(envelope_value(), "body"),
            malformed(),
        ),
        envelope(
            "envelope-unknown-keys-are-ignored",
            Kind::Valid,
            with(envelope_value(), "extra", Value::from("ignored")),
            accepted(body.clone()),
        ),
        envelope(
            "envelope-spelled-out-version-key-is-ignored",
            Kind::Valid,
            with(envelope_value(), "version", Value::from(9u64)),
            accepted(body.clone()),
        ),
        envelope(
            "envelope-integer-key-is-ignored",
            Kind::Valid,
            numeric_key,
            accepted(body.clone()),
        ),
        envelope(
            "envelope-duplicate-v-last-wins-as-unsupported",
            Kind::Invalid,
            with(envelope_value(), "v", Value::from(2u64)),
            unsupported(Some(2)),
        ),
        envelope(
            "envelope-duplicate-v-last-wins-as-accepted",
            Kind::Valid,
            with(
                set(envelope_value(), "v", Value::from(2u64)),
                "v",
                Value::from(1u64),
            ),
            accepted(body.clone()),
        ),
        envelope(
            "envelope-duplicate-name-hash-last-wins-as-accepted",
            Kind::Valid,
            with(
                set(envelope_value(), "name_hash", Value::from("bad")),
                "name_hash",
                origin_bin(10),
            ),
            accepted(body.clone()),
        ),
        envelope(
            "envelope-duplicate-name-hash-last-wins-as-malformed",
            Kind::Invalid,
            with(envelope_value(), "name_hash", origin_bin(9)),
            malformed(),
        ),
        envelope(
            "envelope-duplicate-body-last-wins",
            Kind::Valid,
            with(envelope_value(), "body", Value::from("last")),
            accepted(Value::from("last")),
        ),
        envelope(
            "envelope-version-is-checked-before-name-hash",
            Kind::Invalid,
            set(
                set(envelope_value(), "v", Value::from(2u64)),
                "name_hash",
                origin_bin(3),
            ),
            unsupported(Some(2)),
        ),
        envelope(
            "envelope-version-is-checked-before-body",
            Kind::Invalid,
            without(without(envelope_value(), "v"), "body"),
            unsupported(None),
        ),
        envelope(
            "envelope-with-only-a-string-v-is-unsupported",
            Kind::Invalid,
            map(vec![("v", Value::from("x"))]),
            unsupported(None),
        ),
        envelope(
            "envelope-as-empty-map-is-unsupported",
            Kind::Invalid,
            map(vec![]),
            unsupported(None),
        ),
        envelope(
            "envelope-as-nil-is-malformed",
            Kind::Invalid,
            Value::Nil,
            malformed(),
        ),
        envelope(
            "envelope-as-string-is-malformed",
            Kind::Invalid,
            Value::from("body"),
            malformed(),
        ),
        envelope(
            "envelope-as-array-is-malformed",
            Kind::Invalid,
            Value::Array(vec![Value::from(2u64), origin_bin(10), body.clone()]),
            malformed(),
        ),
        envelope(
            "envelope-wrapped-in-a-bin-is-malformed",
            Kind::Invalid,
            Value::Binary(packed(&envelope_value())),
            malformed(),
        ),
        envelope(
            "envelope-as-integer-is-malformed",
            Kind::Invalid,
            Value::from(1u64),
            malformed(),
        ),
        envelope(
            "envelope-name-hash-as-integer-is-malformed",
            Kind::Invalid,
            set(envelope_value(), "name_hash", Value::from(7u64)),
            malformed(),
        ),
        envelope(
            "envelope-v-65535-is-unsupported-with-found-65535",
            Kind::Boundary,
            set(envelope_value(), "v", Value::from(65535u64)),
            unsupported(Some(65535)),
        ),
        envelope(
            "envelope-v-65536-is-unsupported-with-no-found",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(65536u64)),
            unsupported(None),
        ),
        envelope(
            "envelope-v-u64-max-is-unsupported",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(u64::MAX)),
            unsupported(None),
        ),
        envelope(
            "envelope-negative-v-is-unsupported",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(-1i64)),
            unsupported(None),
        ),
        envelope(
            "envelope-at-the-minimum-supported-version-is-accepted",
            Kind::Boundary,
            set(
                envelope_value(),
                "v",
                Value::from(u64::from(MESH_PROTOCOL_MIN_SUPPORTED)),
            ),
            accepted(body.clone()),
        ),
        envelope(
            "envelope-at-the-current-version-is-accepted",
            Kind::Boundary,
            set(
                envelope_value(),
                "v",
                Value::from(u64::from(MESH_PROTOCOL_VERSION)),
            ),
            accepted(body.clone()),
        ),
        envelope(
            "envelope-below-the-minimum-supported-version-is-unsupported",
            Kind::Invalid,
            set(
                envelope_value(),
                "v",
                Value::from(u64::from(MESH_PROTOCOL_MIN_SUPPORTED) - 1),
            ),
            unsupported(Some(MESH_PROTOCOL_MIN_SUPPORTED - 1)),
        ),
        envelope(
            "envelope-above-the-current-version-is-unsupported",
            Kind::Invalid,
            set(
                envelope_value(),
                "v",
                Value::from(u64::from(MESH_PROTOCOL_VERSION) + 1),
            ),
            unsupported(Some(MESH_PROTOCOL_VERSION + 1)),
        ),
        envelope(
            "envelope-missing-v-reports-no-found-version",
            Kind::Invalid,
            without(envelope_value(), "v"),
            unsupported(None),
        ),
        envelope(
            "envelope-of-only-name-hash-and-body-is-unsupported",
            Kind::Invalid,
            map(vec![("name_hash", origin_bin(10)), ("body", body.clone())]),
            unsupported(None),
        ),
        envelope(
            "envelope-without-lowercase-v-is-unsupported",
            Kind::Invalid,
            without(envelope_value(), "v"),
            unsupported(None),
        ),
        envelope(
            "envelope-uppercase-v-key-is-unsupported",
            Kind::Invalid,
            with(without(envelope_value(), "v"), "V", Value::from(1u64)),
            unsupported(None),
        ),
        encode(
            "envelope-encode-emits-v-name-hash-body",
            Kind::Valid,
            body.clone(),
            expected.clone(),
        ),
        encode(
            "envelope-encode-carries-a-nil-body",
            Kind::Valid,
            Value::Nil,
            set(expected.clone(), "body", Value::Nil),
        ),
        encode(
            "envelope-encode-carries-a-map-body",
            Kind::Valid,
            map(vec![("k", Value::from(1u64))]),
            set(
                expected.clone(),
                "body",
                map(vec![("k", Value::from(1u64))]),
            ),
        ),
        encode(
            "envelope-encode-stamps-the-current-version",
            Kind::Valid,
            body.clone(),
            map(vec![
                ("v", Value::from(u64::from(MESH_PROTOCOL_VERSION))),
                ("name_hash", origin_bin(10)),
                ("body", body),
            ]),
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// The dispatcher's stages
// ---------------------------------------------------------------------------------------

fn unknown(_: &str, _: &str) -> TrustList {
    TrustList::default()
}

fn trusted(identity: &str, _: &str) -> TrustList {
    TrustList::default().identity(identity, true)
}

fn blocked(identity: &str, _: &str) -> TrustList {
    TrustList::default().block(identity)
}

fn trusted_then_blocked(identity: &str, _: &str) -> TrustList {
    TrustList::default()
        .identity(identity, true)
        .block(identity)
}

fn trusted_for_destination(identity: &str, destination: &str) -> TrustList {
    TrustList::default().destination(destination, identity)
}

fn trusted_for_other_destination(identity: &str, _: &str) -> TrustList {
    TrustList::default().destination(OTHER_DESTINATION, identity)
}

fn trusted_but_denied(identity: &str, destination: &str) -> TrustList {
    TrustList::default()
        .identity(identity, true)
        .deny(destination)
}

fn known_not_trusted(identity: &str, _: &str) -> TrustList {
    TrustList::default().identity(identity, false)
}

/// The requester is known while another identity holds the destination that `ORIGIN`
/// derives under the holder's own identity: the requester's instance has been seen under a
/// new key.
fn known_while_another_key_holds_the_instance(identity: &str, _: &str) -> TrustList {
    let holder = PrivateIdentity::new_from_rand(OsRng)
        .as_identity()
        .address_hash;
    TrustList::default().identity(identity, false).destination(
        &destination_address(&ORIGIN, &holder).to_hex_string(),
        &holder.to_hex_string(),
    )
}

const OTHER_DESTINATION: &str = "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2";

fn no_access_answer() -> Answer {
    Answer::Code(no_access().to_wire())
}

fn version_refused(found: Option<u16>) -> Answer {
    Answer::Value(VersionRefusal::current(found).to_value())
}

fn no_provider_answer(path: &str) -> Answer {
    Answer::Value(
        DispatchError::NoProvider {
            path: path.to_string(),
        }
        .to_value(),
    )
}

fn knocked() -> Knock {
    Knock::Filed { with_data: false }
}

fn knocked_with_intro() -> Knock {
    Knock::Filed { with_data: true }
}

fn bad_version() -> Value {
    set(envelope_value(), "v", Value::from(2u64))
}

fn dispatch(
    name: &'static str,
    kind: Kind,
    path: &'static str,
    trust: TrustShape,
    data: Value,
    expect: Answer,
    knock: Knock,
) -> Vector {
    row(
        name,
        kind,
        Case::Dispatch {
            path,
            requester: Requester::Identified,
            trust,
            provider: Provider::None,
            data,
            expect,
            knock,
        },
    )
}

/// `vector` with a handler registered before the request arrives.
fn served(provider: Provider, mut vector: Vector) -> Vector {
    let Case::Dispatch { provider: slot, .. } = &mut vector.case else {
        unreachable!("only dispatch rows register handlers")
    };
    *slot = provider;
    vector
}

fn anonymous(name: &'static str, path: &'static str, trust: TrustShape, data: Value) -> Vector {
    row(
        name,
        Kind::Invalid,
        Case::Dispatch {
            path,
            requester: Requester::Anonymous,
            trust,
            provider: Provider::None,
            data,
            expect: Answer::Silent,
            knock: Knock::None,
        },
    )
}

fn dispatch_vectors() -> Vec<Vector> {
    let body = Value::from("body");
    let garbage = set(
        set(bad_version(), "name_hash", Value::from("x")),
        "body",
        Value::Nil,
    );
    let claimed = with(
        envelope_value(),
        "destination_hash",
        Value::from(OTHER_DESTINATION),
    );
    vec![
        anonymous(
            "anonymous-knock-is-silent",
            KNOCK_PATH,
            trusted,
            envelope_value(),
        ),
        anonymous(
            "anonymous-status-request-is-silent",
            STATUS_PATH,
            trusted,
            envelope_value(),
        ),
        anonymous(
            "anonymous-message-from-a-stranger-is-silent",
            MESSAGE_PATH,
            unknown,
            envelope_value(),
        ),
        anonymous(
            "anonymous-knock-files-no-knock",
            KNOCK_PATH,
            trusted,
            envelope_value(),
        ),
        anonymous(
            "anonymous-wrong-version-on-an-unknown-path-is-silent",
            UNKNOWN_PATH,
            trusted,
            bad_version(),
        ),
        anonymous(
            "anonymous-nil-body-on-status-is-silent",
            STATUS_PATH,
            trusted,
            Value::Nil,
        ),
        dispatch(
            "knock-from-an-unknown-identity-is-silent",
            Kind::Invalid,
            KNOCK_PATH,
            unknown,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "nil-body-from-an-unknown-identity-is-silent",
            Kind::Invalid,
            STATUS_PATH,
            unknown,
            Value::Nil,
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "knock-from-a-blocked-identity-is-silent",
            Kind::Invalid,
            KNOCK_PATH,
            blocked,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "string-body-from-a-blocked-identity-is-silent",
            Kind::Invalid,
            MESSAGE_PATH,
            blocked,
            Value::from("not a map"),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "wrong-version-unknown-path-from-a-blocked-identity-is-silent",
            Kind::Invalid,
            UNKNOWN_PATH,
            blocked,
            bad_version(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "wrong-version-knock-from-an-unknown-identity-is-silent",
            Kind::Invalid,
            KNOCK_PATH,
            unknown,
            bad_version(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "knock-with-v-2-from-a-trusted-identity-is-version-refused",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            bad_version(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "status-request-with-v-2-is-version-refused",
            Kind::Invalid,
            STATUS_PATH,
            trusted,
            bad_version(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "message-without-v-is-version-refused-with-no-found",
            Kind::Invalid,
            MESSAGE_PATH,
            trusted,
            without(envelope_value(), "v"),
            version_refused(None),
            Knock::None,
        ),
        dispatch(
            "wrong-version-on-an-unknown-path-is-version-refused",
            Kind::Invalid,
            UNKNOWN_PATH,
            trusted,
            bad_version(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "knock-without-v-from-a-known-identity-is-version-refused",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            without(envelope_value(), "v"),
            version_refused(None),
            Knock::None,
        ),
        dispatch(
            "knock-with-v-0-is-version-refused-with-found-0",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            set(envelope_value(), "v", Value::from(0u64)),
            version_refused(Some(0)),
            Knock::None,
        ),
        dispatch(
            "knock-with-v-past-u16-is-version-refused-with-no-found",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            set(envelope_value(), "v", Value::from(65536u64)),
            version_refused(None),
            Knock::None,
        ),
        dispatch(
            "knock-with-v-as-string-is-version-refused",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            set(envelope_value(), "v", Value::from("1")),
            version_refused(None),
            Knock::None,
        ),
        dispatch(
            "nil-body-knock-from-a-trusted-identity-is-refused-no-access",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            Value::Nil,
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "string-body-status-request-is-refused-no-access",
            Kind::Invalid,
            STATUS_PATH,
            trusted,
            Value::from("body"),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "array-body-message-is-refused-no-access",
            Kind::Invalid,
            MESSAGE_PATH,
            trusted,
            Value::Array(vec![Value::from(1u64), origin_bin(10), body.clone()]),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "knock-with-a-9-byte-name-hash-is-refused-no-access",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            set(envelope_value(), "name_hash", origin_bin(9)),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "knock-without-body-from-a-trusted-identity-is-refused-no-access",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            without(envelope_value(), "body"),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "knock-without-name-hash-from-a-known-identity-is-refused-no-access",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            without(envelope_value(), "name_hash"),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "knock-with-an-11-byte-name-hash-is-refused-no-access",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            set(envelope_value(), "name_hash", origin_bin(11)),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "knock-without-body-from-a-known-identity-is-refused-no-access",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            without(envelope_value(), "body"),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "nil-body-knock-from-a-known-identity-is-refused-no-access",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            Value::Nil,
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "array-body-knock-from-a-known-identity-is-refused-no-access",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            Value::Array(vec![Value::from(2u64)]),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "status-request-with-an-unknown-key-reports-no-provider",
            Kind::Valid,
            STATUS_PATH,
            trusted,
            with(envelope_value(), "extra", Value::Nil),
            no_provider_answer(STATUS_PATH),
            Knock::None,
        ),
        dispatch(
            "status-request-with-duplicate-v-last-wins-and-reports-no-provider",
            Kind::Valid,
            STATUS_PATH,
            trusted,
            with(
                set(envelope_value(), "v", Value::from(2u64)),
                "v",
                Value::from(1u64),
            ),
            no_provider_answer(STATUS_PATH),
            Knock::None,
        ),
        dispatch(
            "garbage-envelope-is-version-refused-before-anything-else",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            garbage.clone(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "status-request-trusted-for-the-destination-reports-no-provider",
            Kind::Valid,
            STATUS_PATH,
            trusted_for_destination,
            envelope_value(),
            no_provider_answer(STATUS_PATH),
            Knock::None,
        ),
        dispatch(
            "unknown-path-trusted-for-the-destination-is-silent",
            Kind::Invalid,
            UNKNOWN_PATH,
            trusted_for_destination,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "knock-trusted-for-another-destination-is-refused-and-knocks-with-intro",
            Kind::Invalid,
            KNOCK_PATH,
            trusted_for_other_destination,
            envelope_value(),
            no_access_answer(),
            knocked_with_intro(),
        ),
        dispatch(
            "knock-claiming-another-destination-is-refused-and-knocks-with-intro",
            Kind::Invalid,
            KNOCK_PATH,
            trusted_for_other_destination,
            claimed.clone(),
            no_access_answer(),
            knocked_with_intro(),
        ),
        dispatch(
            "status-request-claiming-another-destination-is-refused-and-knocks",
            Kind::Invalid,
            STATUS_PATH,
            trusted_for_other_destination,
            claimed.clone(),
            no_access_answer(),
            knocked(),
        ),
        dispatch(
            "status-request-with-a-destination-key-is-refused-and-knocks",
            Kind::Invalid,
            STATUS_PATH,
            trusted_for_other_destination,
            with(
                envelope_value(),
                "destination",
                Value::from(OTHER_DESTINATION),
            ),
            no_access_answer(),
            knocked(),
        ),
        dispatch(
            "knock-from-a-trusted-then-blocked-identity-is-silent",
            Kind::Invalid,
            KNOCK_PATH,
            trusted_then_blocked,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "status-request-from-a-trusted-then-blocked-identity-is-silent",
            Kind::Invalid,
            STATUS_PATH,
            trusted_then_blocked,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "knock-from-an-untrusted-identity-is-refused-and-knocks-with-intro",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            envelope_value(),
            no_access_answer(),
            knocked_with_intro(),
        ),
        dispatch(
            "nil-body-knock-from-an-untrusted-identity-knocks-with-intro",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            set(envelope_value(), "body", Value::Nil),
            no_access_answer(),
            knocked_with_intro(),
        ),
        dispatch(
            "status-request-from-an-untrusted-identity-is-refused-and-knocks",
            Kind::Invalid,
            STATUS_PATH,
            known_not_trusted,
            envelope_value(),
            no_access_answer(),
            knocked(),
        ),
        dispatch(
            "message-from-an-untrusted-identity-is-refused-and-knocks",
            Kind::Invalid,
            MESSAGE_PATH,
            known_not_trusted,
            envelope_value(),
            no_access_answer(),
            knocked(),
        ),
        dispatch(
            "unknown-path-from-an-untrusted-identity-is-refused-and-knocks",
            Kind::Invalid,
            UNKNOWN_PATH,
            known_not_trusted,
            envelope_value(),
            no_access_answer(),
            knocked(),
        ),
        dispatch(
            "knock-denied-the-destination-is-refused-no-access",
            Kind::Invalid,
            KNOCK_PATH,
            trusted_but_denied,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "status-request-denied-the-destination-is-refused-no-access",
            Kind::Invalid,
            STATUS_PATH,
            trusted_but_denied,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "unknown-path-denied-the-destination-is-refused-no-access",
            Kind::Invalid,
            UNKNOWN_PATH,
            trusted_but_denied,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "knock-while-another-key-holds-the-instance-is-refused-no-access",
            Kind::Invalid,
            KNOCK_PATH,
            known_while_another_key_holds_the_instance,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "status-request-while-another-key-holds-the-instance-is-refused-no-access",
            Kind::Invalid,
            STATUS_PATH,
            known_while_another_key_holds_the_instance,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "message-while-another-key-holds-the-instance-is-refused-no-access",
            Kind::Invalid,
            MESSAGE_PATH,
            known_while_another_key_holds_the_instance,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "unknown-path-while-another-key-holds-the-instance-is-refused-no-access",
            Kind::Invalid,
            UNKNOWN_PATH,
            known_while_another_key_holds_the_instance,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "unknown-path-from-a-trusted-identity-is-silent",
            Kind::Invalid,
            UNKNOWN_PATH,
            trusted,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "status-path-with-a-trailing-slash-is-silent",
            Kind::Invalid,
            "/status/",
            trusted,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "status-request-without-a-provider-reports-no-provider",
            Kind::Valid,
            STATUS_PATH,
            trusted,
            envelope_value(),
            no_provider_answer(STATUS_PATH),
            Knock::None,
        ),
        dispatch(
            "message-without-a-provider-reports-no-provider",
            Kind::Valid,
            MESSAGE_PATH,
            trusted,
            envelope_value(),
            no_provider_answer(MESSAGE_PATH),
            Knock::None,
        ),
        dispatch(
            "list-request-without-a-provider-reports-no-provider",
            Kind::Valid,
            LIST_PATH,
            trusted,
            envelope_value(),
            no_provider_answer(LIST_PATH),
            Knock::None,
        ),
        dispatch(
            "fetch-request-without-a-provider-reports-no-provider",
            Kind::Valid,
            FETCH_PATH,
            trusted,
            envelope_value(),
            no_provider_answer(FETCH_PATH),
            Knock::None,
        ),
        dispatch(
            "access-request-without-a-provider-reports-no-provider",
            Kind::Valid,
            ACCESS_PATH,
            trusted,
            envelope_value(),
            no_provider_answer(ACCESS_PATH),
            Knock::None,
        ),
        dispatch(
            "knock-from-a-trusted-identity-is-answered-nil",
            Kind::Valid,
            KNOCK_PATH,
            trusted,
            envelope_value(),
            Answer::Value(Value::Nil),
            Knock::None,
        ),
        served(
            Provider::Echo(MESSAGE_PATH),
            dispatch(
                "message-with-an-echo-provider-is-answered-with-its-body",
                Kind::Valid,
                MESSAGE_PATH,
                trusted,
                envelope_value(),
                Answer::Value(body.clone()),
                Knock::None,
            ),
        ),
        served(
            Provider::Echo(STATUS_PATH),
            dispatch(
                "status-request-with-an-echo-provider-is-answered-with-its-map-body",
                Kind::Valid,
                STATUS_PATH,
                trusted,
                set(envelope_value(), "body", map(vec![("k", Value::Nil)])),
                Answer::Value(map(vec![("k", Value::Nil)])),
                Knock::None,
            ),
        ),
        served(
            Provider::Silent(MESSAGE_PATH),
            dispatch(
                "message-with-a-silent-provider-is-silent",
                Kind::Valid,
                MESSAGE_PATH,
                trusted,
                envelope_value(),
                Answer::Silent,
                Knock::None,
            ),
        ),
        served(
            Provider::Echo(MESSAGE_PATH),
            dispatch(
                "status-request-with-only-a-message-provider-reports-no-provider",
                Kind::Valid,
                STATUS_PATH,
                trusted,
                envelope_value(),
                no_provider_answer(STATUS_PATH),
                Knock::None,
            ),
        ),
        served(
            Provider::Echo(MESSAGE_PATH),
            dispatch(
                "message-from-an-untrusted-identity-never-reaches-the-provider",
                Kind::Invalid,
                MESSAGE_PATH,
                known_not_trusted,
                envelope_value(),
                no_access_answer(),
                knocked(),
            ),
        ),
        served(
            Provider::Echo(LIST_PATH),
            dispatch(
                "list-request-with-an-echo-provider-is-answered",
                Kind::Valid,
                LIST_PATH,
                trusted,
                envelope_value(),
                Answer::Value(body.clone()),
                Knock::None,
            ),
        ),
        served(
            Provider::Echo(FETCH_PATH),
            dispatch(
                "fetch-request-with-an-echo-provider-is-answered",
                Kind::Valid,
                FETCH_PATH,
                trusted,
                envelope_value(),
                Answer::Value(body.clone()),
                Knock::None,
            ),
        ),
        served(
            Provider::Echo(ACCESS_PATH),
            dispatch(
                "access-request-with-an-echo-provider-is-answered",
                Kind::Valid,
                ACCESS_PATH,
                trusted,
                envelope_value(),
                Answer::Value(body.clone()),
                Knock::None,
            ),
        ),
        dispatch(
            "wrong-version-knock-from-a-blocked-identity-is-silent",
            Kind::Invalid,
            KNOCK_PATH,
            blocked,
            bad_version(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "garbage-knock-from-a-trusted-identity-is-version-refused",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            garbage.clone(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "garbage-status-request-is-version-refused",
            Kind::Invalid,
            STATUS_PATH,
            trusted,
            garbage.clone(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "garbage-message-is-version-refused",
            Kind::Invalid,
            MESSAGE_PATH,
            trusted,
            garbage.clone(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "garbage-on-an-unknown-path-is-version-refused",
            Kind::Invalid,
            UNKNOWN_PATH,
            trusted,
            garbage,
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "wrong-version-knock-from-an-untrusted-identity-is-version-refused",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            bad_version(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "wrong-version-knock-denied-the-destination-is-version-refused",
            Kind::Invalid,
            KNOCK_PATH,
            trusted_but_denied,
            bad_version(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "knock-without-v-from-a-trusted-identity-is-version-refused",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            without(envelope_value(), "v"),
            version_refused(None),
            Knock::None,
        ),
        dispatch(
            "status-request-with-v-65535-is-version-refused-with-found-65535",
            Kind::Boundary,
            STATUS_PATH,
            trusted,
            set(envelope_value(), "v", Value::from(65535u64)),
            version_refused(Some(65535)),
            Knock::None,
        ),
    ]
}

fn verdict(decision: Decision, rule: Rule) -> Verdict {
    Verdict { decision, rule }
}

/// `TrustStore::authorize` for one identity against the destination the row's list names.
fn authorize(list: TrustList) -> Verdict {
    let (store, _tmp) = list.open("conformance-env");
    store.authorize(IDENTITY, DESTINATION)
}

/// `TrustStore::authorize_origin` for a fresh identity naming `ORIGIN` while a second fresh
/// identity holds the destination `ORIGIN` derives under it. `list` receives the requester's
/// identity and the destination `ORIGIN` derives under it, then the holder's identity and
/// destination. Collision protection is off.
fn authorize_rotated_origin(list: fn(&str, &str, &str, &str) -> TrustList) -> Verdict {
    authorize_rotated_origin_with(false, list)
}

/// `authorize_rotated_origin` with `mesh.collision_protection` set to `protection`.
fn authorize_rotated_origin_with(
    protection: bool,
    list: fn(&str, &str, &str, &str) -> TrustList,
) -> Verdict {
    let identity = PrivateIdentity::new_from_rand(OsRng)
        .as_identity()
        .address_hash;
    let derived = destination_address(&ORIGIN, &identity).to_hex_string();
    let holder = PrivateIdentity::new_from_rand(OsRng)
        .as_identity()
        .address_hash;
    let held = destination_address(&ORIGIN, &holder).to_hex_string();
    let (store, _tmp) = list(
        &identity.to_hex_string(),
        &derived,
        &holder.to_hex_string(),
        &held,
    )
    .open("conformance-env");
    store.set_collision_protection(protection);
    store.authorize_origin(&identity, &ORIGIN).verdict
}

/// `authorize_rotated_origin_with` for the presence rung: no record carries `ORIGIN`, the
/// peer table has heard it under the holder's identity instead. `list` receives the same
/// four arguments. The requester is judged at the instant the holder's row was heard.
fn authorize_presence_origin_with(
    protection: bool,
    list: fn(&str, &str, &str, &str) -> TrustList,
) -> Verdict {
    judge_presence_origin(protection, list, |store, identity, _, heard| {
        store.authorize_origin_at(identity, &ORIGIN, heard).verdict
    })
}

/// `authorize_presence_origin_with` with the judging left to `judge`, which receives the
/// store, the requester's identity, the holder's identity and the instant the holder's row
/// was heard. A surface is attached, so an owner line the judge earns arms the memory.
fn judge_presence_origin(
    protection: bool,
    list: fn(&str, &str, &str, &str) -> TrustList,
    judge: fn(&TrustStore, &AddressHash, &AddressHash, SystemTime) -> Verdict,
) -> Verdict {
    let identity = PrivateIdentity::new_from_rand(OsRng)
        .as_identity()
        .address_hash;
    let derived = destination_address(&ORIGIN, &identity).to_hex_string();
    let holder = PrivateIdentity::new_from_rand(OsRng)
        .as_identity()
        .address_hash;
    let held = destination_address(&ORIGIN, &holder).to_hex_string();
    let (store, _tmp) = list(
        &identity.to_hex_string(),
        &derived,
        &holder.to_hex_string(),
        &held,
    )
    .open("conformance-env");
    store.set_collision_protection(protection);
    let heard = UNIX_EPOCH + Duration::from_secs(2_000);
    let peers = TempDir::new("conformance-env-peers");
    let table = Arc::new(PeerTable::load(peers.path.join("peers.json"), heard).unwrap());
    store.attach_presence(Arc::downgrade(&table) as Weak<dyn InstancePresence>);
    let surface = Arc::new(RecordingSurface::default());
    store.attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
    table.observe(
        PeerSighting {
            destination_hash: held,
            identity_hash: holder.to_hex_string(),
            name_hash: hex_lower(&ORIGIN),
            display_name: None,
            protocol_version: MESH_PROTOCOL_VERSION,
            hops: 1,
        },
        heard,
    );
    judge(&store, &identity, &holder, heard)
}

/// Judges the requester at `heard` and tells the owner as every ingress does; the verdict
/// arms the instance's presence memo when it is a presence refusal, the line only spends
/// its dedupe.
fn surface_presence_line(store: &TrustStore, identity: &AddressHash, heard: SystemTime) {
    let verdict = store.authorize_origin_at(identity, &ORIGIN, heard).verdict;
    let outcome = match verdict.decision {
        Decision::Allow => KeyChangeOutcome::Served,
        Decision::Refuse => KeyChangeOutcome::Refused,
    };
    store.note_key_change(
        &identity.to_hex_string(),
        &hex_lower(&ORIGIN),
        outcome,
        heard,
    );
}

const REQUEST_ID: [u8; 16] = [1; 16];
const IDENTITY: &str = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";
const DESTINATION: &str = "d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1";

fn custom(name: &'static str, kind: Kind, check: fn() -> Result<(), String>) -> Vector {
    row(name, kind, Case::Custom(check))
}

fn custom_vectors() -> Vec<Vector> {
    vec![
        custom("max-request-payload-is-256-kib", Kind::Boundary, || {
            same(
                "MAX_REQUEST_PAYLOAD_BYTES",
                MAX_REQUEST_PAYLOAD_BYTES,
                262_144,
            )
        }),
        custom(
            "max-concurrent-inbound-requests-is-16",
            Kind::Boundary,
            || {
                same(
                    "MAX_CONCURRENT_INBOUND_REQUESTS",
                    MAX_CONCURRENT_INBOUND_REQUESTS,
                    16,
                )
            },
        ),
        custom("handler-timeout-is-20-seconds", Kind::Boundary, || {
            same("HANDLER_TIMEOUT", HANDLER_TIMEOUT, Duration::from_secs(20))
        }),
        custom(
            "anonymous-link-is-dropped-at-admission",
            Kind::Invalid,
            || {
                let (store, _tmp) = TrustList::default().open("conformance-env");
                let dispatcher = Dispatcher::new(store, Arc::new(SpySink::default()));
                let admission = dispatcher.admit(LinkId::new_from_rand(OsRng), None);
                ensure(
                    matches!(admission, Admission::Drop),
                    "an anonymous link was admitted",
                )
            },
        ),
        custom("trusted-identity-is-admitted", Kind::Valid, || {
            let identity = *PrivateIdentity::new_from_rand(OsRng).as_identity();
            let (store, _tmp) = TrustList::default()
                .identity(&identity.address_hash.to_hex_string(), true)
                .open("conformance-env");
            let dispatcher = Dispatcher::new(store, Arc::new(SpySink::default()));
            let admission = dispatcher.admit(LinkId::new_from_rand(OsRng), Some(&identity));
            ensure(
                matches!(admission, Admission::Admit),
                "a trusted identity was dropped",
            )
        }),
        custom(
            "blocked-identity-is-dropped-at-admission",
            Kind::Invalid,
            || {
                let identity = *PrivateIdentity::new_from_rand(OsRng).as_identity();
                let (store, _tmp) = TrustList::default()
                    .block(&identity.address_hash.to_hex_string())
                    .open("conformance-env");
                let dispatcher = Dispatcher::new(store, Arc::new(SpySink::default()));
                let admission = dispatcher.admit(LinkId::new_from_rand(OsRng), Some(&identity));
                ensure(
                    matches!(admission, Admission::Drop),
                    "a blocked identity was admitted",
                )
            },
        ),
        custom(
            "no-access-response-frame-pins-its-bytes",
            Kind::Valid,
            || {
                let frame = ResponseFrame::new(RequestId::from(REQUEST_ID), no_access().to_wire());
                same(
                    "refusal bytes",
                    frame.encode(),
                    [
                        vec![0x92, 0xc4, 0x10],
                        REQUEST_ID.to_vec(),
                        vec![0xcc, 0xf1],
                    ]
                    .concat(),
                )
            },
        ),
        custom("trust-deny-wins-over-block", Kind::Valid, || {
            same(
                "deny over block",
                authorize(TrustList::default().block(IDENTITY).deny(DESTINATION)),
                verdict(Decision::Refuse, Rule::DestinationDenied),
            )
        }),
        custom(
            "trust-deny-wins-over-destination-allow",
            Kind::Valid,
            || {
                same(
                    "deny over destination allow",
                    authorize(
                        TrustList::default()
                            .destination(DESTINATION, IDENTITY)
                            .deny(DESTINATION),
                    ),
                    verdict(Decision::Refuse, Rule::DestinationDenied),
                )
            },
        ),
        custom("trust-deny-wins-over-identity-allow", Kind::Valid, || {
            same(
                "deny over identity allow",
                authorize(
                    TrustList::default()
                        .identity(IDENTITY, true)
                        .deny(DESTINATION),
                ),
                verdict(Decision::Refuse, Rule::DestinationDenied),
            )
        }),
        custom(
            "trust-block-wins-over-destination-allow",
            Kind::Valid,
            || {
                same(
                    "block over destination allow",
                    authorize(
                        TrustList::default()
                            .destination(DESTINATION, IDENTITY)
                            .block(IDENTITY),
                    ),
                    verdict(Decision::Refuse, Rule::IdentityBlocked),
                )
            },
        ),
        custom("trust-block-wins-over-identity-allow", Kind::Valid, || {
            same(
                "block over identity allow",
                authorize(
                    TrustList::default()
                        .identity(IDENTITY, true)
                        .block(IDENTITY),
                ),
                verdict(Decision::Refuse, Rule::IdentityBlocked),
            )
        }),
        custom(
            "trust-destination-allow-wins-over-identity-allow",
            Kind::Valid,
            || {
                same(
                    "destination allow over identity allow",
                    authorize(
                        TrustList::default()
                            .identity(IDENTITY, true)
                            .destination(DESTINATION, IDENTITY),
                    ),
                    verdict(Decision::Allow, Rule::DestinationTrusted),
                )
            },
        ),
        custom("trust-destination-allow-alone-allows", Kind::Valid, || {
            same(
                "destination allow alone",
                authorize(TrustList::default().destination(DESTINATION, IDENTITY)),
                verdict(Decision::Allow, Rule::DestinationTrusted),
            )
        }),
        custom("trust-identity-allow-alone-allows", Kind::Valid, || {
            same(
                "identity allow alone",
                authorize(TrustList::default().identity(IDENTITY, true)),
                verdict(Decision::Allow, Rule::IdentityTrusted),
            )
        }),
        custom(
            "trust-identity-allow-wins-over-default-closed",
            Kind::Valid,
            || {
                same(
                    "identity allow over default closed",
                    authorize(
                        TrustList::default()
                            .identity(IDENTITY, true)
                            .destination(OTHER_DESTINATION, IDENTITY),
                    ),
                    verdict(Decision::Allow, Rule::IdentityTrusted),
                )
            },
        ),
        custom("trust-empty-list-is-default-closed", Kind::Invalid, || {
            same(
                "default closed",
                authorize(TrustList::default()),
                verdict(Decision::Refuse, Rule::DefaultClosed),
            )
        }),
        custom(
            "trust-allow-for-another-destination-is-default-closed",
            Kind::Invalid,
            || {
                same(
                    "default closed for another destination",
                    authorize(TrustList::default().destination(OTHER_DESTINATION, IDENTITY)),
                    verdict(Decision::Refuse, Rule::DefaultClosed),
                )
            },
        ),
        custom(
            "trust-identity-without-all-destinations-is-default-closed",
            Kind::Invalid,
            || {
                same(
                    "default closed without all_destinations",
                    authorize(TrustList::default().identity(IDENTITY, false)),
                    verdict(Decision::Refuse, Rule::DefaultClosed),
                )
            },
        ),
        custom("trust-deny-wins-over-identity-changed", Kind::Valid, || {
            same(
                "deny over identity changed",
                authorize_rotated_origin(|_, derived, holder, held| {
                    TrustList::default().deny(derived).destination(held, holder)
                }),
                verdict(Decision::Refuse, Rule::DestinationDenied),
            )
        }),
        custom(
            "trust-block-wins-over-identity-changed",
            Kind::Valid,
            || {
                same(
                    "block over identity changed",
                    authorize_rotated_origin(|identity, _, holder, held| {
                        TrustList::default()
                            .block(identity)
                            .destination(held, holder)
                    }),
                    verdict(Decision::Refuse, Rule::IdentityBlocked),
                )
            },
        ),
        custom(
            "trust-destination-allow-wins-over-identity-changed",
            Kind::Valid,
            || {
                same(
                    "destination allow over identity changed",
                    authorize_rotated_origin(|identity, derived, holder, held| {
                        TrustList::default()
                            .destination(derived, identity)
                            .destination(held, holder)
                    }),
                    verdict(Decision::Allow, Rule::DestinationTrusted),
                )
            },
        ),
        custom(
            "trust-identity-allow-wins-over-identity-changed",
            Kind::Valid,
            || {
                same(
                    "identity allow over identity changed",
                    authorize_rotated_origin(|identity, _, holder, held| {
                        TrustList::default()
                            .identity(identity, true)
                            .destination(held, holder)
                    }),
                    verdict(Decision::Allow, Rule::IdentityTrusted),
                )
            },
        ),
        custom(
            "trust-identity-changed-wins-over-default-closed",
            Kind::Valid,
            || {
                same(
                    "identity changed over default closed",
                    authorize_rotated_origin(|identity, _, holder, held| {
                        TrustList::default()
                            .identity(identity, false)
                            .destination(held, holder)
                    }),
                    verdict(Decision::Refuse, Rule::IdentityChanged),
                )
            },
        ),
        custom(
            "trust-denied-held-record-reads-as-identity-changed",
            Kind::Valid,
            || {
                same(
                    "denied held record is identity changed",
                    authorize_rotated_origin(|identity, _, holder, held| {
                        TrustList::default()
                            .identity(identity, false)
                            .deny(held)
                            .destination(held, holder)
                    }),
                    verdict(Decision::Refuse, Rule::IdentityChanged),
                )
            },
        ),
        custom(
            "trust-identity-changed-wins-over-identity-allow-under-collision-protection",
            Kind::Valid,
            || {
                same(
                    "identity changed over identity allow under collision protection",
                    authorize_rotated_origin_with(true, |identity, _, holder, held| {
                        TrustList::default()
                            .identity(identity, true)
                            .destination(held, holder)
                    }),
                    verdict(Decision::Refuse, Rule::IdentityChanged),
                )
            },
        ),
        custom(
            "trust-destination-allow-wins-over-identity-changed-under-collision-protection",
            Kind::Valid,
            || {
                same(
                    "destination allow over identity changed under collision protection",
                    authorize_rotated_origin_with(true, |identity, derived, holder, held| {
                        TrustList::default()
                            .destination(derived, identity)
                            .destination(held, holder)
                    }),
                    verdict(Decision::Allow, Rule::DestinationTrusted),
                )
            },
        ),
        custom(
            "trust-presence-under-identity-allow-refuses-under-collision-protection",
            Kind::Valid,
            || {
                same(
                    "presence under identity allow under collision protection",
                    authorize_presence_origin_with(true, |identity, _, holder, _| {
                        TrustList::default()
                            .identity(identity, true)
                            .identity(holder, true)
                    }),
                    verdict(Decision::Refuse, Rule::IdentityChanged),
                )
            },
        ),
        custom(
            "trust-presence-under-identity-allow-allows-with-collision-protection-off",
            Kind::Valid,
            || {
                same(
                    "presence under identity allow with collision protection off",
                    authorize_presence_origin_with(false, |identity, _, holder, _| {
                        TrustList::default()
                            .identity(identity, true)
                            .identity(holder, true)
                    }),
                    verdict(Decision::Allow, Rule::IdentityTrusted),
                )
            },
        ),
        custom(
            "trust-presence-with-no-allow-is-default-closed-under-collision-protection",
            Kind::Valid,
            || {
                same(
                    "presence with no allow is default closed under collision protection",
                    authorize_presence_origin_with(true, |_, _, holder, _| {
                        TrustList::default().identity(holder, true)
                    }),
                    verdict(Decision::Refuse, Rule::DefaultClosed),
                )
            },
        ),
        custom(
            "trust-destination-allow-wins-over-presence-under-collision-protection",
            Kind::Valid,
            || {
                same(
                    "destination allow over presence under collision protection",
                    authorize_presence_origin_with(true, |identity, derived, holder, _| {
                        TrustList::default()
                            .destination(derived, identity)
                            .identity(holder, true)
                    }),
                    verdict(Decision::Allow, Rule::DestinationTrusted),
                )
            },
        ),
        custom(
            "trust-remembered-presence-outlives-the-holders-row",
            Kind::Valid,
            || {
                same(
                    "remembered presence outlives the holder's row under collision protection",
                    judge_presence_origin(
                        true,
                        |identity, _, holder, _| {
                            TrustList::default()
                                .identity(identity, true)
                                .identity(holder, true)
                        },
                        |store, identity, _, heard| {
                            surface_presence_line(store, identity, heard);
                            store
                                .authorize_origin_at(identity, &ORIGIN, heard + PEER_TTL)
                                .verdict
                        },
                    ),
                    verdict(Decision::Refuse, Rule::IdentityChanged),
                )
            },
        ),
        custom(
            "trust-remembered-presence-never-refuses-the-holder-it-names",
            Kind::Valid,
            || {
                same(
                    "remembered presence never refuses the holder it names",
                    judge_presence_origin(
                        true,
                        |identity, _, holder, _| {
                            TrustList::default()
                                .identity(identity, true)
                                .identity(holder, true)
                        },
                        |store, identity, holder, heard| {
                            surface_presence_line(store, identity, heard);
                            store.authorize_origin_at(holder, &ORIGIN, heard).verdict
                        },
                    ),
                    verdict(Decision::Allow, Rule::IdentityTrusted),
                )
            },
        ),
        custom("refusal-code-packs-as-a-bare-uint", Kind::Valid, || {
            same(
                "bare uint",
                packed(&no_access().to_wire()),
                vec![0xcc, 0xf1],
            )
        }),
        custom("every-refusal-code-packs-as-its-byte", Kind::Valid, || {
            let codes = [0xf0u8, 0xf1, 0xf3, 0xf4, 0xf5, 0xf6, 0xfd, 0xfe];
            for byte in codes {
                let code = RefusalCode::from_wire(&Value::from(byte))
                    .ok_or(format!("{byte:#x} is not a refusal code"))?;
                same(
                    &format!("{byte:#x} on the wire"),
                    packed(&code.to_wire()),
                    vec![0xcc, byte],
                )?;
            }
            Ok(())
        }),
        custom(
            "protocol-version-constants-pin-at-1",
            Kind::Boundary,
            || {
                same(
                    "MESH_PROTOCOL_MIN_SUPPORTED",
                    MESH_PROTOCOL_MIN_SUPPORTED,
                    1,
                )?;
                same("MESH_PROTOCOL_VERSION", MESH_PROTOCOL_VERSION, 1)?;
                same("protocol_supported(0)", protocol_supported(0), false)?;
                same("protocol_supported(1)", protocol_supported(1), true)?;
                same("protocol_supported(2)", protocol_supported(2), false)?;
                same(
                    "protocol_supported(65535)",
                    protocol_supported(65535),
                    false,
                )
            },
        ),
        custom("envelope-encode-puts-v-first", Kind::Valid, || {
            let envelope = Envelope::new(OriginName(ORIGIN), Value::Nil);
            same("version", envelope.version, MESH_PROTOCOL_VERSION)?;
            let Value::Map(entries) = envelope.into_value() else {
                return Err("an envelope did not encode as a map".to_string());
            };
            same(
                "first key",
                entries
                    .first()
                    .map(|(key, value)| (key.as_str(), value.as_u64())),
                Some((Some("v"), Some(u64::from(MESH_PROTOCOL_VERSION)))),
            )
        }),
    ]
}

// ---------------------------------------------------------------------------------------
// The version refusal
// ---------------------------------------------------------------------------------------

fn refusal_value(found: Value) -> Value {
    map(vec![
        ("refusal", Value::from("unsupported_version")),
        ("found", found),
        ("min", Value::from(1u64)),
        ("max", Value::from(1u64)),
    ])
}

fn refusal(found: Option<u16>) -> Option<VersionRefusal> {
    Some(VersionRefusal {
        found,
        min: 1,
        max: 1,
    })
}

fn version(name: &'static str, kind: Kind, value: Value, expect: Option<VersionRefusal>) -> Vector {
    row(name, kind, Case::VersionRefusalDecode { value, expect })
}

fn version_encode(name: &'static str, found: Option<u16>, expect: Value) -> Vector {
    row(
        name,
        Kind::Valid,
        Case::VersionRefusalEncode { found, expect },
    )
}

fn version_refusal_vectors() -> Vec<Vector> {
    vec![
        version(
            "version-refusal-with-nil-found-is-read",
            Kind::Valid,
            refusal_value(Value::Nil),
            refusal(None),
        ),
        version(
            "version-refusal-with-found-2-is-read",
            Kind::Valid,
            refusal_value(Value::from(2u64)),
            refusal(Some(2)),
        ),
        version(
            "version-refusal-mixed-case-tag-is-malformed",
            Kind::Invalid,
            set(
                refusal_value(Value::Nil),
                "refusal",
                Value::from("Unsupported_Version"),
            ),
            None,
        ),
        version(
            "version-refusal-tag-with-trailing-space-is-malformed",
            Kind::Invalid,
            set(
                refusal_value(Value::Nil),
                "refusal",
                Value::from("unsupported_version "),
            ),
            None,
        ),
        version(
            "version-refusal-tag-as-bin-is-malformed",
            Kind::Invalid,
            set(
                refusal_value(Value::Nil),
                "refusal",
                Value::Binary(b"unsupported_version".to_vec()),
            ),
            None,
        ),
        version(
            "version-refusal-tag-no-access-is-malformed",
            Kind::Invalid,
            set(
                refusal_value(Value::Nil),
                "refusal",
                Value::from("no_access"),
            ),
            None,
        ),
        version(
            "version-refusal-tag-as-nil-is-malformed",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "refusal", Value::Nil),
            None,
        ),
        version(
            "version-refusal-without-tag-is-malformed",
            Kind::Invalid,
            without(refusal_value(Value::Nil), "refusal"),
            None,
        ),
        version(
            "version-refusal-as-bare-string-is-malformed",
            Kind::Invalid,
            Value::from("unsupported_version"),
            None,
        ),
        version(
            "version-refusal-as-nil-is-malformed",
            Kind::Invalid,
            Value::Nil,
            None,
        ),
        version(
            "version-refusal-as-refusal-code-byte-is-malformed",
            Kind::Invalid,
            Value::from(0xf1u8),
            None,
        ),
        version(
            "version-refusal-found-nil-reads-as-unknown",
            Kind::Valid,
            refusal_value(Value::Nil),
            refusal(None),
        ),
        version(
            "version-refusal-found-0-is-read",
            Kind::Boundary,
            refusal_value(Value::from(0u64)),
            refusal(Some(0)),
        ),
        version(
            "version-refusal-found-65535-is-read",
            Kind::Boundary,
            refusal_value(Value::from(65535u64)),
            refusal(Some(65535)),
        ),
        version(
            "version-refusal-without-found-is-malformed",
            Kind::Invalid,
            without(refusal_value(Value::Nil), "found"),
            None,
        ),
        version(
            "version-refusal-found-as-string-is-malformed",
            Kind::Invalid,
            refusal_value(Value::from("2")),
            None,
        ),
        version(
            "version-refusal-found-past-u16-is-malformed",
            Kind::Invalid,
            refusal_value(Value::from(65536u64)),
            None,
        ),
        version(
            "version-refusal-negative-found-is-malformed",
            Kind::Invalid,
            refusal_value(Value::from(-1i64)),
            None,
        ),
        version(
            "version-refusal-found-as-float-is-malformed",
            Kind::Invalid,
            refusal_value(Value::F64(2.0)),
            None,
        ),
        version(
            "version-refusal-found-as-bool-is-malformed",
            Kind::Invalid,
            refusal_value(Value::Boolean(false)),
            None,
        ),
        version(
            "version-refusal-min-65535-is-read",
            Kind::Boundary,
            set(refusal_value(Value::Nil), "min", Value::from(65535u64)),
            Some(VersionRefusal {
                found: None,
                min: 65535,
                max: 1,
            }),
        ),
        version(
            "version-refusal-without-min-is-malformed",
            Kind::Invalid,
            without(refusal_value(Value::Nil), "min"),
            None,
        ),
        version(
            "version-refusal-min-as-string-is-malformed",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "min", Value::from("1")),
            None,
        ),
        version(
            "version-refusal-min-as-nil-is-malformed",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "min", Value::Nil),
            None,
        ),
        version(
            "version-refusal-min-past-u16-is-malformed",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "min", Value::from(65536u64)),
            None,
        ),
        version(
            "version-refusal-negative-min-is-malformed",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "min", Value::from(-1i64)),
            None,
        ),
        version(
            "version-refusal-max-65535-is-read",
            Kind::Boundary,
            set(refusal_value(Value::Nil), "max", Value::from(65535u64)),
            Some(VersionRefusal {
                found: None,
                min: 1,
                max: 65535,
            }),
        ),
        version(
            "version-refusal-without-max-is-malformed",
            Kind::Invalid,
            without(refusal_value(Value::Nil), "max"),
            None,
        ),
        version(
            "version-refusal-max-as-string-is-malformed",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "max", Value::from("1")),
            None,
        ),
        version(
            "version-refusal-max-as-nil-is-malformed",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "max", Value::Nil),
            None,
        ),
        version(
            "version-refusal-max-past-u16-is-malformed",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "max", Value::from(65536u64)),
            None,
        ),
        version(
            "version-refusal-max-as-float-is-malformed",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "max", Value::F64(1.0)),
            None,
        ),
        version(
            "version-refusal-unknown-key-is-ignored",
            Kind::Valid,
            with(refusal_value(Value::Nil), "reason", Value::from("too old")),
            refusal(None),
        ),
        version(
            "version-refusal-with-an-error-key-is-still-read",
            Kind::Valid,
            with(
                refusal_value(Value::from(3u64)),
                "error",
                Value::from("unknown_path"),
            ),
            refusal(Some(3)),
        ),
        version(
            "version-refusal-integer-key-is-ignored",
            Kind::Valid,
            Value::Map(
                [
                    vec![(Value::from(0u64), Value::Nil)],
                    refusal_value(Value::Nil).as_map().unwrap().to_vec(),
                ]
                .concat(),
            ),
            refusal(None),
        ),
        version_encode(
            "version-refusal-encode-writes-nil-found-when-unknown",
            None,
            refusal_value(Value::Nil),
        ),
        version_encode(
            "version-refusal-encode-writes-found-2",
            Some(2),
            refusal_value(Value::from(2u64)),
        ),
        version_encode(
            "version-refusal-encode-writes-found-0",
            Some(0),
            refusal_value(Value::from(0u64)),
        ),
        version_encode(
            "version-refusal-encode-writes-found-65535",
            Some(65535),
            refusal_value(Value::from(65535u64)),
        ),
        version_encode(
            "version-refusal-encode-stamps-min-and-max-from-the-constants",
            None,
            set(
                set(
                    refusal_value(Value::Nil),
                    "min",
                    Value::from(u64::from(MESH_PROTOCOL_MIN_SUPPORTED)),
                ),
                "max",
                Value::from(u64::from(MESH_PROTOCOL_VERSION)),
            ),
        ),
        version(
            "version-refusal-current-round-trips",
            Kind::Valid,
            VersionRefusal::current(None).to_value(),
            Some(VersionRefusal {
                found: None,
                min: MESH_PROTOCOL_MIN_SUPPORTED,
                max: MESH_PROTOCOL_VERSION,
            }),
        ),
        version(
            "no-access-byte-is-not-a-version-refusal",
            Kind::Invalid,
            Value::from(0xf1u8),
            None,
        ),
        version(
            "unknown-path-error-is-not-a-version-refusal",
            Kind::Invalid,
            DispatchError::UnknownPath {
                path_hash: PathHash::of(UNKNOWN_PATH).to_string(),
            }
            .to_value(),
            None,
        ),
        version(
            "no-provider-error-is-not-a-version-refusal",
            Kind::Invalid,
            DispatchError::NoProvider {
                path: STATUS_PATH.to_string(),
            }
            .to_value(),
            None,
        ),
        version(
            "version-refusal-current-with-found-2-round-trips",
            Kind::Valid,
            VersionRefusal::current(Some(2)).to_value(),
            refusal(Some(2)),
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Dispatch errors and refusal codes on the requester's side
// ---------------------------------------------------------------------------------------

const HEX_32: &str = "0123456789abcdef0123456789abcdef";

fn no_provider_value(path: Value) -> Value {
    map(vec![("error", Value::from("no_provider")), ("path", path)])
}

fn no_provider(path: &str) -> Option<DispatchError> {
    Some(DispatchError::NoProvider {
        path: path.to_string(),
    })
}

fn dispatch_error(
    name: &'static str,
    kind: Kind,
    value: Value,
    expect: Option<DispatchError>,
) -> Vector {
    row(name, kind, Case::DispatchErrorDecode { value, expect })
}

fn dispatch_error_vectors() -> Vec<Vector> {
    vec![
        dispatch_error(
            "dispatch-error-no-provider-is-read",
            Kind::Valid,
            no_provider_value(Value::from(STATUS_PATH)),
            no_provider(STATUS_PATH),
        ),
        dispatch_error(
            "dispatch-error-without-error-key-is-malformed",
            Kind::Invalid,
            map(vec![("path_hash", Value::from(HEX_32))]),
            None,
        ),
        dispatch_error(
            "dispatch-error-no-provider-without-path-is-malformed",
            Kind::Invalid,
            map(vec![("error", Value::from("no_provider"))]),
            None,
        ),
        dispatch_error(
            "dispatch-error-as-nil-is-malformed",
            Kind::Invalid,
            Value::Nil,
            None,
        ),
        dispatch_error(
            "dispatch-error-as-refusal-code-byte-is-malformed",
            Kind::Invalid,
            Value::from(0xf1u8),
            None,
        ),
        dispatch_error(
            "version-refusal-is-not-a-dispatch-error",
            Kind::Invalid,
            VersionRefusal::current(None).to_value(),
            None,
        ),
        dispatch_error(
            "dispatch-error-no-provider-for-knock-is-read",
            Kind::Valid,
            no_provider_value(Value::from(KNOCK_PATH)),
            no_provider(KNOCK_PATH),
        ),
        dispatch_error(
            "dispatch-error-no-provider-for-status-is-read",
            Kind::Valid,
            no_provider_value(Value::from(STATUS_PATH)),
            no_provider(STATUS_PATH),
        ),
        dispatch_error(
            "dispatch-error-no-provider-for-message-is-read",
            Kind::Valid,
            no_provider_value(Value::from(MESSAGE_PATH)),
            no_provider(MESSAGE_PATH),
        ),
        dispatch_error(
            "dispatch-error-no-provider-for-list-is-read",
            Kind::Valid,
            no_provider_value(Value::from(LIST_PATH)),
            no_provider(LIST_PATH),
        ),
        dispatch_error(
            "dispatch-error-no-provider-for-fetch-is-read",
            Kind::Valid,
            no_provider_value(Value::from(FETCH_PATH)),
            no_provider(FETCH_PATH),
        ),
        dispatch_error(
            "dispatch-error-no-provider-for-access-is-read",
            Kind::Valid,
            no_provider_value(Value::from(ACCESS_PATH)),
            no_provider(ACCESS_PATH),
        ),
        dispatch_error(
            "dispatch-error-no-provider-for-an-unknown-path-is-malformed",
            Kind::Invalid,
            no_provider_value(Value::from("/other")),
            None,
        ),
        dispatch_error(
            "dispatch-error-no-provider-mixed-case-path-is-malformed",
            Kind::Invalid,
            no_provider_value(Value::from("/Status")),
            None,
        ),
        dispatch_error(
            "dispatch-error-no-provider-path-without-leading-slash-is-malformed",
            Kind::Invalid,
            no_provider_value(Value::from("status")),
            None,
        ),
        dispatch_error(
            "dispatch-error-no-provider-path-with-trailing-slash-is-malformed",
            Kind::Invalid,
            no_provider_value(Value::from("/status/")),
            None,
        ),
        dispatch_error(
            "dispatch-error-no-provider-empty-path-is-malformed",
            Kind::Invalid,
            no_provider_value(Value::from("")),
            None,
        ),
        dispatch_error(
            "dispatch-error-no-provider-path-as-bin-is-malformed",
            Kind::Invalid,
            no_provider_value(Value::Binary(STATUS_PATH.as_bytes().to_vec())),
            None,
        ),
        dispatch_error(
            "dispatch-error-no-provider-path-as-nil-is-malformed",
            Kind::Invalid,
            no_provider_value(Value::Nil),
            None,
        ),
        dispatch_error(
            "dispatch-error-no-provider-keyed-by-path-hash-is-malformed",
            Kind::Invalid,
            map(vec![
                ("error", Value::from("no_provider")),
                ("path_hash", Value::from(HEX_32)),
            ]),
            None,
        ),
        dispatch_error(
            "dispatch-error-no-provider-ignores-a-stray-path-hash",
            Kind::Valid,
            with(
                no_provider_value(Value::from(MESSAGE_PATH)),
                "path_hash",
                Value::from(HEX_32),
            ),
            no_provider(MESSAGE_PATH),
        ),
        dispatch_error(
            "no-access-byte-is-not-a-dispatch-error",
            Kind::Invalid,
            Value::from(0xf1u8),
            None,
        ),
        dispatch_error(
            "version-refusal-with-found-2-is-not-a-dispatch-error",
            Kind::Invalid,
            VersionRefusal::current(Some(2)).to_value(),
            None,
        ),
    ]
}

fn code(name: &'static str, kind: Kind, value: Value, expect: Option<Value>) -> Vector {
    row(name, kind, Case::RefusalCodeDecode { value, expect })
}

fn wire(byte: u8) -> Option<Value> {
    Some(Value::from(byte))
}

fn refusal_code_vectors() -> Vec<Vector> {
    vec![
        code(
            "refusal-code-0xf0-reads-as-no-identity",
            Kind::Valid,
            Value::from(0xf0u8),
            wire(0xf0),
        ),
        code(
            "refusal-code-0xf1-reads-as-no-access",
            Kind::Valid,
            Value::from(0xf1u8),
            wire(0xf1),
        ),
        code(
            "refusal-code-0xf3-reads-as-invalid-key",
            Kind::Valid,
            Value::from(0xf3u8),
            wire(0xf3),
        ),
        code(
            "refusal-code-0xf4-reads-as-invalid-data",
            Kind::Valid,
            Value::from(0xf4u8),
            wire(0xf4),
        ),
        code(
            "refusal-code-0xf5-reads-as-invalid-stamp",
            Kind::Valid,
            Value::from(0xf5u8),
            wire(0xf5),
        ),
        code(
            "refusal-code-0xf6-reads-as-throttled",
            Kind::Valid,
            Value::from(0xf6u8),
            wire(0xf6),
        ),
        code(
            "refusal-code-0xfd-reads-as-not-found",
            Kind::Valid,
            Value::from(0xfdu8),
            wire(0xfd),
        ),
        code(
            "refusal-code-0xfe-reads-as-timeout",
            Kind::Valid,
            Value::from(0xfeu8),
            wire(0xfe),
        ),
        code(
            "refusal-code-unpacked-from-its-bytes-is-read",
            Kind::Valid,
            unpacked(&[0xcc, 0xf1]),
            wire(0xf1),
        ),
        code(
            "refusal-code-as-u64-is-read",
            Kind::Valid,
            Value::from(0xf1u64),
            wire(0xf1),
        ),
        code(
            "refusal-code-round-trips-through-its-wire-form",
            Kind::Valid,
            no_access().to_wire(),
            wire(0xf1),
        ),
        code(
            "no-access-byte-is-a-refusal-code",
            Kind::Valid,
            Value::from(0xf1u8),
            wire(0xf1),
        ),
        code(
            "version-refusal-is-not-a-refusal-code",
            Kind::Invalid,
            VersionRefusal::current(None).to_value(),
            None,
        ),
        code(
            "version-refusal-with-found-2-is-not-a-refusal-code",
            Kind::Invalid,
            VersionRefusal::current(Some(2)).to_value(),
            None,
        ),
        code(
            "unknown-path-error-is-not-a-refusal-code",
            Kind::Invalid,
            DispatchError::UnknownPath {
                path_hash: HEX_32.to_string(),
            }
            .to_value(),
            None,
        ),
        code(
            "no-provider-error-is-not-a-refusal-code",
            Kind::Invalid,
            DispatchError::NoProvider {
                path: STATUS_PATH.to_string(),
            }
            .to_value(),
            None,
        ),
        code(
            "string-body-is-not-a-refusal-code",
            Kind::Invalid,
            Value::from("body"),
            None,
        ),
        code("nil-is-not-a-refusal-code", Kind::Invalid, Value::Nil, None),
        code(
            "refusal-code-0xf2-is-unknown",
            Kind::Invalid,
            Value::from(0xf2u8),
            None,
        ),
        code(
            "refusal-code-0xf7-is-unknown",
            Kind::Invalid,
            Value::from(0xf7u8),
            None,
        ),
        code(
            "refusal-code-0xfc-is-unknown",
            Kind::Invalid,
            Value::from(0xfcu8),
            None,
        ),
        code(
            "refusal-code-0xff-is-unknown",
            Kind::Invalid,
            Value::from(0xffu8),
            None,
        ),
        code(
            "refusal-code-0-is-not-a-code",
            Kind::Invalid,
            Value::from(0u8),
            None,
        ),
        code(
            "refusal-code-1-is-not-a-code",
            Kind::Invalid,
            Value::from(1u8),
            None,
        ),
        code(
            "refusal-code-0xef-below-the-range-is-not-a-code",
            Kind::Invalid,
            Value::from(0xefu8),
            None,
        ),
        code(
            "refusal-code-0x1f1-past-a-byte-is-not-a-code",
            Kind::Invalid,
            Value::from(0x1f1u64),
            None,
        ),
        code(
            "refusal-code-negative-0xf1-is-not-a-code",
            Kind::Invalid,
            Value::from(-0xf1i64),
            None,
        ),
        code(
            "refusal-code-as-float-is-not-a-code",
            Kind::Invalid,
            Value::F64(241.0),
            None,
        ),
        code(
            "refusal-code-as-string-is-not-a-code",
            Kind::Invalid,
            Value::from("0xf1"),
            None,
        ),
        code(
            "refusal-code-as-bin-is-not-a-code",
            Kind::Invalid,
            Value::Binary(vec![0xf1]),
            None,
        ),
        code(
            "refusal-code-as-array-is-not-a-code",
            Kind::Invalid,
            Value::Array(vec![Value::from(0xf1u8)]),
            None,
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
            let tag = format!("{} [{family}, {:?}]", vector.name, vector.kind);
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

    const TESTED: [&str; 8] = [
        "EnvelopeDecode",
        "EnvelopeEncode",
        "Dispatch",
        "VersionRefusalDecode",
        "VersionRefusalEncode",
        "DispatchErrorDecode",
        "RefusalCodeDecode",
        "Custom",
    ];

    #[test]
    fn envelope_vectors_decode_as_the_envelope_table_mandates() {
        run_family("EnvelopeDecode");
    }

    #[test]
    fn envelope_vectors_encode_in_the_canonical_key_order() {
        run_family("EnvelopeEncode");
    }

    #[test]
    fn dispatch_vectors_answer_as_the_dispatcher_stages_mandate() {
        run_family("Dispatch");
    }

    #[test]
    fn version_refusal_vectors_hold_the_refusal_shape() {
        run_family("VersionRefusalDecode");
        run_family("VersionRefusalEncode");
    }

    #[test]
    fn dispatch_error_vectors_read_as_the_error_table_mandates() {
        run_family("DispatchErrorDecode");
    }

    #[test]
    fn refusal_code_vectors_decode_as_the_code_registry_mandates() {
        run_family("RefusalCodeDecode");
    }

    #[test]
    fn custom_vectors_hold() {
        run_family("Custom");
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

    #[test]
    fn every_row_has_a_unique_name() {
        let mut seen = BTreeSet::new();
        for vector in vectors() {
            assert!(!vector.name.is_empty(), "a row has an empty name");
            assert!(
                seen.insert(vector.name),
                "two rows share the name {}",
                vector.name
            );
        }
    }
}
