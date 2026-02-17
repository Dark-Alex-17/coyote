mod agent;
mod input;
mod macros;
mod role;
mod session;
pub(crate) mod todo;

pub use self::agent::{Agent, AgentVariables, complete_agent_variables, list_agents};
pub use self::input::Input;
pub use self::role::{
    CODE_ROLE, CREATE_TITLE_ROLE, EXPLAIN_SHELL_ROLE, Role, RoleLike, SHELL_ROLE,
};
use self::session::Session;
pub use macros::macro_execute;
use mem::take;

use crate::client::{
    ClientConfig, MessageContentToolCalls, Model, ModelType, OPENAI_COMPATIBLE_PROVIDERS,
    ProviderModels, create_client_config, list_client_types, list_models,
};
use crate::function::{FunctionDeclaration, Functions, ToolCallTracker, ToolResult};
use crate::rag::Rag;
use crate::render::{MarkdownRender, RenderOptions};
use crate::utils::*;

use crate::config::macros::Macro;
use crate::mcp::{
    MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX, MCP_INVOKE_META_FUNCTION_NAME_PREFIX,
    MCP_SEARCH_META_FUNCTION_NAME_PREFIX, McpRegistry,
};
use crate::supervisor::Supervisor;
use crate::vault::{GlobalVault, Vault, create_vault_password_file, interpolate_secrets};
use anyhow::{Context, Result, anyhow, bail};
use fancy_regex::Regex;
use indexmap::IndexMap;
use indoc::formatdoc;
use inquire::{Confirm, MultiSelect, Select, Text, list_option::ListOption, validator::Validation};
use log::LevelFilter;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::{
    env,
    fs::{
        File, OpenOptions, create_dir_all, read_dir, read_to_string, remove_dir_all, remove_file,
    },
    io::Write,
    mem,
    path::{Path, PathBuf},
    process,
    sync::{Arc, OnceLock},
};
use syntect::highlighting::ThemeSet;
use terminal_colorsaurus::{ColorScheme, QueryOptions, color_scheme};
use tokio::runtime::Handle;
use crate::supervisor::mailbox::Inbox;

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
    vault_password_file: Option<PathBuf>,

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

    #[serde(skip)]
    pub vault: GlobalVault,

    #[serde(skip)]
    pub macro_flag: bool,
    #[serde(skip)]
    pub info_flag: bool,
    #[serde(skip)]
    pub agent_variables: Option<AgentVariables>,

    #[serde(skip)]
    pub model: Model,
    #[serde(skip)]
    pub functions: Functions,
    #[serde(skip)]
    pub mcp_registry: Option<McpRegistry>,
    #[serde(skip)]
    pub working_mode: WorkingMode,
    #[serde(skip)]
    pub last_message: Option<LastMessage>,

    #[serde(skip)]
    pub role: Option<Role>,
    #[serde(skip)]
    pub session: Option<Session>,
    #[serde(skip)]
    pub rag: Option<Arc<Rag>>,
    #[serde(skip)]
    pub agent: Option<Agent>,
    #[serde(skip)]
    pub(crate) tool_call_tracker: Option<ToolCallTracker>,
    #[serde(skip)]
    pub supervisor: Option<Arc<RwLock<Supervisor>>>,
    #[serde(skip)]
    pub current_depth: usize,
    #[serde(skip)]
    pub inbox: Option<Arc<Inbox>>,
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

            vault: Default::default(),

            macro_flag: false,
            info_flag: false,
            agent_variables: None,

            model: Default::default(),
            functions: Default::default(),
            mcp_registry: Default::default(),
            working_mode: WorkingMode::Cmd,
            last_message: None,

            role: None,
            session: None,
            rag: None,
            agent: None,
            tool_call_tracker: Some(ToolCallTracker::default()),
            supervisor: None,
            current_depth: 0,
            inbox: None,
        }
    }
}

pub type GlobalConfig = Arc<RwLock<Config>>;

impl Config {
    pub fn init_bare() -> Result<Self> {
        let h = Handle::current();
        tokio::task::block_in_place(|| {
            h.block_on(Self::init(
                WorkingMode::Cmd,
                true,
                false,
                None,
                create_abort_signal(),
            ))
        })
    }

