use crate::client::{ModelType, list_models};
use crate::config::{Config, list_agents};
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
    match Config::init_bare() {
        Ok(config) => list_models(&config, ModelType::Chat)
            .into_iter()
            .filter(|&m| m.id().starts_with(&*cur))
            .map(|m| CompletionCandidate::new(m.id()))
            .collect(),
        Err(_) => vec![],
    }
}

pub(super) fn role_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    Config::list_roles(true)
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
    Config::list_rags()
        .into_iter()
        .filter(|r| r.starts_with(&*cur))
        .map(CompletionCandidate::new)
        .collect()
}

pub(super) fn macro_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    Config::list_macros()
        .into_iter()
        .filter(|m| m.starts_with(&*cur))
        .map(CompletionCandidate::new)
        .collect()
}

pub(super) fn session_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    match Config::init_bare() {
        Ok(config) => config
            .list_sessions()
            .into_iter()
            .filter(|s| s.starts_with(&*cur))
            .map(CompletionCandidate::new)
            .collect(),
        Err(_) => vec![],
    }
}

pub(super) fn secrets_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    match Config::init_bare() {
        Ok(config) => config
            .vault
            .list_secrets(false)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.starts_with(&*cur))
            .map(CompletionCandidate::new)
            .collect(),
        Err(_) => vec![],
    }
}
