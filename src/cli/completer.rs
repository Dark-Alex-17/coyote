use crate::client::{list_models, ModelType};
use crate::config::{list_agents, Config};
use clap_complete::CompletionCandidate;
use std::ffi::OsStr;

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
