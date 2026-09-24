use crate::mesh::hex_lower;
use crate::mesh::r3::error::R3Error;

use rmpv::Value;
use rns_transport::Packet;
use rns_transport::destination::{DestinationName, NAME_HASH_LENGTH};
use rns_transport::hash::{ADDRESS_HASH_SIZE, address_hash};
use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};

/// Frames above this are refused: outbound before anything touches the wire, inbound after
/// assembly and before decoding. The bound is enforced on our side only; the upstream
/// 32 MiB advertisement cap (`advertisement_limits.rs`) is the only pre-assembly bound,
/// because the upstream reject path deadlocks the transport (rev 3ed5932).
pub(crate) const MAX_R3_PAYLOAD_BYTES: usize = 256 * 1024;

/// Bytes of a destination name hash, the prefix of the full name hash Reticulum uses to
/// derive a destination address.
pub(crate) const NAME_HASH_LEN: usize = NAME_HASH_LENGTH;

const NAME_HASH_KEY: &str = "name_hash";
const BODY_KEY: &str = "body";

/// `truncated_hash(path)`: SHA-256 of the UTF-8 path, first 16 bytes. Requests carry this,
/// never the path itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PathHash([u8; ADDRESS_HASH_SIZE]);

impl PathHash {
    pub(crate) fn of(path: &str) -> Self {
        Self(address_hash(path.as_bytes()))
    }

    pub(crate) fn to_hex_string(self) -> String {
        hex_lower(&self.0)
    }
}

impl From<[u8; ADDRESS_HASH_SIZE]> for PathHash {
    fn from(bytes: [u8; ADDRESS_HASH_SIZE]) -> Self {
        Self(bytes)
    }
}

/// What a response is correlated with. How it is derived depends on how the request
/// travelled, and the two sides must agree without the id ever being on the wire in the
/// request itself:
///
/// - Sent as a single packet, the id is the first 16 bytes of that packet's hash. The
///   responder derives it from the packet it received (RNS `Link.handle_request` is handed
///   `packet.getTruncatedHash()`), so the requester must take it from the packet it built.
/// - Sent as a resource, the id is `truncated_hash(packed_request)`, the SHA-256 of the
///   encoded frame truncated to 16 bytes. RNS `Link.request` records it on that branch and
///   the responder recomputes it from the assembled resource bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RequestId([u8; ADDRESS_HASH_SIZE]);

impl RequestId {
    /// The id of a request that travels as `packet`.
    pub(crate) fn from_packet(packet: &Packet) -> Self {
        let hash = packet.hash().to_bytes();
        let mut id = [0u8; ADDRESS_HASH_SIZE];
        id.copy_from_slice(&hash[..ADDRESS_HASH_SIZE]);
        Self(id)
    }

    /// The id of a request that travels as a resource carrying `packed`.
    pub(crate) fn of_packed(packed: &[u8]) -> Self {
        Self(address_hash(packed))
    }

    pub(crate) fn as_bytes(&self) -> &[u8; ADDRESS_HASH_SIZE] {
        &self.0
    }

    pub(crate) fn to_vec(self) -> Vec<u8> {
        self.0.to_vec()
    }

    pub(crate) fn to_hex_string(self) -> String {
        hex_lower(&self.0)
    }
}

impl From<[u8; ADDRESS_HASH_SIZE]> for RequestId {
    fn from(bytes: [u8; ADDRESS_HASH_SIZE]) -> Self {
        Self(bytes)
    }
}

/// One request as it travels over a link, in either direction.
///
/// Wire form: a msgpack array of exactly 3 elements. Element 0 is a msgpack float64
/// (`0xcb` + 8 bytes big-endian IEEE 754) holding seconds since the UNIX epoch. Element 1
/// is a msgpack bin of exactly 16 bytes (`0xc4 0x10` + 16 bytes) holding the path hash.
/// Element 2 is the body as its own msgpack value: a map, an array, an integer, a bin; nil
/// when there is no body, never an empty bin. The frame is the whole payload; decoding
/// refuses trailing bytes, any other arity, and a hash that is not a 16-byte bin. This is
/// what RNS `Link.request` packs as `umsgpack.packb([time.time(), request_path_hash, data])`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RequestFrame {
    pub time: f64,
    pub path_hash: PathHash,
    pub data: Value,
}

