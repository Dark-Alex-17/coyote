//! Requirement-id keyed vectors for the R3 transport: request and response frames, the
//! Envelope, the dispatcher's stages, refusal codes and the version refusal, run in-process
//! against this crate's decoders and `Dispatcher`.
//!
//! Every row names the id it exercises and the receiver action sections 6 and 7 of the spec
//! mandate for it. A row written faithfully from the spec that the code does not honour is
//! kept as written and flagged with `known_divergence`; the executor prints such a row
//! instead of asserting it, and fails when the flag goes stale.

use super::{Kind, Listed};
use crate::mesh::protocol::{
    MESH_PROTOCOL_MIN_SUPPORTED, MESH_PROTOCOL_VERSION, VersionRefusal, protocol_supported,
};
use crate::mesh::r3::{
    ACCESS_PATH, Admission, DispatchError, Dispatcher, Envelope, EnvelopeError, FETCH_PATH,
    HANDLER_TIMEOUT, Handler, InboundRequest, KNOCK_PATH, KnockEvent, KnockSink, LIST_PATH,
    MAX_CONCURRENT_INBOUND_REQUESTS, MAX_R3_PAYLOAD_BYTES, MESSAGE_PATH, NAME_HASH_LEN, OriginName,
    PathHash, R3Error, RefusalCode, Reply, RequestFrame, RequestHandler, RequestId, ResponseFrame,
    STATUS_PATH, SizeBranch,
};
use crate::mesh::test_support::TrustList;
use crate::mesh::trust::{Decision, Rule, Verdict};
use crate::mesh::{destination_address, hex_lower};

use async_trait::async_trait;
use rand_core::OsRng;
use rmpv::Value;
use rns_transport::destination::link::LinkId;
use rns_transport::identity::PrivateIdentity;
use std::fmt::Debug;
use std::future::Future;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// The input and expectation of a row, one variant per decoder under test. The variant
/// name is the family the coverage report groups rows by.
enum Case {
    /// `RequestFrame::decode`, section 6.1.
    RequestFrameDecode {
        bytes: Vec<u8>,
        expect: FrameAction<RequestFrame>,
    },
    /// `ResponseFrame::decode`, section 6.2.
    ResponseFrameDecode {
        bytes: Vec<u8>,
        expect: FrameAction<ResponseFrame>,
    },
    /// `Envelope::from_value`, section 6.5.
    EnvelopeDecode {
        value: Value,
        expect: EnvelopeAction,
    },
    /// `Envelope::new(..).into_value()`, the sender's side of section 6.5, compared value
    /// for value so key order is pinned.
    EnvelopeEncode {
        origin: OriginName,
        body: Value,
        expect: Value,
    },
    /// `RequestHandler::handle` on a `Dispatcher`, the stages of section 6.6, with the
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
    /// `VersionRefusal::from_value`, section 7.
    VersionRefusalDecode {
        value: Value,
        expect: Option<VersionRefusal>,
    },
    /// `VersionRefusal::current(found).to_value()`, compared value for value so key order
    /// is pinned.
    VersionRefusalEncode { found: Option<u16>, expect: Value },
    /// `DispatchError::from_value`, section 6.7.
    DispatchErrorDecode {
        value: Value,
        expect: Option<DispatchError>,
    },
    /// `RefusalCode::from_wire`, section 6.7; the expectation is the wire form of the code
    /// read, so no code is named here.
    RefusalCodeDecode { value: Value, expect: Option<Value> },
    /// A check with no single decoder seam: constants, precedence tables, byte pins.
    Custom(fn() -> Result<(), String>),
}

