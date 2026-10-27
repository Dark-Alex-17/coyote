use crate::mesh::protocol::{MESH_PROTOCOL_VERSION, protocol_supported};
#[cfg(test)]
use crate::mesh::request::error::RequestError;
#[cfg(test)]
use crate::mesh::request::redact_hashes;

use rmpv::Value;
pub(crate) use rns_transport::destination::request::{PathHash, RequestId};
use rns_transport::destination::{DestinationName, NAME_HASH_LENGTH};
#[cfg(test)]
use rns_transport::hash::ADDRESS_HASH_SIZE;
#[cfg(test)]
use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};

/// Frames above this are refused: outbound before anything touches the wire, inbound by the
/// node's destination, which is armed with it, so the library refuses an oversize request
/// at advertisement time before any of it transfers and drops an oversize packet-form one
/// before any handler runs.
pub(crate) const MAX_REQUEST_PAYLOAD_BYTES: usize = 256 * 1024;

/// The bound on a `/fetch` RESPONSE alone: the largest file a node serves plus room for the
/// frame and the reply's other keys. Every other path, and every request, keeps
/// `MAX_REQUEST_PAYLOAD_BYTES`. Like that bound it is enforced after assembly: the requester
/// discards an over-limit response once it has been put together, and the upstream 64 MiB
/// advertisement cap is the only pre-assembly bound.
pub(crate) const MAX_FETCH_RESPONSE_BYTES: usize =
    (crate::config::mesh_config::MAX_FETCH_FILE_BYTES as usize) + 4096;

/// The `rmpv` depth budget Coyote's own decoder spends before it refuses a frame as
/// undecodable: on the responses a requester reads, and in tests on the round trip of the
/// request frames the requester builds. Inbound request frames never meet it; their
/// rejection is the library's rule, 128 nested containers counted before any value is
/// built. Each container here costs two, a `bin` leaf two and a `str` leaf three
/// (`read_str_data` charges one before it reaches `read_bin_data`), so about 60 nested
/// containers fit. `rmpv`'s own default (1024) lets ~400 nested arrays exhaust a 2 MiB
/// thread stack in a debug build before the limit is reached.
#[cfg(test)]
pub(crate) const MAX_REQUEST_NESTING_DEPTH: usize = 128;

/// Bytes of a destination name hash, the prefix of the full name hash Reticulum uses to
/// derive a destination address.
pub(crate) const NAME_HASH_LEN: usize = NAME_HASH_LENGTH;

/// The fixed opening of every encoded `ResponseFrame`: a 2-element array header and the
/// `bin 8` header of its 16-byte request id. The requester reads a response's id straight
/// off these bytes before it decodes anything.
#[cfg(test)]
pub(crate) const RESPONSE_FRAME_PREFIX: [u8; 3] = [0x92, 0xc4, 0x10];

const NAME_HASH_KEY: &str = "name_hash";
const BODY_KEY: &str = "body";
const VERSION_KEY: &str = "v";

/// One request as it travels over a link, in either direction.
///
/// Wire form: a msgpack array of exactly 3 elements. Element 0 is a msgpack float64
/// (`0xcb` + 8 bytes big-endian IEEE 754) holding seconds since the UNIX epoch. Element 1
/// is a msgpack bin of exactly 16 bytes (`0xc4 0x10` + 16 bytes) holding the path hash.
/// Element 2 is the body as its own msgpack value: a map, an array, an integer, a bin; nil
/// when there is no body, never an empty bin. The frame is the whole payload; decoding
/// refuses trailing bytes, any other arity, and a hash that is not a 16-byte bin. This is
/// what RNS `Link.request` packs as `umsgpack.packb([time.time(), request_path_hash, data])`.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RequestFrame {
    pub time: f64,
    pub path_hash: PathHash,
    pub data: Value,
}

