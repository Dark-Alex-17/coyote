use crate::config::MeshConfig;
use crate::config::mesh_config::MeshInterface;

use anyhow::{Result, bail};

/// Leading bytes that mark an announce as a Coyote node; anything else is another application.
pub(crate) const ANNOUNCE_MAGIC: [u8; 4] = *b"COYM";
pub(crate) const MESH_PROTOCOL_VERSION: u16 = 1;
pub(crate) const MAX_DISPLAY_NAME_BYTES: usize = 64;

/// This node's own floor between repeated announces of one destination: heartbeats are
/// `HEARTBEAT_SECS` apart, and a manual announce is never sent sooner than this after the
/// previous one. A newly registered destination, at start or after a re-key, announces at once.
pub(crate) const REANNOUNCE_FLOOR_SECS: u64 = 300;
/// How often a running node re-announces so peers can tell it is still there.
pub(crate) const HEARTBEAT_SECS: u64 = 900;
/// A peer silent for this many heartbeats is dropped from the peer table.
pub(crate) const PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT: u32 = 3;

const HEADER_LEN: usize = ANNOUNCE_MAGIC.len() + 2;

/// Everything a Coyote announce says about its sender: `magic(4) | version u16 BE | display name UTF-8`.
/// The layout is protocol; it never carries session, path, or objective data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnnounceAppData {
    pub version: u16,
    pub display_name: Option<String>,
}

impl AnnounceAppData {
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let name = self.display_name.as_deref().unwrap_or_default();
        if name.len() > MAX_DISPLAY_NAME_BYTES {
            bail!(
                "mesh.display_name is {} bytes, which is over the {MAX_DISPLAY_NAME_BYTES}-byte limit announces carry; shorten it in config.yaml",
                name.len()
            );
        }
        if let Some(c) = name.chars().find(|c| is_control_or_invisible(*c)) {
            bail!(
                "mesh.display_name contains a control or invisible formatting character (U+{:04X}); remove it in config.yaml so peers can read the name",
                u32::from(c)
            );
        }
        let mut bytes = Vec::with_capacity(HEADER_LEN + name.len());
        bytes.extend_from_slice(&ANNOUNCE_MAGIC);
        bytes.extend_from_slice(&self.version.to_be_bytes());
        bytes.extend_from_slice(name.as_bytes());
        Ok(bytes)
    }

    /// `None` for anything that is not a well-formed Coyote announce; malformed input is
    /// rejected rather than repaired. A name carrying control characters or the invisible
    /// format characters used for visual spoofing is refused too, since a peer's name ends
    /// up on this node's terminal.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN || bytes[..ANNOUNCE_MAGIC.len()] != ANNOUNCE_MAGIC {
            return None;
        }
        let version = u16::from_be_bytes([bytes[4], bytes[5]]);
        let name = &bytes[HEADER_LEN..];
        if name.len() > MAX_DISPLAY_NAME_BYTES {
            return None;
        }
        let display_name = match std::str::from_utf8(name) {
            Ok("") => None,
            Ok(name) if name.chars().any(is_control_or_invisible) => return None,
            Ok(name) => Some(name.to_string()),
            Err(_) => return None,
        };
        Some(Self {
            version,
            display_name,
        })
    }
}

/// Control characters plus the zero-width, bidirectional-override and joiner format
/// characters that let a name render as something it is not.
pub(crate) fn is_control_or_invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
        )
}