impl Case {
    fn family(&self) -> &'static str {
        match self {
            Case::RequestFrameDecode { .. } => "RequestFrameDecode",
            Case::ResponseFrameDecode { .. } => "ResponseFrameDecode",
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

enum FrameAction<T> {
    Accepted(T),
    Rejected(&'static str),
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
/// hash section 6.5 derives for it; both are only known once the identity is minted.
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
        Case::RequestFrameDecode { bytes, expect } => {
            run_frame("request frame", RequestFrame::decode(bytes), expect)
        }
        Case::ResponseFrameDecode { bytes, expect } => {
            run_frame("response frame", ResponseFrame::decode(bytes), expect)
        }
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

fn run_frame<T: Debug + PartialEq>(
    what: &str,
    observed: Result<T, R3Error>,
    expect: &FrameAction<T>,
) -> Result<(), String> {
    match (observed, expect) {
        (Ok(frame), FrameAction::Accepted(expected)) => same(what, &frame, expected),
        (Ok(frame), FrameAction::Rejected(text)) => {
            Err(format!("expected rejection {text:?}, observed {frame:?}"))
        }
        (Err(R3Error::Decode(detail)), FrameAction::Rejected(text)) => ensure(
            detail.contains(text),
            format!("expected a decode error mentioning {text:?}, observed {detail:?}"),
        ),
        (Err(error), FrameAction::Rejected(text)) => Err(format!(
            "expected a decode error mentioning {text:?}, observed {error:?}"
        )),
        (Err(error), FrameAction::Accepted(expected)) => {
            Err(format!("expected {expected:?}, observed {error:?}"))
        }
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
        requested_at: 0.0,
        data,
        branch: SizeBranch::Packet,
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

fn nested_arrays(depth: usize, leaf: Value) -> Value {
    (0..depth).fold(leaf, |inner, _| Value::Array(vec![inner]))
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
    async fn handle(&self, request: crate::mesh::r3::AdmittedRequest) -> Reply {
        match self {
            Scripted::Echo => Reply::Value(request.body),
            Scripted::Silent => Reply::Silent,
        }
    }
}

fn vectors() -> Vec<Vector> {
    let mut all = Vec::new();
    all.extend(request_frame_vectors());
    all.extend(response_frame_vectors());
    all.extend(envelope_vectors());
    all.extend(dispatch_vectors());
    all.extend(custom_vectors());
    all.extend(version_refusal_vectors());
    all.extend(dispatch_error_vectors());
    all.extend(refusal_code_vectors());
    all
}

// ---------------------------------------------------------------------------------------
// Section 6.1 and 6.2: request and response frames
// ---------------------------------------------------------------------------------------

const REQUEST_ID: [u8; 16] = [1; 16];
const NOT_ARRAY: &str = "is not an array";
const NOT_F64: &str = "time is not a float64";
const PATH_NOT_BIN: &str = "path hash is not a bin";
const ID_NOT_BIN: &str = "request id is not a bin";
const TRAILING: &str = "trailing bytes after the frame";
const TOO_DEEP: &str = "depth limit exceeded";
/// Nested one-element arrays under `data` that fit the decode budget of MESH-ENV-048 once
/// the frame's own array has spent two units of `MAX_R3_NESTING_DEPTH`.
const DEEPEST_DATA_ARRAYS: usize = 62;

fn request_frame() -> RequestFrame {
    RequestFrame {
        time: 1.5,
        path_hash: PathHash::of(STATUS_PATH),
        data: Value::from("data"),
    }
}

fn request_value(time: Value, path_hash: Value, data: Value) -> Value {
    Value::Array(vec![time, path_hash, data])
}

fn path_bin(len: usize) -> Value {
    Value::Binary(vec![0x5a; len])
}

fn request(
    id: &'static str,
    kind: Kind,
    bytes: Vec<u8>,
    expect: FrameAction<RequestFrame>,
) -> Vector {
    row(id, kind, Case::RequestFrameDecode { bytes, expect })
}

fn request_frame_vectors() -> Vec<Vector> {
    let encoded = request_frame().encode();
    let Value::Array(parts) = unpacked(&encoded) else {
        unreachable!("a request frame encodes as an array")
    };
    let path = parts[1].clone();
    let mut trailing = encoded.clone();
    trailing.push(0xc0);
    let mut truncated = encoded.clone();
    truncated.pop();
    vec![
        request(
            "MESH-ENV-001",
            Kind::Valid,
            encoded.clone(),
            FrameAction::Accepted(request_frame()),
        ),
        request(
            "MESH-ENV-001",
            Kind::Invalid,
            packed(&request_value(Value::from(1u64), path.clone(), Value::Nil)),
            FrameAction::Rejected(NOT_F64),
        ),
        request(
            "MESH-ENV-001",
            Kind::Invalid,
            packed(&request_value(Value::F32(1.5), path.clone(), Value::Nil)),
            FrameAction::Rejected(NOT_F64),
        ),
        request(
            "MESH-ENV-001",
            Kind::Invalid,
            packed(&request_value(Value::from("1.5"), path.clone(), Value::Nil)),
            FrameAction::Rejected(NOT_F64),
        ),
        request(
            "MESH-ENV-001",
            Kind::Invalid,
            packed(&request_value(Value::Nil, path.clone(), Value::Nil)),
            FrameAction::Rejected(NOT_F64),
        ),
        request(
            "MESH-ENV-002",
            Kind::Boundary,
            packed(&request_value(Value::F64(0.0), path_bin(16), Value::Nil)),
            FrameAction::Accepted(RequestFrame {
                time: 0.0,
                path_hash: PathHash::from([0x5a; 16]),
                data: Value::Nil,
            }),
        ),
        request(
            "MESH-ENV-002",
            Kind::Invalid,
            packed(&request_value(Value::F64(0.0), path_bin(15), Value::Nil)),
            FrameAction::Rejected("path hash is 15 bytes, expected 16"),
        ),
        request(
            "MESH-ENV-002",
            Kind::Invalid,
            packed(&request_value(Value::F64(0.0), path_bin(17), Value::Nil)),
            FrameAction::Rejected("path hash is 17 bytes, expected 16"),
        ),
        request(
            "MESH-ENV-002",
            Kind::Invalid,
            packed(&request_value(Value::F64(0.0), path_bin(0), Value::Nil)),
            FrameAction::Rejected("path hash is 0 bytes, expected 16"),
        ),
        request(
            "MESH-ENV-002",
            Kind::Invalid,
            packed(&request_value(
                Value::F64(0.0),
                Value::from(PathHash::of(STATUS_PATH).to_hex_string()),
                Value::Nil,
            )),
            FrameAction::Rejected(PATH_NOT_BIN),
        ),
        request(
            "MESH-ENV-002",
            Kind::Invalid,
            packed(&request_value(
                Value::F64(0.0),
                Value::Array(vec![Value::from(0u8); 16]),
                Value::Nil,
            )),
            FrameAction::Rejected(PATH_NOT_BIN),
        ),
        request(
            "MESH-ENV-002",
            Kind::Invalid,
            packed(&request_value(Value::F64(0.0), Value::Nil, Value::Nil)),
            FrameAction::Rejected(PATH_NOT_BIN),
        ),
        request(
            "MESH-ENV-003",
            Kind::Invalid,
            packed(&Value::Array(vec![Value::F64(0.0), path.clone()])),
            FrameAction::Rejected("has 2 elements, expected 3"),
        ),
        request(
            "MESH-ENV-003",
            Kind::Invalid,
            packed(&Value::Array(vec![
                Value::F64(0.0),
                path.clone(),
                Value::Nil,
                Value::Nil,
            ])),
            FrameAction::Rejected("has 4 elements, expected 3"),
        ),
        request(
            "MESH-ENV-003",
            Kind::Invalid,
            packed(&Value::Array(vec![])),
            FrameAction::Rejected("has 0 elements, expected 3"),
        ),
        request(
            "MESH-ENV-003",
            Kind::Invalid,
            packed(&map(vec![
                ("time", Value::F64(0.0)),
                ("path_hash", path.clone()),
                ("data", Value::Nil),
            ])),
            FrameAction::Rejected(NOT_ARRAY),
        ),
        request(
            "MESH-ENV-003",
            Kind::Invalid,
            packed(&Value::Nil),
            FrameAction::Rejected(NOT_ARRAY),
        ),
        request(
            "MESH-ENV-003",
            Kind::Invalid,
            packed(&Value::Binary(encoded.clone())),
            FrameAction::Rejected(NOT_ARRAY),
        ),
        request(
            "MESH-ENV-004",
            Kind::Invalid,
            trailing,
            FrameAction::Rejected(TRAILING),
        ),
        request(
            "MESH-ENV-004",
            Kind::Invalid,
            [encoded.clone(), encoded.clone()].concat(),
            FrameAction::Rejected(TRAILING),
        ),
        request(
            "MESH-ENV-004",
            Kind::Invalid,
            [encoded.clone(), vec![0x00]].concat(),
            FrameAction::Rejected("1 trailing bytes"),
        ),
        request(
            "MESH-ENV-048",
            Kind::Boundary,
            packed(&request_value(
                Value::F64(0.0),
                path_bin(16),
                nested_arrays(DEEPEST_DATA_ARRAYS, Value::Nil),
            )),
            FrameAction::Accepted(RequestFrame {
                time: 0.0,
                path_hash: PathHash::from([0x5a; 16]),
                data: nested_arrays(DEEPEST_DATA_ARRAYS, Value::Nil),
            }),
        ),
        request(
            "MESH-ENV-048",
            Kind::Invalid,
            packed(&request_value(
                Value::F64(0.0),
                path_bin(16),
                nested_arrays(DEEPEST_DATA_ARRAYS + 1, Value::Nil),
            )),
            FrameAction::Rejected(TOO_DEEP),
        ),
        request(
            "MESH-ENV-028",
            Kind::Invalid,
            truncated,
            FrameAction::Rejected(""),
        ),
        request(
            "MESH-ENV-028",
            Kind::Invalid,
            Vec::new(),
            FrameAction::Rejected(""),
        ),
        request(
            "MESH-ENV-028",
            Kind::Invalid,
            vec![0xc1],
            FrameAction::Rejected(""),
        ),
        request(
            "MESH-ENV-028",
            Kind::Invalid,
            packed(&Value::from("not a frame")),
            FrameAction::Rejected(NOT_ARRAY),
        ),
    ]
}

fn response_frame(data: Value) -> ResponseFrame {
    ResponseFrame {
        request_id: RequestId::from(REQUEST_ID),
        data,
    }
}

fn response_value(request_id: Value, data: Value) -> Value {
    Value::Array(vec![request_id, data])
}

fn response(
    id: &'static str,
    kind: Kind,
    bytes: Vec<u8>,
    expect: FrameAction<ResponseFrame>,
) -> Vector {
    row(id, kind, Case::ResponseFrameDecode { bytes, expect })
}

fn response_frame_vectors() -> Vec<Vector> {
    let id = Value::Binary(REQUEST_ID.to_vec());
    let encoded = response_frame(Value::from("reply")).encode();
    let refusal = [
        vec![0x92, 0xc4, 0x10],
        REQUEST_ID.to_vec(),
        vec![0xcc, 0xf1],
    ]
    .concat();
    vec![
        response(
            "MESH-ENV-007",
            Kind::Valid,
            encoded.clone(),
            FrameAction::Accepted(response_frame(Value::from("reply"))),
        ),
        response(
            "MESH-ENV-007",
            Kind::Boundary,
            packed(&response_value(id.clone(), Value::Nil)),
            FrameAction::Accepted(response_frame(Value::Nil)),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            packed(&Value::Array(vec![id.clone()])),
            FrameAction::Rejected("has 1 elements, expected 2"),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            packed(&Value::Array(vec![id.clone(), Value::Nil, Value::Nil])),
            FrameAction::Rejected("has 3 elements, expected 2"),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            packed(&Value::Array(vec![])),
            FrameAction::Rejected("has 0 elements, expected 2"),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            packed(&map(vec![("request_id", id.clone()), ("data", Value::Nil)])),
            FrameAction::Rejected(NOT_ARRAY),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            packed(&Value::Nil),
            FrameAction::Rejected(NOT_ARRAY),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            packed(&response_value(
                Value::from(RequestId::from(REQUEST_ID).to_hex_string()),
                Value::Nil,
            )),
            FrameAction::Rejected(ID_NOT_BIN),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            packed(&response_value(Value::Nil, Value::Nil)),
            FrameAction::Rejected(ID_NOT_BIN),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            packed(&response_value(Value::Binary(vec![1; 15]), Value::Nil)),
            FrameAction::Rejected("request id is 15 bytes, expected 16"),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            packed(&response_value(Value::Binary(vec![1; 17]), Value::Nil)),
            FrameAction::Rejected("request id is 17 bytes, expected 16"),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            [encoded.clone(), vec![0xc0]].concat(),
            FrameAction::Rejected(TRAILING),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            encoded[..encoded.len() - 1].to_vec(),
            FrameAction::Rejected(""),
        ),
        response(
            "MESH-ENV-007",
            Kind::Invalid,
            Vec::new(),
            FrameAction::Rejected(""),
        ),
        response(
            "MESH-ENV-049",
            Kind::Boundary,
            packed(&response_value(
                id.clone(),
                nested_arrays(DEEPEST_DATA_ARRAYS, Value::Nil),
            )),
            FrameAction::Accepted(response_frame(nested_arrays(
                DEEPEST_DATA_ARRAYS,
                Value::Nil,
            ))),
        ),
        response(
            "MESH-ENV-049",
            Kind::Invalid,
            packed(&response_value(
                id.clone(),
                nested_arrays(DEEPEST_DATA_ARRAYS + 1, Value::Nil),
            )),
            FrameAction::Rejected(TOO_DEEP),
        ),
        response(
            "MESH-ENV-038",
            Kind::Valid,
            refusal.clone(),
            FrameAction::Accepted(response_frame(no_access().to_wire())),
        ),
        response(
            "MESH-ENV-038",
            Kind::Valid,
            response_frame(no_access().to_wire()).encode(),
            FrameAction::Accepted(response_frame(Value::from(0xf1u8))),
        ),
        response(
            "MESH-ENV-044",
            Kind::Valid,
            [
                vec![0x92, 0xc4, 0x10],
                REQUEST_ID.to_vec(),
                vec![0xcc, 0xf4],
            ]
            .concat(),
            FrameAction::Accepted(response_frame(Value::from(0xf4u8))),
        ),
        response(
            "MESH-ENV-044",
            Kind::Valid,
            [
                vec![0x92, 0xc4, 0x10],
                REQUEST_ID.to_vec(),
                vec![0xcc, 0xfe],
            ]
            .concat(),
            FrameAction::Accepted(response_frame(Value::from(0xfeu8))),
        ),
        response(
            "MESH-ENV-006",
            Kind::Valid,
            packed(&response_value(
                id.clone(),
                VersionRefusal::current(None).to_value(),
            )),
            FrameAction::Accepted(response_frame(VersionRefusal::current(None).to_value())),
        ),
        response(
            "MESH-ENV-006",
            Kind::Valid,
            packed(&response_value(
                id,
                DispatchError::NoProvider {
                    path: STATUS_PATH.to_string(),
                }
                .to_value(),
            )),
            FrameAction::Accepted(response_frame(
                DispatchError::NoProvider {
                    path: STATUS_PATH.to_string(),
                }
                .to_value(),
            )),
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Section 6.5: the Envelope
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

fn envelope(id: &'static str, kind: Kind, value: Value, expect: EnvelopeAction) -> Vector {
    row(id, kind, Case::EnvelopeDecode { value, expect })
}

fn encode(id: &'static str, kind: Kind, body: Value, expect: Value) -> Vector {
    row(
        id,
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
            "MESH-ENV-014",
            Kind::Valid,
            envelope_value(),
            accepted(body.clone()),
        ),
        envelope(
            "MESH-ENV-014",
            Kind::Boundary,
            set(envelope_value(), "v", Value::from(1u64)),
            accepted(body.clone()),
        ),
        envelope(
            "MESH-ENV-014",
            Kind::Invalid,
            without(envelope_value(), "v"),
            unsupported(None),
        ),
        envelope(
            "MESH-ENV-014",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from("1")),
            unsupported(None),
        ),
        envelope(
            "MESH-ENV-014",
            Kind::Invalid,
            set(envelope_value(), "v", Value::F64(1.0)),
            unsupported(None),
        ),
        envelope(
            "MESH-ENV-014",
            Kind::Invalid,
            set(envelope_value(), "v", Value::Nil),
            unsupported(None),
        ),
        envelope(
            "MESH-ENV-014",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(65536u64)),
            unsupported(None),
        ),
        envelope(
            "MESH-ENV-014",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(0u64)),
            unsupported(Some(0)),
        ),
        envelope(
            "MESH-ENV-014",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(2u64)),
            unsupported(Some(2)),
        ),
        envelope(
            "MESH-ENV-015",
            Kind::Boundary,
            set(envelope_value(), "name_hash", origin_bin(10)),
            accepted(body.clone()),
        ),
        envelope(
            "MESH-ENV-015",
            Kind::Invalid,
            without(envelope_value(), "name_hash"),
            malformed(),
        ),
        envelope(
            "MESH-ENV-015",
            Kind::Invalid,
            set(
                envelope_value(),
                "name_hash",
                Value::from(hex_lower(&ORIGIN)),
            ),
            malformed(),
        ),
        envelope(
            "MESH-ENV-015",
            Kind::Invalid,
            set(envelope_value(), "name_hash", Value::Nil),
            malformed(),
        ),
        envelope(
            "MESH-ENV-015",
            Kind::Invalid,
            set(
                envelope_value(),
                "name_hash",
                Value::Array(vec![Value::from(7u8); 10]),
            ),
            malformed(),
        ),
        envelope(
            "MESH-ENV-015",
            Kind::Invalid,
            set(envelope_value(), "name_hash", origin_bin(9)),
            malformed(),
        ),
        envelope(
            "MESH-ENV-015",
            Kind::Invalid,
            set(envelope_value(), "name_hash", origin_bin(11)),
            malformed(),
        ),
        envelope(
            "MESH-ENV-015",
            Kind::Invalid,
            set(envelope_value(), "name_hash", origin_bin(0)),
            malformed(),
        ),
        envelope(
            "MESH-ENV-015",
            Kind::Invalid,
            set(envelope_value(), "name_hash", origin_bin(16)),
            malformed(),
        ),
        envelope(
            "MESH-ENV-016",
            Kind::Valid,
            set(envelope_value(), "body", Value::Nil),
            accepted(Value::Nil),
        ),
        envelope(
            "MESH-ENV-016",
            Kind::Valid,
            set(envelope_value(), "body", map(vec![])),
            accepted(map(vec![])),
        ),
        envelope(
            "MESH-ENV-016",
            Kind::Invalid,
            without(envelope_value(), "body"),
            malformed(),
        ),
        envelope(
            "MESH-ENV-017",
            Kind::Valid,
            with(envelope_value(), "extra", Value::from("ignored")),
            accepted(body.clone()),
        ),
        envelope(
            "MESH-ENV-017",
            Kind::Valid,
            with(envelope_value(), "version", Value::from(9u64)),
            accepted(body.clone()),
        ),
        envelope(
            "MESH-ENV-017",
            Kind::Valid,
            numeric_key,
            accepted(body.clone()),
        ),
        envelope(
            "MESH-ENV-019",
            Kind::Invalid,
            with(envelope_value(), "v", Value::from(2u64)),
            unsupported(Some(2)),
        ),
        envelope(
            "MESH-ENV-019",
            Kind::Valid,
            with(
                set(envelope_value(), "v", Value::from(2u64)),
                "v",
                Value::from(1u64),
            ),
            accepted(body.clone()),
        ),
        envelope(
            "MESH-ENV-019",
            Kind::Valid,
            with(
                set(envelope_value(), "name_hash", Value::from("bad")),
                "name_hash",
                origin_bin(10),
            ),
            accepted(body.clone()),
        ),
        envelope(
            "MESH-ENV-019",
            Kind::Invalid,
            with(envelope_value(), "name_hash", origin_bin(9)),
            malformed(),
        ),
        envelope(
            "MESH-ENV-019",
            Kind::Valid,
            with(envelope_value(), "body", Value::from("last")),
            accepted(Value::from("last")),
        ),
        envelope(
            "MESH-ENV-020",
            Kind::Invalid,
            set(
                set(envelope_value(), "v", Value::from(2u64)),
                "name_hash",
                origin_bin(3),
            ),
            unsupported(Some(2)),
        ),
        envelope(
            "MESH-ENV-020",
            Kind::Invalid,
            without(without(envelope_value(), "v"), "body"),
            unsupported(None),
        ),
        envelope(
            "MESH-ENV-020",
            Kind::Invalid,
            map(vec![("v", Value::from("x"))]),
            unsupported(None),
        ),
        envelope(
            "MESH-ENV-020",
            Kind::Invalid,
            map(vec![]),
            unsupported(None),
        ),
        envelope("MESH-ENV-021", Kind::Invalid, Value::Nil, malformed()),
        envelope(
            "MESH-ENV-021",
            Kind::Invalid,
            Value::from("body"),
            malformed(),
        ),
        envelope(
            "MESH-ENV-021",
            Kind::Invalid,
            Value::Array(vec![Value::from(2u64), origin_bin(10), body.clone()]),
            malformed(),
        ),
        envelope(
            "MESH-ENV-021",
            Kind::Invalid,
            Value::Binary(packed(&envelope_value())),
            malformed(),
        ),
        envelope(
            "MESH-ENV-030",
            Kind::Invalid,
            Value::from(1u64),
            malformed(),
        ),
        envelope(
            "MESH-ENV-030",
            Kind::Invalid,
            set(envelope_value(), "name_hash", Value::from(7u64)),
            malformed(),
        ),
        envelope(
            "MESH-VER-001",
            Kind::Boundary,
            set(envelope_value(), "v", Value::from(65535u64)),
            unsupported(Some(65535)),
        ),
        envelope(
            "MESH-VER-001",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(65536u64)),
            unsupported(None),
        ),
        envelope(
            "MESH-VER-001",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(u64::MAX)),
            unsupported(None),
        ),
        envelope(
            "MESH-VER-001",
            Kind::Invalid,
            set(envelope_value(), "v", Value::from(-1i64)),
            unsupported(None),
        ),
        envelope(
            "MESH-VER-002",
            Kind::Boundary,
            set(
                envelope_value(),
                "v",
                Value::from(u64::from(MESH_PROTOCOL_MIN_SUPPORTED)),
            ),
            accepted(body.clone()),
        ),
        envelope(
            "MESH-VER-002",
            Kind::Boundary,
            set(
                envelope_value(),
                "v",
                Value::from(u64::from(MESH_PROTOCOL_VERSION)),
            ),
            accepted(body.clone()),
        ),
        envelope(
            "MESH-VER-002",
            Kind::Invalid,
            set(
                envelope_value(),
                "v",
                Value::from(u64::from(MESH_PROTOCOL_MIN_SUPPORTED) - 1),
            ),
            unsupported(Some(MESH_PROTOCOL_MIN_SUPPORTED - 1)),
        ),
        envelope(
            "MESH-VER-002",
            Kind::Invalid,
            set(
                envelope_value(),
                "v",
                Value::from(u64::from(MESH_PROTOCOL_VERSION) + 1),
            ),
            unsupported(Some(MESH_PROTOCOL_VERSION + 1)),
        ),
        envelope(
            "MESH-VER-004",
            Kind::Invalid,
            without(envelope_value(), "v"),
            unsupported(None),
        ),
        envelope(
            "MESH-VER-004",
            Kind::Invalid,
            map(vec![("name_hash", origin_bin(10)), ("body", body.clone())]),
            unsupported(None),
        ),
        envelope(
            "MESH-VER-005",
            Kind::Invalid,
            without(envelope_value(), "v"),
            unsupported(None),
        ),
        envelope(
            "MESH-VER-005",
            Kind::Invalid,
            with(without(envelope_value(), "v"), "V", Value::from(1u64)),
            unsupported(None),
        ),
        encode("MESH-ENV-018", Kind::Valid, body.clone(), expected.clone()),
        encode(
            "MESH-ENV-018",
            Kind::Valid,
            Value::Nil,
            set(expected.clone(), "body", Value::Nil),
        ),
        encode(
            "MESH-ENV-018",
            Kind::Valid,
            map(vec![("k", Value::from(1u64))]),
            set(
                expected.clone(),
                "body",
                map(vec![("k", Value::from(1u64))]),
            ),
        ),
        encode(
            "MESH-VER-003",
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
// Section 6.6: the dispatcher's stages
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

fn unknown_path_answer(path: &str) -> Answer {
    Answer::Value(
        DispatchError::UnknownPath {
            path_hash: PathHash::of(path).to_hex_string(),
        }
        .to_value(),
    )
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
    id: &'static str,
    kind: Kind,
    path: &'static str,
    trust: TrustShape,
    data: Value,
    expect: Answer,
    knock: Knock,
) -> Vector {
    row(
        id,
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

fn anonymous(id: &'static str, path: &'static str, trust: TrustShape, data: Value) -> Vector {
    row(
        id,
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
        anonymous("MESH-ENV-013", KNOCK_PATH, trusted, envelope_value()),
        anonymous("MESH-ENV-013", STATUS_PATH, trusted, envelope_value()),
        anonymous("MESH-ENV-013", MESSAGE_PATH, unknown, envelope_value()),
        anonymous("MESH-ENV-026", KNOCK_PATH, trusted, envelope_value()),
        anonymous("MESH-ENV-026", UNKNOWN_PATH, trusted, bad_version()),
        anonymous("MESH-ENV-026", STATUS_PATH, trusted, Value::Nil),
        dispatch(
            "MESH-ENV-027",
            Kind::Invalid,
            KNOCK_PATH,
            unknown,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-027",
            Kind::Invalid,
            STATUS_PATH,
            unknown,
            Value::Nil,
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-027",
            Kind::Invalid,
            KNOCK_PATH,
            blocked,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-027",
            Kind::Invalid,
            MESSAGE_PATH,
            blocked,
            Value::from("not a map"),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-027",
            Kind::Invalid,
            UNKNOWN_PATH,
            blocked,
            bad_version(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-027",
            Kind::Invalid,
            KNOCK_PATH,
            unknown,
            bad_version(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-029",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            bad_version(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-029",
            Kind::Invalid,
            STATUS_PATH,
            trusted,
            bad_version(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-029",
            Kind::Invalid,
            MESSAGE_PATH,
            trusted,
            without(envelope_value(), "v"),
            version_refused(None),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-029",
            Kind::Invalid,
            UNKNOWN_PATH,
            trusted,
            bad_version(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-029",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            without(envelope_value(), "v"),
            version_refused(None),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-029",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            set(envelope_value(), "v", Value::from(0u64)),
            version_refused(Some(0)),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-029",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            set(envelope_value(), "v", Value::from(65536u64)),
            version_refused(None),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-014",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            set(envelope_value(), "v", Value::from("1")),
            version_refused(None),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-030",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            Value::Nil,
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-030",
            Kind::Invalid,
            STATUS_PATH,
            trusted,
            Value::from("body"),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-030",
            Kind::Invalid,
            MESSAGE_PATH,
            trusted,
            Value::Array(vec![Value::from(1u64), origin_bin(10), body.clone()]),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-030",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            set(envelope_value(), "name_hash", origin_bin(9)),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-030",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            without(envelope_value(), "body"),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-015",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            without(envelope_value(), "name_hash"),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-015",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            set(envelope_value(), "name_hash", origin_bin(11)),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-016",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            without(envelope_value(), "body"),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-021",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            Value::Nil,
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-021",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            Value::Array(vec![Value::from(2u64)]),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-017",
            Kind::Valid,
            STATUS_PATH,
            trusted,
            with(envelope_value(), "extra", Value::Nil),
            no_provider_answer(STATUS_PATH),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-019",
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
            "MESH-ENV-020",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            garbage.clone(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-022",
            Kind::Valid,
            STATUS_PATH,
            trusted_for_destination,
            envelope_value(),
            no_provider_answer(STATUS_PATH),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-022",
            Kind::Valid,
            UNKNOWN_PATH,
            trusted_for_destination,
            envelope_value(),
            unknown_path_answer(UNKNOWN_PATH),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-022",
            Kind::Invalid,
            KNOCK_PATH,
            trusted_for_other_destination,
            envelope_value(),
            no_access_answer(),
            knocked_with_intro(),
        ),
        dispatch(
            "MESH-ENV-023",
            Kind::Invalid,
            KNOCK_PATH,
            trusted_for_other_destination,
            claimed.clone(),
            no_access_answer(),
            knocked_with_intro(),
        ),
        dispatch(
            "MESH-ENV-023",
            Kind::Invalid,
            STATUS_PATH,
            trusted_for_other_destination,
            claimed.clone(),
            no_access_answer(),
            knocked(),
        ),
        dispatch(
            "MESH-ENV-023",
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
            "MESH-ENV-031",
            Kind::Invalid,
            KNOCK_PATH,
            trusted_then_blocked,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-031",
            Kind::Invalid,
            STATUS_PATH,
            trusted_then_blocked,
            envelope_value(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-032",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            envelope_value(),
            no_access_answer(),
            knocked_with_intro(),
        ),
        dispatch(
            "MESH-ENV-032",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            set(envelope_value(), "body", Value::Nil),
            no_access_answer(),
            knocked_with_intro(),
        ),
        dispatch(
            "MESH-ENV-032",
            Kind::Invalid,
            STATUS_PATH,
            known_not_trusted,
            envelope_value(),
            no_access_answer(),
            knocked(),
        ),
        dispatch(
            "MESH-ENV-032",
            Kind::Invalid,
            MESSAGE_PATH,
            known_not_trusted,
            envelope_value(),
            no_access_answer(),
            knocked(),
        ),
        dispatch(
            "MESH-ENV-032",
            Kind::Invalid,
            UNKNOWN_PATH,
            known_not_trusted,
            envelope_value(),
            no_access_answer(),
            knocked(),
        ),
        dispatch(
            "MESH-ENV-033",
            Kind::Invalid,
            KNOCK_PATH,
            trusted_but_denied,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-033",
            Kind::Invalid,
            STATUS_PATH,
            trusted_but_denied,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-033",
            Kind::Invalid,
            UNKNOWN_PATH,
            trusted_but_denied,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-050",
            Kind::Invalid,
            KNOCK_PATH,
            known_while_another_key_holds_the_instance,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-050",
            Kind::Invalid,
            STATUS_PATH,
            known_while_another_key_holds_the_instance,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-050",
            Kind::Invalid,
            MESSAGE_PATH,
            known_while_another_key_holds_the_instance,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-050",
            Kind::Invalid,
            UNKNOWN_PATH,
            known_while_another_key_holds_the_instance,
            envelope_value(),
            no_access_answer(),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-034",
            Kind::Valid,
            UNKNOWN_PATH,
            trusted,
            envelope_value(),
            unknown_path_answer(UNKNOWN_PATH),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-034",
            Kind::Valid,
            "/status/",
            trusted,
            envelope_value(),
            unknown_path_answer("/status/"),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-035",
            Kind::Valid,
            STATUS_PATH,
            trusted,
            envelope_value(),
            no_provider_answer(STATUS_PATH),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-035",
            Kind::Valid,
            MESSAGE_PATH,
            trusted,
            envelope_value(),
            no_provider_answer(MESSAGE_PATH),
            Knock::None,
        ),
        dispatch(
            "MESH-ENV-036",
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
                "MESH-ENV-036",
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
                "MESH-ENV-036",
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
                "MESH-ENV-036",
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
                "MESH-ENV-036",
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
                "MESH-ENV-036",
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
                "MESH-ENV-036",
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
                "MESH-ENV-036",
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
                "MESH-ENV-036",
                Kind::Valid,
                ACCESS_PATH,
                trusted,
                envelope_value(),
                Answer::Value(body.clone()),
                Knock::None,
            ),
        ),
        dispatch(
            "MESH-ENV-036",
            Kind::Valid,
            FETCH_PATH,
            trusted,
            envelope_value(),
            no_provider_answer(FETCH_PATH),
            Knock::None,
        ),
        dispatch(
            "MESH-VER-012",
            Kind::Invalid,
            KNOCK_PATH,
            blocked,
            bad_version(),
            Answer::Silent,
            Knock::None,
        ),
        dispatch(
            "MESH-VER-012",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            garbage.clone(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "MESH-VER-012",
            Kind::Invalid,
            STATUS_PATH,
            trusted,
            garbage.clone(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "MESH-VER-012",
            Kind::Invalid,
            MESSAGE_PATH,
            trusted,
            garbage.clone(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "MESH-VER-012",
            Kind::Invalid,
            UNKNOWN_PATH,
            trusted,
            garbage,
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "MESH-VER-012",
            Kind::Invalid,
            KNOCK_PATH,
            known_not_trusted,
            bad_version(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "MESH-VER-012",
            Kind::Invalid,
            KNOCK_PATH,
            trusted_but_denied,
            bad_version(),
            version_refused(Some(2)),
            Knock::None,
        ),
        dispatch(
            "MESH-VER-005",
            Kind::Invalid,
            KNOCK_PATH,
            trusted,
            without(envelope_value(), "v"),
            version_refused(None),
            Knock::None,
        ),
        dispatch(
            "MESH-VER-001",
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
/// destination.
fn authorize_rotated_origin(list: fn(&str, &str, &str, &str) -> TrustList) -> Verdict {
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
    store.authorize_origin(&identity, &ORIGIN).0
}

const IDENTITY: &str = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";
const DESTINATION: &str = "d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1";

fn custom(id: &'static str, kind: Kind, check: fn() -> Result<(), String>) -> Vector {
    row(id, kind, Case::Custom(check))
}

fn custom_vectors() -> Vec<Vector> {
    vec![
        custom("MESH-ENV-011", Kind::Boundary, || {
            same("MAX_R3_PAYLOAD_BYTES", MAX_R3_PAYLOAD_BYTES, 262_144)
        }),
        custom("MESH-ENV-025", Kind::Boundary, || {
            same(
                "MAX_CONCURRENT_INBOUND_REQUESTS",
                MAX_CONCURRENT_INBOUND_REQUESTS,
                16,
            )
        }),
        custom("MESH-ENV-037", Kind::Boundary, || {
            same("HANDLER_TIMEOUT", HANDLER_TIMEOUT, Duration::from_secs(20))
        }),
        custom("MESH-ENV-013", Kind::Invalid, || {
            let (store, _tmp) = TrustList::default().open("conformance-env");
            let dispatcher = Dispatcher::new(store, Arc::new(SpySink::default()));
            let admission = dispatcher.admit(LinkId::new_from_rand(OsRng), None);
            ensure(
                matches!(admission, Admission::Drop),
                "an anonymous link was admitted",
            )
        }),
        custom("MESH-ENV-013", Kind::Valid, || {
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
        custom("MESH-ENV-027", Kind::Invalid, || {
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
        }),
        custom("MESH-ENV-038", Kind::Valid, || {
            let frame = ResponseFrame {
                request_id: RequestId::from(REQUEST_ID),
                data: no_access().to_wire(),
            };
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
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "deny over block",
                authorize(TrustList::default().block(IDENTITY).deny(DESTINATION)),
                verdict(Decision::Refuse, Rule::DestinationDenied),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "deny over destination allow",
                authorize(
                    TrustList::default()
                        .destination(DESTINATION, IDENTITY)
                        .deny(DESTINATION),
                ),
                verdict(Decision::Refuse, Rule::DestinationDenied),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
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
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "block over destination allow",
                authorize(
                    TrustList::default()
                        .destination(DESTINATION, IDENTITY)
                        .block(IDENTITY),
                ),
                verdict(Decision::Refuse, Rule::IdentityBlocked),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
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
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "destination allow over identity allow",
                authorize(
                    TrustList::default()
                        .identity(IDENTITY, true)
                        .destination(DESTINATION, IDENTITY),
                ),
                verdict(Decision::Allow, Rule::DestinationTrusted),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "destination allow alone",
                authorize(TrustList::default().destination(DESTINATION, IDENTITY)),
                verdict(Decision::Allow, Rule::DestinationTrusted),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "identity allow alone",
                authorize(TrustList::default().identity(IDENTITY, true)),
                verdict(Decision::Allow, Rule::IdentityTrusted),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "identity allow over default closed",
                authorize(
                    TrustList::default()
                        .identity(IDENTITY, true)
                        .destination(OTHER_DESTINATION, IDENTITY),
                ),
                verdict(Decision::Allow, Rule::IdentityTrusted),
            )
        }),
        custom("MESH-ENV-039", Kind::Invalid, || {
            same(
                "default closed",
                authorize(TrustList::default()),
                verdict(Decision::Refuse, Rule::DefaultClosed),
            )
        }),
        custom("MESH-ENV-039", Kind::Invalid, || {
            same(
                "default closed for another destination",
                authorize(TrustList::default().destination(OTHER_DESTINATION, IDENTITY)),
                verdict(Decision::Refuse, Rule::DefaultClosed),
            )
        }),
        custom("MESH-ENV-039", Kind::Invalid, || {
            same(
                "default closed without all_destinations",
                authorize(TrustList::default().identity(IDENTITY, false)),
                verdict(Decision::Refuse, Rule::DefaultClosed),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "deny over identity changed",
                authorize_rotated_origin(|_, derived, holder, held| {
                    TrustList::default().deny(derived).destination(held, holder)
                }),
                verdict(Decision::Refuse, Rule::DestinationDenied),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "block over identity changed",
                authorize_rotated_origin(|identity, _, holder, held| {
                    TrustList::default()
                        .block(identity)
                        .destination(held, holder)
                }),
                verdict(Decision::Refuse, Rule::IdentityBlocked),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "destination allow over identity changed",
                authorize_rotated_origin(|identity, derived, holder, held| {
                    TrustList::default()
                        .destination(derived, identity)
                        .destination(held, holder)
                }),
                verdict(Decision::Allow, Rule::DestinationTrusted),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "identity allow over identity changed",
                authorize_rotated_origin(|identity, _, holder, held| {
                    TrustList::default()
                        .identity(identity, true)
                        .destination(held, holder)
                }),
                verdict(Decision::Allow, Rule::IdentityTrusted),
            )
        }),
        custom("MESH-ENV-039", Kind::Valid, || {
            same(
                "identity changed over default closed",
                authorize_rotated_origin(|identity, _, holder, held| {
                    TrustList::default()
                        .identity(identity, false)
                        .destination(held, holder)
                }),
                verdict(Decision::Refuse, Rule::IdentityChanged),
            )
        }),
        custom("MESH-ENV-039", Kind::Invalid, || {
            same(
                "denied held record is default closed",
                authorize_rotated_origin(|identity, _, holder, held| {
                    TrustList::default()
                        .identity(identity, false)
                        .deny(held)
                        .destination(held, holder)
                }),
                verdict(Decision::Refuse, Rule::DefaultClosed),
            )
        }),
        custom("MESH-ENV-044", Kind::Valid, || {
            same(
                "bare uint",
                packed(&no_access().to_wire()),
                vec![0xcc, 0xf1],
            )
        }),
        custom("MESH-ENV-044", Kind::Valid, || {
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
        custom("MESH-VER-002", Kind::Boundary, || {
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
        }),
        custom("MESH-VER-003", Kind::Valid, || {
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
// Section 7: the version refusal
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

fn version(id: &'static str, kind: Kind, value: Value, expect: Option<VersionRefusal>) -> Vector {
    row(id, kind, Case::VersionRefusalDecode { value, expect })
}

fn version_encode(id: &'static str, found: Option<u16>, expect: Value) -> Vector {
    row(
        id,
        Kind::Valid,
        Case::VersionRefusalEncode { found, expect },
    )
}

fn version_refusal_vectors() -> Vec<Vector> {
    vec![
        version(
            "MESH-VER-006",
            Kind::Valid,
            refusal_value(Value::Nil),
            refusal(None),
        ),
        version(
            "MESH-VER-006",
            Kind::Valid,
            refusal_value(Value::from(2u64)),
            refusal(Some(2)),
        ),
        version(
            "MESH-VER-006",
            Kind::Invalid,
            set(
                refusal_value(Value::Nil),
                "refusal",
                Value::from("Unsupported_Version"),
            ),
            None,
        ),
        version(
            "MESH-VER-006",
            Kind::Invalid,
            set(
                refusal_value(Value::Nil),
                "refusal",
                Value::from("unsupported_version "),
            ),
            None,
        ),
        version(
            "MESH-VER-006",
            Kind::Invalid,
            set(
                refusal_value(Value::Nil),
                "refusal",
                Value::Binary(b"unsupported_version".to_vec()),
            ),
            None,
        ),
        version(
            "MESH-VER-006",
            Kind::Invalid,
            set(
                refusal_value(Value::Nil),
                "refusal",
                Value::from("no_access"),
            ),
            None,
        ),
        version(
            "MESH-VER-006",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "refusal", Value::Nil),
            None,
        ),
        version(
            "MESH-VER-006",
            Kind::Invalid,
            without(refusal_value(Value::Nil), "refusal"),
            None,
        ),
        version(
            "MESH-VER-006",
            Kind::Invalid,
            Value::from("unsupported_version"),
            None,
        ),
        version("MESH-VER-006", Kind::Invalid, Value::Nil, None),
        version("MESH-VER-006", Kind::Invalid, Value::from(0xf1u8), None),
        version(
            "MESH-VER-007",
            Kind::Valid,
            refusal_value(Value::Nil),
            refusal(None),
        ),
        version(
            "MESH-VER-007",
            Kind::Boundary,
            refusal_value(Value::from(0u64)),
            refusal(Some(0)),
        ),
        version(
            "MESH-VER-007",
            Kind::Boundary,
            refusal_value(Value::from(65535u64)),
            refusal(Some(65535)),
        ),
        version(
            "MESH-VER-007",
            Kind::Invalid,
            without(refusal_value(Value::Nil), "found"),
            None,
        ),
        version(
            "MESH-VER-007",
            Kind::Invalid,
            refusal_value(Value::from("2")),
            None,
        ),
        version(
            "MESH-VER-007",
            Kind::Invalid,
            refusal_value(Value::from(65536u64)),
            None,
        ),
        version(
            "MESH-VER-007",
            Kind::Invalid,
            refusal_value(Value::from(-1i64)),
            None,
        ),
        version(
            "MESH-VER-007",
            Kind::Invalid,
            refusal_value(Value::F64(2.0)),
            None,
        ),
        version(
            "MESH-VER-007",
            Kind::Invalid,
            refusal_value(Value::Boolean(false)),
            None,
        ),
        version(
            "MESH-VER-008",
            Kind::Boundary,
            set(refusal_value(Value::Nil), "min", Value::from(65535u64)),
            Some(VersionRefusal {
                found: None,
                min: 65535,
                max: 1,
            }),
        ),
        version(
            "MESH-VER-008",
            Kind::Invalid,
            without(refusal_value(Value::Nil), "min"),
            None,
        ),
        version(
            "MESH-VER-008",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "min", Value::from("1")),
            None,
        ),
        version(
            "MESH-VER-008",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "min", Value::Nil),
            None,
        ),
        version(
            "MESH-VER-008",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "min", Value::from(65536u64)),
            None,
        ),
        version(
            "MESH-VER-008",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "min", Value::from(-1i64)),
            None,
        ),
        version(
            "MESH-VER-009",
            Kind::Boundary,
            set(refusal_value(Value::Nil), "max", Value::from(65535u64)),
            Some(VersionRefusal {
                found: None,
                min: 1,
                max: 65535,
            }),
        ),
        version(
            "MESH-VER-009",
            Kind::Invalid,
            without(refusal_value(Value::Nil), "max"),
            None,
        ),
        version(
            "MESH-VER-009",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "max", Value::from("1")),
            None,
        ),
        version(
            "MESH-VER-009",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "max", Value::Nil),
            None,
        ),
        version(
            "MESH-VER-009",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "max", Value::from(65536u64)),
            None,
        ),
        version(
            "MESH-VER-009",
            Kind::Invalid,
            set(refusal_value(Value::Nil), "max", Value::F64(1.0)),
            None,
        ),
        version(
            "MESH-VER-010",
            Kind::Valid,
            with(refusal_value(Value::Nil), "reason", Value::from("too old")),
            refusal(None),
        ),
        version(
            "MESH-VER-010",
            Kind::Valid,
            with(
                refusal_value(Value::from(3u64)),
                "error",
                Value::from("unknown_path"),
            ),
            refusal(Some(3)),
        ),
        version(
            "MESH-VER-010",
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
        version_encode("MESH-VER-011", None, refusal_value(Value::Nil)),
        version_encode("MESH-VER-011", Some(2), refusal_value(Value::from(2u64))),
        version_encode("MESH-VER-011", Some(0), refusal_value(Value::from(0u64))),
        version_encode(
            "MESH-VER-011",
            Some(65535),
            refusal_value(Value::from(65535u64)),
        ),
        version_encode(
            "MESH-VER-002",
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
            "MESH-VER-002",
            Kind::Valid,
            VersionRefusal::current(None).to_value(),
            Some(VersionRefusal {
                found: None,
                min: MESH_PROTOCOL_MIN_SUPPORTED,
                max: MESH_PROTOCOL_VERSION,
            }),
        ),
        version("MESH-ENV-046", Kind::Invalid, Value::from(0xf1u8), None),
        version(
            "MESH-ENV-046",
            Kind::Invalid,
            DispatchError::UnknownPath {
                path_hash: PathHash::of(UNKNOWN_PATH).to_hex_string(),
            }
            .to_value(),
            None,
        ),
        version(
            "MESH-ENV-046",
            Kind::Invalid,
            DispatchError::NoProvider {
                path: STATUS_PATH.to_string(),
            }
            .to_value(),
            None,
        ),
        version(
            "MESH-ENV-046",
            Kind::Valid,
            VersionRefusal::current(Some(2)).to_value(),
            refusal(Some(2)),
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Section 6.7: dispatch errors and refusal codes on the requester's side
// ---------------------------------------------------------------------------------------

const HEX_32: &str = "0123456789abcdef0123456789abcdef";

fn unknown_path_value(path_hash: Value) -> Value {
    map(vec![
        ("error", Value::from("unknown_path")),
        ("path_hash", path_hash),
    ])
}

fn no_provider_value(path: Value) -> Value {
    map(vec![("error", Value::from("no_provider")), ("path", path)])
}

fn unknown_path(path_hash: &str) -> Option<DispatchError> {
    Some(DispatchError::UnknownPath {
        path_hash: path_hash.to_string(),
    })
}

fn no_provider(path: &str) -> Option<DispatchError> {
    Some(DispatchError::NoProvider {
        path: path.to_string(),
    })
}

fn dispatch_error(
    id: &'static str,
    kind: Kind,
    value: Value,
    expect: Option<DispatchError>,
) -> Vector {
    row(id, kind, Case::DispatchErrorDecode { value, expect })
}

fn dispatch_error_vectors() -> Vec<Vector> {
    let upper = HEX_32.to_ascii_uppercase();
    vec![
        dispatch_error(
            "MESH-ENV-040",
            Kind::Valid,
            unknown_path_value(Value::from(HEX_32)),
            unknown_path(HEX_32),
        ),
        dispatch_error(
            "MESH-ENV-040",
            Kind::Valid,
            no_provider_value(Value::from(STATUS_PATH)),
            no_provider(STATUS_PATH),
        ),
        dispatch_error(
            "MESH-ENV-040",
            Kind::Invalid,
            map(vec![("path_hash", Value::from(HEX_32))]),
            None,
        ),
        dispatch_error(
            "MESH-ENV-040",
            Kind::Invalid,
            set(
                unknown_path_value(Value::from(HEX_32)),
                "error",
                Value::from("not_found"),
            ),
            None,
        ),
        dispatch_error(
            "MESH-ENV-040",
            Kind::Invalid,
            set(
                unknown_path_value(Value::from(HEX_32)),
                "error",
                Value::from("Unknown_Path"),
            ),
            None,
        ),
        dispatch_error(
            "MESH-ENV-040",
            Kind::Invalid,
            set(
                unknown_path_value(Value::from(HEX_32)),
                "error",
                Value::Binary(b"unknown_path".to_vec()),
            ),
            None,
        ),
        dispatch_error(
            "MESH-ENV-040",
            Kind::Invalid,
            set(
                unknown_path_value(Value::from(HEX_32)),
                "error",
                Value::from(1u64),
            ),
            None,
        ),
        dispatch_error(
            "MESH-ENV-040",
            Kind::Invalid,
            map(vec![("error", Value::from("unknown_path"))]),
            None,
        ),
        dispatch_error(
            "MESH-ENV-040",
            Kind::Invalid,
            map(vec![("error", Value::from("no_provider"))]),
            None,
        ),
        dispatch_error(
            "MESH-ENV-040",
            Kind::Invalid,
            Value::from("unknown_path"),
            None,
        ),
        dispatch_error("MESH-ENV-040", Kind::Invalid, Value::Nil, None),
        dispatch_error("MESH-ENV-040", Kind::Invalid, Value::from(0xf1u8), None),
        dispatch_error(
            "MESH-ENV-040",
            Kind::Invalid,
            VersionRefusal::current(None).to_value(),
            None,
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Boundary,
            unknown_path_value(Value::from(HEX_32)),
            unknown_path(HEX_32),
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Valid,
            unknown_path_value(Value::from(upper.as_str())),
            unknown_path(&upper),
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Valid,
            unknown_path_value(Value::from(PathHash::of(UNKNOWN_PATH).to_hex_string())),
            unknown_path(&PathHash::of(UNKNOWN_PATH).to_hex_string()),
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Invalid,
            unknown_path_value(Value::from(&HEX_32[..31])),
            None,
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Invalid,
            unknown_path_value(Value::from(format!("{HEX_32}0"))),
            None,
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Invalid,
            unknown_path_value(Value::from("0123456789abcdef0123456789abcdeg")),
            None,
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Invalid,
            unknown_path_value(Value::from("0123456789abcdef0123456789abcde ")),
            None,
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Invalid,
            unknown_path_value(Value::from("")),
            None,
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Invalid,
            unknown_path_value(Value::Binary(vec![1; 16])),
            None,
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Invalid,
            unknown_path_value(Value::Binary(HEX_32.as_bytes().to_vec())),
            None,
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Invalid,
            unknown_path_value(Value::Nil),
            None,
        ),
        dispatch_error(
            "MESH-ENV-041",
            Kind::Invalid,
            map(vec![
                ("error", Value::from("unknown_path")),
                ("path", Value::from(STATUS_PATH)),
            ]),
            None,
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Valid,
            no_provider_value(Value::from(KNOCK_PATH)),
            no_provider(KNOCK_PATH),
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Valid,
            no_provider_value(Value::from(STATUS_PATH)),
            no_provider(STATUS_PATH),
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Valid,
            no_provider_value(Value::from(MESSAGE_PATH)),
            no_provider(MESSAGE_PATH),
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Valid,
            no_provider_value(Value::from(LIST_PATH)),
            no_provider(LIST_PATH),
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Valid,
            no_provider_value(Value::from(FETCH_PATH)),
            no_provider(FETCH_PATH),
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Valid,
            no_provider_value(Value::from(ACCESS_PATH)),
            no_provider(ACCESS_PATH),
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Invalid,
            no_provider_value(Value::from("/other")),
            None,
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Invalid,
            no_provider_value(Value::from("/Status")),
            None,
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Invalid,
            no_provider_value(Value::from("status")),
            None,
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Invalid,
            no_provider_value(Value::from("/status/")),
            None,
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Invalid,
            no_provider_value(Value::from("")),
            None,
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Invalid,
            no_provider_value(Value::Binary(STATUS_PATH.as_bytes().to_vec())),
            None,
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Invalid,
            no_provider_value(Value::Nil),
            None,
        ),
        dispatch_error(
            "MESH-ENV-042",
            Kind::Invalid,
            map(vec![
                ("error", Value::from("no_provider")),
                ("path_hash", Value::from(HEX_32)),
            ]),
            None,
        ),
        dispatch_error(
            "MESH-ENV-043",
            Kind::Valid,
            with(
                unknown_path_value(Value::from(HEX_32)),
                "detail",
                Value::from("ignored"),
            ),
            unknown_path(HEX_32),
        ),
        dispatch_error(
            "MESH-ENV-043",
            Kind::Valid,
            with(
                no_provider_value(Value::from(MESSAGE_PATH)),
                "path_hash",
                Value::from(HEX_32),
            ),
            no_provider(MESSAGE_PATH),
        ),
        dispatch_error(
            "MESH-ENV-043",
            Kind::Valid,
            with(
                unknown_path_value(Value::from(HEX_32)),
                "path",
                Value::from("/other"),
            ),
            unknown_path(HEX_32),
        ),
        dispatch_error(
            "MESH-ENV-043",
            Kind::Valid,
            Value::Map(
                [
                    vec![(Value::from(0u64), Value::Nil)],
                    unknown_path_value(Value::from(HEX_32))
                        .as_map()
                        .unwrap()
                        .to_vec(),
                ]
                .concat(),
            ),
            unknown_path(HEX_32),
        ),
        dispatch_error("MESH-ENV-046", Kind::Invalid, Value::from(0xf1u8), None),
        dispatch_error(
            "MESH-ENV-046",
            Kind::Invalid,
            VersionRefusal::current(Some(2)).to_value(),
            None,
        ),
        dispatch_error(
            "MESH-ENV-046",
            Kind::Valid,
            DispatchError::UnknownPath {
                path_hash: HEX_32.to_string(),
            }
            .to_value(),
            unknown_path(HEX_32),
        ),
    ]
}

fn code(id: &'static str, kind: Kind, value: Value, expect: Option<Value>) -> Vector {
    row(id, kind, Case::RefusalCodeDecode { value, expect })
}

fn wire(byte: u8) -> Option<Value> {
    Some(Value::from(byte))
}

fn refusal_code_vectors() -> Vec<Vector> {
    vec![
        code("MESH-ENV-045", Kind::Valid, Value::from(0xf0u8), wire(0xf0)),
        code("MESH-ENV-045", Kind::Valid, Value::from(0xf1u8), wire(0xf1)),
        code("MESH-ENV-045", Kind::Valid, Value::from(0xf3u8), wire(0xf3)),
        code("MESH-ENV-045", Kind::Valid, Value::from(0xf4u8), wire(0xf4)),
        code("MESH-ENV-045", Kind::Valid, Value::from(0xf5u8), wire(0xf5)),
        code("MESH-ENV-045", Kind::Valid, Value::from(0xf6u8), wire(0xf6)),
        code("MESH-ENV-045", Kind::Valid, Value::from(0xfdu8), wire(0xfd)),
        code("MESH-ENV-045", Kind::Valid, Value::from(0xfeu8), wire(0xfe)),
        code(
            "MESH-ENV-045",
            Kind::Valid,
            unpacked(&[0xcc, 0xf1]),
            wire(0xf1),
        ),
        code(
            "MESH-ENV-045",
            Kind::Valid,
            Value::from(0xf1u64),
            wire(0xf1),
        ),
        code(
            "MESH-ENV-045",
            Kind::Valid,
            no_access().to_wire(),
            wire(0xf1),
        ),
        code("MESH-ENV-046", Kind::Valid, Value::from(0xf1u8), wire(0xf1)),
        code(
            "MESH-ENV-046",
            Kind::Invalid,
            VersionRefusal::current(None).to_value(),
            None,
        ),
        code(
            "MESH-ENV-046",
            Kind::Invalid,
            VersionRefusal::current(Some(2)).to_value(),
            None,
        ),
        code(
            "MESH-ENV-046",
            Kind::Invalid,
            DispatchError::UnknownPath {
                path_hash: HEX_32.to_string(),
            }
            .to_value(),
            None,
        ),
        code(
            "MESH-ENV-046",
            Kind::Invalid,
            DispatchError::NoProvider {
                path: STATUS_PATH.to_string(),
            }
            .to_value(),
            None,
        ),
        code("MESH-ENV-046", Kind::Invalid, Value::from("body"), None),
        code("MESH-ENV-046", Kind::Invalid, Value::Nil, None),
        code("MESH-ENV-047", Kind::Invalid, Value::from(0xf2u8), None),
        code("MESH-ENV-047", Kind::Invalid, Value::from(0xf7u8), None),
        code("MESH-ENV-047", Kind::Invalid, Value::from(0xfcu8), None),
        code("MESH-ENV-047", Kind::Invalid, Value::from(0xffu8), None),
        code("MESH-ENV-047", Kind::Invalid, Value::from(0u8), None),
        code("MESH-ENV-047", Kind::Invalid, Value::from(1u8), None),
        code("MESH-ENV-047", Kind::Invalid, Value::from(0xefu8), None),
        code("MESH-ENV-047", Kind::Invalid, Value::from(0x1f1u64), None),
        code("MESH-ENV-047", Kind::Invalid, Value::from(-0xf1i64), None),
        code("MESH-ENV-047", Kind::Invalid, Value::F64(241.0), None),
        code("MESH-ENV-047", Kind::Invalid, Value::from("0xf1"), None),
        code(
            "MESH-ENV-047",
            Kind::Invalid,
            Value::Binary(vec![0xf1]),
            None,
        ),
        code(
            "MESH-ENV-047",
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

    const TESTED: [&str; 10] = [
        "RequestFrameDecode",
        "ResponseFrameDecode",
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
    fn request_frame_vectors_decode_as_section_6_1_mandates() {
        run_family("RequestFrameDecode");
    }

    #[test]
    fn response_frame_vectors_decode_as_section_6_2_mandates() {
        run_family("ResponseFrameDecode");
    }

    #[test]
    fn envelope_vectors_decode_as_section_6_5_mandates() {
        run_family("EnvelopeDecode");
    }

    #[test]
    fn envelope_vectors_encode_in_the_key_order_of_section_6_5() {
        run_family("EnvelopeEncode");
    }

    #[test]
    fn dispatch_vectors_answer_as_section_6_6_mandates() {
        run_family("Dispatch");
    }

    #[test]
    fn version_refusal_vectors_hold_the_shape_of_section_7() {
        run_family("VersionRefusalDecode");
        run_family("VersionRefusalEncode");
    }

    #[test]
    fn dispatch_error_vectors_read_as_section_6_7_mandates() {
        run_family("DispatchErrorDecode");
    }

    #[test]
    fn refusal_code_vectors_decode_as_section_6_7_mandates() {
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
}
