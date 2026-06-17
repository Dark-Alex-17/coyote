mod agent;
mod app_config;
mod app_state;
mod input;
mod install_remote;
mod macros;
mod mcp_factory;
pub(crate) mod memory;
pub(crate) mod paths;
pub(crate) mod prompts;
mod rag_cache;
mod request_context;
mod role;
mod session;
mod skill;
mod skill_policy;
mod skill_registry;
pub(crate) mod todo;
mod tool_scope;
mod update;

pub use self::agent::{
    Agent, AgentVariable, AgentVariables, complete_agent_variables, list_agents,
};
#[allow(unused_imports)]
pub use self::app_config::AppConfig;
#[allow(unused_imports)]
pub use self::app_state::AppState;
pub use self::input::Input;
pub use self::install_remote::{install_remote, install_remote_from_repl_args};
#[allow(unused_imports)]
pub use self::request_context::{RenderMode, RequestContext, should_inject_skill_instructions};
pub use self::role::{
    CODE_ROLE, CREATE_TITLE_ROLE, EXPLAIN_SHELL_ROLE, Role, RoleLike, SHELL_ROLE,
};
use self::session::Session;
#[allow(unused_imports)]
pub use self::skill::Skill;
#[allow(unused_imports)]
pub use self::skill_policy::SkillPolicy;
#[allow(unused_imports)]
pub use self::skill_registry::SkillRegistry;
pub use self::update::run_self_update;
use crate::client::{
    ClientConfig, MessageContentToolCalls, Model, ModelType, OPENAI_COMPATIBLE_PROVIDERS,
    ProviderModels, create_client_config, list_client_types,
};
use crate::function::{FunctionDeclaration, Functions};
use crate::rag::Rag;
use crate::utils::*;
pub use macros::macro_execute;

use crate::config::macros::Macro;
use crate::vault::{
    GlobalVault, Vault, create_vault_password_file, interpolate_secrets, prompt_provider_choice,
};
use anyhow::{Context, Result, anyhow, bail};
use fancy_regex::Regex;
use gman::providers::SupportedProvider;
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

fn validate_no_template_in_secrets_provider(content: &str) -> Result<()> {
    let mut in_block = false;

    for (line_num, line) in content.lines().enumerate() {
        if line.starts_with("secrets_provider:") {
            if line.contains("{{") {
                bail!(
                    "secret injection cannot be done on the secrets_provider property (line {}): the secrets_provider config is loaded before the vault is initialized",
                    line_num + 1
                );
            }
            in_block = true;
            continue;
        }

        if in_block {
            let trimmed = line.trim_start();

            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if !line.starts_with(char::is_whitespace) {
                in_block = false;
                continue;
            }

            if line.contains("{{") {
                bail!(
                    "secret injection cannot be done within the secrets_provider block (line {}): the secrets_provider config is loaded before the vault is initialized",
                    line_num + 1
                );
            }
        }
    }

    Ok(())
}

/// Monokai Extended
const DARK_THEME: &[u8] = include_bytes!("../../assets/monokai-extended.theme.bin");
const LIGHT_THEME: &[u8] = include_bytes!("../../assets/monokai-extended-light.theme.bin");

const CONFIG_FILE_NAME: &str = "config.yaml";
const AGENT_GRAPH_FILE_NAME: &str = "graph.yaml";
const ROLES_DIR_NAME: &str = "roles";
const SKILLS_DIR_NAME: &str = "skills";
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
const MEMORY_DIR_NAME: &str = "memory";
const MEMORY_INDEX_FILE_NAME: &str = "MEMORY.md";
const WORKSPACE_MEMORY_FILE_NAME: &str = "COYOTE.md";
const WORKSPACE_MEMORY_DIR_NAME: &str = ".coyote";
const SBX_KIT_DIR_NAME: &str = "sbx-kit";
const SBX_KIT_HASH_FILE: &str = "kit.sha256";
const SBX_MIXIN_FILE_NAME: &str = "sbx-mixin.yaml";
const GIT_DIR_NAME: &str = ".git";
const GITIGNORE_FILE_NAME: &str = ".gitignore";
const DEFAULT_VISIBLE_TOOLS: [&str; 18] = [
    "execute_command.sh",
    "execute_py_code.py",
    "execute_sql_code.sh",
    "fetch_url_via_curl.sh",
    "fs_cat.sh",
    "fs_glob.sh",
    "fs_grep.sh",
    "fs_ls.sh",
    "fs_mkdir.sh",
    "fs_patch.sh",
    "fs_read.sh",
    "fs_rm.sh",
    "fs_write.sh",
    "get_current_time.sh",
    "get_current_weather.sh",
    "search_wikipedia.sh",
    "search_arxiv.sh",
    "web_search_coyote.sh",
];

