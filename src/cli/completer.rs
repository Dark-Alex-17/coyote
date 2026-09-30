use crate::client::{ModelType, list_models};
use crate::config::paths;
use crate::config::{
    AppConfig, Config, labeled_session_names, list_agents_for_humans, sanitize_display_text,
    session_scope_dirs,
};
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
    extract_agent_from_args_in(env::args())
}

fn extract_agent_from_args_in<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let args: Vec<String> = args.into_iter().collect();
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
    session_candidates(
        &current.to_string_lossy(),
        extract_agent_from_args().as_deref(),
    )
}

fn session_candidates(cur: &str, agent: Option<&str>) -> Vec<CompletionCandidate> {
    let (workspace_dir, global_dir) = session_scope_dirs(agent);
    labeled_session_names(&workspace_dir, &global_dir)
        .into_iter()
        .filter(|(name, _)| name.starts_with(cur))
        .map(|(name, scope)| {
            CompletionCandidate::new(sanitize_display_text(&name))
                .help(Some(scope.to_string().into()))
        })
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
    use crate::config::default_sessions_dir;
    use crate::config::reserved_agents::{BuiltinSourceGuard, FixedDirSource};
    use crate::testing::{EnvVarGuard, TestConfigDirGuard};
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

    fn seed(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.yaml")), "model: x\n").unwrap();
    }

    fn labeled(candidates: Vec<CompletionCandidate>) -> Vec<(String, Option<String>)> {
        candidates
            .into_iter()
            .map(|c| {
                (
                    c.get_value().to_string_lossy().into_owned(),
                    c.get_help().map(|h| h.to_string()),
                )
            })
            .collect()
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
    fn session_completer_labels_scope_and_prefers_workspace() {
        let _guard = TestConfigDirGuard::new("cli-session-completer");
        seed(&paths::workspace_sessions_dir(), "both");
        seed(&default_sessions_dir(), "both");
        seed(&default_sessions_dir(), "only-global");
        seed(&paths::workspace_sessions_dir(), "only-ws");

        assert_eq!(
            labeled(session_completer(OsStr::new(""))),
            vec![
                ("both".to_string(), Some("workspace".to_string())),
                ("only-ws".to_string(), Some("workspace".to_string())),
                ("only-global".to_string(), Some("global".to_string())),
            ]
        );
        assert_eq!(
            labeled(session_completer(OsStr::new("on"))),
            vec![
                ("only-ws".to_string(), Some("workspace".to_string())),
                ("only-global".to_string(), Some("global".to_string())),
            ]
        );
    }

    #[test]
    #[serial]
    fn session_candidates_never_list_shadow_reserved_dir() {
        let _guard = TestConfigDirGuard::new("completer-shadow-sessions");
        write_session(&paths::agents_data_dir().join("envoy"), "leak");

        let names = candidate_names(&session_candidates("", Some("envoy")));

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

        let names = candidate_names(&session_candidates("", Some("Envoy")));

        assert_eq!(names, vec!["real".to_string()]);
    }

    #[test]
    #[serial]
    fn session_candidates_list_user_agent_sessions() {
        let _guard = TestConfigDirGuard::new("completer-user-sessions");
        write_session(&paths::agents_data_dir().join("other"), "mine");
        write_session(&paths::agents_data_dir().join("other"), "yours");

        let mut names = candidate_names(&session_candidates("", Some("other")));
        names.sort();
        assert_eq!(names, vec!["mine".to_string(), "yours".to_string()]);

        assert_eq!(
            candidate_names(&session_candidates("m", Some("other"))),
            vec!["mine".to_string()]
        );
    }

    #[test]
    #[serial]
    fn session_candidates_use_the_agent_scope_dirs() {
        let _guard = TestConfigDirGuard::new("cli-agent-session-completer");
        let _agent_data_dir = EnvVarGuard::unset("A_DATA_DIR");
        seed(&paths::workspace_agent_sessions_dir("a"), "both");
        seed(&paths::agent_sessions_dir("a"), "both");
        seed(&paths::agent_sessions_dir("a"), "agent-global");
        seed(&default_sessions_dir(), "plain-global");

        assert_eq!(
            labeled(session_candidates("", Some("a"))),
            vec![
                ("both".to_string(), Some("workspace".to_string())),
                ("agent-global".to_string(), Some("global".to_string())),
            ]
        );
        assert_eq!(
            labeled(session_candidates("", None)),
            vec![("plain-global".to_string(), Some("global".to_string()))]
        );
    }

    #[test]
    fn extract_agent_from_args_in_reads_both_flag_forms() {
        fn args(s: &str) -> Vec<String> {
            s.split_whitespace().map(str::to_string).collect()
        }
        assert_eq!(
            extract_agent_from_args_in(args("coyote --agent a --session x")),
            Some("a".to_string())
        );
        assert_eq!(
            extract_agent_from_args_in(args("coyote -a b")),
            Some("b".to_string())
        );
        assert_eq!(
            extract_agent_from_args_in(args("coyote --agent=c")),
            Some("c".to_string())
        );
        assert_eq!(extract_agent_from_args_in(args("coyote --agent")), None);
        assert_eq!(extract_agent_from_args_in(args("coyote --session x")), None);
    }

    #[test]
    #[serial]
    fn config_dir_guard_clears_and_restores_the_sessions_dir_override() {
        let key = crate::utils::get_env_name("sessions_dir");
        let junk = std::path::PathBuf::from("/nonexistent/junk-sessions");
        let _outer = EnvVarGuard::set(&key, &junk);
        {
            let guard = TestConfigDirGuard::new("cli-sessions-dir-guard");
            assert!(default_sessions_dir().starts_with(&guard.path));
        }
        assert_eq!(default_sessions_dir(), junk);
    }
}
