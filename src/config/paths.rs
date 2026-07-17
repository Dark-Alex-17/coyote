use super::role::Role;
use super::{
    AGENT_GRAPH_FILE_NAME, AGENTS_DIR_NAME, BASH_PROMPT_UTILS_FILE_NAME, CONFIG_FILE_NAME,
    ENV_FILE_NAME, FUNCTIONS_BIN_DIR_NAME, FUNCTIONS_DIR_NAME, GLOBAL_TOOLS_DIR_NAME,
    GLOBAL_TOOLS_UTILS_DIR_NAME, HIDDEN_MCP_FILE_NAME, MACROS_DIR_NAME, MCP_FILE_NAME,
    MEMORY_DIR_NAME, MEMORY_INDEX_FILE_NAME, ModelsOverride, RAGS_DIR_NAME, ROLES_DIR_NAME,
    SBX_KIT_DIR_NAME, SBX_KIT_HASH_FILE, SBX_MIXIN_FILE_NAME, SBX_MIXIN_KITS_DIR_NAME,
    SBX_VAULT_MIXINS_DIR_NAME, SKILLS_DIR_NAME, WORKSPACE_COYOTE_DIR_NAME,
};
use crate::client::ProviderModels;
use crate::config::REPL_HISTORY_DIR_NAME;
use crate::config::session::Session;
use crate::utils::{get_env_name, list_file_names, normalize_env_name};

use anyhow::{Context, Result, anyhow, bail};
use log::LevelFilter;
use std::collections::HashSet;
use std::env;
use std::fs::{read_dir, read_to_string};
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

pub fn local_path(name: &str) -> PathBuf {
    config_dir().join(name)
}

