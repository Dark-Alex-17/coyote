use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const DEFAULT_KNOCK_RETENTION_HOURS: u64 = 24;
pub const DEFAULT_PEER_MAX_CONCURRENT: u32 = 1;
pub const DEFAULT_PEER_MAX_MESSAGES_PER_HOUR: u32 = 60;
pub const DEFAULT_PEER_MAX_TOKENS_PER_HOUR: u64 = 100_000;

pub(crate) const MESH_DIGEST_PROMPT: &str = r#"The session above may be shared with a trusted collaborator's Coyote instance. Write a digest of it that lets that collaborator understand what is happening here without reading the transcript.

Cover, when present:
- What is being worked on and its current state.
- Decisions made and the reasons behind them.
- Names and interfaces that were chosen: types, functions, commands, config keys, endpoints, file paths.
- Open questions and anything a collaborator is likely to ask about.

Omit secrets, credentials, tokens, API keys, file contents, and anything the user marked private or asked not to share. Refer to files by path only.

Be dense and factual; prefer bullet points; no preamble or commentary before or after the digest. Keep it under roughly 200 words."#;

/// The `mesh:` block of config.yaml: how this Coyote joins and behaves on the Coyote Mesh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeshConfig {
    pub enabled: bool,
    pub announce: bool,
    pub display_name: Option<String>,
    pub display_name_on_public: bool,
    pub interfaces: Vec<MeshInterface>,
    pub brief: MeshBrief,
    pub digest_prompt: Option<String>,
    pub brief_model: Option<String>,
    pub envoy_model: Option<String>,
    pub knock_retention_hours: u64,
    pub peer_max_concurrent: u32,
    pub peer_max_messages_per_hour: u32,
    pub peer_max_tokens_per_hour: u64,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            announce: true,
            display_name: None,
            display_name_on_public: false,
            interfaces: vec![MeshInterface::Lan],
            brief: MeshBrief::default(),
            digest_prompt: None,
            brief_model: None,
            envoy_model: None,
            knock_retention_hours: DEFAULT_KNOCK_RETENTION_HOURS,
            peer_max_concurrent: DEFAULT_PEER_MAX_CONCURRENT,
            peer_max_messages_per_hour: DEFAULT_PEER_MAX_MESSAGES_PER_HOUR,
            peer_max_tokens_per_hour: DEFAULT_PEER_MAX_TOKENS_PER_HOUR,
        }
    }
}

impl MeshConfig {
    // Scaffolding for the mesh runtime: the first consumer of these accessors removes the allows.
    #[allow(dead_code)]
    pub fn interfaces(&self) -> &[MeshInterface] {
        &self.interfaces
    }

    #[allow(dead_code)]
    pub fn digest_prompt(&self) -> &str {
        self.digest_prompt.as_deref().unwrap_or(MESH_DIGEST_PROMPT)
    }

    pub fn validate(&self, function_calling_support: bool) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !function_calling_support {
            bail!(
                "mesh.enabled is true but function_calling_support is false: the mesh sends and receives messages through tools, so it needs function calling. Set function_calling_support: true in config.yaml, or run .set function_calling_support true in the REPL. Function calling can only be enabled when at least one tool is installed, so install tools first if you have none."
            );
        }
        if self.interfaces.is_empty() {
            bail!(
                "mesh.interfaces is empty; list at least one interface (the default is a single {{type: lan}} entry)"
            );
        }
        let lan_count = self
            .interfaces
            .iter()
            .filter(|interface| **interface == MeshInterface::Lan)
            .count();
        if lan_count > 1 {
            bail!(
                "mesh.interfaces lists lan more than once; only one lan interface can be bound per process"
            );
        }
        for (name, value) in [
            ("knock_retention_hours", self.knock_retention_hours),
            ("peer_max_concurrent", u64::from(self.peer_max_concurrent)),
            (
                "peer_max_messages_per_hour",
                u64::from(self.peer_max_messages_per_hour),
            ),
            ("peer_max_tokens_per_hour", self.peer_max_tokens_per_hour),
        ] {
            if value == 0 {
                bail!("mesh.{name} is 0, which is out of range; use 1 or more");
            }
        }
        Ok(())
    }
}