/// The app_data this node announces under `config`. The display name is withheld when any
/// configured interface is public unless the user opted in with `display_name_on_public`.
pub(crate) fn announce_app_data(config: &MeshConfig) -> Result<Vec<u8>> {
    let public = config
        .interfaces()
        .iter()
        .any(|interface| matches!(interface, MeshInterface::Public { .. }));
    let display_name = if public && !config.display_name_on_public {
        None
    } else {
        config.display_name.clone()
    };
    AnnounceAppData {
        version: MESH_PROTOCOL_VERSION,
        display_name,
    }
    .encode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Session;

    fn decoded_name(config: &MeshConfig) -> Option<String> {
        let bytes = announce_app_data(config).unwrap();
        AnnounceAppData::decode(&bytes).unwrap().display_name
    }

    fn private() -> MeshInterface {
        MeshInterface::Private {
            host: "relay.internal".to_string(),
            port: 4242,
        }
    }

    fn public() -> MeshInterface {
        MeshInterface::Public {
            host: "relay.example.com".to_string(),
            port: 4242,
        }
    }

    #[test]
    fn announce_constants_are_pinned() {
        assert_eq!(MESH_PROTOCOL_VERSION, 1);
        assert_eq!(ANNOUNCE_MAGIC, *b"COYM");
        assert_eq!(MAX_DISPLAY_NAME_BYTES, 64);
        assert_eq!(REANNOUNCE_FLOOR_SECS, 300);
        assert_eq!(HEARTBEAT_SECS, 900);
        assert_eq!(PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT, 3);
        assert_eq!(
            HEARTBEAT_SECS * u64::from(PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT),
            2700
        );
    }

    #[test]
    fn encode_layout_is_magic_version_name() {
        let bytes = AnnounceAppData {
            version: 1,
            display_name: Some("Alex".to_string()),
        }
        .encode()
        .unwrap();

        assert_eq!(bytes, b"COYM\x00\x01Alex");
    }

    #[test]
    fn codec_round_trips_with_and_without_name() {
        for name in [None, Some("Alex".to_string()), Some("Zoë".to_string())] {
            let data = AnnounceAppData {
                version: MESH_PROTOCOL_VERSION,
                display_name: name,
            };
            assert_eq!(AnnounceAppData::decode(&data.encode().unwrap()), Some(data));
        }
    }

    #[test]
    fn encode_accepts_64_byte_name_and_rejects_65() {
        let ok = AnnounceAppData {
            version: 1,
            display_name: Some("a".repeat(64)),
        };
        assert_eq!(AnnounceAppData::decode(&ok.encode().unwrap()), Some(ok));

        let err = AnnounceAppData {
            version: 1,
            display_name: Some("a".repeat(65)),
        }
        .encode()
        .unwrap_err()
        .to_string();
        assert!(err.contains("mesh.display_name"), "{err}");
        assert!(err.contains("65 bytes"), "{err}");
    }

    #[test]
    fn encode_rejects_control_and_invisible_characters_naming_the_key() {
        for (name, code_point) in [
            ("\u{1b}[2Jname", "U+001B"),
            ("Al\u{202E}ex", "U+202E"),
            ("zero\u{200B}width", "U+200B"),
        ] {
            let err = AnnounceAppData {
                version: 1,
                display_name: Some(name.to_string()),
            }
            .encode()
            .unwrap_err()
            .to_string();
            assert!(err.contains("mesh.display_name"), "{name:?}: {err}");
            assert!(err.contains(code_point), "{name:?}: {err}");
        }

        let ok = AnnounceAppData {
            version: 1,
            display_name: Some("Zoë".to_string()),
        };
        assert_eq!(AnnounceAppData::decode(&ok.encode().unwrap()), Some(ok));
    }

    #[test]
    fn decode_rejects_short_wrong_magic_oversized_and_invalid_utf8() {
        assert_eq!(AnnounceAppData::decode(b"COYM\x00"), None);
        assert_eq!(AnnounceAppData::decode(b""), None);
        assert_eq!(AnnounceAppData::decode(b"LXMF\x00\x01Alex"), None);
        assert_eq!(AnnounceAppData::decode(b"COYM\x00\x01\xff\xfe"), None);
        let mut oversized = b"COYM\x00\x01".to_vec();
        oversized.extend(std::iter::repeat_n(b'a', 65));
        assert_eq!(AnnounceAppData::decode(&oversized), None);
    }

    #[test]
    fn decode_reads_version_big_endian() {
        let decoded = AnnounceAppData::decode(b"COYM\x01\x02").unwrap();
        assert_eq!(decoded.version, 0x0102);
        assert_eq!(decoded.display_name, None);
    }

    #[test]
    fn decode_rejects_control_characters_in_display_name() {
        for name in ["\u{1b}[2Jname", "na\rme", "name\n", "\u{7f}name", "na\tme"] {
            let mut bytes = b"COYM\x00\x01".to_vec();
            bytes.extend_from_slice(name.as_bytes());
            assert_eq!(AnnounceAppData::decode(&bytes), None, "{name:?}");
        }
    }

    #[test]
    fn decode_rejects_spoofing_format_characters_but_keeps_plain_non_ascii() {
        for name in [
            "\u{202E}evil",
            "zero\u{200B}width",
            "join\u{2060}er",
            "\u{FEFF}bom",
        ] {
            let mut bytes = b"COYM\x00\x01".to_vec();
            bytes.extend_from_slice(name.as_bytes());
            assert_eq!(AnnounceAppData::decode(&bytes), None, "{name:?}");
        }

        let mut bytes = b"COYM\x00\x01".to_vec();
        bytes.extend_from_slice("Zoë".as_bytes());
        let decoded = AnnounceAppData::decode(&bytes).unwrap();
        assert_eq!(decoded.display_name.as_deref(), Some("Zoë"));
    }

    #[test]
    fn app_data_carries_only_version_and_display_name() {
        let mut session: Session = serde_yaml::from_str(
            "model: provider:test\nmessages:\n  - role: user\n    content: secret objective text about the roadmap\n  - role: assistant\n    content: edit /Users/aclarke/code/coyote\n",
        )
        .unwrap();
        session.set_name("secret-session-name".to_string());
        let config = MeshConfig {
            display_name: Some("Alex".to_string()),
            ..MeshConfig::default()
        };

        let bytes = announce_app_data(&config).unwrap();

        let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        assert!(contains(b"Alex"), "the probe must be able to match");
        for secret in [
            b"secret-session-name".as_slice(),
            b"secret objective text",
            b"roadmap",
            b"/Users/aclarke/code/coyote",
        ] {
            assert!(
                !contains(secret),
                "{} leaked into app_data",
                String::from_utf8_lossy(secret)
            );
        }
        assert_eq!(bytes.len(), HEADER_LEN + "Alex".len());
        assert_eq!(session.name(), "secret-session-name");
        assert_eq!(session.messages().len(), 2);
    }

    #[test]
    fn public_interface_withholds_display_name_unless_opted_in() {
        let base = MeshConfig {
            display_name: Some("Alex".to_string()),
            interfaces: vec![MeshInterface::Lan, public()],
            display_name_on_public: false,
            ..MeshConfig::default()
        };
        assert_eq!(decoded_name(&base), None);

        let opted_in = MeshConfig {
            display_name_on_public: true,
            ..base.clone()
        };
        assert_eq!(decoded_name(&opted_in), Some("Alex".to_string()));

        let private_only = MeshConfig {
            interfaces: vec![MeshInterface::Lan, private()],
            ..base
        };
        assert_eq!(decoded_name(&private_only), Some("Alex".to_string()));
    }
}
