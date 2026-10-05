use super::paths;
use crate::mesh::card::ABOUT_MAX_CHARS;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

pub const DEFAULT_KNOCK_RETENTION_HOURS: u64 = 24;
pub const DEFAULT_PEER_MAX_CONCURRENT: u32 = 1;
pub const DEFAULT_PEER_MAX_MESSAGES_PER_HOUR: u32 = 60;
pub const DEFAULT_PEER_MAX_TOKENS_PER_HOUR: u64 = 100_000;
pub const DEFAULT_PEER_MAX_COST_USD_PER_HOUR: f64 = 0.0;
pub const DEFAULT_PROPAGATION_SYNC_INTERVAL_SECS: u64 = 300;
/// One year: the longest automatic sync interval `validate` accepts.
pub const MAX_PROPAGATION_SYNC_INTERVAL_SECS: u64 = 31_536_000;
pub const DEFAULT_INLINE_MAX_BYTES: u64 = 64 * 1024;
/// Σ inline file bytes one message may carry; `inline_max_bytes` cannot exceed it.
pub const MAX_INLINE_FILE_TOTAL: u64 = 96 * 1024;
pub const DEFAULT_FETCH_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Largest file a fetch may serve; `max_bytes` cannot exceed it.
pub const MAX_FETCH_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Width of the label column in `.mesh info`, shared by every row so the values line up
/// whichever module renders them.
pub const MESH_INFO_LABEL_WIDTH: usize = 32;

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
    /// One human-written line on what this node's envoy can help with; carried on the
    /// status card to trusted peers, never in the announce.
    pub about: Option<String>,
    pub interfaces: Vec<MeshInterface>,
    pub brief: MeshBrief,
    pub digest_prompt: Option<String>,
    pub brief_model: Option<String>,
    pub envoy_model: Option<String>,
    /// Seconds the envoy holds a peer's question open for the human before handing it
    /// off; 0 = hand off at once, the question stays open for `.mesh answer`.
    pub envoy_escalation_timeout: u64,
    pub knock_retention_hours: u64,
    /// Envoy runs one sending identity may have queued or running at once; further
    /// messages are refused with a typed reason until one finishes; the message is
    /// still filed in the inbox for the human.
    pub peer_max_concurrent: u32,
    /// Messages accepted from one sending identity per hour. Windows are fixed hours
    /// kept in memory, so a restart opens a fresh window; further messages are refused
    /// with a typed reason: on a live link in the reply itself, on store-and-forward
    /// with one reply per identity per reason per hour, and on store-and-forward the
    /// message is still filed in the inbox for the human, without an envoy run; plus
    /// one folded REPL line per identity, per reason, per hour, with the folded count
    /// reported the next time that peer is heard from after the hour rolls over; on a
    /// live link the refused message is not filed; the peer is told to retry.
    pub peer_max_messages_per_hour: u32,
    /// Model tokens one sending identity may cost per hour, counted after each envoy
    /// run, so the runs in flight may overshoot the ceiling by at most
    /// `peer_max_concurrent` runs before the next is refused; the message is still
    /// filed in the inbox for the human.
    pub peer_max_tokens_per_hour: u64,
    /// USD one sending identity may cost per hour, counted like the token ceiling; 0 =
    /// no cost ceiling. Enforced only when the envoy model's prices are known; the
    /// message is still filed in the inbox for the human.
    pub peer_max_cost_usd_per_hour: f64,
    /// Seconds between automatic fetches of the messages a propagation node holds for
    /// this node, the first running once a propagation node is heard after the node
    /// joins; 0 = fetch only on `.mesh sync`; off while `announce` is false, since a
    /// fetch identifies this node to the propagation node.
    /// Sideband's `lxmf_sync_interval` defaults to 43200 s with periodic sync off and
    /// NomadNet's to 21600 s; neither fits an interactive REPL, so 300 s is used.
    pub propagation_sync_interval_secs: u64,
    pub fetch: MeshFetch,
}

/// File-sharing knobs; later work adds the rest of the block, the name is fixed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeshFetch {
    /// Largest file a peer may attach inline to a message; the receiver drops larger ones.
    pub inline_max_bytes: u64,
    /// Largest file this node serves to a peer that fetches it; a larger one is refused.
    pub max_bytes: u64,
    /// Where fetched files are staged, under `<inbox_dir>/<instance id>`; unset stages them
    /// under the cache dir.
    pub inbox_dir: Option<PathBuf>,
}