#[cfg(test)]
impl RequestFrame {
    pub(crate) fn new(path: &str, data: Value) -> Self {
        Self {
            time: now_secs(),
            path_hash: PathHash::of(path),
            data,
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        encode_value(Value::Array(vec![
            Value::F64(self.time),
            Value::Binary(self.path_hash.to_bytes().to_vec()),
            self.data.clone(),
        ]))
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, RequestError> {
        let Value::Array(elements) = decode_whole(bytes)? else {
            return Err(RequestError::Decode("request frame is not an array".into()));
        };
        let [time, path_hash, data] = <[Value; 3]>::try_from(elements).map_err(|elements| {
            RequestError::Decode(format!(
                "request frame has {} elements, expected 3",
                elements.len()
            ))
        })?;
        let Value::F64(time) = time else {
            return Err(RequestError::Decode("request time is not a float64".into()));
        };
        Ok(Self {
            time,
            path_hash: PathHash::from(hash_bytes(&path_hash, "request path hash")?),
            data,
        })
    }
}

/// One response as it travels over a link, in either direction.
///
/// Wire form: a msgpack array of exactly 2 elements. Element 0 is a msgpack bin of exactly
/// 16 bytes holding the request id the response answers. Element 1 is the body as its own
/// msgpack value, nil for no body. Decoding refuses trailing bytes, any other arity, and an
/// id that is not a 16-byte bin. This is what RNS `Link.handle_request` packs as
/// `umsgpack.packb([request_id, response])`.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResponseFrame {
    pub request_id: RequestId,
    pub data: Value,
}

#[cfg(test)]
impl ResponseFrame {
    pub(crate) fn encode(&self) -> Vec<u8> {
        encode_value(Value::Array(vec![
            Value::Binary(self.request_id.to_bytes().to_vec()),
            self.data.clone(),
        ]))
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, RequestError> {
        let Value::Array(elements) = decode_whole(bytes)? else {
            return Err(RequestError::Decode(
                "response frame is not an array".into(),
            ));
        };
        let [request_id, data] = <[Value; 2]>::try_from(elements).map_err(|elements| {
            RequestError::Decode(format!(
                "response frame has {} elements, expected 2",
                elements.len()
            ))
        })?;
        Ok(Self {
            request_id: RequestId::from(hash_bytes(&request_id, "response request id")?),
            data,
        })
    }
}

/// The name hash of the instance a requester speaks for. It names nothing on its own: the
/// dispatcher derives the destination from it and the identity proven on the link, so a
/// peer can only ever claim an instance of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OriginName(pub [u8; NAME_HASH_LEN]);

impl OriginName {
    pub(crate) fn of(name: &DestinationName) -> Self {
        let mut bytes = [0; NAME_HASH_LEN];
        bytes.copy_from_slice(name.as_name_hash_slice());
        Self(bytes)
    }
}

/// A msgpack map with `v`, `name_hash` and `body` keys; every request body travels in one.
pub(crate) struct Envelope {
    pub version: u16,
    pub origin: OriginName,
    pub body: Value,
}

/// Why a value is not an envelope this node will read. The version is judged first, so a
/// peer on another version hears that and never a verdict on a body it may have shaped
/// differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnvelopeError {
    /// `found` is `None` when the key is missing or not an unsigned 16-bit integer.
    UnsupportedVersion {
        found: Option<u16>,
    },
    Malformed,
}

impl Envelope {
    /// An envelope at the protocol version this node speaks; production code builds no other.
    pub(crate) fn new(origin: OriginName, body: Value) -> Self {
        Self {
            version: MESH_PROTOCOL_VERSION,
            origin,
            body,
        }
    }

    pub(crate) fn into_value(self) -> Value {
        Value::Map(vec![
            (Value::from(VERSION_KEY), Value::from(self.version)),
            (
                Value::from(NAME_HASH_KEY),
                Value::Binary(self.origin.0.to_vec()),
            ),
            (Value::from(BODY_KEY), self.body),
        ])
    }

    /// Keys this version does not know are ignored. A missing `v` is refused, not read as
    /// version 1: this is the version-1 rule, and pre-release nothing older exists to be
    /// lenient toward.
    pub(crate) fn from_value(value: Value) -> Result<Self, EnvelopeError> {
        let Value::Map(entries) = value else {
            return Err(EnvelopeError::Malformed);
        };
        let mut version = None;
        let mut origin = None;
        let mut body = None;
        for (key, value) in entries {
            match key.as_str() {
                Some(VERSION_KEY) => {
                    version = value.as_u64().and_then(|v| u16::try_from(v).ok());
                }
                Some(NAME_HASH_KEY) => origin = Some(value),
                Some(BODY_KEY) => body = Some(value),
                _ => {}
            }
        }
        let version = match version {
            Some(version) if protocol_supported(version) => version,
            found => return Err(EnvelopeError::UnsupportedVersion { found }),
        };
        let Some(Value::Binary(bytes)) = origin else {
            return Err(EnvelopeError::Malformed);
        };
        let origin = OriginName(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| EnvelopeError::Malformed)?,
        );
        Ok(Self {
            version,
            origin,
            body: body.ok_or(EnvelopeError::Malformed)?,
        })
    }
}

pub(super) fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
fn encode_value(value: Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    // The only error source is the writer, and a `Vec` never fails to grow.
    if let Err(err) = rmpv::encode::write_value(&mut bytes, &value) {
        warn!(
            "Failed to encode a mesh request frame: {}",
            redact_hashes(&err.to_string())
        );
    }
    bytes
}

#[cfg(test)]
fn decode_whole(bytes: &[u8]) -> Result<Value, RequestError> {
    let mut cursor = Cursor::new(bytes);
    let value = rmpv::decode::read_value_with_max_depth(&mut cursor, MAX_REQUEST_NESTING_DEPTH)
        .map_err(|err| RequestError::Decode(err.to_string()))?;
    let consumed = usize::try_from(cursor.position()).unwrap_or(usize::MAX);
    if consumed != bytes.len() {
        return Err(RequestError::Decode(format!(
            "{} trailing bytes after the frame",
            bytes.len().saturating_sub(consumed)
        )));
    }
    Ok(value)
}

#[cfg(test)]
fn hash_bytes(value: &Value, what: &str) -> Result<[u8; ADDRESS_HASH_SIZE], RequestError> {
    let Value::Binary(bytes) = value else {
        return Err(RequestError::Decode(format!("{what} is not a bin")));
    };
    <[u8; ADDRESS_HASH_SIZE]>::try_from(bytes.as_slice()).map_err(|_| {
        RequestError::Decode(format!(
            "{what} is {} bytes, expected {ADDRESS_HASH_SIZE}",
            bytes.len()
        ))
    })
}
