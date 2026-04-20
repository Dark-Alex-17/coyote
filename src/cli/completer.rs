use crate::client::{ModelType, list_models};
use crate::config::paths;
use crate::config::{AppConfig, Config, list_agents, list_sessions};
use crate::vault::Vault;
use clap_complete::{CompletionCandidate, Shell, generate};
use clap_complete_nushell::Nushell;
use std::ffi::OsStr;
use std::io;

const LOKI_CLI_NAME: &str = "loki";

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
            Self::Bash => generate(Shell::Bash, cmd, LOKI_CLI_NAME, &mut io::stdout()),
            Self::Elvish => generate(Shell::Elvish, cmd, LOKI_CLI_NAME, &mut io::stdout()),
            Self::Fish => generate(Shell::Fish, cmd, LOKI_CLI_NAME, &mut io::stdout()),
            Self::PowerShell => generate(Shell::PowerShell, cmd, LOKI_CLI_NAME, &mut io::stdout()),
            Self::Zsh => generate(Shell::Zsh, cmd, LOKI_CLI_NAME, &mut io::stdout()),
            Self::Nushell => generate(Nushell, cmd, LOKI_CLI_NAME, &mut io::stdout()),
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
        Some(handle) => {
            tokio::task::block_in_place(|| handle.block_on(Config::load_with_interpolation(true)))?
        }
        None => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(Config::load_with_interpolation(true))?
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
    list_agents()
        .into_iter()
        .filter(|a| a.starts_with(&*cur))
        .map(CompletionCandidate::new)
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

pub(super) fn session_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    list_sessions()
        .into_iter()
        .filter(|s| s.starts_with(&*cur))
        .map(CompletionCandidate::new)
        .collect()
}

pub(super) fn secrets_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    match load_app_config_for_completion() {
        Ok(app_config) => Vault::init(&app_config)
            .list_secrets(false)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.starts_with(&*cur))
            .map(CompletionCandidate::new)
            .collect(),
        Err(_) => vec![],
    }
}
