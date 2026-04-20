mod agent;
mod app_config;
mod app_state;
mod input;
mod macros;
mod mcp_factory;
pub(crate) mod paths;
mod prompts;
mod rag_cache;
mod request_context;
mod role;
mod session;
pub(crate) mod todo;
mod tool_scope;

pub use self::agent::{Agent, AgentVariables, complete_agent_variables, list_agents};
#[allow(unused_imports)]
pub use self::app_config::AppConfig;
#[allow(unused_imports)]
pub use self::app_state::AppState;
pub use self::input::Input;
#[allow(unused_imports)]
pub use self::request_context::RequestContext;
pub use self::role::{
    CODE_ROLE, CREATE_TITLE_ROLE, EXPLAIN_SHELL_ROLE, Role, RoleLike, SHELL_ROLE,
};
use self::session::Session;
use crate::client::{
    ClientConfig, MessageContentToolCalls, Model, ModelType, OPENAI_COMPATIBLE_PROVIDERS,
    ProviderModels, create_client_config, list_client_types,
};
use crate::function::{FunctionDeclaration, Functions};
use crate::rag::Rag;
use crate::utils::*;
pub use macros::macro_execute;

use crate::config::macros::Macro;
use crate::vault::{GlobalVault, Vault, create_vault_password_file, interpolate_secrets};
use anyhow::{Context, Result, anyhow, bail};
use fancy_regex::Regex;
use indexmap::IndexMap;
use indoc::formatdoc;
use inquire::{Confirm, Select};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::{
    env,
    fs::{File, create_dir_all, read_dir, read_to_string},
    io::Write,
    path::{Path, PathBuf},
    process,
    sync::{Arc, OnceLock},
};

pub const TEMP_ROLE_NAME: &str = "temp";
pub const TEMP_RAG_NAME: &str = "temp";
pub const TEMP_SESSION_NAME: &str = "temp";

