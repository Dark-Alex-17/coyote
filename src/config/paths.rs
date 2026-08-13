use super::role::Role;
use super::{
    AGENT_GRAPH_FILE_NAME, AGENTS_DIR_NAME, BASH_PROMPT_UTILS_FILE_NAME, CONFIG_FILE_NAME,
    ENV_FILE_NAME, FUNCTIONS_BIN_DIR_NAME, FUNCTIONS_DIR_NAME, GLOBAL_TOOLS_DIR_NAME,
    GLOBAL_TOOLS_UTILS_DIR_NAME, HIDDEN_MCP_FILE_NAME, MACROS_DIR_NAME, MCP_FILE_NAME,
    MEMORY_DIR_NAME, MEMORY_INDEX_FILE_NAME, ModelsOverride, RAGS_DIR_NAME, ROLES_DIR_NAME,
    SBX_KIT_DIR_NAME, SBX_KIT_HASH_FILE, SBX_MIXIN_FILE_NAME, SBX_MIXIN_KITS_DIR_NAME,
    SKILLS_DIR_NAME, WORKSPACE_COYOTE_DIR_NAME,
};
use crate::client::ProviderModels;
use crate::config::REPL_HISTORY_DIR_NAME;
use crate::config::session::Session;
use crate::utils::{get_env_name, list_file_names, normalize_env_name};

use anyhow::{Context, Result, anyhow, bail};
use log::LevelFilter;
use std::collections::HashSet;
use std::env;
use std::fs::{OpenOptions, create_dir_all, read_dir, read_to_string, remove_file, rename, write};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process;
use uuid::Uuid;

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

pub fn sbx_mixin_file() -> PathBuf {
    config_dir().join(SBX_MIXIN_FILE_NAME)
}

pub fn global_tools_sbx_mixin_file() -> PathBuf {
    functions_dir().join(SBX_MIXIN_FILE_NAME)
}

pub fn find_workspace_sbx_mixin(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir
            .join(WORKSPACE_COYOTE_DIR_NAME)
            .join(SBX_MIXIN_FILE_NAME);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
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

pub fn sbx_kit_dir() -> PathBuf {
    cache_dir().join(SBX_KIT_DIR_NAME)
}

pub fn sbx_kit_hash_file() -> PathBuf {
    sbx_kit_dir().join(SBX_KIT_HASH_FILE)
}

pub fn sbx_mixin_kits_dir() -> PathBuf {
    cache_dir().join(SBX_MIXIN_KITS_DIR_NAME)
}

pub fn config_file() -> PathBuf {
    match env::var(get_env_name("config_file")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_dir(CONFIG_FILE_NAME),
    }
}

const INSTALL_ID_FILE_NAME: &str = "install-id";

/// A UUID identifying this installation, minted on first use.
///
/// It lives next to the config rather than inside it, so it survives config
/// edits and is never carried along when a config file is copied elsewhere.
#[allow(dead_code)]
pub fn install_id() -> Result<String> {
    read_or_mint_install_id(&local_dir(INSTALL_ID_FILE_NAME))
}

/// Reads the id stored at `path`, minting one if the file is missing or unusable.
///
/// The returned id is always the one on disk. If another process mints an id
/// first, we adopt it and drop the one we made, so concurrent installs converge
/// on a single identity instead of each keeping its own.
fn read_or_mint_install_id(path: &Path) -> Result<String> {
    if let Some(existing) = read_install_id(path) {
        return Ok(existing);
    }

    if let Some(parent) = path.parent() {
        create_dir_all(parent)
            .with_context(|| format!("Failed to create '{}'", parent.display()))?;
    }

    let mint = Uuid::new_v4().to_string();
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            let err = || format!("Failed to write the install id to '{}'", path.display());
            file.write_all(mint.as_bytes()).with_context(err)?;
            file.flush().with_context(err)?;
        }
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            // Either someone minted an id between our read and this create, or
            // the file holds something we cannot use. Prefer theirs; replace it
            // only when it is unusable.
            if let Some(existing) = read_install_id(path) {
                return Ok(existing);
            }
            replace_install_id(path, &mint)?;
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to create the install id file at '{}'",
                    path.display()
                )
            });
        }
    }

    // Read back rather than returning `mint`: a concurrent writer may have
    // landed a different id, and the caller must see what is on disk.
    read_install_id(path).ok_or_else(|| {
        anyhow!(
            "Failed to read back the install id from '{}'",
            path.display()
        )
    })
}