const CLIENTS_FIELD: &str = "clients";

const SYNC_MODELS_URL: &str =
    "https://raw.githubusercontent.com/Dark-Alex-17/coyote/refs/heads/main/models.yaml";

const SUMMARIZATION_PROMPT: &str =
    "Summarize the discussion briefly in 200 words or less to use as a prompt for future context.";
const SUMMARY_CONTEXT_PROMPT: &str = "This is a summary of the chat history as a recap: ";

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

    #[serde(default)]
    pub(super) secrets_provider: Option<SupportedProvider>,

    pub function_calling_support: bool,
    pub mapping_tools: IndexMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_csv_or_vec")]
    pub enabled_tools: Option<Vec<String>>,
    pub visible_tools: Option<Vec<String>>,

    pub skills_enabled: bool,
    #[serde(default, deserialize_with = "deserialize_csv_or_vec")]
    pub enabled_skills: Option<Vec<String>>,
    pub visible_skills: Option<Vec<String>>,

    pub mcp_server_support: bool,
    pub mapping_mcp_servers: IndexMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_csv_or_vec")]
    pub enabled_mcp_servers: Option<Vec<String>>,

    pub auto_continue: bool,
    pub max_auto_continues: usize,
    pub inject_todo_instructions: bool,
    pub continuation_prompt: Option<String>,
    pub inject_skill_instructions: bool,
    pub skill_instructions: Option<String>,

    pub repl_prelude: Option<String>,
    pub cmd_prelude: Option<String>,
    pub agent_session: Option<String>,

    pub save_session: Option<bool>,
    pub compression_threshold: usize,
    pub summarization_prompt: Option<String>,
    pub summary_context_prompt: Option<String>,

    pub memory: Option<bool>,
    pub memory_cap_with_tools: Option<usize>,
    pub memory_cap_without_tools: Option<usize>,

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
            secrets_provider: None,

            function_calling_support: true,
            mapping_tools: Default::default(),
            enabled_tools: None,
            visible_tools: None,

            skills_enabled: true,
            enabled_skills: None,
            visible_skills: None,

            mcp_server_support: true,
            mapping_mcp_servers: Default::default(),
            enabled_mcp_servers: None,

            auto_continue: false,
            max_auto_continues: 10,
            inject_todo_instructions: true,
            continuation_prompt: None,
            inject_skill_instructions: true,
            skill_instructions: None,

            repl_prelude: None,
            cmd_prelude: None,
            agent_session: None,

            save_session: None,
            compression_threshold: 4000,
            summarization_prompt: None,
            summary_context_prompt: None,

            memory: None,
            memory_cap_with_tools: None,
            memory_cap_without_tools: None,

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
    Functions::install_builtin_global_tools(false)?;
    Agent::install_builtin_agents(false)?;
    Macro::install_macros(false)?;
    Skill::install_builtin_skills(false)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AssetCategory {
    Agents,
    Macros,
    Functions,
    Skills,
    #[value(name = "mcp_config")]
    McpConfig,
}