impl Default for MeshFetch {
    fn default() -> Self {
        Self {
            inline_max_bytes: DEFAULT_INLINE_MAX_BYTES,
            max_bytes: DEFAULT_FETCH_MAX_BYTES,
            inbox_dir: None,
        }
    }
}

impl MeshFetch {
    /// `inbox_dir` as configured, or its sandboxed-home translation when the configured
    /// directory does not exist and the translation does; `None` when unset. Resolving
    /// is quiet and repeatable, so callers may resolve as often as they like; the node
    /// announces the translation once, when it starts.
    pub fn inbox_dir(&self) -> Option<PathBuf> {
        self.inbox_dir
            .as_deref()
            .map(|configured| resolve_inbox_dir(configured, paths::translate_sandboxed_home_dir))
    }
}

fn resolve_inbox_dir(configured: &Path, translate: impl Fn(&Path) -> Option<PathBuf>) -> PathBuf {
    if configured.exists() {
        return configured.to_path_buf();
    }
    if let Some(translated) = translate(configured)
        && translated.exists()
    {
        debug!(
            "mesh.fetch.inbox_dir '{}' not found; resolved to sandboxed path '{}'",
            configured.display(),
            translated.display()
        );
        return translated;
    }
    configured.to_path_buf()
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            announce: true,
            display_name: None,
            display_name_on_public: false,
            about: None,
            interfaces: vec![MeshInterface::Lan],
            brief: MeshBrief::default(),
            digest_prompt: None,
            brief_model: None,
            envoy_model: None,
            envoy_escalation_timeout: 0,
            knock_retention_hours: DEFAULT_KNOCK_RETENTION_HOURS,
            peer_max_concurrent: DEFAULT_PEER_MAX_CONCURRENT,
            peer_max_messages_per_hour: DEFAULT_PEER_MAX_MESSAGES_PER_HOUR,
            peer_max_tokens_per_hour: DEFAULT_PEER_MAX_TOKENS_PER_HOUR,
            peer_max_cost_usd_per_hour: DEFAULT_PEER_MAX_COST_USD_PER_HOUR,
            propagation_sync_interval_secs: DEFAULT_PROPAGATION_SYNC_INTERVAL_SECS,
            fetch: MeshFetch::default(),
        }
    }
}

impl MeshConfig {
    pub fn interfaces(&self) -> &[MeshInterface] {
        &self.interfaces
    }