impl RequestFrame {
    /// A frame for `path` stamped with the current time.
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
            Value::Binary(self.path_hash.0.to_vec()),
            self.data.clone(),
        ]))
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, R3Error> {
        let Value::Array(elements) = decode_whole(bytes)? else {
            return Err(R3Error::Decode("request frame is not an array".into()));
        };
        let [time, path_hash, data] = <[Value; 3]>::try_from(elements).map_err(|elements| {
            R3Error::Decode(format!(
                "request frame has {} elements, expected 3",
                elements.len()
            ))
        })?;
        let Value::F64(time) = time else {
            return Err(R3Error::Decode("request time is not a float64".into()));
        };
        Ok(Self {
            time,
            path_hash: PathHash(hash_bytes(&path_hash, "request path hash")?),
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
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResponseFrame {
    pub request_id: RequestId,
    pub data: Value,
}

impl ResponseFrame {
    pub(crate) fn encode(&self) -> Vec<u8> {
        encode_value(Value::Array(vec![
            Value::Binary(self.request_id.to_vec()),
            self.data.clone(),
        ]))
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, R3Error> {
        let Value::Array(elements) = decode_whole(bytes)? else {
            return Err(R3Error::Decode("response frame is not an array".into()));
        };
        let [request_id, data] = <[Value; 2]>::try_from(elements).map_err(|elements| {
            R3Error::Decode(format!(
                "response frame has {} elements, expected 2",
                elements.len()
            ))
        })?;
        Ok(Self {
            request_id: RequestId(hash_bytes(&request_id, "response request id")?),
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

/// What every request body travels in: the requester's origin beside the body, as a
/// msgpack map with `name_hash` and `body` keys.
pub(crate) struct Envelope {
    pub origin: OriginName,
    pub body: Value,
}

impl Envelope {
    pub(crate) fn into_value(self) -> Value {
        Value::Map(vec![
            (
                Value::from(NAME_HASH_KEY),
                Value::Binary(self.origin.0.to_vec()),
            ),
            (Value::from(BODY_KEY), self.body),
        ])
    }

    /// Both keys are required and the name hash must be binary of exactly `NAME_HASH_LEN`
    /// bytes; keys this version does not know are ignored.
    pub(crate) fn from_value(value: Value) -> Option<Self> {
        let Value::Map(entries) = value else {
            return None;
        };
        let mut origin = None;
        let mut body = None;
        for (key, value) in entries {
            match key.as_str() {
                Some(NAME_HASH_KEY) => {
                    let Value::Binary(bytes) = value else {
                        return None;
                    };
                    origin = Some(OriginName(bytes.as_slice().try_into().ok()?));
                }
                Some(BODY_KEY) => body = Some(value),
                _ => {}
            }
        }
        Some(Self {
            origin: origin?,
            body: body?,
        })
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

fn encode_value(value: Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    // The only error source is the writer, and a `Vec` never fails to grow.
    if let Err(err) = rmpv::encode::write_value(&mut bytes, &value) {
        warn!("Failed to encode a mesh request frame: {err}");
    }
    bytes
}

fn decode_whole(bytes: &[u8]) -> Result<Value, R3Error> {
    let mut cursor = Cursor::new(bytes);
    let value =
        rmpv::decode::read_value(&mut cursor).map_err(|err| R3Error::Decode(err.to_string()))?;
    let consumed = usize::try_from(cursor.position()).unwrap_or(usize::MAX);
    if consumed != bytes.len() {
        return Err(R3Error::Decode(format!(
            "{} trailing bytes after the frame",
            bytes.len().saturating_sub(consumed)
        )));
    }
    Ok(value)
}

fn hash_bytes(value: &Value, what: &str) -> Result<[u8; ADDRESS_HASH_SIZE], R3Error> {
    let Value::Binary(bytes) = value else {
        return Err(R3Error::Decode(format!("{what} is not a bin")));
    };
    <[u8; ADDRESS_HASH_SIZE]>::try_from(bytes.as_slice()).map_err(|_| {
        R3Error::Decode(format!(
            "{what} is {} bytes, expected {ADDRESS_HASH_SIZE}",
            bytes.len()
        ))
    })
}
