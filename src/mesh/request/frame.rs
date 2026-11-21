use crate::mesh::protocol::{MESH_PROTOCOL_VERSION, protocol_supported};

use rmpv::Value;
pub(crate) use rns_transport::destination::request::{PathHash, RequestId};
use rns_transport::destination::{DestinationName, NAME_HASH_LENGTH};

/// Bytes of a destination name hash, the prefix of the full name hash Reticulum uses to
/// derive a destination address.
pub(crate) const NAME_HASH_LEN: usize = NAME_HASH_LENGTH;

const NAME_HASH_KEY: &str = "name_hash";
const BODY_KEY: &str = "body";
const VERSION_KEY: &str = "v";

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