static PASSWORD_FILE_SECRET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"vault_password_file:.*['|"]?\{\{(.+)}}['|"]?"#).unwrap());

/// Monokai Extended
const DARK_THEME: &[u8] = include_bytes!("../../assets/monokai-extended.theme.bin");
const LIGHT_THEME: &[u8] = include_bytes!("../../assets/monokai-extended-light.theme.bin");

const CONFIG_FILE_NAME: &str = "config.yaml";
const ROLES_DIR_NAME: &str = "roles";
const MACROS_DIR_NAME: &str = "macros";
const ENV_FILE_NAME: &str = ".env";
const MESSAGES_FILE_NAME: &str = "messages.md";
const SESSIONS_DIR_NAME: &str = "sessions";
const RAGS_DIR_NAME: &str = "rags";
const FUNCTIONS_DIR_NAME: &str = "functions";
const FUNCTIONS_BIN_DIR_NAME: &str = "bin";
const AGENTS_DIR_NAME: &str = "agents";
const GLOBAL_TOOLS_DIR_NAME: &str = "tools";
const GLOBAL_TOOLS_UTILS_DIR_NAME: &str = "utils";
const BASH_PROMPT_UTILS_FILE_NAME: &str = "prompt-utils.sh";
const MCP_FILE_NAME: &str = "mcp.json";

const CLIENTS_FIELD: &str = "clients";

const SYNC_MODELS_URL: &str =
    "https://raw.githubusercontent.com/Dark-Alex-17/loki/refs/heads/main/models.yaml";

const SUMMARIZATION_PROMPT: &str =
    "Summarize the discussion briefly in 200 words or less to use as a prompt for future context.";
const SUMMARY_CONTEXT_PROMPT: &str = "This is a summary of the chat history as a recap: ";

const RAG_TEMPLATE: &str = r#"Answer the query based on the context while respecting the rules. (user query, some textual context and rules, all inside xml tags)

<context>
__CONTEXT__
</context>

<sources>
__SOURCES__
</sources>

<rules>
- If you don't know, just say so.
- If you are not sure, ask for clarification.
- Answer in the same language as the user query.
- If the context appears unreadable or of poor quality, tell the user then answer as best as you can.
- If the answer is not in the context but you think you know the answer, explain that to the user then answer with your own knowledge.
- Answer directly and without using xml tags.
- When using information from the context, cite the relevant source from the <sources> section.
</rules>

<user_query>
__INPUT__
</user_query>"#;

const LEFT_PROMPT: &str = "{color.red}{model}){color.green}{?session {?agent {agent}>}{session}{?role /}}{!session {?agent {agent}>}}{role}{?rag @{rag}}{color.cyan}{?session )}{!session >}{color.reset} ";
const RIGHT_PROMPT: &str = "{color.purple}{?session {?consume_tokens {consume_tokens}({consume_percent}%)}{!consume_tokens {consume_tokens}}}{color.reset}";

static EDITOR: OnceLock<Option<String>> = OnceLock::new();

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename(serialize = "model", deserialize = "model"))]
    #[serde(default)]
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,

    pub dry_run: bool,
    pub stream: bool,
    pub save: bool,
    pub keybindings: String,
    pub editor: Option<String>,
    pub wrap: Option<String>,
    pub wrap_code: bool,
    pub(super) vault_password_file: Option<PathBuf>,

    pub function_calling_support: bool,
    pub mapping_tools: IndexMap<String, String>,
    pub enabled_tools: Option<String>,
    pub visible_tools: Option<Vec<String>>,

    pub mcp_server_support: bool,
    pub mapping_mcp_servers: IndexMap<String, String>,
    pub enabled_mcp_servers: Option<String>,

    pub repl_prelude: Option<String>,
    pub cmd_prelude: Option<String>,
    pub agent_session: Option<String>,

    pub save_session: Option<bool>,
    pub compression_threshold: usize,
    pub summarization_prompt: Option<String>,
    pub summary_context_prompt: Option<String>,

    pub rag_embedding_model: Option<String>,
    pub rag_reranker_model: Option<String>,
    pub rag_top_k: usize,
    pub rag_chunk_size: Option<usize>,
    pub rag_chunk_overlap: Option<usize>,
    pub rag_template: Option<String>,

    #[serde(default)]
    pub document_loaders: HashMap<String, String>,

    pub highlight: bool,
    pub theme: Option<String>,
    pub left_prompt: Option<String>,
    pub right_prompt: Option<String>,

    pub user_agent: Option<String>,
    pub save_shell_history: bool,
    pub sync_models_url: Option<String>,

    pub clients: Vec<ClientConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_id: Default::default(),
            temperature: None,
            top_p: None,

            dry_run: false,
            stream: true,
            save: false,
            keybindings: "emacs".into(),
            editor: None,
            wrap: None,
            wrap_code: false,
            vault_password_file: None,

            function_calling_support: true,
            mapping_tools: Default::default(),
            enabled_tools: None,
            visible_tools: None,

            mcp_server_support: true,
            mapping_mcp_servers: Default::default(),
            enabled_mcp_servers: None,

            repl_prelude: None,
            cmd_prelude: None,
            agent_session: None,

            save_session: None,
            compression_threshold: 4000,
            summarization_prompt: None,
            summary_context_prompt: None,

            rag_embedding_model: None,
            rag_reranker_model: None,
            rag_top_k: 5,
            rag_chunk_size: None,
            rag_chunk_overlap: None,
            rag_template: None,

            document_loaders: Default::default(),

            highlight: true,
            theme: None,
            left_prompt: None,
            right_prompt: None,

            user_agent: None,
            save_shell_history: true,
            sync_models_url: None,

            clients: vec![],
        }
    }
}

pub fn install_builtins() -> Result<()> {
    Functions::install_builtin_global_tools()?;
    Agent::install_builtin_agents()?;
    Macro::install_macros()?;
    Ok(())
}