    pub async fn init(
        working_mode: WorkingMode,
        info_flag: bool,
        start_mcp_servers: bool,
        log_path: Option<PathBuf>,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let config_path = Self::config_file();
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

        let setup = async |config: &mut Self| -> Result<()> {
            let vault = Vault::init(config);

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
                *config = new_config.clone();
            }

            config.working_mode = working_mode;
            config.info_flag = info_flag;
            config.vault = Arc::new(vault);

            Agent::install_builtin_agents()?;

            config.load_envs();

            if let Some(wrap) = config.wrap.clone() {
                config.set_wrap(&wrap)?;
            }

            config.load_functions()?;
            config
                .load_mcp_servers(log_path, start_mcp_servers, abort_signal)
                .await?;

            config.setup_model()?;
            config.setup_document_loaders();
            config.setup_user_agent();
            Macro::install_macros()?;
            Ok(())
        };
        let ret = setup(&mut config).await;
        if !info_flag {
            ret?;
        }
        Ok(config)
    }

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
        Self::config_dir().join(name)
    }

    pub fn cache_path() -> PathBuf {
        let base_dir = dirs::cache_dir().unwrap_or_else(env::temp_dir);

        base_dir.join(env!("CARGO_CRATE_NAME"))
    }

    pub fn log_path() -> PathBuf {
        Config::cache_path().join(format!("{}.log", env!("CARGO_CRATE_NAME")))
    }

    pub fn config_file() -> PathBuf {
        match env::var(get_env_name("config_file")) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::local_path(CONFIG_FILE_NAME),
        }
    }

    pub fn vault_password_file(&self) -> PathBuf {
        match &self.vault_password_file {
            Some(path) => match path.exists() {
                true => path.clone(),
                false => gman::config::Config::local_provider_password_file(),
            },
            None => gman::config::Config::local_provider_password_file(),
        }
    }

    pub fn roles_dir() -> PathBuf {
        match env::var(get_env_name("roles_dir")) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::local_path(ROLES_DIR_NAME),
        }
    }

    pub fn role_file(name: &str) -> PathBuf {
        Self::roles_dir().join(format!("{name}.md"))
    }

    pub fn macros_dir() -> PathBuf {
        match env::var(get_env_name("macros_dir")) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::local_path(MACROS_DIR_NAME),
        }
    }

    pub fn macro_file(name: &str) -> PathBuf {
        Self::macros_dir().join(format!("{name}.yaml"))
    }

    pub fn env_file() -> PathBuf {
        match env::var(get_env_name("env_file")) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::local_path(ENV_FILE_NAME),
        }
    }

    pub fn messages_file(&self) -> PathBuf {
        match &self.agent {
            None => match env::var(get_env_name("messages_file")) {
                Ok(value) => PathBuf::from(value),
                Err(_) => Self::cache_path().join(MESSAGES_FILE_NAME),
            },
            Some(agent) => Self::cache_path()
                .join(AGENTS_DIR_NAME)
                .join(agent.name())
                .join(MESSAGES_FILE_NAME),
        }
    }

    pub fn sessions_dir(&self) -> PathBuf {
        match &self.agent {
            None => match env::var(get_env_name("sessions_dir")) {
                Ok(value) => PathBuf::from(value),
                Err(_) => Self::local_path(SESSIONS_DIR_NAME),
            },
            Some(agent) => Self::agent_data_dir(agent.name()).join(SESSIONS_DIR_NAME),
        }
    }

    pub fn rags_dir() -> PathBuf {
        match env::var(get_env_name("rags_dir")) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::local_path(RAGS_DIR_NAME),
        }
    }

    pub fn functions_dir() -> PathBuf {
        match env::var(get_env_name("functions_dir")) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::local_path(FUNCTIONS_DIR_NAME),
        }
    }

    pub fn functions_bin_dir() -> PathBuf {
        Self::functions_dir().join(FUNCTIONS_BIN_DIR_NAME)
    }

    pub fn mcp_config_file() -> PathBuf {
        Self::functions_dir().join(MCP_FILE_NAME)
    }

    pub fn global_tools_dir() -> PathBuf {
        Self::functions_dir().join(GLOBAL_TOOLS_DIR_NAME)
    }

    pub fn global_utils_dir() -> PathBuf {
        Self::functions_dir().join(GLOBAL_TOOLS_UTILS_DIR_NAME)
    }

    pub fn bash_prompt_utils_file() -> PathBuf {
        Self::global_utils_dir().join(BASH_PROMPT_UTILS_FILE_NAME)
    }

    pub fn session_file(&self, name: &str) -> PathBuf {
        match name.split_once("/") {
            Some((dir, name)) => self.sessions_dir().join(dir).join(format!("{name}.yaml")),
            None => self.sessions_dir().join(format!("{name}.yaml")),
        }
    }

    pub fn rag_file(&self, name: &str) -> PathBuf {
        match &self.agent {
            Some(agent) => Self::agent_rag_file(agent.name(), name),
            None => Self::rags_dir().join(format!("{name}.yaml")),
        }
    }

    pub fn agents_data_dir() -> PathBuf {
        Self::local_path(AGENTS_DIR_NAME)
    }

    pub fn agent_data_dir(name: &str) -> PathBuf {
        match env::var(format!("{}_DATA_DIR", normalize_env_name(name))) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::agents_data_dir().join(name),
        }
    }

    pub fn agent_config_file(name: &str) -> PathBuf {
        match env::var(format!("{}_CONFIG_FILE", normalize_env_name(name))) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::agent_data_dir(name).join(CONFIG_FILE_NAME),
        }
    }

    pub fn agent_bin_dir(name: &str) -> PathBuf {
        Self::agent_data_dir(name).join(FUNCTIONS_BIN_DIR_NAME)
    }

    pub fn agent_rag_file(agent_name: &str, rag_name: &str) -> PathBuf {
        Self::agent_data_dir(agent_name).join(format!("{rag_name}.yaml"))
    }

    pub fn agent_functions_file(name: &str) -> Result<PathBuf> {
        let allowed = ["tools.sh", "tools.py", "tools.js"];

        for entry in read_dir(Self::agent_data_dir(name))? {
            let entry = entry?;
            if let Some(file) = entry.file_name().to_str()
                && allowed.contains(&file)
            {
                return Ok(entry.path());
            }
        }

        Err(anyhow!(
            "No tools script found in agent functions directory"
        ))
    }

    pub fn models_override_file() -> PathBuf {
        Self::local_path("models-override.yaml")
    }

    pub fn state(&self) -> StateFlags {
        let mut flags = StateFlags::empty();
        if let Some(session) = &self.session {
            if session.is_empty() {
                flags |= StateFlags::SESSION_EMPTY;
            } else {
                flags |= StateFlags::SESSION;
            }
            if session.role_name().is_some() {
                flags |= StateFlags::ROLE;
            }
        } else if self.role.is_some() {
            flags |= StateFlags::ROLE;
        }
        if self.agent.is_some() {
            flags |= StateFlags::AGENT;
        }
        if self.rag.is_some() {
            flags |= StateFlags::RAG;
        }
        flags
    }

    pub fn log_config() -> Result<(LevelFilter, Option<PathBuf>)> {
        let log_level = env::var(get_env_name("log_level"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(match cfg!(debug_assertions) {
                true => LevelFilter::Debug,
                false => LevelFilter::Info,
            });
        let log_path = match env::var(get_env_name("log_path")) {
            Ok(v) => Some(PathBuf::from(v)),
            Err(_) => Some(Config::log_path()),
        };
        Ok((log_level, log_path))
    }

    pub fn edit_config(&self) -> Result<()> {
        let config_path = Self::config_file();
        let editor = self.editor()?;
        edit_file(&editor, &config_path)?;
        println!(
            "NOTE: Remember to restart {} if there are changes made to '{}'",
            env!("CARGO_CRATE_NAME"),
            config_path.display(),
        );
        Ok(())
    }

    pub fn current_model(&self) -> &Model {
        if let Some(session) = self.session.as_ref() {
            session.model()
        } else if let Some(agent) = self.agent.as_ref() {
            agent.model()
        } else if let Some(role) = self.role.as_ref() {
            role.model()
        } else {
            &self.model
        }
    }

    pub fn role_like_mut(&mut self) -> Option<&mut dyn RoleLike> {
        if let Some(session) = self.session.as_mut() {
            Some(session)
        } else if let Some(agent) = self.agent.as_mut() {
            Some(agent)
        } else if let Some(role) = self.role.as_mut() {
            Some(role)
        } else {
            None
        }
    }

    pub fn extract_role(&self) -> Role {
        if let Some(session) = self.session.as_ref() {
            session.to_role()
        } else if let Some(agent) = self.agent.as_ref() {
            agent.to_role()
        } else if let Some(role) = self.role.as_ref() {
            role.clone()
        } else {
            let mut role = Role::default();
            role.batch_set(
                &self.model,
                self.temperature,
                self.top_p,
                self.enabled_tools.clone(),
                self.enabled_mcp_servers.clone(),
            );
            role
        }
    }

    pub fn info(&self) -> Result<String> {
        if let Some(agent) = &self.agent {
            let output = agent.export()?;
            if let Some(session) = &self.session {
                let session = session
                    .export()?
                    .split('\n')
                    .map(|v| format!("  {v}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(format!("{output}session:\n{session}"))
            } else {
                Ok(output)
            }
        } else if let Some(session) = &self.session {
            session.export()
        } else if let Some(role) = &self.role {
            Ok(role.export())
        } else if let Some(rag) = &self.rag {
            rag.export()
        } else {
            self.sysinfo()
        }
    }

    pub fn sysinfo(&self) -> Result<String> {
        let display_path = |path: &Path| path.display().to_string();
        let wrap = self
            .wrap
            .clone()
            .map_or_else(|| String::from("no"), |v| v.to_string());
        let (rag_reranker_model, rag_top_k) = match &self.rag {
            Some(rag) => rag.get_config(),
            None => (self.rag_reranker_model.clone(), self.rag_top_k),
        };
        let role = self.extract_role();
        let mut items = vec![
            ("model", role.model().id()),
            ("temperature", format_option_value(&role.temperature())),
            ("top_p", format_option_value(&role.top_p())),
            ("enabled_tools", format_option_value(&role.enabled_tools())),
            (
                "enabled_mcp_servers",
                format_option_value(&role.enabled_mcp_servers()),
            ),
            (
                "max_output_tokens",
                role.model()
                    .max_tokens_param()
                    .map(|v| format!("{v} (current model)"))
                    .unwrap_or_else(|| "null".into()),
            ),
            ("save_session", format_option_value(&self.save_session)),
            (
                "compression_threshold",
                self.compression_threshold.to_string(),
            ),
            (
                "rag_reranker_model",
                format_option_value(&rag_reranker_model),
            ),
            ("rag_top_k", rag_top_k.to_string()),
            ("dry_run", self.dry_run.to_string()),
            (
                "function_calling_support",
                self.function_calling_support.to_string(),
            ),
            ("mcp_server_support", self.mcp_server_support.to_string()),
            ("stream", self.stream.to_string()),
            ("save", self.save.to_string()),
            ("keybindings", self.keybindings.clone()),
            ("wrap", wrap),
            ("wrap_code", self.wrap_code.to_string()),
            ("highlight", self.highlight.to_string()),
            ("theme", format_option_value(&self.theme)),
            ("config_file", display_path(&Self::config_file())),
            ("env_file", display_path(&Self::env_file())),
            ("agents_dir", display_path(&Self::agents_data_dir())),
            ("roles_dir", display_path(&Self::roles_dir())),
            ("sessions_dir", display_path(&self.sessions_dir())),
            ("rags_dir", display_path(&Self::rags_dir())),
            ("macros_dir", display_path(&Self::macros_dir())),
            ("functions_dir", display_path(&Self::functions_dir())),
            ("messages_file", display_path(&self.messages_file())),
            (
                "vault_password_file",
                display_path(&self.vault_password_file()),
            ),
        ];
        if let Ok((_, Some(log_path))) = Self::log_config() {
            items.push(("log_path", display_path(&log_path)));
        }
        let output = items
            .iter()
            .map(|(name, value)| format!("{name:<30}{value}\n"))
            .collect::<Vec<String>>()
            .join("");
        Ok(output)
    }

    pub async fn update(
        config: &GlobalConfig,
        data: &str,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let parts: Vec<&str> = data.split_whitespace().collect();
        if parts.len() != 2 {
            bail!("Usage: .set <key> <value>. If value is null, unset key.");
        }
        let key = parts[0];
        let value = parts[1];
        match key {
            "temperature" => {
                let value = parse_value(value)?;
                config.write().set_temperature(value);
            }
            "top_p" => {
                let value = parse_value(value)?;
                config.write().set_top_p(value);
            }
            "enabled_tools" => {
                let value = parse_value(value)?;
                config.write().set_enabled_tools(value);
            }
            "enabled_mcp_servers" => {
                let value: Option<String> = parse_value(value)?;
                if let Some(servers) = value.as_ref()
                    && let Some(registry) = &config.read().mcp_registry
                {
                    if registry.list_configured_servers().is_empty() {
                        bail!(
                            "No MCP servers are configured. Please configure MCP servers first before setting 'enabled_mcp_servers'."
                        );
                    }

                    if !servers.split(',').all(|s| {
                        registry
                            .list_configured_servers()
                            .contains(&s.trim().to_string())
                            || s == "all"
                    }) {
                        bail!(
                            "Some of the specified MCP servers in 'enabled_mcp_servers' are not fully configured. Please check your MCP server configuration."
                        );
                    }
                }
                config.write().set_enabled_mcp_servers(value.clone());
                if config.read().mcp_server_support {
                    config.write().functions.clear_mcp_meta_functions();
                    let registry = config
                        .write()
                        .mcp_registry
                        .take()
                        .expect("MCP registry should be initialized");
                    let new_mcp_registry =
                        McpRegistry::reinit(registry, value, abort_signal.clone()).await?;

                    if !new_mcp_registry.is_empty() {
                        config
                            .write()
                            .functions
                            .append_mcp_meta_functions(new_mcp_registry.list_started_servers());
                    }

                    config.write().mcp_registry = Some(new_mcp_registry);
                }
            }
            "max_output_tokens" => {
                let value = parse_value(value)?;
                config.write().set_max_output_tokens(value);
            }
            "save_session" => {
                let value = parse_value(value)?;
                config.write().set_save_session(value);
            }
            "compression_threshold" => {
                let value = parse_value(value)?;
                config.write().set_compression_threshold(value);
            }
            "rag_reranker_model" => {
                let value = parse_value(value)?;
                Self::set_rag_reranker_model(config, value)?;
            }
            "rag_top_k" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                Self::set_rag_top_k(config, value)?;
            }
            "dry_run" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().dry_run = value;
            }
            "function_calling_support" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                if value && config.write().functions.is_empty() {
                    bail!("Function calling cannot be enabled because no functions are installed.")
                }
                config.write().function_calling_support = value;
            }
            "mcp_server_support" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().functions.clear_mcp_meta_functions();

                let registry = config
                    .write()
                    .mcp_registry
                    .take()
                    .expect("MCP registry should be initialized");
                let enabled_mcp_servers = if value {
                    config.read().enabled_mcp_servers.clone()
                } else {
                    None
                };
                let new_registry =
                    McpRegistry::reinit(registry, enabled_mcp_servers, abort_signal.clone())
                        .await?;
                if !new_registry.is_empty() && value {
                    config
                        .write()
                        .functions
                        .append_mcp_meta_functions(new_registry.list_started_servers());
                }
                config.write().mcp_registry = Some(new_registry);
                config.write().mcp_server_support = value;
            }
            "stream" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().stream = value;
            }
            "save" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().save = value;
            }
            "highlight" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().highlight = value;
            }
            _ => bail!("Unknown key '{key}'"),
        }
        Ok(())
    }

    pub fn delete(config: &GlobalConfig, kind: &str) -> Result<()> {
        let (dir, file_ext) = match kind {
            "role" => (Self::roles_dir(), Some(".md")),
            "session" => (config.read().sessions_dir(), Some(".yaml")),
            "rag" => (Self::rags_dir(), Some(".yaml")),
            "macro" => (Self::macros_dir(), Some(".yaml")),
            "agent-data" => (Self::agents_data_dir(), None),
            _ => bail!("Unknown kind '{kind}'"),
        };
        let names = match read_dir(&dir) {
            Ok(rd) => {
                let mut names = vec![];
                for entry in rd.flatten() {
                    let name = entry.file_name();
                    match file_ext {
                        Some(file_ext) => {
                            if let Some(name) = name.to_string_lossy().strip_suffix(file_ext) {
                                names.push(name.to_string());
                            }
                        }
                        None => {
                            if entry.path().is_dir() {
                                names.push(name.to_string_lossy().to_string());
                            }
                        }
                    }
                }
                names.sort_unstable();
                names
            }
            Err(_) => vec![],
        };

        if names.is_empty() {
            bail!("No {kind} to delete")
        }

        let select_names = MultiSelect::new(&format!("Select {kind} to delete:"), names)
            .with_validator(|list: &[ListOption<&String>]| {
                if list.is_empty() {
                    Ok(Validation::Invalid(
                        "At least one item must be selected".into(),
                    ))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt()?;

        for name in select_names {
            match file_ext {
                Some(ext) => {
                    let path = dir.join(format!("{name}{ext}"));
                    remove_file(&path).with_context(|| {
                        format!("Failed to delete {kind} at '{}'", path.display())
                    })?;
                }
                None => {
                    let path = dir.join(name);
                    remove_dir_all(&path).with_context(|| {
                        format!("Failed to delete {kind} at '{}'", path.display())
                    })?;
                }
            }
        }
        println!("✓ Successfully deleted {kind}.");
        Ok(())
    }

    pub fn set_temperature(&mut self, value: Option<f64>) {
        match self.role_like_mut() {
            Some(role_like) => role_like.set_temperature(value),
            None => self.temperature = value,
        }
    }

    pub fn set_top_p(&mut self, value: Option<f64>) {
        match self.role_like_mut() {
            Some(role_like) => role_like.set_top_p(value),
            None => self.top_p = value,
        }
    }

    pub fn set_enabled_tools(&mut self, value: Option<String>) {
        match self.role_like_mut() {
            Some(role_like) => role_like.set_enabled_tools(value),
            None => self.enabled_tools = value,
        }
    }

    pub fn set_enabled_mcp_servers(&mut self, value: Option<String>) {
        match self.role_like_mut() {
            Some(role_like) => role_like.set_enabled_mcp_servers(value),
            None => self.enabled_mcp_servers = value,
        }
    }

    pub fn set_save_session(&mut self, value: Option<bool>) {
        if let Some(session) = self.session.as_mut() {
            session.set_save_session(value);
        } else {
            self.save_session = value;
        }
    }

    pub fn set_compression_threshold(&mut self, value: Option<usize>) {
        if let Some(session) = self.session.as_mut() {
            session.set_compression_threshold(value);
        } else {
            self.compression_threshold = value.unwrap_or_default();
        }
    }

    pub fn set_rag_reranker_model(config: &GlobalConfig, value: Option<String>) -> Result<()> {
        if let Some(id) = &value {
            Model::retrieve_model(&config.read(), id, ModelType::Reranker)?;
        }
        let has_rag = config.read().rag.is_some();
        match has_rag {
            true => update_rag(config, |rag| {
                rag.set_reranker_model(value)?;
                Ok(())
            })?,
            false => config.write().rag_reranker_model = value,
        }
        Ok(())
    }

    pub fn set_rag_top_k(config: &GlobalConfig, value: usize) -> Result<()> {
        let has_rag = config.read().rag.is_some();
        match has_rag {
            true => update_rag(config, |rag| {
                rag.set_top_k(value)?;
                Ok(())
            })?,
            false => config.write().rag_top_k = value,
        }
        Ok(())
    }

    pub fn set_wrap(&mut self, value: &str) -> Result<()> {
        if value == "no" {
            self.wrap = None;
        } else if value == "auto" {
            self.wrap = Some(value.into());
        } else {
            value
                .parse::<u16>()
                .map_err(|_| anyhow!("Invalid wrap value"))?;
            self.wrap = Some(value.into())
        }
        Ok(())
    }

    pub fn set_max_output_tokens(&mut self, value: Option<isize>) {
        match self.role_like_mut() {
            Some(role_like) => {
                let mut model = role_like.model().clone();
                model.set_max_tokens(value, true);
                role_like.set_model(model);
            }
            None => {
                self.model.set_max_tokens(value, true);
            }
        };
    }

    pub fn set_model(&mut self, model_id: &str) -> Result<()> {
        let model = Model::retrieve_model(self, model_id, ModelType::Chat)?;
        match self.role_like_mut() {
            Some(role_like) => role_like.set_model(model),
            None => {
                self.model = model;
            }
        }
        Ok(())
    }

    pub fn use_prompt(&mut self, prompt: &str) -> Result<()> {
        let mut role = Role::new(TEMP_ROLE_NAME, prompt);
        role.set_model(self.current_model().clone());
        self.use_role_obj(role)
    }

    pub async fn use_role_safely(
        config: &GlobalConfig,
        name: &str,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let mut cfg = {
            let mut guard = config.write();
            take(&mut *guard)
        };

        cfg.use_role(name, abort_signal.clone()).await?;

        {
            let mut guard = config.write();
            *guard = cfg;
        }

        Ok(())
    }

    pub async fn use_role(&mut self, name: &str, abort_signal: AbortSignal) -> Result<()> {
        let role = self.retrieve_role(name)?;
        let mcp_servers = if self.mcp_server_support {
            role.enabled_mcp_servers()
        } else {
            eprintln!(
                "{}",
                formatdoc!(
                    "
										This role uses MCP servers, but MCP support is disabled.
										To enable it, exit the role and set 'mcp_server_support: true', then try again
										"
                )
            );
            None
        };
        self.functions.clear_mcp_meta_functions();
        let registry = self
            .mcp_registry
            .take()
            .with_context(|| "MCP registry should be populated")?;
        let new_mcp_registry =
            McpRegistry::reinit(registry, mcp_servers, abort_signal.clone()).await?;

        if !new_mcp_registry.is_empty() {
            self.functions
                .append_mcp_meta_functions(new_mcp_registry.list_started_servers());
        }

        self.mcp_registry = Some(new_mcp_registry);
        self.use_role_obj(role)
    }

    pub fn use_role_obj(&mut self, role: Role) -> Result<()> {
        if self.agent.is_some() {
            bail!("Cannot perform this operation because you are using a agent")
        }
        if let Some(session) = self.session.as_mut() {
            session.guard_empty()?;
            session.set_role(role);
        } else {
            self.role = Some(role);
        }
        Ok(())
    }

    pub fn role_info(&self) -> Result<String> {
        if let Some(session) = &self.session {
            if session.role_name().is_some() {
                let role = session.to_role();
                Ok(role.export())
            } else {
                bail!("No session role")
            }
        } else if let Some(role) = &self.role {
            Ok(role.export())
        } else {
            bail!("No role")
        }
    }

    pub fn exit_role(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.guard_empty()?;
            session.clear_role();
        } else if self.role.is_some() {
            self.role = None;
        }
        Ok(())
    }

    pub fn retrieve_role(&self, name: &str) -> Result<Role> {
        let names = Self::list_roles(false);
        let mut role = if names.contains(&name.to_string()) {
            let path = Self::role_file(name);
            let content = read_to_string(&path)?;
            Role::new(name, &content)
        } else {
            Role::builtin(name)?
        };
        let current_model = self.current_model().clone();
        match role.model_id() {
            Some(model_id) => {
                if current_model.id() != model_id {
                    let model = Model::retrieve_model(self, model_id, ModelType::Chat)?;
                    role.set_model(model);
                } else {
                    role.set_model(current_model);
                }
            }
            None => {
                role.set_model(current_model);
                if role.temperature().is_none() {
                    role.set_temperature(self.temperature);
                }
                if role.top_p().is_none() {
                    role.set_top_p(self.top_p);
                }
            }
        }
        Ok(role)
    }

    pub fn new_role(&mut self, name: &str) -> Result<()> {
        if self.macro_flag {
            bail!("No role");
        }
        let ans = Confirm::new("Create a new role?")
            .with_default(true)
            .prompt()?;
        if ans {
            self.upsert_role(name)?;
        } else {
            bail!("No role");
        }
        Ok(())
    }

    pub async fn edit_role(&mut self, abort_signal: AbortSignal) -> Result<()> {
        let role_name;
        if let Some(session) = self.session.as_ref() {
            if let Some(name) = session.role_name().map(|v| v.to_string()) {
                if session.is_empty() {
                    role_name = Some(name);
                } else {
                    bail!("Cannot perform this operation because you are in a non-empty session")
                }
            } else {
                bail!("No role")
            }
        } else {
            role_name = self.role.as_ref().map(|v| v.name().to_string());
        }
        let name = role_name.ok_or_else(|| anyhow!("No role"))?;
        self.upsert_role(&name)?;
        self.use_role(&name, abort_signal.clone()).await
    }

    pub fn upsert_role(&mut self, name: &str) -> Result<()> {
        let role_path = Self::role_file(name);
        ensure_parent_exists(&role_path)?;
        let editor = self.editor()?;
        edit_file(&editor, &role_path)?;
        if self.working_mode.is_repl() {
            println!("✓ Saved the role to '{}'.", role_path.display());
        }
        Ok(())
    }

    pub fn save_role(&mut self, name: Option<&str>) -> Result<()> {
        let mut role_name = match &self.role {
            Some(role) => {
                if role.has_args() {
                    bail!("Unable to save the role with arguments (whose name contains '#')")
                }
                match name {
                    Some(v) => v.to_string(),
                    None => role.name().to_string(),
                }
            }
            None => bail!("No role"),
        };
        if role_name == TEMP_ROLE_NAME {
            role_name = Text::new("Role name:")
                .with_validator(|input: &str| {
                    let input = input.trim();
                    if input.is_empty() {
                        Ok(Validation::Invalid("This name is required".into()))
                    } else if input == TEMP_ROLE_NAME {
                        Ok(Validation::Invalid("This name is reserved".into()))
                    } else {
                        Ok(Validation::Valid)
                    }
                })
                .prompt()?;
        }
        let role_path = Self::role_file(&role_name);
        if let Some(role) = self.role.as_mut() {
            role.save(&role_name, &role_path, self.working_mode.is_repl())?;
        }

        Ok(())
    }

    pub fn list_roles(with_builtin: bool) -> Vec<String> {
        let mut names = HashSet::new();
        if let Ok(rd) = read_dir(Self::roles_dir()) {
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
        let names = Self::list_roles(true);
        names.contains(&name.to_string())
    }

    pub async fn use_session_safely(
        config: &GlobalConfig,
        session_name: Option<&str>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let mut cfg = {
            let mut guard = config.write();
            take(&mut *guard)
        };

        cfg.use_session(session_name, abort_signal.clone()).await?;

        {
            let mut guard = config.write();
            *guard = cfg;
        }

        Ok(())
    }

    pub async fn use_session(
        &mut self,
        session_name: Option<&str>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        if self.session.is_some() {
            bail!(
                "Already in a session, please run '.exit session' first to exit the current session."
            );
        }
        let mut session;
        match session_name {
            None | Some(TEMP_SESSION_NAME) => {
                let session_file = self.session_file(TEMP_SESSION_NAME);
                if session_file.exists() {
                    remove_file(session_file).with_context(|| {
                        format!("Failed to cleanup previous '{TEMP_SESSION_NAME}' session")
                    })?;
                }
                session = Some(Session::new(self, TEMP_SESSION_NAME));
            }
            Some(name) => {
                let session_path = self.session_file(name);
                if !session_path.exists() {
                    session = Some(Session::new(self, name));
                } else {
                    session = Some(Session::load(self, name, &session_path)?);
                }
            }
        }
        let mut new_session = false;
        if let Some(session) = session.as_mut() {
            let mcp_servers = if self.mcp_server_support {
                session.enabled_mcp_servers()
            } else {
                eprintln!(
                    "{}",
                    formatdoc!(
                        "
												This session uses MCP servers, but MCP support is disabled.
												To enable it, exit the session and set 'mcp_server_support: true', then try again
												"
                    )
                );
                None
            };
            self.functions.clear_mcp_meta_functions();
            let registry = self
                .mcp_registry
                .take()
                .with_context(|| "MCP registry should be populated")?;
            let new_mcp_registry =
                McpRegistry::reinit(registry, mcp_servers, abort_signal.clone()).await?;

            if !new_mcp_registry.is_empty() {
                self.functions
                    .append_mcp_meta_functions(new_mcp_registry.list_started_servers());
            }

            self.mcp_registry = Some(new_mcp_registry);
            if session.is_empty() {
                new_session = true;
                if let Some(LastMessage {
                    input,
                    output,
                    continuous,
                }) = &self.last_message
                    && (*continuous && !output.is_empty())
                    && self.agent.is_some() == input.with_agent()
                {
                    let ans = Confirm::new(
                        "Start a session that incorporates the last question and answer?",
                    )
                    .with_default(false)
                    .prompt()?;
                    if ans {
                        session.add_message(input, output)?;
                    }
                }
            }
        }
        self.session = session;
        self.init_agent_session_variables(new_session)?;
        Ok(())
    }

    pub fn session_info(&self) -> Result<String> {
        if let Some(session) = &self.session {
            let render_options = self.render_options()?;
            let mut markdown_render = MarkdownRender::init(render_options)?;
            let agent_info: Option<(String, Vec<String>)> = self.agent.as_ref().map(|agent| {
                let functions = agent
                    .functions()
                    .declarations()
                    .iter()
                    .filter_map(|v| if v.agent { Some(v.name.clone()) } else { None })
                    .collect();
                (agent.name().to_string(), functions)
            });
            session.render(&mut markdown_render, &agent_info)
        } else {
            bail!("No session")
        }
    }

    pub fn exit_session(&mut self) -> Result<()> {
        if let Some(mut session) = self.session.take() {
            let sessions_dir = self.sessions_dir();
            session.exit(&sessions_dir, self.working_mode.is_repl())?;
            self.discontinuous_last_message();
        }
        Ok(())
    }

    pub fn save_session(&mut self, name: Option<&str>) -> Result<()> {
        let session_name = match &self.session {
            Some(session) => match name {
                Some(v) => v.to_string(),
                None => session
                    .autoname()
                    .unwrap_or_else(|| session.name())
                    .to_string(),
            },
            None => bail!("No session"),
        };
        let session_path = self.session_file(&session_name);
        if let Some(session) = self.session.as_mut() {
            session.save(&session_name, &session_path, self.working_mode.is_repl())?;
        }
        Ok(())
    }

    pub fn edit_session(&mut self) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.name().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);
        self.save_session(Some(&name))?;
        let editor = self.editor()?;
        edit_file(&editor, &session_path).with_context(|| {
            format!(
                "Failed to edit '{}' with '{editor}'",
                session_path.display()
            )
        })?;
        self.session = Some(Session::load(self, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn empty_session(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            if let Some(agent) = self.agent.as_ref() {
                session.sync_agent(agent);
            }
            session.clear_messages();
        } else {
            bail!("No session")
        }
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn set_save_session_this_time(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.set_save_session_this_time();
        } else {
            bail!("No session")
        }
        Ok(())
    }

    pub fn list_sessions(&self) -> Vec<String> {
        list_file_names(self.sessions_dir(), ".yaml")
    }

    pub fn list_autoname_sessions(&self) -> Vec<String> {
        list_file_names(self.sessions_dir().join("_"), ".yaml")
    }

    pub fn maybe_compress_session(config: GlobalConfig) {
        let mut needs_compression = false;
        {
            let mut config = config.write();
            let compression_threshold = config.compression_threshold;
            if let Some(session) = config.session.as_mut()
                && session.needs_compression(compression_threshold)
            {
                session.set_compressing(true);
                needs_compression = true;
            }
        };
        if !needs_compression {
            return;
        }
        let color = if config.read().light_theme() {
            nu_ansi_term::Color::LightGray
        } else {
            nu_ansi_term::Color::DarkGray
        };
        print!(
            "\n📢 {}\n",
            color.italic().paint("Compressing the session."),
        );
        tokio::spawn(async move {
            if let Err(err) = Config::compress_session(&config).await {
                warn!("Failed to compress the session: {err}");
            }
            if let Some(session) = config.write().session.as_mut() {
                session.set_compressing(false);
            }
        });
    }

    pub async fn compress_session(config: &GlobalConfig) -> Result<()> {
        match config.read().session.as_ref() {
            Some(session) => {
                if !session.has_user_messages() {
                    bail!("No need to compress since there are no messages in the session")
                }
            }
            None => bail!("No session"),
        }

        let prompt = config
            .read()
            .summarization_prompt
            .clone()
            .unwrap_or_else(|| SUMMARIZATION_PROMPT.into());
        let input = Input::from_str(config, &prompt, None);
        let summary = input.fetch_chat_text().await?;
        let summary_context_prompt = config
            .read()
            .summary_context_prompt
            .clone()
            .unwrap_or_else(|| SUMMARY_CONTEXT_PROMPT.into());

        let todo_prefix = config
            .read()
            .agent
            .as_ref()
            .map(|agent| agent.todo_list())
            .filter(|todos| !todos.is_empty())
            .map(|todos| format!("[ACTIVE TODO LIST]\n{}\n\n", todos.render_for_model()))
            .unwrap_or_default();

        if let Some(session) = config.write().session.as_mut() {
            session.compress(format!("{todo_prefix}{summary_context_prompt}{summary}"));
        }
        config.write().discontinuous_last_message();
        Ok(())
    }

    pub fn is_compressing_session(&self) -> bool {
        self.session
            .as_ref()
            .map(|v| v.compressing())
            .unwrap_or_default()
    }

    pub fn maybe_autoname_session(config: GlobalConfig) {
        let mut need_autoname = false;
        if let Some(session) = config.write().session.as_mut()
            && session.need_autoname()
        {
            session.set_autonaming(true);
            need_autoname = true;
        }
        if !need_autoname {
            return;
        }
        let color = if config.read().light_theme() {
            nu_ansi_term::Color::LightGray
        } else {
            nu_ansi_term::Color::DarkGray
        };
        print!("\n📢 {}\n", color.italic().paint("Autonaming the session."),);
        tokio::spawn(async move {
            if let Err(err) = Config::autoname_session(&config).await {
                warn!("Failed to autonaming the session: {err}");
            }
            if let Some(session) = config.write().session.as_mut() {
                session.set_autonaming(false);
            }
        });
    }

    pub async fn autoname_session(config: &GlobalConfig) -> Result<()> {
        let text = match config
            .read()
            .session
            .as_ref()
            .and_then(|v| v.chat_history_for_autonaming())
        {
            Some(v) => v,
            None => bail!("No chat history"),
        };
        let role = config.read().retrieve_role(CREATE_TITLE_ROLE)?;
        let input = Input::from_str(config, &text, Some(role));
        let text = input.fetch_chat_text().await?;
        if let Some(session) = config.write().session.as_mut() {
            session.set_autoname(&text);
        }
        Ok(())
    }

    pub async fn use_rag(
        config: &GlobalConfig,
        rag: Option<&str>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        if config.read().agent.is_some() {
            bail!("Cannot perform this operation because you are using a agent")
        }
        let rag = match rag {
            None => {
                let rag_path = config.read().rag_file(TEMP_RAG_NAME);
                if rag_path.exists() {
                    remove_file(&rag_path).with_context(|| {
                        format!("Failed to cleanup previous '{TEMP_RAG_NAME}' rag")
                    })?;
                }
                Rag::init(config, TEMP_RAG_NAME, &rag_path, &[], abort_signal).await?
            }
            Some(name) => {
                let rag_path = config.read().rag_file(name);
                if !rag_path.exists() {
                    if config.read().working_mode.is_cmd() {
                        bail!("Unknown RAG '{name}'")
                    }
                    Rag::init(config, name, &rag_path, &[], abort_signal).await?
                } else {
                    Rag::load(config, name, &rag_path)?
                }
            }
        };
        config.write().rag = Some(Arc::new(rag));
        Ok(())
    }

    pub async fn edit_rag_docs(config: &GlobalConfig, abort_signal: AbortSignal) -> Result<()> {
        let mut rag = match config.read().rag.clone() {
            Some(v) => v.as_ref().clone(),
            None => bail!("No RAG"),
        };

        let document_paths = rag.document_paths();
        let temp_file = temp_file(&format!("-rag-{}", rag.name()), ".txt");
        tokio::fs::write(&temp_file, &document_paths.join("\n"))
            .await
            .with_context(|| format!("Failed to write to '{}'", temp_file.display()))?;
        let editor = config.read().editor()?;
        edit_file(&editor, &temp_file)?;
        let new_document_paths = tokio::fs::read_to_string(&temp_file)
            .await
            .with_context(|| format!("Failed to read '{}'", temp_file.display()))?;
        let new_document_paths = new_document_paths
            .split('\n')
            .filter_map(|v| {
                let v = v.trim();
                if v.is_empty() {
                    None
                } else {
                    Some(v.to_string())
                }
            })
            .collect::<Vec<_>>();
        if new_document_paths.is_empty() || new_document_paths == document_paths {
            bail!("No changes")
        }
        rag.refresh_document_paths(&new_document_paths, false, config, abort_signal)
            .await?;
        config.write().rag = Some(Arc::new(rag));
        Ok(())
    }

    pub async fn rebuild_rag(config: &GlobalConfig, abort_signal: AbortSignal) -> Result<()> {
        let mut rag = match config.read().rag.clone() {
            Some(v) => v.as_ref().clone(),
            None => bail!("No RAG"),
        };
        let document_paths = rag.document_paths().to_vec();
        rag.refresh_document_paths(&document_paths, true, config, abort_signal)
            .await?;
        config.write().rag = Some(Arc::new(rag));
        Ok(())
    }

    pub fn rag_sources(config: &GlobalConfig) -> Result<String> {
        match config.read().rag.as_ref() {
            Some(rag) => match rag.get_last_sources() {
                Some(v) => Ok(v),
                None => bail!("No sources"),
            },
            None => bail!("No RAG"),
        }
    }

    pub fn rag_info(&self) -> Result<String> {
        if let Some(rag) = &self.rag {
            rag.export()
        } else {
            bail!("No RAG")
        }
    }

    pub fn exit_rag(&mut self) -> Result<()> {
        self.rag.take();
        Ok(())
    }

    pub async fn search_rag(
        config: &GlobalConfig,
        rag: &Rag,
        text: &str,
        abort_signal: AbortSignal,
    ) -> Result<String> {
        let (reranker_model, top_k) = rag.get_config();
        let (embeddings, sources, ids) = rag
            .search(text, top_k, reranker_model.as_deref(), abort_signal)
            .await?;
        let text = config.read().rag_template(&embeddings, &sources, text);
        rag.set_last_sources(&ids);
        Ok(text)
    }

    pub fn list_rags() -> Vec<String> {
        match read_dir(Self::rags_dir()) {
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

    pub fn rag_template(&self, embeddings: &str, sources: &str, text: &str) -> String {
        if embeddings.is_empty() {
            return text.to_string();
        }
        self.rag_template
            .as_deref()
            .unwrap_or(RAG_TEMPLATE)
            .replace("__CONTEXT__", embeddings)
            .replace("__SOURCES__", sources)
            .replace("__INPUT__", text)
    }

    pub async fn use_agent(
        config: &GlobalConfig,
        agent_name: &str,
        session_name: Option<&str>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        if !config.read().function_calling_support {
            bail!("Please enable function calling support before using the agent.");
        }
        if config.read().agent.is_some() {
            bail!("Already in an agent, please run '.exit agent' first to exit the current agent.");
        }
        let agent = Agent::init(config, agent_name, abort_signal.clone()).await?;
        let session = session_name.map(|v| v.to_string()).or_else(|| {
            if config.read().macro_flag {
                None
            } else {
                agent.agent_session().map(|v| v.to_string())
            }
        });
        let should_init_supervisor = agent.can_spawn_agents();
        let max_concurrent = agent.max_concurrent_agents();
        let max_depth = agent.max_agent_depth();
        config.write().rag = agent.rag();
        config.write().agent = Some(agent);
        if should_init_supervisor {
            config.write().supervisor = Some(Arc::new(RwLock::new(Supervisor::new(
                max_concurrent,
                max_depth,
            ))));
        }
        if let Some(session) = session {
            Config::use_session_safely(config, Some(&session), abort_signal).await?;
        } else {
            config.write().init_agent_shared_variables()?;
        }
        Ok(())
    }

    pub fn agent_info(&self) -> Result<String> {
        if let Some(agent) = &self.agent {
            agent.export()
        } else {
            bail!("No agent")
        }
    }

    pub fn agent_banner(&self) -> Result<String> {
        if let Some(agent) = &self.agent {
            Ok(agent.banner())
        } else {
            bail!("No agent")
        }
    }

    pub fn edit_agent_config(&self) -> Result<()> {
        let agent_name = match &self.agent {
            Some(agent) => agent.name(),
            None => bail!("No agent"),
        };
        let agent_config_path = Config::agent_config_file(agent_name);
        ensure_parent_exists(&agent_config_path)?;
        if !agent_config_path.exists() {
            std::fs::write(
                &agent_config_path,
                "# see https://github.com/Dark-Alex-17/loki/blob/main/config.agent.example.yaml\n",
            )
            .with_context(|| format!("Failed to write to '{}'", agent_config_path.display()))?;
        }
        let editor = self.editor()?;
        edit_file(&editor, &agent_config_path)?;
        println!(
            "NOTE: Remember to reload the agent if there are changes made to '{}'",
            agent_config_path.display()
        );
        Ok(())
    }

    pub fn exit_agent(&mut self) -> Result<()> {
        self.exit_session()?;
        self.load_functions()?;
        if self.agent.take().is_some() {
            self.rag.take();
            self.discontinuous_last_message();
        }
        Ok(())
    }

    pub fn exit_agent_session(&mut self) -> Result<()> {
        self.exit_session()?;
        if let Some(agent) = self.agent.as_mut() {
            agent.exit_session();
            if self.working_mode.is_repl() {
                self.init_agent_shared_variables()?;
            }
        }
        Ok(())
    }

    pub fn list_macros() -> Vec<String> {
        list_file_names(Self::macros_dir(), ".yaml")
    }

    pub fn load_macro(name: &str) -> Result<Macro> {
        let path = Self::macro_file(name);
        let err = || format!("Failed to load macro '{name}' at '{}'", path.display());
        let content = read_to_string(&path).with_context(err)?;
        let value: Macro = serde_yaml::from_str(&content).with_context(err)?;
        Ok(value)
    }

    pub fn has_macro(name: &str) -> bool {
        let names = Self::list_macros();
        names.contains(&name.to_string())
    }

    pub fn new_macro(&mut self, name: &str) -> Result<()> {
        if self.macro_flag {
            bail!("No macro");
        }
        let ans = Confirm::new("Create a new macro?")
            .with_default(true)
            .prompt()?;
        if ans {
            let macro_path = Self::macro_file(name);
            ensure_parent_exists(&macro_path)?;
            let editor = self.editor()?;
            edit_file(&editor, &macro_path)?;
        } else {
            bail!("No macro");
        }
        Ok(())
    }

    pub async fn apply_prelude(&mut self, abort_signal: AbortSignal) -> Result<()> {
        if self.macro_flag || !self.state().is_empty() {
            return Ok(());
        }
        let prelude = match self.working_mode {
            WorkingMode::Repl => self.repl_prelude.as_ref(),
            WorkingMode::Cmd => self.cmd_prelude.as_ref(),
        };
        let prelude = match prelude {
            Some(v) => {
                if v.is_empty() {
                    return Ok(());
                }
                v.to_string()
            }
            None => return Ok(()),
        };

        let err_msg = || format!("Invalid prelude '{prelude}");
        match prelude.split_once(':') {
            Some(("role", name)) => {
                self.use_role(name, abort_signal)
                    .await
                    .with_context(err_msg)?;
            }
            Some(("session", name)) => {
                self.use_session(Some(name), abort_signal)
                    .await
                    .with_context(err_msg)?;
            }
            Some((session_name, role_name)) => {
                self.use_session(Some(session_name), abort_signal.clone())
                    .await
                    .with_context(err_msg)?;
                if let Some(true) = self.session.as_ref().map(|v| v.is_empty()) {
                    self.use_role(role_name, abort_signal)
                        .await
                        .with_context(err_msg)?;
                }
            }
            _ => {
                bail!("{}", err_msg())
            }
        }
        Ok(())
    }

    pub fn select_functions(&self, role: &Role) -> Option<Vec<FunctionDeclaration>> {
        let mut functions = vec![];
        functions.extend(self.select_enabled_functions(role));
        functions.extend(self.select_enabled_mcp_servers(role));

        if functions.is_empty() {
            None
        } else {
            Some(functions)
        }
    }

    fn select_enabled_functions(&self, role: &Role) -> Vec<FunctionDeclaration> {
        let mut functions = vec![];
        if self.function_calling_support {
            if let Some(enabled_tools) = role.enabled_tools() {
                let mut tool_names: HashSet<String> = Default::default();
                let declaration_names: HashSet<String> = self
                    .functions
                    .declarations()
                    .iter()
                    .filter(|v| {
                        !v.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                            && !v.name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX)
                            && !v.name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX)
                    })
                    .map(|v| v.name.to_string())
                    .collect();
                if enabled_tools == "all" {
                    tool_names.extend(declaration_names);
                } else {
                    for item in enabled_tools.split(',') {
                        let item = item.trim();
                        if let Some(values) = self.mapping_tools.get(item) {
                            tool_names.extend(
                                values
                                    .split(',')
                                    .map(|v| v.to_string())
                                    .filter(|v| declaration_names.contains(v)),
                            )
                        } else if declaration_names.contains(item) {
                            tool_names.insert(item.to_string());
                        }
                    }
                }
                functions = self
                    .functions
                    .declarations()
                    .iter()
                    .filter_map(|v| {
                        if tool_names.contains(&v.name) {
                            Some(v.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
            }

            if let Some(agent) = &self.agent {
                let mut agent_functions: Vec<FunctionDeclaration> = agent
                    .functions()
                    .declarations()
                    .to_vec()
                    .into_iter()
                    .filter(|v| {
                        !v.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                            && !v.name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX)
                            && !v.name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX)
                    })
                    .collect();
                let tool_names: HashSet<String> = agent_functions
                    .iter()
                    .filter_map(|v| {
                        if v.agent {
                            None
                        } else {
                            Some(v.name.to_string())
                        }
                    })
                    .collect();
                agent_functions.extend(
                    functions
                        .into_iter()
                        .filter(|v| !tool_names.contains(&v.name)),
                );
                functions = agent_functions;
            }
        }

        functions
    }

    fn select_enabled_mcp_servers(&self, role: &Role) -> Vec<FunctionDeclaration> {
        let mut mcp_functions = vec![];
        if self.mcp_server_support {
            if let Some(enabled_mcp_servers) = role.enabled_mcp_servers() {
                let mut server_names: HashSet<String> = Default::default();
                let mcp_declaration_names: HashSet<String> = self
                    .functions
                    .declarations()
                    .iter()
                    .filter(|v| {
                        v.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                            || v.name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX)
                            || v.name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX)
                    })
                    .map(|v| v.name.to_string())
                    .collect();
                if enabled_mcp_servers == "all" {
                    server_names.extend(mcp_declaration_names);
                } else {
                    for item in enabled_mcp_servers.split(',') {
                        let item = item.trim();
                        let item_invoke_name =
                            format!("{}_{item}", MCP_INVOKE_META_FUNCTION_NAME_PREFIX);
                        let item_search_name =
                            format!("{}_{item}", MCP_SEARCH_META_FUNCTION_NAME_PREFIX);
                        let item_describe_name =
                            format!("{}_{item}", MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX);
                        if let Some(values) = self.mapping_mcp_servers.get(item) {
                            server_names.extend(
                                values
                                    .split(',')
                                    .flat_map(|v| {
                                        vec![
                                            format!(
                                                "{}_{}",
                                                MCP_INVOKE_META_FUNCTION_NAME_PREFIX,
                                                v.to_string()
                                            ),
                                            format!(
                                                "{}_{}",
                                                MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
                                                v.to_string()
                                            ),
                                            format!(
                                                "{}_{}",
                                                MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX,
                                                v.to_string()
                                            ),
                                        ]
                                    })
                                    .filter(|v| mcp_declaration_names.contains(v)),
                            )
                        } else if mcp_declaration_names.contains(&item_invoke_name) {
                            server_names.insert(item_invoke_name);
                            server_names.insert(item_search_name);
                            server_names.insert(item_describe_name);
                        }
                    }
                }
                mcp_functions = self
                    .functions
                    .declarations()
                    .iter()
                    .filter_map(|v| {
                        if server_names.contains(&v.name) {
                            Some(v.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
            }

            if let Some(agent) = &self.agent {
                let mut agent_functions: Vec<FunctionDeclaration> = agent
                    .functions()
                    .declarations()
                    .to_vec()
                    .into_iter()
                    .filter(|v| {
                        v.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                            || v.name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX)
                            || v.name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX)
                    })
                    .collect();
                let tool_names: HashSet<String> = agent_functions
                    .iter()
                    .filter_map(|v| {
                        if v.agent {
                            None
                        } else {
                            Some(v.name.to_string())
                        }
                    })
                    .collect();
                agent_functions.extend(
                    mcp_functions
                        .into_iter()
                        .filter(|v| !tool_names.contains(&v.name)),
                );
                mcp_functions = agent_functions;
            }
        }

        mcp_functions
    }

    pub fn editor(&self) -> Result<String> {
        EDITOR.get_or_init(move || {
			let editor = self.editor.clone()
				.or_else(|| env::var("VISUAL").ok().or_else(|| env::var("EDITOR").ok()))
				.unwrap_or_else(|| {
					if cfg!(windows) {
						"notepad".to_string()
					} else {
						"nano".to_string()
					}
				});
			which::which(&editor).ok().map(|_| editor)
		})
			.clone()
			.ok_or_else(|| anyhow!("Editor not found. Please add the `editor` configuration or set the $EDITOR or $VISUAL environment variable."))
    }

    pub fn repl_complete(
        &self,
        cmd: &str,
        args: &[&str],
        _line: &str,
    ) -> Vec<(String, Option<String>)> {
        let mut values: Vec<(String, Option<String>)> = vec![];
        let filter = args.last().unwrap_or(&"");
        if args.len() == 1 {
            values = match cmd {
                ".role" => map_completion_values(Self::list_roles(true)),
                ".model" => list_models(self, ModelType::Chat)
                    .into_iter()
                    .map(|v| (v.id(), Some(v.description())))
                    .collect(),
                ".session" => {
                    if args[0].starts_with("_/") {
                        map_completion_values(
                            self.list_autoname_sessions()
                                .iter()
                                .rev()
                                .map(|v| format!("_/{v}"))
                                .collect::<Vec<String>>(),
                        )
                    } else {
                        map_completion_values(self.list_sessions())
                    }
                }
                ".rag" => map_completion_values(Self::list_rags()),
                ".agent" => map_completion_values(list_agents()),
                ".macro" => map_completion_values(Self::list_macros()),
                ".starter" => match &self.agent {
                    Some(agent) => agent
                        .conversation_starters()
                        .iter()
                        .enumerate()
                        .map(|(i, v)| ((i + 1).to_string(), Some(v.to_string())))
                        .collect(),
                    None => vec![],
                },
                ".set" => {
                    let mut values = vec![
                        "temperature",
                        "top_p",
                        "enabled_tools",
                        "enabled_mcp_servers",
                        "save_session",
                        "compression_threshold",
                        "rag_reranker_model",
                        "rag_top_k",
                        "max_output_tokens",
                        "dry_run",
                        "function_calling_support",
                        "mcp_server_support",
                        "stream",
                        "save",
                        "highlight",
                    ];
                    values.sort_unstable();
                    values
                        .into_iter()
                        .map(|v| (format!("{v} "), None))
                        .collect()
                }
                ".delete" => {
                    map_completion_values(vec!["role", "session", "rag", "macro", "agent-data"])
                }
                ".vault" => {
                    let mut values = vec!["add", "get", "update", "delete", "list"];
                    values.sort_unstable();
                    values
                        .into_iter()
                        .map(|v| (format!("{v} "), None))
                        .collect()
                }
                _ => vec![],
            };
        } else if cmd == ".set" && args.len() == 2 {
            let candidates = match args[0] {
                "max_output_tokens" => match self.current_model().max_output_tokens() {
                    Some(v) => vec![v.to_string()],
                    None => vec![],
                },
                "dry_run" => complete_bool(self.dry_run),
                "stream" => complete_bool(self.stream),
                "save" => complete_bool(self.save),
                "function_calling_support" => complete_bool(self.function_calling_support),
                "enabled_tools" => {
                    let mut prefix = String::new();
                    let mut ignores = HashSet::new();
                    if let Some((v, _)) = args[1].rsplit_once(',') {
                        ignores = v.split(',').collect();
                        prefix = format!("{v},");
                    }
                    let mut values = vec![];
                    if prefix.is_empty() {
                        values.push("all".to_string());
                    }
                    values.extend(self.functions.declarations().iter().map(|v| v.name.clone()));
                    values.extend(self.mapping_tools.keys().map(|v| v.to_string()));
                    values
                        .into_iter()
                        .filter(|v| !ignores.contains(v.as_str()))
                        .map(|v| format!("{prefix}{v}"))
                        .collect()
                }
                "mcp_server_support" => complete_bool(self.mcp_server_support),
                "enabled_mcp_servers" => {
                    let mut prefix = String::new();
                    let mut ignores = HashSet::new();
                    if let Some((v, _)) = args[1].rsplit_once(',') {
                        ignores = v.split(',').collect();
                        prefix = format!("{v},");
                    }
                    let mut values = vec![];
                    if prefix.is_empty() {
                        values.push("all".to_string());
                    }

                    if let Some(registry) = &self.mcp_registry {
                        values.extend(registry.list_configured_servers());
                    }

                    values.extend(self.mapping_mcp_servers.keys().map(|v| v.to_string()));
                    values
                        .into_iter()
                        .filter(|v| !ignores.contains(v.as_str()))
                        .map(|v| format!("{prefix}{v}"))
                        .collect()
                }
                "save_session" => {
                    let save_session = if let Some(session) = &self.session {
                        session.save_session()
                    } else {
                        self.save_session
                    };
                    complete_option_bool(save_session)
                }
                "rag_reranker_model" => list_models(self, ModelType::Reranker)
                    .iter()
                    .map(|v| v.id())
                    .collect(),
                "highlight" => complete_bool(self.highlight),
                _ => vec![],
            };
            values = candidates.into_iter().map(|v| (v, None)).collect();
        } else if cmd == ".vault" && args.len() == 2 {
            values = self
                .vault
                .list_secrets(false)
                .unwrap_or_default()
                .into_iter()
                .map(|v| (v, None))
                .collect();
        } else if cmd == ".agent" {
            if args.len() == 2 {
                let dir = Self::agent_data_dir(args[0]).join(SESSIONS_DIR_NAME);
                values = list_file_names(dir, ".yaml")
                    .into_iter()
                    .map(|v| (v, None))
                    .collect();
            }
            values.extend(complete_agent_variables(args[0]));
        };
        fuzzy_filter(values, |v| v.0.as_str(), filter)
    }

    pub fn sync_models_url(&self) -> String {
        self.sync_models_url
            .clone()
            .unwrap_or_else(|| SYNC_MODELS_URL.into())
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

        let model_override_path = Self::models_override_file();
        ensure_parent_exists(&model_override_path)?;
        std::fs::write(&model_override_path, models_override_data)
            .with_context(|| format!("Failed to write to '{}'", model_override_path.display()))?;
        println!("✓ Updated '{}'", model_override_path.display());
        Ok(())
    }

    pub fn local_models_override() -> Result<Vec<ProviderModels>> {
        let model_override_path = Self::models_override_file();
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

    pub fn light_theme(&self) -> bool {
        matches!(self.theme.as_deref(), Some("light"))
    }

    pub fn render_options(&self) -> Result<RenderOptions> {
        let theme = if self.highlight {
            let theme_mode = if self.light_theme() { "light" } else { "dark" };
            let theme_filename = format!("{theme_mode}.tmTheme");
            let theme_path = Self::local_path(&theme_filename);
            if theme_path.exists() {
                let theme = ThemeSet::get_theme(&theme_path)
                    .with_context(|| format!("Invalid theme at '{}'", theme_path.display()))?;
                Some(theme)
            } else {
                let theme = if self.light_theme() {
                    decode_bin(LIGHT_THEME).context("Invalid builtin light theme")?
                } else {
                    decode_bin(DARK_THEME).context("Invalid builtin dark theme")?
                };
                Some(theme)
            }
        } else {
            None
        };
        let wrap = if *IS_STDOUT_TERMINAL {
            self.wrap.clone()
        } else {
            None
        };
        let truecolor = matches!(
            env::var("COLORTERM").as_ref().map(|v| v.as_str()),
            Ok("truecolor")
        );
        Ok(RenderOptions::new(theme, wrap, self.wrap_code, truecolor))
    }

    pub fn render_prompt_left(&self) -> String {
        let variables = self.generate_prompt_context();
        let left_prompt = self.left_prompt.as_deref().unwrap_or(LEFT_PROMPT);
        render_prompt(left_prompt, &variables)
    }

    pub fn render_prompt_right(&self) -> String {
        let variables = self.generate_prompt_context();
        let right_prompt = self.right_prompt.as_deref().unwrap_or(RIGHT_PROMPT);
        render_prompt(right_prompt, &variables)
    }

    pub fn print_markdown(&self, text: &str) -> Result<()> {
        if *IS_STDOUT_TERMINAL {
            let render_options = self.render_options()?;
            let mut markdown_render = MarkdownRender::init(render_options)?;
            println!("{}", markdown_render.render(text));
        } else {
            println!("{text}");
        }
        Ok(())
    }

    fn generate_prompt_context(&self) -> HashMap<&str, String> {
        let mut output = HashMap::new();
        let role = self.extract_role();
        output.insert("model", role.model().id());
        output.insert("client_name", role.model().client_name().to_string());
        output.insert("model_name", role.model().name().to_string());
        output.insert(
            "max_input_tokens",
            role.model()
                .max_input_tokens()
                .unwrap_or_default()
                .to_string(),
        );
        if let Some(temperature) = role.temperature()
            && temperature != 0.0
        {
            output.insert("temperature", temperature.to_string());
        }
        if let Some(top_p) = role.top_p()
            && top_p != 0.0
        {
            output.insert("top_p", top_p.to_string());
        }
        if self.dry_run {
            output.insert("dry_run", "true".to_string());
        }
        if self.stream {
            output.insert("stream", "true".to_string());
        }
        if self.save {
            output.insert("save", "true".to_string());
        }
        if let Some(wrap) = &self.wrap
            && wrap != "no"
        {
            output.insert("wrap", wrap.clone());
        }
        if !role.is_derived() {
            output.insert("role", role.name().to_string());
        }
        if let Some(session) = &self.session {
            output.insert("session", session.name().to_string());
            if let Some(autoname) = session.autoname() {
                output.insert("session_autoname", autoname.to_string());
            }
            output.insert("dirty", session.dirty().to_string());
            let (tokens, percent) = session.tokens_usage();
            output.insert("consume_tokens", tokens.to_string());
            output.insert("consume_percent", percent.to_string());
            output.insert("user_messages_len", session.user_messages_len().to_string());
        }
        if let Some(rag) = &self.rag {
            output.insert("rag", rag.name().to_string());
        }
        if let Some(agent) = &self.agent {
            output.insert("agent", agent.name().to_string());
        }

        if self.highlight {
            output.insert("color.reset", "\u{1b}[0m".to_string());
            output.insert("color.black", "\u{1b}[30m".to_string());
            output.insert("color.dark_gray", "\u{1b}[90m".to_string());
            output.insert("color.red", "\u{1b}[31m".to_string());
            output.insert("color.light_red", "\u{1b}[91m".to_string());
            output.insert("color.green", "\u{1b}[32m".to_string());
            output.insert("color.light_green", "\u{1b}[92m".to_string());
            output.insert("color.yellow", "\u{1b}[33m".to_string());
            output.insert("color.light_yellow", "\u{1b}[93m".to_string());
            output.insert("color.blue", "\u{1b}[34m".to_string());
            output.insert("color.light_blue", "\u{1b}[94m".to_string());
            output.insert("color.purple", "\u{1b}[35m".to_string());
            output.insert("color.light_purple", "\u{1b}[95m".to_string());
            output.insert("color.magenta", "\u{1b}[35m".to_string());
            output.insert("color.light_magenta", "\u{1b}[95m".to_string());
            output.insert("color.cyan", "\u{1b}[36m".to_string());
            output.insert("color.light_cyan", "\u{1b}[96m".to_string());
            output.insert("color.white", "\u{1b}[37m".to_string());
            output.insert("color.light_gray", "\u{1b}[97m".to_string());
        }

        output
    }

    pub fn before_chat_completion(&mut self, input: &Input) -> Result<()> {
        self.last_message = Some(LastMessage::new(input.clone(), String::new()));
        Ok(())
    }

    pub fn after_chat_completion(
        &mut self,
        input: &Input,
        output: &str,
        tool_results: &[ToolResult],
    ) -> Result<()> {
        if !tool_results.is_empty() {
            return Ok(());
        }
        self.last_message = Some(LastMessage::new(input.clone(), output.to_string()));
        if !self.dry_run {
            self.save_message(input, output)?;
        }
        Ok(())
    }

    fn discontinuous_last_message(&mut self) {
        if let Some(last_message) = self.last_message.as_mut() {
            last_message.continuous = false;
        }
    }

    fn save_message(&mut self, input: &Input, output: &str) -> Result<()> {
        let mut input = input.clone();
        input.clear_patch();
        if let Some(session) = input.session_mut(&mut self.session) {
            session.add_message(&input, output)?;
            return Ok(());
        }

        if !self.save {
            return Ok(());
        }
        let mut file = self.open_message_file()?;
        if output.is_empty() && input.tool_calls().is_none() {
            return Ok(());
        }
        let now = now();
        let summary = input.summary();
        let raw_input = input.raw();
        let scope = if self.agent.is_none() {
            let role_name = if input.role().is_derived() {
                None
            } else {
                Some(input.role().name())
            };
            match (role_name, input.rag_name()) {
                (Some(role), Some(rag_name)) => format!(" ({role}#{rag_name})"),
                (Some(role), _) => format!(" ({role})"),
                (None, Some(rag_name)) => format!(" (#{rag_name})"),
                _ => String::new(),
            }
        } else {
            String::new()
        };
        let tool_calls = match input.tool_calls() {
            Some(MessageContentToolCalls {
                tool_results, text, ..
            }) => {
                let mut lines = vec!["<tool_calls>".to_string()];
                if !text.is_empty() {
                    lines.push(text.clone());
                }
                lines.push(serde_json::to_string(&tool_results).unwrap_or_default());
                lines.push("</tool_calls>\n".to_string());
                lines.join("\n")
            }
            None => String::new(),
        };
        let output = format!(
            "# CHAT: {summary} [{now}]{scope}\n{raw_input}\n--------\n{tool_calls}{output}\n--------\n\n",
        );
        file.write_all(output.as_bytes())
            .with_context(|| "Failed to save message")
    }

    fn init_agent_shared_variables(&mut self) -> Result<()> {
        let agent = match self.agent.as_mut() {
            Some(v) => v,
            None => return Ok(()),
        };
        if !agent.defined_variables().is_empty() && agent.shared_variables().is_empty() {
            let new_variables = Agent::init_agent_variables(
                agent.defined_variables(),
                self.agent_variables.as_ref(),
                self.info_flag,
            )?;
            agent.set_shared_variables(new_variables);
        }
        if !self.info_flag {
            agent.update_shared_dynamic_instructions(false)?;
        }
        Ok(())
    }

    fn init_agent_session_variables(&mut self, new_session: bool) -> Result<()> {
        let (agent, session) = match (self.agent.as_mut(), self.session.as_mut()) {
            (Some(agent), Some(session)) => (agent, session),
            _ => return Ok(()),
        };
        if new_session {
            let shared_variables = agent.shared_variables().clone();
            let session_variables =
                if !agent.defined_variables().is_empty() && shared_variables.is_empty() {
                    let new_variables = Agent::init_agent_variables(
                        agent.defined_variables(),
                        self.agent_variables.as_ref(),
                        self.info_flag,
                    )?;
                    agent.set_shared_variables(new_variables.clone());
                    new_variables
                } else {
                    shared_variables
                };
            agent.set_session_variables(session_variables);
            if !self.info_flag {
                agent.update_session_dynamic_instructions(None)?;
            }
            session.sync_agent(agent);
        } else {
            let variables = session.agent_variables();
            agent.set_session_variables(variables.clone());
            agent.update_session_dynamic_instructions(Some(
                session.agent_instructions().to_string(),
            ))?;
        }
        Ok(())
    }

    fn open_message_file(&self) -> Result<File> {
        let path = self.messages_file();
        ensure_parent_exists(&path)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to create/append {}", path.display()))
    }

    fn load_from_file(config_path: &Path) -> Result<(Self, String)> {
        let err = || format!("Failed to load config at '{}'", config_path.display());
        let content = read_to_string(config_path).with_context(err)?;
        let config = Self::load_from_str(&content).with_context(err)?;

        Ok((config, content))
    }

    fn load_from_str(content: &str) -> Result<Self> {
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

    fn load_dynamic(model_id: &str) -> Result<Self> {
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

    fn load_envs(&mut self) {
        if let Ok(v) = env::var(get_env_name("model")) {
            self.model_id = v;
        }
        if let Some(v) = read_env_value::<f64>(&get_env_name("temperature")) {
            self.temperature = v;
        }
        if let Some(v) = read_env_value::<f64>(&get_env_name("top_p")) {
            self.top_p = v;
        }

        if let Some(Some(v)) = read_env_bool(&get_env_name("dry_run")) {
            self.dry_run = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("stream")) {
            self.stream = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("save")) {
            self.save = v;
        }
        if let Ok(v) = env::var(get_env_name("keybindings"))
            && v == "vi"
        {
            self.keybindings = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("editor")) {
            self.editor = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("wrap")) {
            self.wrap = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("wrap_code")) {
            self.wrap_code = v;
        }

        if let Some(Some(v)) = read_env_bool(&get_env_name("function_calling_support")) {
            self.function_calling_support = v;
        }
        if let Ok(v) = env::var(get_env_name("mapping_tools"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.mapping_tools = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("enabled_tools")) {
            self.enabled_tools = v;
        }

        if let Some(Some(v)) = read_env_bool(&get_env_name("mcp_server_support")) {
            self.mcp_server_support = v;
        }
        if let Ok(v) = env::var(get_env_name("mapping_mcp_servers"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.mapping_mcp_servers = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("enabled_mcp_servers")) {
            self.enabled_mcp_servers = v;
        }

        if let Some(v) = read_env_value::<String>(&get_env_name("repl_prelude")) {
            self.repl_prelude = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("cmd_prelude")) {
            self.cmd_prelude = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("agent_session")) {
            self.agent_session = v;
        }

        if let Some(v) = read_env_bool(&get_env_name("save_session")) {
            self.save_session = v;
        }
        if let Some(Some(v)) = read_env_value::<usize>(&get_env_name("compression_threshold")) {
            self.compression_threshold = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("summarization_prompt")) {
            self.summarization_prompt = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("summary_context_prompt")) {
            self.summary_context_prompt = v;
        }

        if let Some(v) = read_env_value::<String>(&get_env_name("rag_embedding_model")) {
            self.rag_embedding_model = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("rag_reranker_model")) {
            self.rag_reranker_model = v;
        }
        if let Some(Some(v)) = read_env_value::<usize>(&get_env_name("rag_top_k")) {
            self.rag_top_k = v;
        }
        if let Some(v) = read_env_value::<usize>(&get_env_name("rag_chunk_size")) {
            self.rag_chunk_size = v;
        }
        if let Some(v) = read_env_value::<usize>(&get_env_name("rag_chunk_overlap")) {
            self.rag_chunk_overlap = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("rag_template")) {
            self.rag_template = v;
        }

        if let Ok(v) = env::var(get_env_name("document_loaders"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.document_loaders = v;
        }

        if let Some(Some(v)) = read_env_bool(&get_env_name("highlight")) {
            self.highlight = v;
        }
        if *NO_COLOR {
            self.highlight = false;
        }
        if self.highlight && self.theme.is_none() {
            if let Some(v) = read_env_value::<String>(&get_env_name("theme")) {
                self.theme = v;
            } else if *IS_STDOUT_TERMINAL
                && let Ok(color_scheme) = color_scheme(QueryOptions::default())
            {
                let theme = match color_scheme {
                    ColorScheme::Dark => "dark",
                    ColorScheme::Light => "light",
                };
                self.theme = Some(theme.into());
            }
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("left_prompt")) {
            self.left_prompt = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("right_prompt")) {
            self.right_prompt = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("user_agent")) {
            self.user_agent = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("save_shell_history")) {
            self.save_shell_history = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("sync_models_url")) {
            self.sync_models_url = v;
        }
    }

    fn load_functions(&mut self) -> Result<()> {
        self.functions = Functions::init(self.visible_tools.as_ref().unwrap_or(&Vec::new()))?;
        Ok(())
    }

    async fn load_mcp_servers(
        &mut self,
        log_path: Option<PathBuf>,
        start_mcp_servers: bool,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let mcp_registry = McpRegistry::init(
            log_path,
            start_mcp_servers,
            self.enabled_mcp_servers.clone(),
            abort_signal.clone(),
            self,
        )
        .await?;
        match mcp_registry.is_empty() {
            false => {
                if self.mcp_server_support {
                    self.functions
                        .append_mcp_meta_functions(mcp_registry.list_started_servers());
                } else {
                    debug!(
                        "Skipping global MCP functions registration since 'mcp_server_support' was 'false'"
                    );
                }
            }
            _ => debug!(
                "Skipping global MCP functions registration since 'start_mcp_servers' was 'false'"
            ),
        }
        self.mcp_registry = Some(mcp_registry);

        Ok(())
    }

    fn setup_model(&mut self) -> Result<()> {
        let mut model_id = self.model_id.clone();
        if model_id.is_empty() {
            let models = list_models(self, ModelType::Chat);
            if models.is_empty() {
                bail!("No available model");
            }
            model_id = models[0].id()
        }
        self.set_model(&model_id)?;
        self.model_id = model_id;

        Ok(())
    }

    fn setup_document_loaders(&mut self) {
        [("pdf", "pdftotext $1 -"), ("docx", "pandoc --to plain $1")]
            .into_iter()
            .for_each(|(k, v)| {
                let (k, v) = (k.to_string(), v.to_string());
                self.document_loaders.entry(k).or_insert(v);
            });
    }

    fn setup_user_agent(&mut self) {
        if let Some("auto") = self.user_agent.as_deref() {
            self.user_agent = Some(format!(
                "{}/{}",
                env!("CARGO_CRATE_NAME"),
                env!("CARGO_PKG_VERSION")
            ));
        }
    }
}

pub fn load_env_file() -> Result<()> {
    let env_file_path = Config::env_file();
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

async fn create_config_file(config_path: &Path) -> Result<()> {
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

fn parse_value<T>(value: &str) -> Result<Option<T>>
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

fn complete_bool(value: bool) -> Vec<String> {
    vec![(!value).to_string()]
}

fn complete_option_bool(value: Option<bool>) -> Vec<String> {
    match value {
        Some(true) => vec!["false".to_string(), "null".to_string()],
        Some(false) => vec!["true".to_string(), "null".to_string()],
        None => vec!["true".to_string(), "false".to_string()],
    }
}

fn map_completion_values<T: ToString>(value: Vec<T>) -> Vec<(String, Option<String>)> {
    value.into_iter().map(|v| (v.to_string(), None)).collect()
}

fn update_rag<F>(config: &GlobalConfig, f: F) -> Result<()>
where
    F: FnOnce(&mut Rag) -> Result<()>,
{
    let mut rag = match config.read().rag.clone() {
        Some(v) => v.as_ref().clone(),
        None => bail!("No RAG"),
    };
    f(&mut rag)?;
    config.write().rag = Some(Arc::new(rag));
    Ok(())
}

fn format_option_value<T>(value: &Option<T>) -> String
where
    T: std::fmt::Display,
{
    match value {
        Some(value) => value.to_string(),
        None => "null".to_string(),
    }
}