/// An empty, blank or non-UUID file counts as absent, so it gets re-minted
/// instead of being handed out as an identity.
fn read_install_id(path: &Path) -> Option<String> {
    let raw = read_to_string(path).ok()?;
    let trimmed = raw.trim();
    Uuid::parse_str(trimmed).ok()?;
    Some(trimmed.to_string())
}

/// Overwrites `path` atomically, so a reader never observes a partial id.
fn replace_install_id(path: &Path, id: &str) -> Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", process::id()));
    let temp = PathBuf::from(temp);

    write(&temp, id).with_context(|| format!("Failed to write '{}'", temp.display()))?;
    if let Err(err) = rename(&temp, path) {
        let _ = remove_file(&temp);
        return Err(err).with_context(|| {
            format!(
                "Failed to replace the install id file at '{}'",
                path.display()
            )
        });
    }

    Ok(())
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

pub fn workspace_config_dir() -> PathBuf {
    let workspace_dir_name = match env::var(get_env_name("workspace_config_dir")) {
        Ok(value) => value,
        Err(_) => WORKSPACE_COYOTE_DIR_NAME.to_string(),
    };

    env::current_dir()
        .unwrap_or_default()
        .join(workspace_dir_name)
}

pub fn workspace_skills_dir() -> PathBuf {
    workspace_config_dir().join(SKILLS_DIR_NAME)
}

pub fn workspace_skill_file(name: &str) -> PathBuf {
    workspace_skills_dir().join(name).join("SKILL.md")
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
    match env::var(format!("{}_DATA_DIR", normalize_env_name(name))) {
        Ok(value) => PathBuf::from(value),
        Err(_) => agents_data_dir().join(name),
    }
}

pub fn agent_graph_file(agent_name: &str) -> PathBuf {
    agent_data_dir(agent_name).join(AGENT_GRAPH_FILE_NAME)
}

pub fn agent_config_file(name: &str) -> PathBuf {
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

pub fn workspace_memory_dir_for(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(WORKSPACE_COYOTE_DIR_NAME)
        .join(MEMORY_DIR_NAME)
}

pub fn workspace_memory_index_file_for(workspace_root: &Path) -> PathBuf {
    workspace_memory_dir_for(workspace_root).join(MEMORY_INDEX_FILE_NAME)
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
                    if is_rag_sidecar_name(name) {
                        continue;
                    }

                    names.push(name.to_string());
                }
            }
            names.sort_unstable();
            names
        }
        Err(_) => vec![],
    }
}

pub(crate) fn is_rag_sidecar_name(name: &str) -> bool {
    name.ends_with(".sbx-mixin")
}

pub(crate) fn remove_rag_sidecars(dir: &Path, name: &str) -> Result<()> {
    let duckdb_path = dir.join(format!("{name}.duckdb"));
    if duckdb_path.exists() {
        let _ = remove_file(&duckdb_path);
    }
    let wal_path = dir.join(format!("{name}.duckdb.wal"));
    if wal_path.exists() {
        let _ = remove_file(&wal_path);
    }
    let mixin_path = dir.join(format!("{name}.sbx-mixin.yaml"));
    if mixin_path.exists() {
        remove_file(&mixin_path).with_context(|| {
            format!(
                "Failed to remove the sandbox mixin for RAG '{name}' at '{}'. \
                 The RAG was NOT deleted so you can retry; this host remains \
                 whitelisted in the sandbox until the file is removed.",
                mixin_path.display()
            )
        })?;
    }

    Ok(())
}

pub fn list_macros() -> Vec<String> {
    list_file_names(macros_dir(), ".yaml")
}

