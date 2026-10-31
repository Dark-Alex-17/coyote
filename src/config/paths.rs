use super::reserved_agents::{builtin_agent_dir, reserved_agent};
use super::role::Role;
use super::{
    AGENT_GRAPH_FILE_NAME, AGENTS_DIR_NAME, BASE_KIT_HANDLE, BASH_PROMPT_UTILS_FILE_NAME,
    CONFIG_FILE_NAME, ENV_FILE_NAME, FUNCTIONS_BIN_DIR_NAME, FUNCTIONS_DIR_NAME,
    GLOBAL_TOOLS_DIR_NAME, GLOBAL_TOOLS_UTILS_DIR_NAME, HIDDEN_MCP_FILE_NAME, HOOKS_DIR_NAME,
    INSTALLED_BUNDLES_FILE_NAME, KITS_DIR_NAME, MACROS_DIR_NAME, MCP_FILE_NAME, MEMORY_DIR_NAME,
    MEMORY_INDEX_FILE_NAME, ModelsOverride, RAGS_DIR_NAME, ROLES_DIR_NAME, SBX_KITS_DIR_NAME,
    SESSIONS_DIR_NAME, SKILLS_DIR_NAME, WORKSPACE_COYOTE_DIR_NAME,
};
use crate::client::ProviderModels;
use crate::config::REPL_HISTORY_DIR_NAME;
use crate::config::session::Session;
use crate::utils::{get_env_name, list_file_names, normalize_env_name};
use crate::{rag, sandbox};

use anyhow::{Context, Result, anyhow, bail};
use log::LevelFilter;
use std::collections::HashSet;
use std::env;
use std::fs::{read_dir, read_to_string, remove_dir_all, remove_file};
use std::path::{Path, PathBuf};

pub fn config_dir() -> PathBuf {
    if let Ok(v) = env::var(get_env_name("config_dir")) {
        PathBuf::from(v)
    } else if let Ok(v) = env::var("XDG_CONFIG_HOME") {
        PathBuf::from(v).join(env!("CARGO_CRATE_NAME"))
    } else {
        let dir = dirs::config_dir().expect("No user's config directory");
        dir.join(env!("CARGO_CRATE_NAME"))
    }
}

pub fn local_dir(name: &str) -> PathBuf {
    config_dir().join(name)
}

pub fn cache_dir() -> PathBuf {
    if let Ok(v) = env::var(get_env_name("cache_dir")) {
        PathBuf::from(v)
    } else if let Ok(v) = env::var("XDG_CACHE_HOME") {
        PathBuf::from(v).join(env!("CARGO_CRATE_NAME"))
    } else {
        let base_dir = dirs::cache_dir().unwrap_or_else(env::temp_dir);
        base_dir.join(env!("CARGO_CRATE_NAME"))
    }
}

pub fn sandbox_kit_override() -> Option<PathBuf> {
    env::var_os(get_env_name("sandbox_kit")).map(PathBuf::from)
}

pub fn translate_sandboxed_home_dir(path: &Path) -> Option<PathBuf> {
    env::var_os("IS_SANDBOX")?;

    let s = path.to_str()?;

    if let Some(translated) = translate_unix_home_style(s, "/home/") {
        return Some(translated);
    }

    if let Some(translated) = translate_unix_home_style(s, "/Users/") {
        return Some(translated);
    }

    translate_windows_users_dir(s)
}

fn translate_unix_home_style(s: &str, prefix: &str) -> Option<PathBuf> {
    let rest = s.strip_prefix(prefix)?;
    let (user, tail) = match rest.split_once('/') {
        Some((u, t)) => (u, t),
        None => (rest, ""),
    };

    if user.is_empty() || user == "agent" {
        return None;
    }

    Some(if tail.is_empty() {
        PathBuf::from("/home/agent")
    } else {
        PathBuf::from(format!("/home/agent/{tail}"))
    })
}

fn translate_windows_users_dir(s: &str) -> Option<PathBuf> {
    let bytes = s.as_bytes();
    if bytes.len() < 4 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return None;
    }

    let after_drive = &s[3..];
    let rest = after_drive.strip_prefix("Users\\")?;
    let (user, tail) = match rest.split_once('\\') {
        Some((u, t)) => (u, t.replace('\\', "/")),
        None => (rest, String::new()),
    };

    if user.is_empty() || user == "agent" {
        return None;
    }

    Some(if tail.is_empty() {
        PathBuf::from("/home/agent")
    } else {
        PathBuf::from(format!("/home/agent/{tail}"))
    })
}

pub(crate) fn kits_dir(root: &Path) -> PathBuf {
    root.join(KITS_DIR_NAME)
}

/// Every `.coyote/kits/` directory on the walk from `start` up to the
/// filesystem root, nearest first. Only existing directories are returned.
pub(crate) fn workspace_kit_roots(start: &Path) -> Vec<PathBuf> {
    start
        .ancestors()
        .map(|dir| kits_dir(&dir.join(WORKSPACE_COYOTE_DIR_NAME)))
        .filter(|candidate| candidate.is_dir())
        .collect()
}

pub fn oauth_tokens_dir() -> PathBuf {
    cache_dir().join("oauth")
}

pub fn token_file(client_name: &str) -> PathBuf {
    oauth_tokens_dir().join(format!("{client_name}_oauth_tokens.json"))
}

pub fn log_file() -> PathBuf {
    cache_dir().join(format!("{}.log", env!("CARGO_CRATE_NAME")))
}

/// How many rotated generations the logger keeps next to the live file.
pub const LOG_ARCHIVE_COUNT: u32 = 5;

/// The rotated siblings the logger writes for `log`, in the roller's
/// naming: `<stem>.archived.<n>.log`.
pub fn log_archive_files(log: &Path) -> Vec<PathBuf> {
    (0..LOG_ARCHIVE_COUNT)
        .map(|index| log.with_extension(format!("archived.{index}.log")))
        .collect()
}

pub fn sbx_kits_dir() -> PathBuf {
    cache_dir().join(SBX_KITS_DIR_NAME)
}

pub fn base_kit_dir() -> PathBuf {
    sbx_kits_dir().join(BASE_KIT_HANDLE)
}

pub fn base_kit_hash_file() -> PathBuf {
    sbx_kits_dir().join(format!("{BASE_KIT_HANDLE}.sha256"))
}

pub(crate) fn generated_kit_dir(handle: &str) -> PathBuf {
    sbx_kits_dir().join(handle)
}