pub fn cache_path() -> PathBuf {
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

pub fn translate_sandboxed_home_path(path: &Path) -> Option<PathBuf> {
    env::var_os("IS_SANDBOX")?;

    let s = path.to_str()?;

    if let Some(translated) = translate_unix_home_style(s, "/home/") {
        return Some(translated);
    }

    if let Some(translated) = translate_unix_home_style(s, "/Users/") {
        return Some(translated);
    }

    translate_windows_users_path(s)
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

fn translate_windows_users_path(s: &str) -> Option<PathBuf> {
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

pub fn oauth_tokens_path() -> PathBuf {
    cache_path().join("oauth")
}

pub fn token_file(client_name: &str) -> PathBuf {
    oauth_tokens_path().join(format!("{client_name}_oauth_tokens.json"))
}

pub fn log_path() -> PathBuf {
    cache_path().join(format!("{}.log", env!("CARGO_CRATE_NAME")))
}

pub fn sbx_kit_dir() -> PathBuf {
    cache_path().join(SBX_KIT_DIR_NAME)
}

pub fn sbx_kit_hash_file() -> PathBuf {
    sbx_kit_dir().join(SBX_KIT_HASH_FILE)
}

pub fn sbx_vault_mixins_dir() -> PathBuf {
    cache_path().join(SBX_VAULT_MIXINS_DIR_NAME)
}

pub fn sbx_vault_mixins_hash_file() -> PathBuf {
    sbx_vault_mixins_dir().join(SBX_KIT_HASH_FILE)
}

pub fn sbx_mixin_kits_dir() -> PathBuf {
    cache_path().join(SBX_MIXIN_KITS_DIR_NAME)
}

pub fn config_file() -> PathBuf {
    match env::var(get_env_name("config_file")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_path(CONFIG_FILE_NAME),
    }
}

pub fn roles_dir() -> PathBuf {
    match env::var(get_env_name("roles_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_path(ROLES_DIR_NAME),
    }
}

pub fn role_file(name: &str) -> PathBuf {
    roles_dir().join(format!("{name}.md"))
}

pub fn skills_dir() -> PathBuf {
    match env::var(get_env_name("skills_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_path(SKILLS_DIR_NAME),
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
        Err(_) => local_path(MACROS_DIR_NAME),
    }
}

pub fn macro_file(name: &str) -> PathBuf {
    macros_dir().join(format!("{name}.yaml"))
}

pub fn env_file() -> PathBuf {
    match env::var(get_env_name("env_file")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_path(ENV_FILE_NAME),
    }
}

pub fn rags_dir() -> PathBuf {
    match env::var(get_env_name("rags_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_path(RAGS_DIR_NAME),
    }
}

pub fn functions_dir() -> PathBuf {
    match env::var(get_env_name("functions_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_path(FUNCTIONS_DIR_NAME),
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
    local_path(AGENTS_DIR_NAME)
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
    local_path("models-override.yaml")
}

pub fn global_memory_dir() -> PathBuf {
    config_dir().join(MEMORY_DIR_NAME)
}

pub fn global_memory_index_path() -> PathBuf {
    global_memory_dir().join(MEMORY_INDEX_FILE_NAME)
}

pub fn workspace_memory_dir_for(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(WORKSPACE_COYOTE_DIR_NAME)
        .join(MEMORY_DIR_NAME)
}

pub fn repl_history_dir() -> PathBuf {
    cache_path().join(REPL_HISTORY_DIR_NAME)
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
        Err(_) => Some(log_path()),
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
                assert_eq!(translate_sandboxed_home_path(p), None);
            });
        }

        #[test]
        #[serial]
        fn translates_host_home_to_agent_home() {
            with_sandbox(|| {
                let p = Path::new("/home/atusa/.coyote_password");
                assert_eq!(
                    translate_sandboxed_home_path(p),
                    Some(PathBuf::from("/home/agent/.coyote_password"))
                );
            });
        }

        #[test]
        #[serial]
        fn translates_nested_host_home_path() {
            with_sandbox(|| {
                let p = Path::new("/home/atusa/.config/coyote/.password");
                assert_eq!(
                    translate_sandboxed_home_path(p),
                    Some(PathBuf::from("/home/agent/.config/coyote/.password"))
                );
            });
        }

        #[test]
        #[serial]
        fn returns_none_when_path_already_targets_agent_home() {
            with_sandbox(|| {
                let p = Path::new("/home/agent/.coyote_password");
                assert_eq!(translate_sandboxed_home_path(p), None);
            });
        }

        #[test]
        #[serial]
        fn returns_none_when_path_is_outside_home() {
            with_sandbox(|| {
                let p = Path::new("/etc/coyote/.coyote_password");
                assert_eq!(translate_sandboxed_home_path(p), None);
            });
        }

        #[test]
        #[serial]
        fn returns_none_for_relative_path() {
            with_sandbox(|| {
                let p = Path::new(".coyote_password");
                assert_eq!(translate_sandboxed_home_path(p), None);
            });
        }

        #[test]
        #[serial]
        fn returns_none_for_first_segment_not_home() {
            with_sandbox(|| {
                let p = Path::new("/opt/atusa/.coyote_password");
                assert_eq!(translate_sandboxed_home_path(p), None);
            });
        }

        #[test]
        #[serial]
        fn translates_macos_users_path() {
            with_sandbox(|| {
                let p = Path::new("/Users/atusa/.coyote_password");
                assert_eq!(
                    translate_sandboxed_home_path(p),
                    Some(PathBuf::from("/home/agent/.coyote_password"))
                );
            });
        }

        #[test]
        #[serial]
        fn translates_macos_nested_path() {
            with_sandbox(|| {
                let p = Path::new("/Users/atusa/.config/coyote/.password");
                assert_eq!(
                    translate_sandboxed_home_path(p),
                    Some(PathBuf::from("/home/agent/.config/coyote/.password"))
                );
            });
        }

        #[test]
        #[serial]
        fn returns_none_when_macos_path_already_targets_agent() {
            with_sandbox(|| {
                let p = Path::new("/Users/agent/.coyote_password");
                assert_eq!(translate_sandboxed_home_path(p), None);
            });
        }

        #[test]
        #[serial]
        fn translates_windows_drive_letter_path() {
            with_sandbox(|| {
                let p = Path::new("C:\\Users\\atusa\\.coyote_password");
                assert_eq!(
                    translate_sandboxed_home_path(p),
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
                    translate_sandboxed_home_path(p),
                    Some(PathBuf::from("/home/agent/.config/coyote/.password"))
                );
            });
        }

        #[test]
        #[serial]
        fn returns_none_when_windows_path_already_targets_agent() {
            with_sandbox(|| {
                let p = Path::new("C:\\Users\\agent\\.coyote_password");
                assert_eq!(translate_sandboxed_home_path(p), None);
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
}