pub fn default_sessions_dir() -> PathBuf {
    match env::var(get_env_name("sessions_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => paths::local_path(SESSIONS_DIR_NAME),
    }
}

pub fn list_sessions() -> Vec<String> {
    list_file_names(default_sessions_dir(), ".yaml")
}

impl Config {
    pub async fn search_rag(
        app: &AppConfig,
        rag: &Rag,
        text: &str,
        abort_signal: AbortSignal,
    ) -> Result<String> {
        let (reranker_model, top_k) = rag.get_config();
        let (embeddings, sources, ids) = rag
            .search(text, top_k, reranker_model.as_deref(), abort_signal)
            .await?;
        let rag_template = app.rag_template.as_deref().unwrap_or(RAG_TEMPLATE);
        let text = if embeddings.is_empty() {
            text.to_string()
        } else {
            rag_template
                .replace("__CONTEXT__", &embeddings)
                .replace("__SOURCES__", &sources)
                .replace("__INPUT__", text)
        };
        rag.set_last_sources(&ids);
        Ok(text)
    }

    pub fn load_macro(name: &str) -> Result<Macro> {
        let path = paths::macro_file(name);
        let err = || format!("Failed to load macro '{name}' at '{}'", path.display());
        let content = read_to_string(&path).with_context(err)?;
        let value: Macro = serde_yaml::from_str(&content).with_context(err)?;
        Ok(value)
    }

    pub async fn sync_models(url: &str, abort_signal: AbortSignal) -> Result<()> {
        let content = abortable_run_with_spinner(fetch(url), "Fetching models.yaml", abort_signal)
            .await
            .with_context(|| format!("Failed to fetch '{url}'"))?;
        println!("✓ Fetched '{url}'");
        let list = serde_yaml::from_str::<Vec<ProviderModels>>(&content)
            .with_context(|| "Failed to parse models.yaml")?;
        let models_override = ModelsOverride {
            version: env!("CARGO_PKG_VERSION").to_string(),
            list,
        };
        let models_override_data =
            serde_yaml::to_string(&models_override).with_context(|| "Failed to serde {}")?;

        let model_override_path = paths::models_override_file();
        ensure_parent_exists(&model_override_path)?;
        std::fs::write(&model_override_path, models_override_data)
            .with_context(|| format!("Failed to write to '{}'", model_override_path.display()))?;
        println!("✓ Updated '{}'", model_override_path.display());
        Ok(())
    }

    pub async fn load_with_interpolation(info_flag: bool) -> Result<Self> {
        let config_path = paths::config_file();
        let (mut config, content) = if !config_path.exists() {
            match env::var(get_env_name("provider"))
                .ok()
                .or_else(|| env::var(get_env_name("platform")).ok())
            {
                Some(v) => (Self::load_dynamic(&v)?, String::new()),
                None => {
                    if *IS_STDOUT_TERMINAL {
                        create_config_file(&config_path).await?;
                    }
                    Self::load_from_file(&config_path)?
                }
            }
        } else {
            Self::load_from_file(&config_path)?
        };

        let bootstrap_app = AppConfig {
            vault_password_file: config.vault_password_file.clone(),
            ..AppConfig::default()
        };
        let vault = Vault::init(&bootstrap_app);
        let (parsed_config, missing_secrets) = interpolate_secrets(&content, &vault);
        if !missing_secrets.is_empty() && !info_flag {
            debug!(
                "Global config references secrets that are missing from the vault: {missing_secrets:?}"
            );
            return Err(anyhow!(formatdoc!(
                "
								Global config file references secrets that are missing from the vault: {:?}
								Please add these secrets to the vault and try again.",
                missing_secrets
            )));
        }
        if !parsed_config.is_empty() && !info_flag {
            debug!("Global config is invalid once secrets are injected: {parsed_config}");
            let new_config = Self::load_from_str(&parsed_config).with_context(|| {
                formatdoc!(
                    "
										Global config is invalid once secrets are injected.
										Double check the secret values and file syntax, then try again.
										"
                )
            })?;
            config = new_config;
        }
        Ok(config)
    }

    pub fn load_from_file(config_path: &Path) -> Result<(Self, String)> {
        let err = || format!("Failed to load config at '{}'", config_path.display());
        let content = read_to_string(config_path).with_context(err)?;
        let config = Self::load_from_str(&content).with_context(err)?;

        Ok((config, content))
    }

    pub fn load_from_str(content: &str) -> Result<Self> {
        if PASSWORD_FILE_SECRET_RE.is_match(content)? {
            bail!("secret injection cannot be done on the vault_password_file property");
        }

        let config: Self = serde_yaml::from_str(content)
            .map_err(|err| {
                let err_msg = err.to_string();
                let err_msg = if err_msg.starts_with(&format!("{CLIENTS_FIELD}: ")) {
                    // location is incorrect, get rid of it
                    err_msg
                        .split_once(" at line")
                        .map(|(v, _)| {
                            format!("{v} (Sorry for being unable to provide an exact location)")
                        })
                        .unwrap_or_else(|| "clients: invalid value".into())
                } else {
                    err_msg
                };
                anyhow!("{err_msg}")
            })
            .with_context(|| "Failed to load config from str")?;

        Ok(config)
    }

    pub fn load_dynamic(model_id: &str) -> Result<Self> {
        let provider = match model_id.split_once(':') {
            Some((v, _)) => v,
            _ => model_id,
        };
        let is_openai_compatible = OPENAI_COMPATIBLE_PROVIDERS
            .into_iter()
            .any(|(name, _)| provider == name);
        let client = if is_openai_compatible {
            json!({ "type": "openai-compatible", "name": provider })
        } else {
            json!({ "type": provider })
        };
        let config = json!({
            "model": model_id.to_string(),
            "save": false,
            "clients": vec![client],
        });
        let config =
            serde_json::from_value(config).with_context(|| "Failed to load config from env")?;
        Ok(config)
    }
}

pub fn load_env_file() -> Result<()> {
    let env_file_path = paths::env_file();
    let contents = match read_to_string(&env_file_path) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    debug!("Use env file '{}'", env_file_path.display());
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            unsafe { env::set_var(key.trim(), value.trim()) };
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkingMode {
    Cmd,
    Repl,
}

impl WorkingMode {
    pub fn is_cmd(&self) -> bool {
        *self == WorkingMode::Cmd
    }
    pub fn is_repl(&self) -> bool {
        *self == WorkingMode::Repl
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsOverride {
    pub version: String,
    pub list: Vec<ProviderModels>,
}

#[derive(Debug, Clone)]
pub struct LastMessage {
    pub input: Input,
    pub output: String,
    pub continuous: bool,
}

impl LastMessage {
    pub fn new(input: Input, output: String) -> Self {
        Self {
            input,
            output,
            continuous: true,
        }
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct StateFlags: u32 {
        const ROLE = 1 << 0;
        const SESSION_EMPTY = 1 << 1;
        const SESSION = 1 << 2;
        const RAG = 1 << 3;
        const AGENT = 1 << 4;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssertState {
    True(StateFlags),
    False(StateFlags),
    TrueFalse(StateFlags, StateFlags),
    Equal(StateFlags),
}

impl AssertState {
    pub fn pass() -> Self {
        AssertState::False(StateFlags::empty())
    }

    pub fn bare() -> Self {
        AssertState::Equal(StateFlags::empty())
    }

    pub fn assert(self, flags: StateFlags) -> bool {
        match self {
            AssertState::True(true_flags) => true_flags & flags != StateFlags::empty(),
            AssertState::False(false_flags) => false_flags & flags == StateFlags::empty(),
            AssertState::TrueFalse(true_flags, false_flags) => {
                (true_flags & flags != StateFlags::empty())
                    && (false_flags & flags == StateFlags::empty())
            }
            AssertState::Equal(check_flags) => check_flags == flags,
        }
    }
}

pub async fn create_config_file(config_path: &Path) -> Result<()> {
    let ans = Confirm::new("No config file, create a new one?")
        .with_default(true)
        .prompt()?;
    if !ans {
        process::exit(0);
    }

    let mut vault = Vault::init_bare();
    create_vault_password_file(&mut vault)?;

    let client = Select::new("API Provider (required):", list_client_types()).prompt()?;

    let mut config = json!({});
    let (model, clients_config) = create_client_config(client, &vault).await?;
    config["model"] = model.into();
    config["vault_password_file"] = vault.password_file()?.display().to_string().into();
    config[CLIENTS_FIELD] = clients_config;

    let config_data = serde_yaml::to_string(&config).with_context(|| "Failed to create config")?;
    let config_data = format!(
        "# see https://github.com/Dark-Alex-17/loki/blob/main/config.example.yaml\n\n{config_data}"
    );

    ensure_parent_exists(config_path)?;
    std::fs::write(config_path, config_data)
        .with_context(|| format!("Failed to write to '{}'", config_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::prelude::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(config_path, perms)?;
    }

    println!("✓ Saved the config file to '{}'.\n", config_path.display());

    Ok(())
}

pub(crate) fn ensure_parent_exists(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Failed to write to '{}', No parent path", path.display()))?;
    if !parent.exists() {
        create_dir_all(parent).with_context(|| {
            format!(
                "Failed to write to '{}', Cannot create parent directory",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn read_env_value<T>(key: &str) -> Option<Option<T>>
where
    T: std::str::FromStr,
{
    let value = env::var(key).ok()?;
    let value = parse_value(&value).ok()?;
    Some(value)
}

pub(super) fn parse_value<T>(value: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
{
    let value = if value == "null" {
        None
    } else {
        let value = match value.parse() {
            Ok(value) => value,
            Err(_) => bail!("Invalid value '{}'", value),
        };
        Some(value)
    };
    Ok(value)
}

fn read_env_bool(key: &str) -> Option<Option<bool>> {
    let value = env::var(key).ok()?;
    Some(parse_bool(&value))
}

pub(super) fn complete_bool(value: bool) -> Vec<String> {
    vec![(!value).to_string()]
}

pub(super) fn complete_option_bool(value: Option<bool>) -> Vec<String> {
    match value {
        Some(true) => vec!["false".to_string(), "null".to_string()],
        Some(false) => vec!["true".to_string(), "null".to_string()],
        None => vec!["true".to_string(), "false".to_string()],
    }
}

pub(super) fn map_completion_values<T: ToString>(value: Vec<T>) -> Vec<(String, Option<String>)> {
    value.into_iter().map(|v| (v.to_string(), None)).collect()
}

pub(super) fn format_option_value<T>(value: &Option<T>) -> String
where
    T: std::fmt::Display,
{
    match value {
        Some(value) => value.to_string(),
        None => "null".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_expected() {
        let cfg = Config::default();

        assert_eq!(cfg.model_id, "");
        assert_eq!(cfg.temperature, None);
        assert_eq!(cfg.top_p, None);
        assert!(!cfg.dry_run);
        assert!(cfg.stream);
        assert!(!cfg.save);
        assert!(cfg.highlight);
        assert!(cfg.function_calling_support);
        assert!(cfg.mcp_server_support);
        assert_eq!(cfg.compression_threshold, 4000);
        assert_eq!(cfg.rag_top_k, 5);
        assert!(cfg.save_shell_history);
        assert_eq!(cfg.keybindings, "emacs");
        assert!(cfg.clients.is_empty());
        assert!(cfg.save_session.is_none());
        assert!(cfg.enabled_tools.is_none());
        assert!(cfg.enabled_mcp_servers.is_none());
    }
}
