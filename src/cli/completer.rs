use crate::client::{ModelType, list_models};
use crate::config::paths;
use crate::config::{AppConfig, Config, agent_sessions_dir, list_agents_for_humans, list_sessions};
use crate::utils::list_file_names;
use crate::vault::Vault;
use clap_complete::{CompletionCandidate, Shell, generate};
use clap_complete_nushell::Nushell;
use std::ffi::OsStr;
use std::io;
use std::{env, fs};

const COYOTE_CLI_NAME: &str = "coyote";

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum ShellCompletion {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
    Nushell,
}

impl ShellCompletion {
    pub fn generate_completions(self, cmd: &mut clap::Command) {
        match self {
            Self::Bash => generate(Shell::Bash, cmd, COYOTE_CLI_NAME, &mut io::stdout()),
            Self::Elvish => generate(Shell::Elvish, cmd, COYOTE_CLI_NAME, &mut io::stdout()),
            Self::Fish => generate(Shell::Fish, cmd, COYOTE_CLI_NAME, &mut io::stdout()),
            Self::PowerShell => {
                generate(Shell::PowerShell, cmd, COYOTE_CLI_NAME, &mut io::stdout())
            }
            Self::Zsh => generate(Shell::Zsh, cmd, COYOTE_CLI_NAME, &mut io::stdout()),
            Self::Nushell => generate(Nushell, cmd, COYOTE_CLI_NAME, &mut io::stdout()),
        }
    }
}

pub(super) fn model_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    match load_app_config_for_completion() {
        Ok(app_config) => list_models(&app_config, ModelType::Chat)
            .into_iter()
            .filter(|&m| m.id().starts_with(&*cur))
            .map(|m| CompletionCandidate::new(m.id()))
            .collect(),
        Err(_) => vec![],
    }
}

fn load_app_config_for_completion() -> anyhow::Result<AppConfig> {
    let h = tokio::runtime::Handle::try_current().ok();
    let cfg = match h {
        Some(handle) => tokio::task::block_in_place(|| {
            handle.block_on(Config::load_with_interpolation(true, true))
        })?,
        None => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(Config::load_with_interpolation(true, true))?
        }
    };
    AppConfig::from_config(cfg)
}

pub(super) fn role_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    paths::list_roles(true)
        .into_iter()
        .filter(|r| r.starts_with(&*cur))
        .map(CompletionCandidate::new)
        .collect()
}

pub(super) fn agent_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    list_agents_for_humans()
        .into_iter()
        .filter(|listing| listing.name.starts_with(&*cur))
        .map(|listing| {
            let help = listing.help_option().map(Into::into);
            CompletionCandidate::new(listing.name).help(help)
        })
        .collect()
}

pub(super) fn rag_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    paths::list_rags()
        .into_iter()
        .filter(|r| r.starts_with(&*cur))
        .map(CompletionCandidate::new)
        .collect()
}

pub(super) fn macro_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    paths::list_macros()
        .into_iter()
        .filter(|m| m.starts_with(&*cur))
        .map(CompletionCandidate::new)
        .collect()
}

pub(super) fn bundle_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    crate::config::installed_bundle_names()
        .into_iter()
        .filter(|b| b.starts_with(&*cur))
        .map(CompletionCandidate::new)
        .collect()
}

fn extract_agent_from_args() -> Option<String> {
    let args: Vec<String> = env::args().collect();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if let Some(value) = arg.strip_prefix("--agent=") {
            return Some(value.to_string());
        }

        if (arg == "--agent" || arg == "-a") && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }

        i += 1;
    }
    None
}

pub(super) fn session_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    session_candidates(extract_agent_from_args().as_deref(), current)
}

fn session_candidates(agent: Option<&str>, current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();

    let sessions = match agent {
        Some(agent) => agent_sessions_dir(agent)
            .map(|dir| list_file_names(dir, ".yaml"))
            .unwrap_or_default(),
        None => list_sessions(),
    };

    sessions
        .into_iter()
        .filter(|s| s.starts_with(&*cur))
        .map(CompletionCandidate::new)
        .collect()
}