pub fn has_macro(name: &str) -> bool {
    let names = list_macros();
    names.contains(&name.to_string())
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
    use super::*;
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
    fn is_rag_sidecar_name_accepts_dotted_rag_names() {
        assert!(!is_rag_sidecar_name("v2.docs"));
        assert!(!is_rag_sidecar_name("myrag"));
        assert!(is_rag_sidecar_name("myrag.sbx-mixin"));
        assert!(is_rag_sidecar_name("v2.docs.sbx-mixin"));
    }

    #[test]
    fn remove_rag_sidecars_removes_duckdb_wal_and_mixin() {
        let root = sidecar_temp_dir("rag-sidecars-both");
        let duckdb = root.join("docs.duckdb");
        let wal = root.join("docs.duckdb.wal");
        let mixin = root.join("docs.sbx-mixin.yaml");
        fs::write(&duckdb, "db").unwrap();
        fs::write(&wal, "wal").unwrap();
        fs::write(&mixin, "mixin").unwrap();

        remove_rag_sidecars(&root, "docs").unwrap();

        assert!(!duckdb.exists(), "the .duckdb sidecar must be removed");
        assert!(!wal.exists(), "the .duckdb.wal sidecar must be removed");
        assert!(
            !mixin.exists(),
            "the .sbx-mixin.yaml sidecar must be removed"
        );
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
        let mixin = root.join("docs.sbx-mixin.yaml");
        fs::create_dir_all(&mixin).unwrap();
        fs::write(mixin.join("blocker"), "x").unwrap();

        let err = remove_rag_sidecars(&root, "docs").unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to remove the sandbox mixin"),
            "got: {err}"
        );
        assert!(
            yaml.exists(),
            "the .yaml must survive a sidecar-removal failure so the delete is retryable"
        );
        let _ = fs::remove_dir_all(&root);
    }

    mod install_id_file {
        use super::*;
        use serial_test::serial;

        fn assert_is_uuid_v4(id: &str) {
            let parsed =
                Uuid::parse_str(id).unwrap_or_else(|e| panic!("'{id}' is not a UUID: {e}"));
            assert_eq!(parsed.get_version(), Some(uuid::Version::Random));
        }

        #[test]
        #[serial]
        fn mints_once_then_reuses_the_file() {
            let root = sidecar_temp_dir("install-id-mint");
            let env_name = get_env_name("config_dir");
            let prev = env::var_os(&env_name);
            unsafe {
                env::set_var(&env_name, &root);
            }

            let first = install_id().unwrap();
            assert_is_uuid_v4(&first);
            let path = root.join(INSTALL_ID_FILE_NAME);
            assert!(path.is_file(), "the id must be minted into the config dir");
            assert_eq!(fs::read_to_string(&path).unwrap().trim(), first);

            let second = install_id().unwrap();
            assert_eq!(second, first, "a second call must not mint a new id");
            assert_eq!(fs::read_to_string(&path).unwrap().trim(), first);

            unsafe {
                match prev {
                    Some(v) => env::set_var(&env_name, v),
                    None => env::remove_var(&env_name),
                }
            }
            let _ = fs::remove_dir_all(&root);
        }

        #[test]
        fn adopts_a_pre_existing_id_and_leaves_the_file_alone() {
            let root = sidecar_temp_dir("install-id-existing");
            let path = root.join(INSTALL_ID_FILE_NAME);
            let existing = Uuid::new_v4().to_string();
            let stored = format!("  {existing}\n");
            fs::write(&path, &stored).unwrap();

            assert_eq!(read_or_mint_install_id(&path).unwrap(), existing);
            assert_eq!(fs::read_to_string(&path).unwrap(), stored);

            let _ = fs::remove_dir_all(&root);
        }

        #[test]
        fn returns_the_id_on_disk_instead_of_a_fresh_mint() {
            let root = sidecar_temp_dir("install-id-race");
            let path = root.join(INSTALL_ID_FILE_NAME);
            let winner = Uuid::new_v4().to_string();
            fs::write(&path, &winner).unwrap();

            assert_eq!(read_or_mint_install_id(&path).unwrap(), winner);
            assert_eq!(fs::read_to_string(&path).unwrap(), winner);

            let _ = fs::remove_dir_all(&root);
        }

        #[test]
        fn re_mints_a_file_that_does_not_hold_a_uuid() {
            let root = sidecar_temp_dir("install-id-garbage");

            for (i, garbage) in ["", "   \n", "not-a-uuid"].iter().enumerate() {
                let path = root.join(format!("{INSTALL_ID_FILE_NAME}-{i}"));
                fs::write(&path, garbage).unwrap();

                let minted = read_or_mint_install_id(&path).unwrap();
                assert_is_uuid_v4(&minted);
                assert_ne!(minted, *garbage);
                assert_eq!(
                    fs::read_to_string(&path).unwrap().trim(),
                    minted,
                    "the re-minted id must be the one left on disk"
                );
            }

            let _ = fs::remove_dir_all(&root);
        }

        #[test]
        fn creates_the_missing_parent_directory() {
            let root = sidecar_temp_dir("install-id-nested");
            let path = root
                .join("nested")
                .join("deeper")
                .join(INSTALL_ID_FILE_NAME);

            let minted = read_or_mint_install_id(&path).unwrap();
            assert_is_uuid_v4(&minted);
            assert_eq!(fs::read_to_string(&path).unwrap().trim(), minted);

            let _ = fs::remove_dir_all(&root);
        }
    }
}