    /// The configured digest prompt, trimmed; blank counts as unset and yields the
    /// code-owned default.
    pub fn digest_prompt(&self) -> &str {
        self.digest_prompt
            .as_deref()
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .unwrap_or(MESH_DIGEST_PROMPT)
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
        if let Some(about) = &self.about {
            let n = about.chars().count();
            if n > ABOUT_MAX_CHARS {
                bail!(
                    "mesh.about is {n} characters, which is over the cap; use {ABOUT_MAX_CHARS} or fewer"
                );
            }
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
        let cost = self.peer_max_cost_usd_per_hour;
        if !cost.is_finite() || cost < 0.0 {
            bail!(
                "mesh.peer_max_cost_usd_per_hour is {cost}, which is out of range; use 0 (no ceiling) or a positive amount"
            );
        }
        let sync = self.propagation_sync_interval_secs;
        if sync > MAX_PROPAGATION_SYNC_INTERVAL_SECS {
            bail!(
                "mesh.propagation_sync_interval_secs is {sync}, which is out of range; use 0 (manual) or up to {MAX_PROPAGATION_SYNC_INTERVAL_SECS} (one year)"
            );
        }
        let inline = self.fetch.inline_max_bytes;
        if !(1..=MAX_INLINE_FILE_TOTAL).contains(&inline) {
            bail!(
                "mesh.fetch.inline_max_bytes is {inline}, which is out of range; use 1 to {MAX_INLINE_FILE_TOTAL}"
            );
        }
        let max = self.fetch.max_bytes;
        if !(1..=MAX_FETCH_FILE_BYTES).contains(&max) {
            bail!(
                "mesh.fetch.max_bytes is {max}, which is out of range; use 1 to {MAX_FETCH_FILE_BYTES}"
            );
        }
        if let Some(dir) = &self.fetch.inbox_dir
            && !dir.is_absolute()
        {
            bail!(
                "mesh.fetch.inbox_dir is '{}', which is not absolute; use an absolute path",
                dir.display()
            );
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
    let digest_prompt = if mesh.digest_prompt() == MESH_DIGEST_PROMPT {
        "default"
    } else {
        "custom"
    };
    let mut output = String::new();
    let mut row = |name: &str, value: String| {
        output.push_str(&format!("  {name:<MESH_INFO_LABEL_WIDTH$}{value}\n"))
    };
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
    row("about", super::format_option_value(&mesh.about));
    for (i, interface) in mesh.interfaces.iter().enumerate() {
        row(&format!("interfaces[{i}]"), interface.to_string());
    }
    row("brief", mesh.brief.to_string());
    row("digest_prompt", digest_prompt.to_string());
    row("brief_model", super::format_option_value(&mesh.brief_model));
    row("envoy_model", super::format_option_value(&mesh.envoy_model));
    row(
        "envoy_escalation_timeout",
        mesh.envoy_escalation_timeout.to_string(),
    );
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
    let cost = mesh.peer_max_cost_usd_per_hour;
    row(
        "peer_max_cost_usd_per_hour",
        if cost == 0.0 {
            "0 (off)".to_string()
        } else {
            cost.to_string()
        },
    );
    let sync = mesh.propagation_sync_interval_secs;
    row(
        "propagation_sync_interval_secs",
        if sync == 0 {
            "0 (manual)".to_string()
        } else if !mesh.announce {
            format!("{sync} (off: announce is false)")
        } else {
            sync.to_string()
        },
    );
    row(
        "fetch.inline_max_bytes",
        mesh.fetch.inline_max_bytes.to_string(),
    );
    row("fetch.max_bytes", mesh.fetch.max_bytes.to_string());
    row(
        "fetch.inbox_dir",
        super::format_option_value(&mesh.fetch.inbox_dir.as_ref().map(|dir| dir.display())),
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mesh::test_support::TempDir;
    use crate::testing::EnvVarGuard;
    use serial_test::serial;

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
        assert_eq!(mesh.about, None);
        assert_eq!(mesh.interfaces, vec![MeshInterface::Lan]);
        assert_eq!(mesh.brief, MeshBrief::Auto);
        assert_eq!(mesh.digest_prompt, None);
        assert_eq!(mesh.brief_model, None);
        assert_eq!(mesh.envoy_model, None);
        assert_eq!(mesh.envoy_escalation_timeout, 0);
        assert_eq!(mesh.knock_retention_hours, 24);
        assert_eq!(mesh.peer_max_concurrent, 1);
        assert_eq!(mesh.peer_max_messages_per_hour, 60);
        assert_eq!(mesh.peer_max_tokens_per_hour, 100_000);
        assert_eq!(mesh.peer_max_cost_usd_per_hour, 0.0);
        assert_eq!(mesh.propagation_sync_interval_secs, 300);
        assert_eq!(mesh.fetch.inline_max_bytes, 65_536);
        assert_eq!(mesh.fetch.max_bytes, 4_194_304);
        assert_eq!(mesh.fetch.inbox_dir, None);
        assert_eq!(DEFAULT_INLINE_MAX_BYTES, 65_536);
        assert_eq!(MAX_INLINE_FILE_TOTAL, 98_304);
        assert_eq!(DEFAULT_FETCH_MAX_BYTES, 4_194_304);
        assert_eq!(MAX_FETCH_FILE_BYTES, 4_194_304);
    }

    #[test]
    fn config_without_mesh_block_uses_mesh_defaults() {
        let cfg: Config = serde_yaml::from_str("model: provider:test").unwrap();
        assert_eq!(cfg.mesh, MeshConfig::default());
    }

    // The config tolerates unknown keys inside `mesh`, as it does at the top level; interface
    // entries are the exception. This pins that so adding `deny_unknown_fields` later is a
    // deliberate change that also updates the docs.
    #[test]
    fn mesh_block_ignores_unknown_key_within_mesh() {
        let cfg: Config = serde_yaml::from_str("mesh: {bogus: 1}\n").unwrap();
        assert_eq!(cfg.mesh, MeshConfig::default());
    }

    #[test]
    fn mesh_fetch_block_ignores_unknown_key_within_fetch() {
        let cfg: Config = serde_yaml::from_str("mesh:\n  fetch: {bogus: 1}\n").unwrap();
        assert_eq!(cfg.mesh, MeshConfig::default());
        let cfg: Config =
            serde_yaml::from_str("mesh:\n  fetch:\n    inline_max_bytes: 1024\n").unwrap();
        assert_eq!(cfg.mesh.fetch.inline_max_bytes, 1024);
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
        for blank in ["", "  \n"] {
            let mesh = MeshConfig {
                digest_prompt: Some(blank.into()),
                ..Default::default()
            };
            assert_eq!(mesh.digest_prompt(), MESH_DIGEST_PROMPT, "{blank:?}");
        }
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

    /// Usage probe: the documented `0 = fetch only on .mesh sync` is a VALID setting for an
    /// enabled mesh, unlike the rate and retention keys where 0 is out of range; an absent
    /// key reads as the documented 300; a negative or fractional value is refused at parse
    /// time naming the key rather than silently clamped.
    #[test]
    fn propagation_sync_interval_zero_is_manual_and_valid_while_negatives_fail_to_parse() {
        let enabled = "mesh:\n  enabled: true\n  interfaces:\n    - {type: private, host: relay, port: 4242}\n";
        let manual: Config =
            serde_yaml::from_str(&format!("{enabled}  propagation_sync_interval_secs: 0\n"))
                .unwrap();
        assert_eq!(manual.mesh.propagation_sync_interval_secs, 0);
        manual
            .mesh
            .validate(true)
            .expect("0 is the documented manual-only setting, not out of range");

        let implicit: Config = serde_yaml::from_str(enabled).unwrap();
        assert_eq!(
            implicit.mesh.propagation_sync_interval_secs,
            DEFAULT_PROPAGATION_SYNC_INTERVAL_SECS
        );
        assert_eq!(DEFAULT_PROPAGATION_SYNC_INTERVAL_SECS, 300);

        for bad in ["-1", "1.5", "five"] {
            let err = serde_yaml::from_str::<Config>(&format!(
                "{enabled}  propagation_sync_interval_secs: {bad}\n"
            ))
            .expect_err(bad)
            .to_string();
            assert!(
                err.contains("propagation_sync_interval_secs"),
                "{bad}: the refusal names the key: {err}"
            );
        }
    }

    #[test]
    fn validate_accepts_only_zero_or_a_positive_finite_cost_ceiling() {
        let enabled = MeshConfig {
            enabled: true,
            ..Default::default()
        };
        for accepted in [0.0, 1.5] {
            let mesh = MeshConfig {
                peer_max_cost_usd_per_hour: accepted,
                ..enabled.clone()
            };
            mesh.validate(true).unwrap();
        }
        for (refused, rendered) in [(-0.5, "-0.5"), (f64::NAN, "NaN"), (f64::INFINITY, "inf")] {
            let mesh = MeshConfig {
                peer_max_cost_usd_per_hour: refused,
                ..enabled.clone()
            };
            let err = mesh.validate(true).unwrap_err().to_string();
            let expected = format!(
                "mesh.peer_max_cost_usd_per_hour is {rendered}, which is out of range; use 0 (no ceiling) or a positive amount"
            );
            assert!(err.contains(&expected), "{rendered}: {err}");
        }
    }

    #[test]
    fn validate_caps_the_sync_interval_at_one_year() {
        let enabled = MeshConfig {
            enabled: true,
            ..Default::default()
        };
        for accepted in [0, 1, MAX_PROPAGATION_SYNC_INTERVAL_SECS] {
            let mesh = MeshConfig {
                propagation_sync_interval_secs: accepted,
                ..enabled.clone()
            };
            mesh.validate(true).unwrap();
        }
        for refused in [MAX_PROPAGATION_SYNC_INTERVAL_SECS + 1, u64::MAX] {
            let mesh = MeshConfig {
                propagation_sync_interval_secs: refused,
                ..enabled.clone()
            };
            let err = mesh.validate(true).unwrap_err().to_string();
            assert_eq!(
                err,
                format!(
                    "mesh.propagation_sync_interval_secs is {refused}, which is out of range; use 0 (manual) or up to 31536000 (one year)"
                )
            );
        }
    }

    #[test]
    fn validate_caps_about_at_the_card_limit_counting_characters() {
        let enabled = MeshConfig {
            enabled: true,
            ..Default::default()
        };
        for accepted in [
            String::new(),
            "\u{e9}".repeat(ABOUT_MAX_CHARS),
            "a".repeat(ABOUT_MAX_CHARS),
        ] {
            let mesh = MeshConfig {
                about: Some(accepted),
                ..enabled.clone()
            };
            mesh.validate(true).unwrap();
        }
        let mesh = MeshConfig {
            about: Some("\u{e9}".repeat(ABOUT_MAX_CHARS + 1)),
            ..enabled
        };
        let err = mesh.validate(true).unwrap_err().to_string();
        assert_eq!(
            err,
            "mesh.about is 201 characters, which is over the cap; use 200 or fewer"
        );
    }

    #[test]
    fn validate_keeps_inline_max_bytes_between_one_and_the_per_message_total() {
        let enabled = MeshConfig {
            enabled: true,
            ..Default::default()
        };
        for accepted in [1, DEFAULT_INLINE_MAX_BYTES, MAX_INLINE_FILE_TOTAL] {
            let mesh = MeshConfig {
                fetch: MeshFetch {
                    inline_max_bytes: accepted,
                    ..Default::default()
                },
                ..enabled.clone()
            };
            mesh.validate(true).unwrap();
        }
        for refused in [0, MAX_INLINE_FILE_TOTAL + 1, u64::MAX] {
            let mesh = MeshConfig {
                fetch: MeshFetch {
                    inline_max_bytes: refused,
                    ..Default::default()
                },
                ..enabled.clone()
            };
            let err = mesh.validate(true).unwrap_err().to_string();
            assert_eq!(
                err,
                format!(
                    "mesh.fetch.inline_max_bytes is {refused}, which is out of range; use 1 to 98304"
                )
            );
        }
    }

    #[test]
    fn validate_keeps_fetch_max_bytes_between_one_and_the_file_ceiling() {
        let enabled = MeshConfig {
            enabled: true,
            ..Default::default()
        };
        for accepted in [1, DEFAULT_FETCH_MAX_BYTES, MAX_FETCH_FILE_BYTES] {
            let mesh = MeshConfig {
                fetch: MeshFetch {
                    max_bytes: accepted,
                    ..Default::default()
                },
                ..enabled.clone()
            };
            mesh.validate(true).unwrap();
        }
        for refused in [0, MAX_FETCH_FILE_BYTES + 1, u64::MAX] {
            let mesh = MeshConfig {
                fetch: MeshFetch {
                    max_bytes: refused,
                    ..Default::default()
                },
                ..enabled.clone()
            };
            let err = mesh.validate(true).unwrap_err().to_string();
            assert_eq!(
                err,
                format!(
                    "mesh.fetch.max_bytes is {refused}, which is out of range; use 1 to 4194304"
                )
            );
        }
    }

    #[test]
    fn validate_requires_an_absolute_inbox_dir_when_one_is_set() {
        let enabled = MeshConfig {
            enabled: true,
            ..Default::default()
        };
        let absolute = std::env::temp_dir();
        let mesh = MeshConfig {
            fetch: MeshFetch {
                inbox_dir: Some(absolute.clone()),
                ..Default::default()
            },
            ..enabled.clone()
        };
        mesh.validate(true).unwrap();

        let relative = MeshConfig {
            fetch: MeshFetch {
                inbox_dir: Some(PathBuf::from("relative/inbox")),
                ..Default::default()
            },
            ..enabled
        };
        let err = relative.validate(true).unwrap_err().to_string();
        assert_eq!(
            err,
            "mesh.fetch.inbox_dir is 'relative/inbox', which is not absolute; use an absolute path"
        );
    }

    #[test]
    fn an_existing_inbox_dir_is_used_as_configured_without_translating() {
        let tmp = TempDir::new("mesh-config-inbox-exists");

        let resolved = resolve_inbox_dir(&tmp.path, |_| {
            panic!("an inbox_dir that exists is not translated")
        });

        assert_eq!(resolved, tmp.path);
    }

    #[test]
    fn a_missing_inbox_dir_whose_translation_exists_resolves_to_the_translation() {
        let tmp = TempDir::new("mesh-config-inbox-translated");
        let configured = Path::new("/home/someone/inbox");
        let translated = tmp.path.join("inbox");
        std::fs::create_dir_all(&translated).unwrap();

        let resolved = resolve_inbox_dir(configured, |path| {
            assert_eq!(path, configured);
            Some(translated.clone())
        });

        assert_eq!(resolved, translated);
    }

    #[test]
    fn resolving_the_same_missing_inbox_dir_twice_translates_it_the_same_way_both_times() {
        let tmp = TempDir::new("mesh-config-inbox-twice");
        let configured = Path::new("/home/someone/inbox");
        let translated = tmp.path.join("inbox");
        std::fs::create_dir_all(&translated).unwrap();
        let translate = |_: &Path| Some(translated.clone());

        let first = resolve_inbox_dir(configured, translate);
        let second = resolve_inbox_dir(configured, translate);

        assert_eq!(first, translated);
        assert_eq!(second, first);
    }

    #[test]
    fn a_missing_inbox_dir_with_no_usable_translation_is_returned_as_configured() {
        let tmp = TempDir::new("mesh-config-inbox-untranslated");
        let configured = tmp.path.join("missing");
        let also_missing = tmp.path.join("also-missing");

        assert_eq!(resolve_inbox_dir(&configured, |_| None), configured);
        assert_eq!(
            resolve_inbox_dir(&configured, |_| Some(also_missing.clone())),
            configured
        );
        assert!(!configured.exists(), "resolving creates nothing");
        assert!(!also_missing.exists());
    }

    /// The real translator is wired in: under `IS_SANDBOX` a `/home/<user>` path maps to
    /// `/home/agent`, and when nothing is there either the configured path comes back.
    #[test]
    #[serial]
    fn inbox_dir_falls_through_to_the_configured_path_when_the_sandbox_translation_is_missing() {
        let _sandbox = EnvVarGuard::set("IS_SANDBOX", "1");
        let unique = format!("coyote-inbox-{}", uuid::Uuid::new_v4().simple());
        let configured = PathBuf::from(format!("/home/someone/{unique}"));
        assert_eq!(
            paths::translate_sandboxed_home_dir(&configured),
            Some(PathBuf::from(format!("/home/agent/{unique}")))
        );
        assert!(!Path::new(&format!("/home/agent/{unique}")).exists());
        let fetch = MeshFetch {
            inbox_dir: Some(configured.clone()),
            ..Default::default()
        };

        assert_eq!(fetch.inbox_dir(), Some(configured));
        assert_eq!(MeshFetch::default().inbox_dir(), None);
    }

    /// The public method, not just the seam: an `inbox_dir` that exists comes back exactly
    /// as configured even under `IS_SANDBOX`, with nothing created and the stored field
    /// untouched (the field and the method share a name, as `vault_password_file` does).
    #[test]
    #[serial]
    fn usage_probe_an_existing_inbox_dir_is_returned_as_configured_by_the_method() {
        let _sandbox = EnvVarGuard::set("IS_SANDBOX", "1");
        let tmp = TempDir::new("mesh-config-inbox-method-exists");
        let fetch = MeshFetch {
            inbox_dir: Some(tmp.path.clone()),
            ..Default::default()
        };

        assert_eq!(fetch.inbox_dir(), Some(tmp.path.clone()));
        assert_eq!(fetch.inbox_dir.as_deref(), Some(tmp.path.as_path()));
        assert_eq!(std::fs::read_dir(&tmp.path).unwrap().count(), 0);
    }

    #[test]
    fn fetch_block_reads_max_bytes_and_inbox_dir_and_serialises_an_unset_inbox_dir_as_null() {
        let cfg: Config = serde_yaml::from_str(
            "mesh:\n  fetch:\n    max_bytes: 1024\n    inbox_dir: /srv/inbox\n",
        )
        .unwrap();
        assert_eq!(cfg.mesh.fetch.max_bytes, 1024);
        assert_eq!(cfg.mesh.fetch.inbox_dir, Some(PathBuf::from("/srv/inbox")));
        assert_eq!(cfg.mesh.fetch.inline_max_bytes, DEFAULT_INLINE_MAX_BYTES);

        let serialized = serde_yaml::to_string(&MeshFetch::default()).unwrap();
        assert!(serialized.contains("inbox_dir: null"), "{serialized}");
        assert!(serialized.contains("max_bytes: 4194304\n"), "{serialized}");
        let null: Config = serde_yaml::from_str("mesh:\n  fetch:\n    inbox_dir: null\n").unwrap();
        assert_eq!(null.mesh.fetch, MeshFetch::default());
    }

    #[test]
    fn render_mesh_info_marks_a_zero_cost_ceiling_as_off() {
        let info = render_mesh_info(&MeshConfig::default());
        assert!(
            info.contains("  peer_max_cost_usd_per_hour      0 (off)\n"),
            "{info}"
        );
        let priced = MeshConfig {
            peer_max_cost_usd_per_hour: 1.5,
            ..Default::default()
        };
        let info = render_mesh_info(&priced);
        assert!(
            info.contains("  peer_max_cost_usd_per_hour      1.5\n"),
            "{info}"
        );
    }

    #[test]
    fn render_mesh_info_marks_a_zero_sync_interval_as_manual() {
        let info = render_mesh_info(&MeshConfig::default());
        assert!(
            info.contains("  propagation_sync_interval_secs  300\n"),
            "{info}"
        );
        let manual = MeshConfig {
            propagation_sync_interval_secs: 0,
            ..Default::default()
        };
        let info = render_mesh_info(&manual);
        assert!(
            info.contains("  propagation_sync_interval_secs  0 (manual)\n"),
            "{info}"
        );
    }

    #[test]
    fn render_mesh_info_marks_the_sync_interval_off_while_announce_is_false() {
        let quiet = MeshConfig {
            announce: false,
            ..Default::default()
        };
        let info = render_mesh_info(&quiet);
        assert!(
            info.contains("  propagation_sync_interval_secs  300 (off: announce is false)\n"),
            "{info}"
        );
        let manual_and_quiet = MeshConfig {
            announce: false,
            propagation_sync_interval_secs: 0,
            ..Default::default()
        };
        let info = render_mesh_info(&manual_and_quiet);
        assert!(
            info.contains("  propagation_sync_interval_secs  0 (manual)\n"),
            "{info}"
        );
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
            info.contains("  interfaces[0]                   private relay.example.com:4242\n"),
            "{info}"
        );
        assert!(
            info.contains(
                "  interfaces[1]                   public node.example.com:4242 (world-visible)\n"
            ),
            "{info}"
        );
        assert!(
            info.contains("  enabled                         false\n"),
            "{info}"
        );
        assert!(
            info.contains("  display_name                    null\n"),
            "{info}"
        );
        assert!(
            info.contains("  about                           null\n"),
            "{info}"
        );
        assert!(
            info.contains("  brief                           manual\n"),
            "{info}"
        );
        assert!(
            info.contains("  peer_max_tokens_per_hour        100000\n"),
            "{info}"
        );
        assert!(
            info.contains("  fetch.inline_max_bytes          65536\n"),
            "{info}"
        );
        assert!(
            info.ends_with(
                "  fetch.max_bytes                 4194304\n  fetch.inbox_dir                 null\n"
            ),
            "{info}"
        );
    }

    #[test]
    fn render_mesh_info_shows_a_configured_inbox_dir() {
        let mesh = MeshConfig {
            fetch: MeshFetch {
                inbox_dir: Some(PathBuf::from("/srv/inbox")),
                ..Default::default()
            },
            ..Default::default()
        };

        let info = render_mesh_info(&mesh);

        assert!(
            info.ends_with("  fetch.inbox_dir                 /srv/inbox\n"),
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
            info.contains("  digest_prompt                   custom\n"),
            "{info}"
        );
        assert!(!info.contains(secret), "{info}");
        assert!(
            render_mesh_info(&MeshConfig::default())
                .contains("  digest_prompt                   default\n")
        );
        let blank = MeshConfig {
            digest_prompt: Some("  \n".into()),
            ..Default::default()
        };
        assert!(
            render_mesh_info(&blank).contains("  digest_prompt                   default\n"),
            "{}",
            render_mesh_info(&blank)
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
