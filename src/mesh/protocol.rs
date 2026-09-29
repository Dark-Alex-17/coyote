//! The one negotiated mesh protocol version. It rides the announce `app_data`, where it
//! decides whether a peer is worth a link at all, and the R3 request envelope, where a
//! request from a version this node does not speak is refused before its body is read.
//! `STATUS_CARD_VERSION` (card.rs) and `PEER_WIRE_VERSION` (message.rs) are per-record
//! payload schema versions that evolve under one fixed protocol version.

use rmpv::Value;
use serde::{Deserialize, Serialize};

pub(crate) const MESH_PROTOCOL_VERSION: u16 = 1;
pub(crate) const MESH_PROTOCOL_MIN_SUPPORTED: u16 = 1;

pub(crate) fn protocol_supported(version: u16) -> bool {
    (MESH_PROTOCOL_MIN_SUPPORTED..=MESH_PROTOCOL_VERSION).contains(&version)
}

/// `found` as user-facing text; a version that never arrived reads as `none`.
pub(crate) fn describe_version(found: Option<u16>) -> String {
    found.map_or_else(|| "none".to_string(), |version| version.to_string())
}

/// Whether this Coyote speaks the protocol a peer announced. Decided once per announce and
/// kept on the peer record, so the outbound gate reads a verdict rather than a number. It
/// is persisted inside `PeerRecord`, so like every on-disk shape it rejects unknown fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Compatibility {
    #[default]
    Compatible,
    Incompatible {
        found: u16,
    },
}

impl Compatibility {
    pub(crate) fn of(version: u16) -> Self {
        if protocol_supported(version) {
            Self::Compatible
        } else {
            Self::Incompatible { found: version }
        }
    }

    /// One line for a peer listing, `None` when there is nothing to warn about.
    pub(crate) fn line(&self) -> Option<String> {
        match self {
            Self::Compatible => None,
            Self::Incompatible { found } => Some(format!(
                "incompatible: speaks protocol {found}, this Coyote supports {MESH_PROTOCOL_MIN_SUPPORTED}..={MESH_PROTOCOL_VERSION}"
            )),
        }
    }
}

const REFUSAL_KEY: &str = "refusal";
const UNSUPPORTED_VERSION: &str = "unsupported_version";
const FOUND_KEY: &str = "found";
const MIN_KEY: &str = "min";
const MAX_KEY: &str = "max";

/// What the dispatcher answers a request whose envelope names a protocol version this node
/// does not speak. `found` is the version the refusing side saw, `None` when the envelope
/// carried none it could read; `min..=max` is the refusing side's window. It travels as a
/// msgpack map, which no reading of the wire can confuse with a refusal code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VersionRefusal {
    pub found: Option<u16>,
    pub min: u16,
    pub max: u16,
}

impl VersionRefusal {
    /// The refusal this node sends, with its own window.
    pub(crate) fn current(found: Option<u16>) -> Self {
        Self {
            found,
            min: MESH_PROTOCOL_MIN_SUPPORTED,
            max: MESH_PROTOCOL_VERSION,
        }
    }

    pub(crate) fn to_value(&self) -> Value {
        Value::Map(vec![
            (Value::from(REFUSAL_KEY), Value::from(UNSUPPORTED_VERSION)),
            (
                Value::from(FOUND_KEY),
                self.found.map_or(Value::Nil, Value::from),
            ),
            (Value::from(MIN_KEY), Value::from(self.min)),
            (Value::from(MAX_KEY), Value::from(self.max)),
        ])
    }

    /// Reads a version refusal off the wire. Every key must be present and of its type;
    /// anything looser reads as a body, since a handler's real value must never be taken
    /// for a refusal.
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        let entries = value.as_map()?;
        let field = |name: &str| {
            entries
                .iter()
                .find(|(key, _)| key.as_str() == Some(name))
                .map(|(_, value)| value)
        };
        if field(REFUSAL_KEY)?.as_str()? != UNSUPPORTED_VERSION {
            return None;
        }
        let found = match field(FOUND_KEY)? {
            Value::Nil => None,
            value => Some(u16_of(value)?),
        };
        Some(Self {
            found,
            min: u16_of(field(MIN_KEY)?)?,
            max: u16_of(field(MAX_KEY)?)?,
        })
    }
}