pub(super) fn mcp_server_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    let content = match fs::read_to_string(paths::mcp_config_file()) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let servers = match json.get("mcpServers").and_then(|v| v.as_object()) {
        Some(s) => s,
        None => return vec![],
    };

    servers
        .iter()
        .filter(|(_, v)| {
            v.get("type")
                .and_then(|t| t.as_str())
                .map(|t| t == "http" || t == "sse")
                .unwrap_or(false)
        })
        .filter(|(k, _)| k.starts_with(&*cur))
        .map(|(k, _)| CompletionCandidate::new(k))
        .collect()
}

pub(super) fn secrets_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    match load_app_config_for_completion() {
        Ok(app_config) => match Vault::init(&app_config) {
            Ok(vault) => vault
                .list_secrets(false)
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.starts_with(&*cur))
                .map(CompletionCandidate::new)
                .collect(),
            Err(_) => vec![],
        },
        Err(_) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::reserved_agents::{BuiltinSourceGuard, FixedDirSource};
    use crate::testing::TestConfigDirGuard;
    use serial_test::serial;
    use std::path::Path;
    use std::sync::Arc;

    fn candidate_names(candidates: &[CompletionCandidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect()
    }

    fn write_session(dir: &Path, name: &str) {
        let sessions = dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join(format!("{name}.yaml")), "").unwrap();
    }

    #[test]
    #[serial]
    fn agent_completer_lists_builtin_envoy_with_marker() {
        let _guard = TestConfigDirGuard::new("completer-envoy");

        let candidates = agent_completer(OsStr::new(""));

        let envoy = candidates
            .iter()
            .find(|c| c.get_value() == "envoy")
            .expect("envoy candidate");
        let help = envoy.get_help().expect("envoy help").to_string();
        assert!(help.contains("(built-in)"), "{help}");
    }

    #[test]
    #[serial]
    fn agent_completer_filters_by_prefix() {
        let _guard = TestConfigDirGuard::new("completer-prefix");

        assert_eq!(
            candidate_names(&agent_completer(OsStr::new("env"))),
            vec!["envoy".to_string()]
        );
        assert!(agent_completer(OsStr::new("zzz")).is_empty());
    }

    #[test]
    #[serial]
    fn session_candidates_never_list_shadow_reserved_dir() {
        let _guard = TestConfigDirGuard::new("completer-shadow-sessions");
        write_session(&paths::agents_data_dir().join("envoy"), "leak");

        let names = candidate_names(&session_candidates(Some("envoy"), OsStr::new("")));

        assert!(!names.contains(&"leak".to_string()), "got: {names:?}");
    }

    #[test]
    #[serial]
    fn session_candidates_list_registered_builtin_sessions() {
        let guard = TestConfigDirGuard::new("completer-builtin-sessions");
        write_session(&paths::agents_data_dir().join("envoy"), "leak");
        let dir = guard.path.join("builtin-envoy");
        write_session(&dir, "real");
        let _source = BuiltinSourceGuard::new(Arc::new(FixedDirSource(dir)));

        let names = candidate_names(&session_candidates(Some("Envoy"), OsStr::new("")));

        assert_eq!(names, vec!["real".to_string()]);
    }

    #[test]
    #[serial]
    fn session_candidates_list_user_agent_sessions() {
        let _guard = TestConfigDirGuard::new("completer-user-sessions");
        write_session(&paths::agents_data_dir().join("other"), "mine");
        write_session(&paths::agents_data_dir().join("other"), "yours");

        let mut names = candidate_names(&session_candidates(Some("other"), OsStr::new("")));
        names.sort();
        assert_eq!(names, vec!["mine".to_string(), "yours".to_string()]);

        assert_eq!(
            candidate_names(&session_candidates(Some("other"), OsStr::new("m"))),
            vec!["mine".to_string()]
        );
    }
}