/// What the envoy serves to trusted peers that ask about this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MeshBrief {
    #[default]
    Auto,
    Manual,
    Off,
}

impl fmt::Display for MeshBrief {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Off => "off",
        })
    }
}

/// One entry of `mesh.interfaces`. Exactly what is listed is joined; there is no fallback between kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawMeshInterface", into = "RawMeshInterface")]
pub enum MeshInterface {
    Lan,
    Private { host: String, port: u16 },
    Public { host: String, port: u16 },
}

impl fmt::Display for MeshInterface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lan => f.write_str("lan"),
            Self::Private { host, port } => write!(f, "private {host}:{port}"),
            Self::Public { host, port } => write!(f, "public {host}:{port} (world-visible)"),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    expecting = "a mapping like {type: lan} or {type: private, host: relay.example.com, port: 4242}"
)]
struct RawMeshInterface {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
}

impl TryFrom<RawMeshInterface> for MeshInterface {
    type Error = anyhow::Error;

    fn try_from(raw: RawMeshInterface) -> Result<Self> {
        let RawMeshInterface { kind, host, port } = raw;
        match kind.as_str() {
            "lan" => {
                if host.is_some() || port.is_some() {
                    bail!(
                        "type 'lan' takes no host or port; use 'private' or 'public' to attach to a relay"
                    );
                }
                Ok(Self::Lan)
            }
            "private" | "public" => {
                let (Some(host), Some(port)) = (host, port) else {
                    bail!("type '{kind}' requires both host and port");
                };
                if host.trim().is_empty() {
                    bail!("type '{kind}' requires a non-empty host");
                }
                if port == 0 {
                    bail!("port 0 is out of range; use 1-65535");
                }
                Ok(if kind == "private" {
                    Self::Private { host, port }
                } else {
                    Self::Public { host, port }
                })
            }
            "auto" => bail!(
                "type 'auto' was removed and is no longer valid; use 'lan' for link-local discovery (the likely intent), or 'private'/'public' with host and port for a relay"
            ),
            other => bail!("unknown type '{other}'; valid values are lan, private, public"),
        }
    }
}

impl From<MeshInterface> for RawMeshInterface {
    fn from(interface: MeshInterface) -> Self {
        match interface {
            MeshInterface::Lan => Self {
                kind: "lan".into(),
                host: None,
                port: None,
            },
            MeshInterface::Private { host, port } => Self {
                kind: "private".into(),
                host: Some(host),
                port: Some(port),
            },
            MeshInterface::Public { host, port } => Self {
                kind: "public".into(),
                host: Some(host),
                port: Some(port),
            },
        }
    }
}