pub fn sandbox_mixin_hashes_dir() -> PathBuf {
    cache_dir().join("sandbox-mixin-hashes")
}

pub fn sandbox_mixin_hash_file(sandbox_name: &str) -> PathBuf {
    // Sandbox names are sanitized by the caller, but never trust a path
    // component: a stray separator must not escape the hash directory.
    sandbox_mixin_hashes_dir().join(format!("{}.hash", sandbox_name.replace('/', "_")))
}

pub fn config_file() -> PathBuf {
    match env::var(get_env_name("config_file")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_dir(CONFIG_FILE_NAME),
    }
}

pub fn installed_bundles_file() -> PathBuf {
    local_dir(INSTALLED_BUNDLES_FILE_NAME)
}

pub fn roles_dir() -> PathBuf {
    match env::var(get_env_name("roles_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_dir(ROLES_DIR_NAME),
    }
}

pub fn role_file(name: &str) -> PathBuf {
    roles_dir().join(format!("{name}.md"))
}

pub fn hooks_dir() -> PathBuf {
    match env::var(get_env_name("hooks_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_dir(HOOKS_DIR_NAME),
    }
}

pub fn skills_dir() -> PathBuf {
    match env::var(get_env_name("skills_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_dir(SKILLS_DIR_NAME),
    }
}

pub fn skill_dir(name: &str) -> PathBuf {
    skills_dir().join(name)
}

pub fn skill_file(name: &str) -> PathBuf {
    skill_dir(name).join("SKILL.md")
}

/// The name the workspace config directory goes by under a workspace root; the env
/// override is returned as given, so it may be a relative or an absolute path. Sessions,
/// agent sessions, skills, macros and the workspace mcp.json live under this name;
/// workspace memory and the sbx mixin always use the literal `.coyote`.
pub fn workspace_config_dir_name() -> String {
    match env::var(get_env_name("workspace_config_dir")) {
        Ok(value) => value,
        Err(_) => WORKSPACE_COYOTE_DIR_NAME.to_string(),
    }
}

/// Every name the workspace config directory goes by under a workspace root. The first
/// is the runtime name: the env override as given, which may be a relative or an
/// absolute path, and is where sessions, agent sessions, skills, macros and the
/// workspace mcp.json live. The literal default follows when it differs: workspace
/// memory and the sbx mixin always live under it, override or not. Everything that
/// protects or denies the workspace config directory protects every name listed here.
pub fn workspace_config_dirs() -> Vec<String> {
    let mut names = vec![
        workspace_config_dir_name(),
        WORKSPACE_COYOTE_DIR_NAME.to_string(),
    ];
    names.dedup();
    names
}

pub fn workspace_config_dir() -> PathBuf {
    env::current_dir()
        .unwrap_or_default()
        .join(workspace_config_dir_name())
}

/// `workspace_config_dirs` joined under the current directory, in the same order.
pub fn workspace_config_dir_paths() -> Vec<PathBuf> {
    let cwd = env::current_dir().unwrap_or_default();
    workspace_config_dirs()
        .iter()
        .map(|name| cwd.join(name))
        .collect()
}

pub fn workspace_skills_dir() -> PathBuf {
    workspace_config_dir().join(SKILLS_DIR_NAME)
}

pub fn workspace_skill_file(name: &str) -> PathBuf {
    workspace_skills_dir().join(name).join("SKILL.md")
}

pub fn workspace_macros_dir() -> PathBuf {
    workspace_config_dir().join(MACROS_DIR_NAME)
}

pub fn workspace_sessions_dir() -> PathBuf {
    workspace_config_dir().join(SESSIONS_DIR_NAME)
}

pub fn workspace_agent_sessions_dir(name: &str) -> PathBuf {
    workspace_config_dir()
        .join(AGENTS_DIR_NAME)
        .join(name)
        .join(SESSIONS_DIR_NAME)
}

pub fn workspace_mcp_config_file() -> Option<PathBuf> {
    workspace_mcp_config_file_in(&env::current_dir().unwrap_or_default())
}

fn workspace_mcp_config_file_in(workspace_root: &Path) -> Option<PathBuf> {
    let dir = workspace_config_dir();
    [
        dir.join(MCP_FILE_NAME),
        dir.join(HIDDEN_MCP_FILE_NAME),
        workspace_root.join(HIDDEN_MCP_FILE_NAME),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

pub fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Skill name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("Invalid skill name '{name}': only letters, digits, '-', and '_' are allowed");
    }
    Ok(())
}

pub fn macros_dir() -> PathBuf {
    match env::var(get_env_name("macros_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_dir(MACROS_DIR_NAME),
    }
}

pub fn macro_file(name: &str) -> PathBuf {
    macros_dir().join(format!("{name}.yaml"))
}

pub fn env_file() -> PathBuf {
    match env::var(get_env_name("env_file")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_dir(ENV_FILE_NAME),
    }
}

pub fn rags_dir() -> PathBuf {
    match env::var(get_env_name("rags_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_dir(RAGS_DIR_NAME),
    }
}

pub fn functions_dir() -> PathBuf {
    match env::var(get_env_name("functions_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_dir(FUNCTIONS_DIR_NAME),
    }
}

pub fn functions_bin_dir() -> PathBuf {
    functions_dir().join(FUNCTIONS_BIN_DIR_NAME)
}

pub fn mcp_config_file() -> PathBuf {
    let preferred = local_dir(MCP_FILE_NAME);
    if preferred.exists() {
        return preferred;
    }
    let legacy = legacy_mcp_config_file();
    if legacy.exists() {
        return legacy;
    }
    preferred
}

pub fn legacy_mcp_config_file() -> PathBuf {
    functions_dir().join(MCP_FILE_NAME)
}

pub fn global_tools_dir() -> PathBuf {
    functions_dir().join(GLOBAL_TOOLS_DIR_NAME)
}

pub fn global_utils_dir() -> PathBuf {
    functions_dir().join(GLOBAL_TOOLS_UTILS_DIR_NAME)
}

pub fn bash_prompt_utils_file() -> PathBuf {
    global_utils_dir().join(BASH_PROMPT_UTILS_FILE_NAME)
}

pub fn agents_data_dir() -> PathBuf {
    local_dir(AGENTS_DIR_NAME)
}

pub fn agent_data_dir(name: &str) -> PathBuf {
    if let Some(canonical) = reserved_agent(name) {
        return builtin_agent_dir(canonical).unwrap_or_else(|| unregistered_builtin_dir(canonical));
    }
    match env::var(format!("{}_DATA_DIR", normalize_env_name(name))) {
        Ok(value) => PathBuf::from(value),
        Err(_) => agents_data_dir().join(name),
    }
}

/// Fail-closed placeholder for a reserved agent while no built-in source is
/// registered. The interior NUL bytes make the path unrepresentable on every
/// platform, so nothing can be planted there; `Agent::init` returns
/// `BuiltinAgentUnavailable` before it ever touches this path.
fn unregistered_builtin_dir(canonical: &str) -> PathBuf {
    PathBuf::from(format!("\0unregistered-builtin\0/{canonical}"))
}

pub fn agent_sessions_dir(name: &str) -> PathBuf {
    agent_data_dir(name).join(SESSIONS_DIR_NAME)
}

pub fn agent_graph_file(agent_name: &str) -> PathBuf {
    agent_data_dir(agent_name).join(AGENT_GRAPH_FILE_NAME)
}

pub fn agent_config_file(name: &str) -> PathBuf {
    if reserved_agent(name).is_some() {
        return agent_data_dir(name).join(CONFIG_FILE_NAME);
    }
    match env::var(format!("{}_CONFIG_FILE", normalize_env_name(name))) {
        Ok(value) => PathBuf::from(value),
        Err(_) => agent_data_dir(name).join(CONFIG_FILE_NAME),
    }
}

pub fn agent_bin_dir(name: &str) -> PathBuf {
    agent_data_dir(name).join(FUNCTIONS_BIN_DIR_NAME)
}

pub fn agent_rag_file(agent_name: &str, rag_name: &str) -> PathBuf {
    agent_data_dir(agent_name).join(format!("{rag_name}.yaml"))
}

pub fn agent_functions_file(name: &str) -> Result<PathBuf> {
    let priority = ["tools.sh", "tools.py", "tools.ts", "tools.js"];
    let dir = agent_data_dir(name);

    for filename in priority {
        let path = dir.join(filename);
        if path.exists() {
            return Ok(path);
        }
    }

    Err(anyhow!(
        "No tools script found in agent functions directory"
    ))
}

pub fn models_override_file() -> PathBuf {
    local_dir("models-override.yaml")
}

pub fn global_memory_dir() -> PathBuf {
    config_dir().join(MEMORY_DIR_NAME)
}

pub fn global_memory_index_file() -> PathBuf {
    global_memory_dir().join(MEMORY_INDEX_FILE_NAME)
}

/// The directory workspace memory lives under: always the literal default name, since
/// memory ignores the workspace config dir override.
pub fn workspace_memory_home_name() -> &'static str {
    WORKSPACE_COYOTE_DIR_NAME
}

pub fn workspace_memory_dir_for(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(workspace_memory_home_name())
        .join(MEMORY_DIR_NAME)
}

pub fn workspace_memory_index_file_for(workspace_root: &Path) -> PathBuf {
    workspace_memory_dir_for(workspace_root).join(MEMORY_INDEX_FILE_NAME)
}

/// The `.gitignore` line that keeps workspace memory out of the repository: a
/// slash-separated, slash-terminated path relative to the repository root, not an OS
/// path, since `.gitignore` reads it the same way on every platform.
pub fn workspace_memory_gitignore_entry() -> String {
    format!("{WORKSPACE_COYOTE_DIR_NAME}/{MEMORY_DIR_NAME}/")
}

pub fn repl_history_dir() -> PathBuf {
    cache_dir().join(REPL_HISTORY_DIR_NAME)
}

pub fn repl_history_file(session: &Option<Session>) -> PathBuf {
    let history_key = if let Some(session) = &session {
        format!("session_{}", session.name().replace('/', "_"))
    } else {
        "default".to_string()
    };

    repl_history_dir().join(history_key)
}

pub fn log_config() -> Result<(LevelFilter, Option<PathBuf>)> {
    let log_level = env::var(get_env_name("log_level"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(match cfg!(debug_assertions) {
            true => LevelFilter::Debug,
            false => LevelFilter::Info,
        });
    let resolved_log_path = match env::var(get_env_name("log_path")) {
        Ok(v) => Some(PathBuf::from(v)),
        Err(_) => Some(log_file()),
    };
    Ok((log_level, resolved_log_path))
}

pub fn list_roles(with_builtin: bool) -> Vec<String> {
    let mut names = HashSet::new();
    if let Ok(rd) = read_dir(roles_dir()) {
        for entry in rd.flatten() {
            if let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|v| v.strip_suffix(".md"))
            {
                names.insert(name.to_string());
            }
        }
    }
    if with_builtin {
        names.extend(Role::list_builtin_role_names());
    }
    let mut names: Vec<_> = names.into_iter().collect();
    names.sort_unstable();
    names
}

pub fn has_role(name: &str) -> bool {
    let names = list_roles(true);
    names.contains(&name.to_string())
}

pub fn list_rags() -> Vec<String> {
    match read_dir(rags_dir()) {
        Ok(rd) => {
            let mut names = vec![];
            for entry in rd.flatten() {
                let name = entry.file_name();
                if let Some(name) = name.to_string_lossy().strip_suffix(".yaml") {
                    names.push(name.to_string());
                }
            }
            names.sort_unstable();
            names
        }
        Err(_) => vec![],
    }
}

pub(crate) fn rag_kit_handle(id: &str) -> String {
    format!("rag-{id}")
}

pub(crate) fn rag_kit_dir(rag_dir: &Path, id: &str) -> PathBuf {
    kits_dir(rag_dir).join(rag_kit_handle(id))
}

pub(crate) fn remove_rag_sidecars(dir: &Path, name: &str) -> Result<()> {
    // The kit step can refuse, and the duckdb unlink cannot be undone, so the
    // kit goes first: a refusal must leave the RAG whole for a retry.
    let id = sandbox::mcp_credentials::secret_service_id(name);
    if !id.is_empty() {
        let kit_dir = rag_kit_dir(dir, &id);
        if kit_dir.symlink_metadata().is_ok() {
            let prefix = format!(
                "Failed to remove the sandbox kit for RAG '{name}' at '{}'",
                kit_dir.display()
            );
            if !rag::rag_kit_belongs_to(&kit_dir, name) {
                bail!(
                    "{prefix}: the kit at this handle belongs to another RAG or was \
                     written by hand. The RAG was NOT deleted so you can retry; remove \
                     that other RAG first, or move the directory aside by hand."
                );
            }
            remove_dir_all(&kit_dir).with_context(|| {
                format!(
                    "{prefix}. The RAG was NOT deleted so you can retry; this host \
                     remains whitelisted in the sandbox until the directory is removed."
                )
            })?;
        }
    }
    let duckdb_path = dir.join(format!("{name}.duckdb"));
    if duckdb_path.exists() {
        let _ = remove_file(&duckdb_path);
    }
    let wal_path = dir.join(format!("{name}.duckdb.wal"));
    if wal_path.exists() {
        let _ = remove_file(&wal_path);
    }

    Ok(())
}

pub fn list_macros() -> Vec<String> {
    list_file_names(macros_dir(), ".yaml")
}

pub fn list_skills() -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();

    for dir in [workspace_skills_dir(), skills_dir()] {
        if let Ok(rd) = read_dir(dir) {
            for entry in rd.flatten() {
                if let Ok(file_type) = entry.file_type()
                    && file_type.is_dir()
                    && let Some(name) = entry.file_name().to_str()
                    && !seen.contains(name)
                    && entry.path().join("SKILL.md").is_file()
                    && validate_skill_name(name).is_ok()
                {
                    seen.insert(name.to_string());
                    names.push(name.to_string());
                }
            }
        }
    }

    names.sort_unstable();
    names
}

pub fn has_skill(name: &str) -> bool {
    workspace_skill_file(name).is_file() || skill_file(name).is_file()
}

pub fn local_models_override() -> Result<Vec<ProviderModels>> {
    let model_override_path = models_override_file();
    let err = || {
        format!(
            "Failed to load models at '{}'",
            model_override_path.display()
        )
    };
    let content = read_to_string(&model_override_path).with_context(err)?;
    let models_override: ModelsOverride = serde_yaml::from_str(&content).with_context(err)?;
    if models_override.version != env!("CARGO_PKG_VERSION") {
        bail!("Incompatible version")
    }
    Ok(models_override.list)
}

#[cfg(test)]
mod tests {
    use crate::testing;

    use super::*;
    use crate::config::reserved_agents::{BuiltinSourceGuard, FixedDirSource};
    use crate::testing::{EnvVarGuard, TestConfigDirGuard};
    use serial_test::serial;
    use std::sync::Arc;
    use std::{fs, time};

    #[test]
    fn validate_skill_name_accepts_alphanumerics_and_dashes() {
        assert!(validate_skill_name("git-master").is_ok());
        assert!(validate_skill_name("code_review").is_ok());
        assert!(validate_skill_name("Skill1").is_ok());
    }

    #[test]
    fn validate_skill_name_rejects_empty() {
        let err = validate_skill_name("").unwrap_err();
        assert!(err.to_string().contains("cannot be empty"));
    }

    #[test]
    fn validate_skill_name_rejects_path_traversal() {
        for bad in ["../escape", "..", "foo/bar", "foo\\bar", "./hidden"] {
            let err = validate_skill_name(bad).unwrap_err();
            assert!(
                err.to_string().contains("Invalid skill name"),
                "expected rejection for {bad:?}, got: {err}"
            );
        }
    }

    #[test]
    fn validate_skill_name_rejects_other_special_chars() {
        for bad in ["with space", "null\0byte", "weird?char", "dot.name"] {
            assert!(
                validate_skill_name(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn has_skill_returns_false_for_missing_paths() {
        for absent in ["definitely-not-installed-skill-xyz", "another-missing"] {
            assert!(
                !has_skill(absent),
                "has_skill({absent:?}) should be false for a missing skill"
            );
        }
    }

    #[test]
    #[serial]
    fn workspace_config_dir_name_is_the_env_override_or_the_default() {
        let env_name = get_env_name("workspace_config_dir");
        {
            let _unset = EnvVarGuard::unset(&env_name);
            assert_eq!(workspace_config_dir_name(), WORKSPACE_COYOTE_DIR_NAME);
            assert_eq!(
                workspace_config_dir(),
                env::current_dir().unwrap().join(WORKSPACE_COYOTE_DIR_NAME)
            );
        }
        let absolute = env::temp_dir().join("coyote-ws-name-override");
        let _set = EnvVarGuard::set(&env_name, &absolute);
        assert_eq!(workspace_config_dir_name(), absolute.to_str().unwrap());
        assert_eq!(workspace_config_dir(), absolute);
        let _relative = EnvVarGuard::set(&env_name, "conf/.hidden");
        assert_eq!(workspace_config_dir_name(), "conf/.hidden");
    }

    #[test]
    #[serial]
    fn workspace_config_dirs_lists_the_override_first_and_the_default_once() {
        let env_name = get_env_name("workspace_config_dir");
        let cwd = env::current_dir().unwrap();
        {
            let _unset = EnvVarGuard::unset(&env_name);
            assert_eq!(workspace_config_dirs(), [WORKSPACE_COYOTE_DIR_NAME]);
            assert_eq!(
                workspace_config_dir_paths(),
                [cwd.join(WORKSPACE_COYOTE_DIR_NAME)]
            );
        }
        {
            let _same = EnvVarGuard::set(&env_name, WORKSPACE_COYOTE_DIR_NAME);
            assert_eq!(workspace_config_dirs(), [WORKSPACE_COYOTE_DIR_NAME]);
        }
        let absolute = env::temp_dir().join("coyote-ws-dirs-override");
        let _set = EnvVarGuard::set(&env_name, &absolute);
        assert_eq!(
            workspace_config_dirs(),
            [absolute.to_str().unwrap(), WORKSPACE_COYOTE_DIR_NAME]
        );
        assert_eq!(
            workspace_config_dir_paths(),
            [absolute.clone(), cwd.join(WORKSPACE_COYOTE_DIR_NAME)]
        );
    }

    /// A relative override is listed as given, ahead of the literal default, and joins
    /// under the cwd segment by segment; the memory dir never follows it. An empty
    /// override is passed through as the empty name, which joins to the cwd itself.
    #[test]
    #[serial]
    fn a_relative_override_is_listed_as_given_and_memory_ignores_it() {
        let env_name = get_env_name("workspace_config_dir");
        let cwd = env::current_dir().unwrap();
        let root = Path::new("/ws");
        {
            let _relative = EnvVarGuard::set(&env_name, "conf/.hidden");
            assert_eq!(
                workspace_config_dirs(),
                ["conf/.hidden", WORKSPACE_COYOTE_DIR_NAME]
            );
            assert_eq!(
                workspace_config_dir_paths(),
                [
                    cwd.join("conf").join(".hidden"),
                    cwd.join(WORKSPACE_COYOTE_DIR_NAME)
                ]
            );
            assert_eq!(workspace_memory_dir_for(root), root.join(".coyote/memory"));
        }
        let _empty = EnvVarGuard::set(&env_name, "");
        assert_eq!(workspace_config_dirs(), ["", WORKSPACE_COYOTE_DIR_NAME]);
        assert_eq!(workspace_config_dir_paths()[0], cwd);
    }

    #[test]
    #[serial]
    fn workspace_memory_stays_under_the_literal_name_when_the_config_dir_is_overridden() {
        let env_name = get_env_name("workspace_config_dir");
        let root = env::temp_dir().join("coyote-ws-memory-override-root");
        let override_dir = env::temp_dir().join("coyote-ws-memory-override-cfg");
        let _set = EnvVarGuard::set(&env_name, &override_dir);

        assert_eq!(
            workspace_config_dirs(),
            [override_dir.to_str().unwrap(), WORKSPACE_COYOTE_DIR_NAME]
        );
        assert_eq!(
            workspace_memory_dir_for(&root),
            root.join(WORKSPACE_COYOTE_DIR_NAME).join(MEMORY_DIR_NAME)
        );
        assert_eq!(
            workspace_memory_index_file_for(&root),
            root.join(WORKSPACE_COYOTE_DIR_NAME)
                .join(MEMORY_DIR_NAME)
                .join(MEMORY_INDEX_FILE_NAME)
        );
        assert_eq!(workspace_memory_gitignore_entry(), ".coyote/memory/");
    }

    mod sandbox_home_translation {
        use super::*;
        use serial_test::serial;

        fn with_sandbox<F: FnOnce()>(f: F) {
            let prev = env::var_os("IS_SANDBOX");
            unsafe {
                env::set_var("IS_SANDBOX", "1");
            }
            f();
            unsafe {
                match prev {
                    Some(v) => env::set_var("IS_SANDBOX", v),
                    None => env::remove_var("IS_SANDBOX"),
                }
            }
        }

        fn without_sandbox<F: FnOnce()>(f: F) {
            let prev = env::var_os("IS_SANDBOX");
            unsafe {
                env::remove_var("IS_SANDBOX");
            }
            f();
            unsafe {
                if let Some(v) = prev {
                    env::set_var("IS_SANDBOX", v);
                }
            }
        }

        #[test]
        #[serial]
        fn returns_none_when_not_in_sandbox() {
            without_sandbox(|| {
                let p = Path::new("/home/atusa/.coyote_password");
                assert_eq!(translate_sandboxed_home_dir(p), None);
            });
        }

        #[test]
        #[serial]
        fn translates_host_home_to_agent_home() {
            with_sandbox(|| {
                let p = Path::new("/home/atusa/.coyote_password");
                assert_eq!(
                    translate_sandboxed_home_dir(p),
                    Some(PathBuf::from("/home/agent/.coyote_password"))
                );
            });
        }

        #[test]
        #[serial]
        fn translates_nested_host_home_dir() {
            with_sandbox(|| {
                let p = Path::new("/home/atusa/.config/coyote/.password");
                assert_eq!(
                    translate_sandboxed_home_dir(p),
                    Some(PathBuf::from("/home/agent/.config/coyote/.password"))
                );
            });
        }

        #[test]
        #[serial]
        fn returns_none_when_path_already_targets_agent_home() {
            with_sandbox(|| {
                let p = Path::new("/home/agent/.coyote_password");
                assert_eq!(translate_sandboxed_home_dir(p), None);
            });
        }

        #[test]
        #[serial]
        fn returns_none_when_path_is_outside_home() {
            with_sandbox(|| {
                let p = Path::new("/etc/coyote/.coyote_password");
                assert_eq!(translate_sandboxed_home_dir(p), None);
            });
        }

        #[test]
        #[serial]
        fn returns_none_for_relative_path() {
            with_sandbox(|| {
                let p = Path::new(".coyote_password");
                assert_eq!(translate_sandboxed_home_dir(p), None);
            });
        }

        #[test]
        #[serial]
        fn returns_none_for_first_segment_not_home() {
            with_sandbox(|| {
                let p = Path::new("/opt/atusa/.coyote_password");
                assert_eq!(translate_sandboxed_home_dir(p), None);
            });
        }

        #[test]
        #[serial]
        fn translates_macos_users_dir() {
            with_sandbox(|| {
                let p = Path::new("/Users/atusa/.coyote_password");
                assert_eq!(
                    translate_sandboxed_home_dir(p),
                    Some(PathBuf::from("/home/agent/.coyote_password"))
                );
            });
        }

        #[test]
        #[serial]
        fn translates_macos_nested_dir() {
            with_sandbox(|| {
                let p = Path::new("/Users/atusa/.config/coyote/.password");
                assert_eq!(
                    translate_sandboxed_home_dir(p),
                    Some(PathBuf::from("/home/agent/.config/coyote/.password"))
                );
            });
        }

        #[test]
        #[serial]
        fn returns_none_when_macos_dir_already_targets_agent() {
            with_sandbox(|| {
                let p = Path::new("/Users/agent/.coyote_password");
                assert_eq!(translate_sandboxed_home_dir(p), None);
            });
        }

        #[test]
        #[serial]
        fn translates_windows_drive_letter_path() {
            with_sandbox(|| {
                let p = Path::new("C:\\Users\\atusa\\.coyote_password");
                assert_eq!(
                    translate_sandboxed_home_dir(p),
                    Some(PathBuf::from("/home/agent/.coyote_password"))
                );
            });
        }

        #[test]
        #[serial]
        fn translates_windows_nested_path() {
            with_sandbox(|| {
                let p = Path::new("D:\\Users\\atusa\\.config\\coyote\\.password");
                assert_eq!(
                    translate_sandboxed_home_dir(p),
                    Some(PathBuf::from("/home/agent/.config/coyote/.password"))
                );
            });
        }

        #[test]
        #[serial]
        fn returns_none_when_windows_path_already_targets_agent() {
            with_sandbox(|| {
                let p = Path::new("C:\\Users\\agent\\.coyote_password");
                assert_eq!(translate_sandboxed_home_dir(p), None);
            });
        }
    }

    mod workspace_mcp_resolution {
        use super::*;
        use serial_test::serial;

        fn with_workspace_dir<F: FnOnce(&Path, &Path)>(f: F) {
            let unique = time::SystemTime::now()
                .duration_since(time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!("coyote-workspace-mcp-test-{unique}"));
            let ws_dir = root.join(WORKSPACE_COYOTE_DIR_NAME);
            fs::create_dir_all(&ws_dir).unwrap();
            let env_name = get_env_name("workspace_config_dir");
            let prev = env::var_os(&env_name);
            unsafe {
                env::set_var(&env_name, &ws_dir);
            }
            f(&root, &ws_dir);
            unsafe {
                match prev {
                    Some(v) => env::set_var(&env_name, v),
                    None => env::remove_var(&env_name),
                }
            }
            let _ = fs::remove_dir_all(&root);
        }

        #[test]
        #[serial]
        fn returns_none_when_no_config_exists() {
            with_workspace_dir(|root, _| {
                assert_eq!(workspace_mcp_config_file_in(root), None);
            });
        }

        #[test]
        #[serial]
        fn finds_mcp_json() {
            with_workspace_dir(|root, ws_dir| {
                fs::write(ws_dir.join("mcp.json"), "{}").unwrap();
                assert_eq!(
                    workspace_mcp_config_file_in(root),
                    Some(ws_dir.join("mcp.json"))
                );
            });
        }

        #[test]
        #[serial]
        fn falls_back_to_claude_style_hidden_mcp_json() {
            with_workspace_dir(|root, ws_dir| {
                fs::write(ws_dir.join(".mcp.json"), "{}").unwrap();
                assert_eq!(
                    workspace_mcp_config_file_in(root),
                    Some(ws_dir.join(".mcp.json"))
                );
            });
        }

        #[test]
        #[serial]
        fn prefers_mcp_json_when_both_exist() {
            with_workspace_dir(|root, ws_dir| {
                fs::write(ws_dir.join("mcp.json"), "{}").unwrap();
                fs::write(ws_dir.join(".mcp.json"), "{}").unwrap();
                assert_eq!(
                    workspace_mcp_config_file_in(root),
                    Some(ws_dir.join("mcp.json"))
                );
            });
        }

        #[test]
        #[serial]
        fn falls_back_to_project_root_hidden_mcp_json() {
            with_workspace_dir(|root, _| {
                fs::write(root.join(".mcp.json"), "{}").unwrap();
                assert_eq!(
                    workspace_mcp_config_file_in(root),
                    Some(root.join(".mcp.json"))
                );
            });
        }

        #[test]
        #[serial]
        fn prefers_workspace_dir_config_over_project_root() {
            with_workspace_dir(|root, ws_dir| {
                fs::write(ws_dir.join(".mcp.json"), "{}").unwrap();
                fs::write(root.join(".mcp.json"), "{}").unwrap();
                assert_eq!(
                    workspace_mcp_config_file_in(root),
                    Some(ws_dir.join(".mcp.json"))
                );
            });
        }

        #[test]
        #[serial]
        fn workspace_session_dirs_live_under_workspace_config_dir() {
            with_workspace_dir(|_, ws_dir| {
                assert_eq!(workspace_sessions_dir(), ws_dir.join(SESSIONS_DIR_NAME));
                assert_eq!(
                    workspace_agent_sessions_dir("helper"),
                    ws_dir
                        .join(AGENTS_DIR_NAME)
                        .join("helper")
                        .join(SESSIONS_DIR_NAME)
                );
            });
        }
    }

    mod user_mcp_resolution {
        use super::*;
        use serial_test::serial;

        fn with_config_dir<F: FnOnce(&Path)>(f: F) {
            let guard = testing::TestConfigDirGuard::new("user-mcp-test");
            let functions = guard.path.join(FUNCTIONS_DIR_NAME);
            fs::create_dir_all(&functions).unwrap();
            let _functions_env =
                testing::EnvVarGuard::set(get_env_name("functions_dir"), &functions);
            f(&guard.path);
        }

        #[test]
        #[serial]
        fn defaults_to_preferred_location_when_neither_exists() {
            with_config_dir(|root| {
                assert_eq!(mcp_config_file(), root.join(MCP_FILE_NAME));
            });
        }

        #[test]
        #[serial]
        fn falls_back_to_legacy_location_when_only_it_exists() {
            with_config_dir(|root| {
                let legacy = root.join(FUNCTIONS_DIR_NAME).join(MCP_FILE_NAME);
                fs::write(&legacy, "{}").unwrap();
                assert_eq!(mcp_config_file(), legacy);
            });
        }

        #[test]
        #[serial]
        fn prefers_new_location_when_both_exist() {
            with_config_dir(|root| {
                let preferred = root.join(MCP_FILE_NAME);
                let legacy = root.join(FUNCTIONS_DIR_NAME).join(MCP_FILE_NAME);
                fs::write(&preferred, "{}").unwrap();
                fs::write(&legacy, "{}").unwrap();
                assert_eq!(mcp_config_file(), preferred);
            });
        }

        #[test]
        #[serial]
        fn uses_preferred_location_when_only_it_exists() {
            with_config_dir(|root| {
                let preferred = root.join(MCP_FILE_NAME);
                fs::write(&preferred, "{}").unwrap();
                assert_eq!(mcp_config_file(), preferred);
            });
        }
    }

    #[test]
    fn sandbox_kit_override_reflects_env_var_state() {
        let env_name = get_env_name("sandbox_kit");
        let prev = env::var_os(&env_name);

        unsafe {
            env::remove_var(&env_name);
        }
        assert_eq!(sandbox_kit_override(), None);

        let probe = PathBuf::from("/tmp/coyote-sandbox-kit-probe");
        unsafe {
            env::set_var(&env_name, &probe);
        }
        assert_eq!(sandbox_kit_override(), Some(probe));

        unsafe {
            match prev {
                Some(v) => env::set_var(&env_name, v),
                None => env::remove_var(&env_name),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn hooks_dir_reflects_env_var_state() {
        let env_name = get_env_name("hooks_dir");
        let prev = env::var_os(&env_name);

        unsafe {
            env::remove_var(&env_name);
        }
        assert_eq!(hooks_dir(), config_dir().join("hooks"));

        let probe = PathBuf::from("/tmp/coyote-hooks-dir-probe");
        unsafe {
            env::set_var(&env_name, &probe);
        }
        assert_eq!(hooks_dir(), probe);

        unsafe {
            match prev {
                Some(v) => env::set_var(&env_name, v),
                None => env::remove_var(&env_name),
            }
        }
    }

    #[test]
    fn list_skills_skips_invalid_directory_names() {
        let unique = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-list-skills-test-{unique}"));
        fs::create_dir_all(&root).unwrap();
        let prev = env::var_os(get_env_name("skills_dir"));
        unsafe {
            env::set_var(get_env_name("skills_dir"), &root);
        }

        for name in ["valid-skill", "with space", ".hidden", "dot.name"] {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("SKILL.md"), "body").unwrap();
        }

        let listed = list_skills();
        assert_eq!(listed, vec!["valid-skill".to_string()]);

        unsafe {
            match prev {
                Some(v) => env::set_var(get_env_name("skills_dir"), v),
                None => env::remove_var(get_env_name("skills_dir")),
            }
        }
        let _ = fs::remove_dir_all(&root);
    }

    fn sidecar_temp_dir(label: &str) -> PathBuf {
        let unique = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-{label}-test-{unique}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn rag_kit_dir_nests_the_prefixed_handle_under_kits() {
        assert_eq!(
            rag_kit_dir(Path::new("r"), "v2-docs"),
            Path::new("r").join("kits").join("rag-v2-docs")
        );
    }

    #[test]
    fn kits_dir_nests_kits_under_the_root() {
        assert_eq!(kits_dir(Path::new("r")), Path::new("r").join("kits"));
    }

    #[test]
    #[serial_test::serial]
    fn generated_kit_dir_sits_beside_the_base_kit_in_the_cache() {
        let cache = testing::CacheDirGuard::new("generated-kit-dir");
        assert_eq!(
            generated_kit_dir("coyote-mcp"),
            cache.root.join("sbx-kits").join("coyote-mcp")
        );
        assert_eq!(
            generated_kit_dir("coyote-mcp").parent(),
            base_kit_dir().parent()
        );
    }

    #[test]
    fn workspace_kit_roots_walks_ancestors_nearest_first_and_skips_files() {
        let root = sidecar_temp_dir("workspace-kit-roots");
        let outer = root.join(".coyote").join("kits");
        let inner = root.join("a").join(".coyote").join("kits");
        let file_not_dir = root.join("a").join("b").join(".coyote").join("kits");
        let start = root.join("a").join("b").join("c");
        fs::create_dir_all(&outer).unwrap();
        fs::create_dir_all(&inner).unwrap();
        fs::create_dir_all(file_not_dir.parent().unwrap()).unwrap();
        fs::write(&file_not_dir, b"not a dir").unwrap();
        fs::create_dir_all(&start).unwrap();

        let found = workspace_kit_roots(&start);

        assert_eq!(found, vec![inner, outer]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_kit_roots_is_empty_when_nothing_is_found() {
        let root = sidecar_temp_dir("workspace-kit-roots-empty");
        assert!(workspace_kit_roots(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_rag_sidecars_removes_duckdb_wal_and_kit_dir() {
        let root = sidecar_temp_dir("rag-sidecars-both");
        let duckdb = root.join("docs.duckdb");
        let wal = root.join("docs.duckdb.wal");
        let kit_dir = rag_kit_dir(&root, "docs");
        fs::write(&duckdb, "db").unwrap();
        fs::write(&wal, "wal").unwrap();
        fs::create_dir_all(&kit_dir).unwrap();
        fs::write(kit_dir.join("rag-docs.yaml"), generated_kit_yaml("docs")).unwrap();

        remove_rag_sidecars(&root, "docs").unwrap();

        assert!(!duckdb.exists(), "the .duckdb sidecar must be removed");
        assert!(!wal.exists(), "the .duckdb.wal sidecar must be removed");
        assert!(!kit_dir.exists(), "the sandbox kit dir must be removed");
        let _ = fs::remove_dir_all(&root);
    }

    fn generated_kit_yaml(name: &str) -> String {
        format!(
            "description: \"{}\"\n",
            crate::rag::rag_kit_description(name)
        )
    }

    #[test]
    fn remove_rag_sidecars_removes_the_kit_of_a_normalising_name() {
        let root = sidecar_temp_dir("rag-sidecars-normalised");
        let kit_dir = rag_kit_dir(&root, "v2-docs");
        fs::create_dir_all(&kit_dir).unwrap();
        fs::write(
            kit_dir.join("rag-v2-docs.yaml"),
            generated_kit_yaml("V2.Docs"),
        )
        .unwrap();

        remove_rag_sidecars(&root, "V2.Docs").unwrap();

        assert!(!kit_dir.exists(), "the sandbox kit dir must be removed");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_rag_sidecars_refuses_a_kit_it_did_not_generate() {
        let root = sidecar_temp_dir("rag-sidecars-foreign");
        let kit_dir = rag_kit_dir(&root, "docs");
        fs::create_dir_all(&kit_dir).unwrap();
        fs::write(kit_dir.join("rag-docs.yaml"), "description: my own kit\n").unwrap();

        let err = remove_rag_sidecars(&root, "docs").unwrap_err();

        let text = err.to_string();
        assert!(
            text.contains("Failed to remove the sandbox kit"),
            "got: {err}"
        );
        assert!(
            text.contains("belongs to another RAG or was written by hand"),
            "got: {err}"
        );
        assert!(kit_dir.join("rag-docs.yaml").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_rag_sidecars_refuses_the_kit_of_a_colliding_name() {
        let root = sidecar_temp_dir("rag-sidecars-colliding");
        let kit_dir = rag_kit_dir(&root, "v2-docs");
        fs::create_dir_all(&kit_dir).unwrap();
        let kit_file = kit_dir.join("rag-v2-docs.yaml");
        fs::write(&kit_file, generated_kit_yaml("V2.Docs")).unwrap();

        let err = remove_rag_sidecars(&root, "v2-docs").unwrap_err();

        let text = err.to_string();
        assert!(
            text.contains(&format!(
                "Failed to remove the sandbox kit for RAG 'v2-docs' at '{}'",
                kit_dir.display()
            )),
            "got: {err}"
        );
        assert!(text.contains("belongs to another RAG"), "got: {err}");
        assert!(kit_file.exists(), "the other RAG's kit must survive");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_rag_sidecars_refusal_leaves_the_duckdb_index_intact() {
        let root = sidecar_temp_dir("rag-sidecars-refusal-duckdb");
        let kit_dir = rag_kit_dir(&root, "v2-docs");
        fs::create_dir_all(&kit_dir).unwrap();
        fs::write(
            kit_dir.join("rag-v2-docs.yaml"),
            generated_kit_yaml("V2.Docs"),
        )
        .unwrap();
        let duckdb = root.join("v2-docs.duckdb");
        let wal = root.join("v2-docs.duckdb.wal");
        fs::write(&duckdb, "db").unwrap();
        fs::write(&wal, "wal").unwrap();

        remove_rag_sidecars(&root, "v2-docs").unwrap_err();

        assert!(duckdb.exists(), "the .duckdb index must survive a refusal");
        assert!(wal.exists(), "the .duckdb.wal must survive a refusal");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_rag_sidecars_reclaims_an_empty_kit_dir() {
        let root = sidecar_temp_dir("rag-sidecars-empty-kit");
        let kit_dir = rag_kit_dir(&root, "docs");
        fs::create_dir_all(&kit_dir).unwrap();

        remove_rag_sidecars(&root, "docs").unwrap();

        assert!(!kit_dir.exists(), "an empty kit dir is reclaimable");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_rag_sidecars_reclaims_a_dir_holding_only_a_stale_tmp_file() {
        let root = sidecar_temp_dir("rag-sidecars-stale-tmp");
        let kit_dir = rag_kit_dir(&root, "docs");
        fs::create_dir_all(&kit_dir).unwrap();
        fs::write(kit_dir.join(".rag-docs.yaml.tmp.1.0"), "partial").unwrap();

        remove_rag_sidecars(&root, "docs").unwrap();

        assert!(
            !kit_dir.exists(),
            "a dir of staging leftovers is reclaimable"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_rag_sidecars_leaves_the_kits_tree_alone_for_an_empty_id() {
        let root = sidecar_temp_dir("rag-sidecars-empty-id");
        let stray = rag_kit_dir(&root, "");
        fs::create_dir_all(&stray).unwrap();

        remove_rag_sidecars(&root, "...").unwrap();

        assert!(stray.exists(), "an empty id must not reach the kits tree");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_rag_sidecars_is_ok_when_absent() {
        let root = sidecar_temp_dir("rag-sidecars-absent");
        assert!(remove_rag_sidecars(&root, "docs").is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_rag_sidecars_runs_before_yaml_unlink() {
        let root = sidecar_temp_dir("rag-sidecars-order");
        let yaml = root.join("docs.yaml");
        fs::write(&yaml, "rag").unwrap();
        // A plain file where the kit dir belongs cannot hold a generated kit,
        // so the ownership check refuses it.
        let kit_dir = rag_kit_dir(&root, "docs");
        fs::create_dir_all(kit_dir.parent().unwrap()).unwrap();
        fs::write(&kit_dir, "not a directory").unwrap();

        let err = remove_rag_sidecars(&root, "docs").unwrap_err();
        assert!(
            err.to_string().contains("Failed to remove the sandbox kit"),
            "got: {err}"
        );
        assert!(
            yaml.exists(),
            "the .yaml must survive a sidecar-removal failure so the delete is retryable"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[serial]
    fn reserved_agent_paths_ignore_env_overrides() {
        let guard = TestConfigDirGuard::new("paths-reserved-env");
        let dir = guard.path.join("builtin-envoy");
        let _source = BuiltinSourceGuard::new(Arc::new(FixedDirSource(dir.clone())));
        let _data_dir = EnvVarGuard::set("ENVOY_DATA_DIR", "/evil");
        let _config_file = EnvVarGuard::set("ENVOY_CONFIG_FILE", "/evil/config.yaml");

        for name in ["envoy", "En-Voy"] {
            assert_eq!(agent_data_dir(name), dir, "{name}");
            assert_eq!(
                agent_config_file(name),
                dir.join(CONFIG_FILE_NAME),
                "{name}"
            );
        }
    }

    #[test]
    #[serial]
    fn reserved_agent_derived_paths_resolve_under_registered_dir() {
        let guard = TestConfigDirGuard::new("paths-reserved-derived");
        let dir = guard.path.join("builtin-envoy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tools.sh"), "").unwrap();
        let _source = BuiltinSourceGuard::new(Arc::new(FixedDirSource(dir.clone())));
        let _data_dir = EnvVarGuard::set("ENVOY_DATA_DIR", "/evil");
        let _config_file = EnvVarGuard::set("ENVOY_CONFIG_FILE", "/evil/config.yaml");

        assert_eq!(agent_functions_file("envoy").unwrap(), dir.join("tools.sh"));
        assert_eq!(agent_bin_dir("envoy"), dir.join(FUNCTIONS_BIN_DIR_NAME));
        assert_eq!(agent_rag_file("envoy", "rag"), dir.join("rag.yaml"));
        assert_eq!(agent_graph_file("envoy"), dir.join(AGENT_GRAPH_FILE_NAME));
    }

    #[test]
    #[serial]
    fn user_agent_paths_honour_env_overrides() {
        let _guard = TestConfigDirGuard::new("paths-user-env");
        let _data_dir = EnvVarGuard::set("MY_AGENT_DATA_DIR", "/custom/dir");
        let _config_file = EnvVarGuard::set("MY_AGENT_CONFIG_FILE", "/custom/cfg.yaml");

        assert_eq!(agent_data_dir("my-agent"), PathBuf::from("/custom/dir"));
        assert_eq!(
            agent_config_file("my-agent"),
            PathBuf::from("/custom/cfg.yaml")
        );
    }

    #[test]
    #[serial]
    fn unregistered_reserved_agent_paths_cannot_exist() {
        let _guard = TestConfigDirGuard::new("paths-reserved-unregistered");
        let _data_dir = EnvVarGuard::set("ENVOY_DATA_DIR", "/evil");

        let dir = agent_data_dir("envoy");

        assert!(!dir.exists());
        assert!(!dir.starts_with(agents_data_dir()));
        assert_ne!(dir, PathBuf::from("/evil"));
        assert!(fs::create_dir_all(&dir).is_err());
        assert!(agent_config_file("envoy").starts_with(&dir));
    }
}