impl AssetCategory {
    pub const NAMES: [&'static str; 5] = ["agents", "macros", "functions", "skills", "mcp_config"];

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "agents" => Some(Self::Agents),
            "macros" => Some(Self::Macros),
            "functions" => Some(Self::Functions),
            "skills" => Some(Self::Skills),
            "mcp_config" => Some(Self::McpConfig),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum MemoryScope {
    Global,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum InstallFilter {
    Agents,
    Roles,
    Skills,
    Macros,
    Functions,
    #[value(name = "mcp_config")]
    McpConfig,
}

impl InstallFilter {
    pub const NAMES: [&'static str; 6] = [
        "agents",
        "roles",
        "skills",
        "macros",
        "functions",
        "mcp_config",
    ];

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "agents" => Some(Self::Agents),
            "roles" => Some(Self::Roles),
            "skills" => Some(Self::Skills),
            "macros" => Some(Self::Macros),
            "functions" => Some(Self::Functions),
            "mcp_config" => Some(Self::McpConfig),
            _ => None,
        }
    }
}

pub fn install_assets(category: AssetCategory) -> Result<()> {
    let (label, target) = match category {
        AssetCategory::Agents => ("agents", paths::agents_data_dir()),
        AssetCategory::Macros => ("macros", paths::macros_dir()),
        AssetCategory::Functions => ("functions", paths::functions_dir()),
        AssetCategory::Skills => ("skills", paths::skills_dir()),
        AssetCategory::McpConfig => ("MCP config", paths::mcp_config_file()),
    };

    if !confirm_asset_overwrite(category, label, &target)? {
        println!("Aborted. No files were changed.");
        return Ok(());
    }

    match category {
        AssetCategory::Agents => Agent::install_builtin_agents(true)?,
        AssetCategory::Macros => Macro::install_macros(true)?,
        AssetCategory::Functions => Functions::install_builtin_global_tools(true)?,
        AssetCategory::Skills => Skill::install_builtin_skills(true)?,
        AssetCategory::McpConfig => Functions::install_mcp_config()?,
    }

    println!("Reinstalled bundled {label} ({})", target.display());

    Ok(())
}

fn confirm_asset_overwrite(category: AssetCategory, label: &str, target: &Path) -> Result<bool> {
    if !*IS_STDOUT_TERMINAL {
        return Ok(true);
    }
    let body = match category {
        AssetCategory::McpConfig => format!(
            "This replaces your MCP server configuration at {} with this \
             build's bundled template. Your configured MCP servers (and any \
             custom secret references they contain) will be lost.",
            target.display()
        ),
        _ => format!(
            "Reinstalling bundled {label} overwrites every bundled {label} in \
             {} with this build's packaged versions. Local changes to bundled \
             {label} will be lost; {label} you created yourself are left \
             untouched.",
            target.display()
        ),
    };
    let prompt = format!("{} {body}\nContinue? [y/N] ", warning_text("WARNING:"));
    let answer = read_single_key(&['y', 'Y', 'n', 'N'], 'n', &prompt)?;

    Ok(matches!(answer, 'y' | 'Y'))
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

impl Config {
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
            secrets_provider: config.secrets_provider.clone(),
            ..AppConfig::default()
        };
        let vault = Vault::init(&bootstrap_app)?;
        let (parsed_config, missing_secrets) = interpolate_secrets(&content, &vault)?;
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
        validate_no_template_in_secrets_provider(content)?;

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

    let provider_choice = prompt_provider_choice()?;
    let mut vault = match &provider_choice {
        None => Vault::default_local(),
        Some(provider) => Vault {
            provider: provider.clone(),
        },
    };
    create_vault_password_file(&mut vault)?;
    if provider_choice.is_some() {
        vault.validate_round_trip()?;
    }

    let client = Select::new("API Provider (required):", list_client_types()).prompt()?;

    let mut config = json!({});
    let (model, clients_config) = create_client_config(client, &vault).await?;
    config["model"] = model.into();
    match &provider_choice {
        None => {
            config["vault_password_file"] =
                vault.local_password_file()?.display().to_string().into();
        }
        Some(provider) => {
            config["secrets_provider"] = serde_json::to_value(provider)
                .with_context(|| "failed to serialize secrets_provider config")?;
        }
    }
    config["stream"] = json!(true);
    config["save"] = json!(true);
    config["keybindings"] = json!("vi");
    config["wrap"] = json!("auto");
    config["wrap_code"] = json!(false);
    config["function_calling_support"] = json!(true);
    config["enabled_tools"] = json!(null);
    config["visible_tools"] = json!(DEFAULT_VISIBLE_TOOLS);
    config["mcp_server_support"] = json!(true);
    config["enabled_mcp_servers"] = json!(null);
    config["highlight"] = json!(true);
    config["light_theme"] = json!(false);
    config[CLIENTS_FIELD] = clients_config;

    let config_data = serde_yaml::to_string(&config).with_context(|| "Failed to create config")?;
    let config_data = format!(
        "# see https://github.com/Dark-Alex-17/coyote/blob/main/config.example.yaml\n\n{config_data}"
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

pub(super) fn csv_to_vec(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

pub(super) fn deserialize_csv_or_vec<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, SeqAccess, Visitor};
    use std::fmt;

    struct CsvOrVec;

    impl<'de> Visitor<'de> for CsvOrVec {
        type Value = Option<Vec<String>>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a comma-separated string, a list of strings, or null")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
            Ok(Some(csv_to_vec(value)))
        }

        fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<Self::Value, E> {
            Ok(Some(csv_to_vec(&value)))
        }

        fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: serde::Deserializer<'de>>(
            self,
            deserializer: D2,
        ) -> std::result::Result<Self::Value, D2::Error> {
            deserializer.deserialize_any(self)
        }

        fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut vec = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                let trimmed = item.trim().to_string();
                if !trimmed.is_empty() {
                    vec.push(trimmed);
                }
            }
            Ok(Some(vec))
        }
    }

    deserializer.deserialize_option(CsvOrVec)
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
    fn validate_secrets_provider_rejects_template_in_field() {
        let yaml = "\
secrets_provider:
  type: aws_secrets_manager
  aws_profile: '{{AWS_PROFILE}}'
  aws_region: us-east-1
";
        assert!(validate_no_template_in_secrets_provider(yaml).is_err());
    }

    #[test]
    fn validate_secrets_provider_rejects_template_in_local_password_file() {
        let yaml = "\
secrets_provider:
  type: local
  password_file: '{{COYOTE_PASSWORD}}'
";
        assert!(validate_no_template_in_secrets_provider(yaml).is_err());
    }

    #[test]
    fn validate_secrets_provider_accepts_clean_yaml() {
        let yaml = "\
secrets_provider:
  type: aws_secrets_manager
  aws_profile: default
  aws_region: us-east-1
";
        assert!(validate_no_template_in_secrets_provider(yaml).is_ok());
    }

    #[test]
    fn validate_secrets_provider_allows_templates_outside_block() {
        let yaml = "\
secrets_provider:
  type: local
  password_file: ~/.coyote_password
clients:
  - type: openai
    api_key: '{{OPENAI_KEY}}'
";
        assert!(validate_no_template_in_secrets_provider(yaml).is_ok());
    }

    #[test]
    fn validate_secrets_provider_handles_missing_block() {
        let yaml = "\
model: openai:gpt-4
clients:
  - type: openai
    api_key: '{{OPENAI_KEY}}'
";
        assert!(validate_no_template_in_secrets_provider(yaml).is_ok());
    }

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

    #[test]
    fn assert_state_pass_always_true() {
        let pass = AssertState::pass();
        assert!(pass.assert(StateFlags::empty()));
        assert!(pass.assert(StateFlags::ROLE));
        assert!(pass.assert(StateFlags::SESSION | StateFlags::AGENT));
        assert!(pass.assert(StateFlags::all()));
    }

    #[test]
    fn assert_state_bare_only_empty() {
        let bare = AssertState::bare();
        assert!(bare.assert(StateFlags::empty()));
        assert!(!bare.assert(StateFlags::ROLE));
        assert!(!bare.assert(StateFlags::SESSION));
    }

    #[test]
    fn assert_state_true_requires_flag_present() {
        let state = AssertState::True(StateFlags::ROLE);
        assert!(state.assert(StateFlags::ROLE));
        assert!(state.assert(StateFlags::ROLE | StateFlags::SESSION));
        assert!(!state.assert(StateFlags::empty()));
        assert!(!state.assert(StateFlags::SESSION));
    }

    #[test]
    fn assert_state_true_with_multiple_flags_any_match() {
        let state = AssertState::True(StateFlags::SESSION_EMPTY | StateFlags::SESSION);
        assert!(state.assert(StateFlags::SESSION_EMPTY));
        assert!(state.assert(StateFlags::SESSION));
        assert!(state.assert(StateFlags::SESSION | StateFlags::ROLE));
        assert!(!state.assert(StateFlags::ROLE));
        assert!(!state.assert(StateFlags::empty()));
    }

    #[test]
    fn assert_state_false_requires_flag_absent() {
        let state = AssertState::False(StateFlags::AGENT);
        assert!(state.assert(StateFlags::empty()));
        assert!(state.assert(StateFlags::ROLE));
        assert!(!state.assert(StateFlags::AGENT));
        assert!(!state.assert(StateFlags::AGENT | StateFlags::ROLE));
    }

    #[test]
    fn assert_state_false_with_multiple_flags() {
        let state = AssertState::False(StateFlags::SESSION | StateFlags::AGENT);
        assert!(state.assert(StateFlags::empty()));
        assert!(state.assert(StateFlags::ROLE));
        assert!(!state.assert(StateFlags::SESSION));
        assert!(!state.assert(StateFlags::AGENT));
        assert!(!state.assert(StateFlags::SESSION | StateFlags::AGENT));
    }

    #[test]
    fn assert_state_truefalse_requires_true_present_and_false_absent() {
        let state = AssertState::TrueFalse(StateFlags::ROLE, StateFlags::SESSION);
        assert!(state.assert(StateFlags::ROLE));
        assert!(state.assert(StateFlags::ROLE | StateFlags::RAG));
        assert!(!state.assert(StateFlags::empty()));
        assert!(!state.assert(StateFlags::SESSION));
        assert!(!state.assert(StateFlags::ROLE | StateFlags::SESSION));
    }

    #[test]
    fn assert_state_equal_exact_match() {
        let state = AssertState::Equal(StateFlags::ROLE | StateFlags::SESSION);
        assert!(state.assert(StateFlags::ROLE | StateFlags::SESSION));
        assert!(!state.assert(StateFlags::ROLE));
        assert!(!state.assert(StateFlags::SESSION));
        assert!(!state.assert(StateFlags::empty()));
    }
}