/// The `mesh:` section of `.info`, one row per setting. The digest prompt body is never printed.
pub fn render_mesh_info(mesh: &MeshConfig) -> String {
    let digest_prompt = match mesh.digest_prompt {
        Some(_) => "custom",
        None => "default",
    };
    let mut output = String::new();
    let mut row = |name: &str, value: String| output.push_str(&format!("  {name:<28}{value}\n"));
    row("enabled", mesh.enabled.to_string());
    row("announce", mesh.announce.to_string());
    row(
        "display_name",
        super::format_option_value(&mesh.display_name),
    );
    row(
        "display_name_on_public",
        mesh.display_name_on_public.to_string(),
    );
    for (i, interface) in mesh.interfaces.iter().enumerate() {
        row(&format!("interfaces[{i}]"), interface.to_string());
    }
    row("brief", mesh.brief.to_string());
    row("digest_prompt", digest_prompt.to_string());
    row("brief_model", super::format_option_value(&mesh.brief_model));
    row("envoy_model", super::format_option_value(&mesh.envoy_model));
    row(
        "knock_retention_hours",
        mesh.knock_retention_hours.to_string(),
    );
    row("peer_max_concurrent", mesh.peer_max_concurrent.to_string());
    row(
        "peer_max_messages_per_hour",
        mesh.peer_max_messages_per_hour.to_string(),
    );
    row(
        "peer_max_tokens_per_hour",
        mesh.peer_max_tokens_per_hour.to_string(),
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn interface_error(yaml: &str) -> String {
        serde_yaml::from_str::<Config>(yaml)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn mesh_defaults_match_documented_values() {
        let mesh = MeshConfig::default();
        assert!(!mesh.enabled);
        assert!(mesh.announce);
        assert_eq!(mesh.display_name, None);
        assert!(!mesh.display_name_on_public);
        assert_eq!(mesh.interfaces, vec![MeshInterface::Lan]);
        assert_eq!(mesh.brief, MeshBrief::Auto);
        assert_eq!(mesh.digest_prompt, None);
        assert_eq!(mesh.brief_model, None);
        assert_eq!(mesh.envoy_model, None);
        assert_eq!(mesh.knock_retention_hours, 24);
        assert_eq!(mesh.peer_max_concurrent, 1);
        assert_eq!(mesh.peer_max_messages_per_hour, 60);
        assert_eq!(mesh.peer_max_tokens_per_hour, 100_000);
    }

    #[test]
    fn config_without_mesh_block_uses_mesh_defaults() {
        let cfg: Config = serde_yaml::from_str("model: provider:test").unwrap();
        assert_eq!(cfg.mesh, MeshConfig::default());
    }

    #[test]
    fn mesh_interfaces_lists_exactly_what_was_configured() {
        let cfg: Config =
            serde_yaml::from_str("mesh:\n  interfaces:\n    - {type: private, host: h, port: 1}\n")
                .unwrap();
        assert_eq!(
            cfg.mesh.interfaces(),
            &[MeshInterface::Private {
                host: "h".into(),
                port: 1
            }]
        );
    }

    #[test]
    fn mesh_interface_lan_parses_without_host_or_port() {
        let cfg: Config = serde_yaml::from_str("mesh:\n  interfaces:\n    - type: lan\n").unwrap();
        assert_eq!(cfg.mesh.interfaces(), &[MeshInterface::Lan]);
    }

    #[test]
    fn mesh_interface_public_parses_with_host_and_port() {
        let cfg: Config = serde_yaml::from_str(
            "mesh:\n  interfaces:\n    - type: public\n      host: node.example.com\n      port: 4242\n",
        )
        .unwrap();
        assert_eq!(
            cfg.mesh.interfaces(),
            &[MeshInterface::Public {
                host: "node.example.com".into(),
                port: 4242
            }]
        );
    }

    #[test]
    fn mesh_interface_round_trips_through_serde() {
        let mesh = MeshConfig {
            interfaces: vec![
                MeshInterface::Lan,
                MeshInterface::Private {
                    host: "relay.example.com".into(),
                    port: 4242,
                },
            ],
            ..Default::default()
        };
        let yaml = serde_yaml::to_string(&mesh).unwrap();
        let parsed: MeshConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, mesh);
    }

    #[test]
    fn mesh_interface_rejects_removed_auto_type() {
        let err = interface_error("mesh:\n  interfaces:\n    - type: auto\n");
        assert!(
            err.contains("type 'auto' was removed and is no longer valid"),
            "{err}"
        );
        assert!(err.contains("use 'lan' for link-local discovery"), "{err}");
    }

    #[test]
    fn disabled_block_with_bad_interface_is_refused_at_parse() {
        let err = interface_error(
            "mesh:\n  enabled: false\n  interfaces:\n    - type: private\n      host: relay.example.com\n      port: 0\n",
        );
        assert!(err.contains("port 0 is out of range"), "{err}");
    }

    #[test]
    fn disabled_block_with_out_of_range_rate_limit_parses_and_validates() {
        let cfg: Config = serde_yaml::from_str(
            "mesh:\n  enabled: false\n  interfaces: []\n  peer_max_concurrent: 0\n  knock_retention_hours: 0\n",
        )
        .unwrap();
        assert!(!cfg.mesh.enabled);
        assert_eq!(cfg.mesh.peer_max_concurrent, 0);
        cfg.mesh.validate(false).unwrap();
    }

    #[test]
    fn mesh_interface_rejects_unknown_type() {
        let err = interface_error("mesh:\n  interfaces:\n    - type: bluetooth\n");
        assert!(
            err.contains("unknown type 'bluetooth'; valid values are lan, private, public"),
            "{err}"
        );
    }

    #[test]
    fn mesh_interface_private_requires_host_and_port() {
        let err = interface_error("mesh:\n  interfaces:\n    - type: private\n      host: h\n");
        assert!(
            err.contains("type 'private' requires both host and port"),
            "{err}"
        );

        let err = interface_error("mesh:\n  interfaces:\n    - type: public\n      port: 4242\n");
        assert!(
            err.contains("type 'public' requires both host and port"),
            "{err}"
        );
    }

    #[test]
    fn mesh_interface_lan_rejects_host_or_port() {
        let err = interface_error("mesh:\n  interfaces:\n    - type: lan\n      host: h\n");
        assert!(err.contains("type 'lan' takes no host or port"), "{err}");

        let err = interface_error("mesh:\n  interfaces:\n    - type: lan\n      port: 4242\n");
        assert!(err.contains("type 'lan' takes no host or port"), "{err}");
    }

    #[test]
    fn mesh_interface_rejects_port_zero() {
        let err = interface_error(
            "mesh:\n  interfaces:\n    - type: private\n      host: h\n      port: 0\n",
        );
        assert!(err.contains("port 0 is out of range; use 1-65535"), "{err}");
    }

    #[test]
    fn mesh_interface_rejects_empty_host() {
        let err = interface_error(
            "mesh:\n  interfaces:\n    - type: private\n      host: \"\"\n      port: 4242\n",
        );
        assert!(
            err.contains("type 'private' requires a non-empty host"),
            "{err}"
        );

        let err = interface_error(
            "mesh:\n  interfaces:\n    - type: public\n      host: \"   \"\n      port: 4242\n",
        );
        assert!(
            err.contains("type 'public' requires a non-empty host"),
            "{err}"
        );
    }

    #[test]
    fn mesh_interface_error_carries_the_path_once() {
        let err = Config::load_from_str("mesh:\n  interfaces:\n    - type: auto\n").unwrap_err();
        let err = format!("{err:#}");
        assert_eq!(err.matches("mesh.interfaces").count(), 1, "{err}");
    }

    #[test]
    fn mesh_interface_bare_string_error_shows_the_expected_shape() {
        let err = interface_error("mesh:\n  interfaces:\n    - lan\n");
        assert!(err.contains("{type: lan}"), "{err}");
        assert!(!err.contains("RawMeshInterface"), "{err}");
    }

    #[test]
    fn mesh_interface_rejects_unknown_field() {
        let err = interface_error(
            "mesh:\n  interfaces:\n    - type: private\n      hots: h\n      port: 4242\n",
        );
        assert!(err.contains("hots"), "{err}");
    }

    #[test]
    fn mesh_brief_parses_lowercase_and_displays_lowercase() {
        let cfg: Config = serde_yaml::from_str("mesh:\n  brief: manual\n").unwrap();
        assert_eq!(cfg.mesh.brief, MeshBrief::Manual);
        assert_eq!(MeshBrief::Manual.to_string(), "manual");
        assert_eq!(MeshBrief::Off.to_string(), "off");
        let err = interface_error("mesh:\n  brief: sometimes\n");
        for allowed in ["auto", "manual", "off"] {
            assert!(err.contains(allowed), "{err}");
        }
    }

    #[test]
    fn digest_prompt_falls_back_to_builtin() {
        assert_eq!(MeshConfig::default().digest_prompt(), MESH_DIGEST_PROMPT);
        let mesh = MeshConfig {
            digest_prompt: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(mesh.digest_prompt(), "x");
    }

    #[test]
    fn validate_refuses_enabled_without_function_calling() {
        let mesh = MeshConfig {
            enabled: true,
            ..Default::default()
        };
        let err = mesh.validate(false).unwrap_err().to_string();
        assert!(err.contains(".set function_calling_support true"), "{err}");
        assert!(err.contains("install tools"), "{err}");
    }

    #[test]
    fn validate_passes_enabled_with_function_calling() {
        let mesh = MeshConfig {
            enabled: true,
            ..Default::default()
        };
        mesh.validate(true).unwrap();
    }

    #[test]
    fn validate_ignores_function_calling_when_disabled() {
        MeshConfig::default().validate(false).unwrap();
    }

    #[test]
    fn validate_ignores_structural_problems_when_disabled() {
        let mesh = MeshConfig {
            enabled: false,
            interfaces: vec![],
            peer_max_concurrent: 0,
            ..Default::default()
        };
        mesh.validate(false).unwrap();
    }

    #[test]
    fn validate_rejects_empty_interfaces_when_enabled() {
        let mesh = MeshConfig {
            enabled: true,
            interfaces: vec![],
            ..Default::default()
        };
        let err = mesh.validate(true).unwrap_err().to_string();
        assert!(err.contains("mesh.interfaces is empty"), "{err}");
        assert!(err.contains("{type: lan}"), "{err}");
    }

    #[test]
    fn validate_rejects_duplicate_lan_interfaces() {
        let mesh = MeshConfig {
            enabled: true,
            interfaces: vec![MeshInterface::Lan, MeshInterface::Lan],
            ..Default::default()
        };
        let err = mesh.validate(true).unwrap_err().to_string();
        assert!(
            err.contains("mesh.interfaces lists lan more than once"),
            "{err}"
        );
        assert!(
            err.contains("only one lan interface can be bound per process"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_zero_peer_max_concurrent() {
        let mesh = MeshConfig {
            enabled: true,
            peer_max_concurrent: 0,
            ..Default::default()
        };
        let err = mesh.validate(true).unwrap_err().to_string();
        assert!(
            err.contains("mesh.peer_max_concurrent is 0, which is out of range"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_zero_for_every_rate_and_retention_key() {
        let enabled = MeshConfig {
            enabled: true,
            ..Default::default()
        };
        let cases = [
            (
                "knock_retention_hours",
                MeshConfig {
                    knock_retention_hours: 0,
                    ..enabled.clone()
                },
            ),
            (
                "peer_max_concurrent",
                MeshConfig {
                    peer_max_concurrent: 0,
                    ..enabled.clone()
                },
            ),
            (
                "peer_max_messages_per_hour",
                MeshConfig {
                    peer_max_messages_per_hour: 0,
                    ..enabled.clone()
                },
            ),
            (
                "peer_max_tokens_per_hour",
                MeshConfig {
                    peer_max_tokens_per_hour: 0,
                    ..enabled
                },
            ),
        ];
        for (key, mesh) in cases {
            let err = mesh.validate(true).unwrap_err().to_string();
            let expected = format!("mesh.{key} is 0, which is out of range; use 1 or more");
            assert!(err.contains(&expected), "{key}: {err}");
        }
    }

    #[test]
    fn render_mesh_info_distinguishes_private_and_public_interfaces() {
        let mesh = MeshConfig {
            interfaces: vec![
                MeshInterface::Private {
                    host: "relay.example.com".into(),
                    port: 4242,
                },
                MeshInterface::Public {
                    host: "node.example.com".into(),
                    port: 4242,
                },
            ],
            brief: MeshBrief::Manual,
            ..Default::default()
        };
        let info = render_mesh_info(&mesh);
        assert!(
            info.contains("  interfaces[0]               private relay.example.com:4242\n"),
            "{info}"
        );
        assert!(
            info.contains(
                "  interfaces[1]               public node.example.com:4242 (world-visible)\n"
            ),
            "{info}"
        );
        assert!(
            info.contains("  enabled                     false\n"),
            "{info}"
        );
        assert!(
            info.contains("  display_name                null\n"),
            "{info}"
        );
        assert!(
            info.contains("  brief                       manual\n"),
            "{info}"
        );
        assert!(
            info.contains("  peer_max_tokens_per_hour    100000\n"),
            "{info}"
        );
    }

    #[test]
    fn render_mesh_info_never_prints_the_digest_prompt_body() {
        let secret = "do not leak this";
        let mesh = MeshConfig {
            digest_prompt: Some(secret.into()),
            ..Default::default()
        };
        let info = render_mesh_info(&mesh);
        assert!(
            info.contains("  digest_prompt               custom\n"),
            "{info}"
        );
        assert!(!info.contains(secret), "{info}");
        assert!(
            render_mesh_info(&MeshConfig::default())
                .contains("  digest_prompt               default\n")
        );
    }

    #[test]
    fn mesh_config_has_no_platform_conditional_code() {
        let source = include_str!("mesh_config.rs");
        for marker in [
            "cfg(target_os",
            "cfg(unix",
            "cfg(windows",
            "cfg!(",
            "target_family",
        ] {
            let occurrences = source.matches(marker).count();
            assert_eq!(occurrences, 1, "{marker} appears outside this test");
        }
    }
}
