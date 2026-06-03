use super::role::Role;
use super::{
    AGENT_GRAPH_FILE_NAME, AGENTS_DIR_NAME, BASH_PROMPT_UTILS_FILE_NAME, CONFIG_FILE_NAME,
    ENV_FILE_NAME, FUNCTIONS_BIN_DIR_NAME, FUNCTIONS_DIR_NAME, GLOBAL_TOOLS_DIR_NAME,
    GLOBAL_TOOLS_UTILS_DIR_NAME, MACROS_DIR_NAME, MCP_FILE_NAME, ModelsOverride, RAGS_DIR_NAME,
    ROLES_DIR_NAME, SKILLS_DIR_NAME,
};
use crate::client::ProviderModels;
use crate::utils::{get_env_name, list_file_names, normalize_env_name};

use anyhow::{Context, Result, anyhow, bail};
use log::LevelFilter;
use std::collections::HashSet;
use std::env;
use std::fs::{read_dir, read_to_string};
use std::path::PathBuf;

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
    let base_dir = dirs::cache_dir().unwrap_or_else(env::temp_dir);
    base_dir.join(env!("CARGO_CRATE_NAME"))
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
    if let Ok(rd) = read_dir(skills_dir()) {
        for entry in rd.flatten() {
            if let Ok(file_type) = entry.file_type()
                && file_type.is_dir()
                && let Some(name) = entry.file_name().to_str()
                && entry.path().join("SKILL.md").is_file()
            {
                names.push(name.to_string());
            }
        }
    }

    names.sort_unstable();
    names
}

pub fn has_skill(name: &str) -> bool {
    if validate_skill_name(name).is_err() {
        return false;
    }

    skill_file(name).is_file()
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
    fn has_skill_returns_false_for_invalid_names() {
        for bad in ["", "../escape", "foo/bar", ".hidden", "with space"] {
            assert!(
                !has_skill(bad),
                "has_skill({bad:?}) should be false for an invalid name"
            );
        }
    }
}