fn u16_of(value: &Value) -> Option<u16> {
    u16::try_from(value.as_u64()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_constants_are_pinned() {
        assert_eq!(MESH_PROTOCOL_VERSION, 1);
        assert_eq!(MESH_PROTOCOL_MIN_SUPPORTED, 1);
        // Both ends of the window are inside it, which also pins MIN <= CURRENT: an
        // inverted window is empty and would fail both.
        assert!(protocol_supported(MESH_PROTOCOL_MIN_SUPPORTED));
        assert!(protocol_supported(MESH_PROTOCOL_VERSION));
        assert!(!protocol_supported(0));
        assert!(!protocol_supported(2));
    }

    #[test]
    fn unsupported_version_refusal_shape_is_pinned() {
        let refused = VersionRefusal::current(Some(2));
        assert_eq!(
            refused.to_value(),
            Value::Map(vec![
                (Value::from("refusal"), Value::from("unsupported_version")),
                (Value::from("found"), Value::from(2u16)),
                (Value::from("min"), Value::from(1u16)),
                (Value::from("max"), Value::from(1u16)),
            ])
        );
        assert_eq!(
            VersionRefusal::from_value(&refused.to_value()),
            Some(refused)
        );

        let unreadable = VersionRefusal::current(None);
        assert_eq!(
            unreadable.to_value(),
            Value::Map(vec![
                (Value::from("refusal"), Value::from("unsupported_version")),
                (Value::from("found"), Value::Nil),
                (Value::from("min"), Value::from(1u16)),
                (Value::from("max"), Value::from(1u16)),
            ])
        );
        assert_eq!(
            VersionRefusal::from_value(&unreadable.to_value()),
            Some(unreadable)
        );

        let rejected = [
            Value::Nil,
            Value::Map(vec![
                (Value::from("found"), Value::from(2u16)),
                (Value::from("min"), Value::from(1u16)),
                (Value::from("max"), Value::from(1u16)),
            ]),
            Value::Map(vec![
                (Value::from("refusal"), Value::from("other")),
                (Value::from("found"), Value::from(2u16)),
                (Value::from("min"), Value::from(1u16)),
                (Value::from("max"), Value::from(1u16)),
            ]),
            Value::Map(vec![
                (Value::from("refusal"), Value::from("unsupported_version")),
                (Value::from("found"), Value::from("2")),
                (Value::from("min"), Value::from(1u16)),
                (Value::from("max"), Value::from(1u16)),
            ]),
            Value::Map(vec![
                (Value::from("refusal"), Value::from("unsupported_version")),
                (Value::from("found"), Value::from(2u16)),
                (Value::from("max"), Value::from(1u16)),
            ]),
        ];
        for value in rejected {
            assert_eq!(
                VersionRefusal::from_value(&value),
                None,
                "{value:?} must not read as a version refusal"
            );
        }
    }

    #[test]
    fn compatibility_is_derived_from_the_announced_version() {
        assert_eq!(Compatibility::of(1), Compatibility::Compatible);
        assert_eq!(
            Compatibility::of(2),
            Compatibility::Incompatible { found: 2 }
        );
        // With MESH_PROTOCOL_MIN_SUPPORTED at 1 this is the below-the-floor case too.
        assert_eq!(
            Compatibility::of(0),
            Compatibility::Incompatible { found: 0 }
        );
        assert_eq!(Compatibility::Compatible.line(), None);
        assert_eq!(
            Compatibility::Incompatible { found: 2 }.line().as_deref(),
            Some("incompatible: speaks protocol 2, this Coyote supports 1..=1")
        );
        assert_eq!(describe_version(Some(2)), "2");
        assert_eq!(describe_version(None), "none");
    }
}
