use super::todo::TodoList;
use super::*;

use crate::{
    client::Model,
    function::{Functions, run_llm_function},
};

use crate::vault::SECRET_RE;
use anyhow::{Context, Result};
use fancy_regex::Captures;
use inquire::{Text, validator::Validation};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use std::{ffi::OsStr, path::Path};

const DEFAULT_AGENT_NAME: &str = "rag";
const DEFAULT_TODO_INSTRUCTIONS: &str = "\
\n## Task Tracking\n\
You have built-in task tracking tools. Use them to track your progress:\n\
- `todo__init`: Initialize a todo list with a goal. Call this at the start of every multi-step task.\n\
- `todo__add`: Add individual tasks. Add all planned steps before starting work.\n\
- `todo__done`: Mark a task done by id. Call this immediately after completing each step.\n\
- `todo__list`: Show the current todo list.\n\
\n\
RULES:\n\
- Always create a todo list before starting work.\n\
- Mark each task done as soon as you finish it; do not batch.\n\
- If you stop with incomplete tasks, the system will automatically prompt you to continue.";

pub type AgentVariables = IndexMap<String, String>;

#[derive(Embed)]
#[folder = "assets/agents/"]
struct AgentAssets;

#[derive(Debug, Clone)]
pub struct Agent {
    name: String,
    config: AgentConfig,
    shared_variables: AgentVariables,
    session_variables: Option<AgentVariables>,
    shared_dynamic_instructions: Option<String>,
    session_dynamic_instructions: Option<String>,
    functions: Functions,
    rag: Option<Arc<Rag>>,
    model: Model,
    vault: GlobalVault,
    todo_list: TodoList,
    continuation_count: usize,
    last_continuation_response: Option<String>,
}

impl Agent {
    pub fn install_builtin_agents() -> Result<()> {
        info!(
            "Installing built-in agents in {}",
            Config::agents_data_dir().display()
        );

        for file in AgentAssets::iter() {
            debug!("Processing agent file: {}", file.as_ref());

            let embedded_file = AgentAssets::get(&file)
                .ok_or_else(|| anyhow!("Failed to load embedded agent file: {}", file.as_ref()))?;
            let content = unsafe { std::str::from_utf8_unchecked(&embedded_file.data) };
            let file_path = Config::agents_data_dir().join(file.as_ref());
            let file_extension = file_path
                .extension()
                .and_then(OsStr::to_str)
                .map(|s| s.to_lowercase());
            #[cfg_attr(not(unix), expect(unused))]
            let is_script = matches!(file_extension.as_deref(), Some("sh") | Some("py"));

            if file_path.exists() {
                debug!(
                    "Agent file already exists, skipping: {}",
                    file_path.display()
                );
                continue;
            }

            ensure_parent_exists(&file_path)?;
            info!("Creating agent file: {}", file_path.display());
            let mut agent_file = File::create(&file_path)?;
            agent_file.write_all(content.as_bytes())?;

            #[cfg(unix)]
            if is_script {
                use std::{fs, os::unix::fs::PermissionsExt};
                fs::set_permissions(&file_path, fs::Permissions::from_mode(0o755))?;
            }
        }

        Ok(())
    }

    pub async fn init(
        config: &GlobalConfig,
        name: &str,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let agent_data_dir = Config::agent_data_dir(name);
        let loaders = config.read().document_loaders.clone();
        let rag_path = Config::agent_rag_file(name, DEFAULT_AGENT_NAME);
        let config_path = Config::agent_config_file(name);
        let mut agent_config = if config_path.exists() {
            AgentConfig::load(&config_path)?
        } else {
            bail!("Agent config file not found at '{}'", config_path.display())
        };
        let mut functions = Functions::init_agent(name, &agent_config.global_tools)?;

        config.write().functions.clear_mcp_meta_functions();
        let mcp_servers = if config.read().mcp_server_support {
            (!agent_config.mcp_servers.is_empty()).then(|| agent_config.mcp_servers.join(","))
        } else {
            eprintln!(
                "{}",
                formatdoc!(
                    "
										This agent uses MCP servers, but MCP support is disabled.
										To enable it, exit the agent and set 'mcp_server_support: true', then try again
										"
                )
            );
            None
        };

        let registry = config
            .write()
            .mcp_registry
            .take()
            .with_context(|| "MCP registry should be populated")?;
        let new_mcp_registry =
            McpRegistry::reinit(registry, mcp_servers, abort_signal.clone()).await?;

        if !new_mcp_registry.is_empty() {
            functions.append_mcp_meta_functions(new_mcp_registry.list_started_servers());
        }

        config.write().mcp_registry = Some(new_mcp_registry);

        agent_config.load_envs(&config.read());

        let model = {
            let config = config.read();
            match agent_config.model_id.as_ref() {
                Some(model_id) => Model::retrieve_model(&config, model_id, ModelType::Chat)?,
                None => {
                    if agent_config.temperature.is_none() {
                        agent_config.temperature = config.temperature;
                    }
                    if agent_config.top_p.is_none() {
                        agent_config.top_p = config.top_p;
                    }
                    config.current_model().clone()
                }
            }
        };

        let rag = if rag_path.exists() {
            Some(Arc::new(Rag::load(config, DEFAULT_AGENT_NAME, &rag_path)?))
        } else if !agent_config.documents.is_empty() && !config.read().info_flag {
            let mut ans = false;
            if *IS_STDOUT_TERMINAL {
                ans = Confirm::new("The agent has documents attached, init RAG?")
                    .with_default(true)
                    .prompt()?;
            }
            if ans {
                let mut document_paths = vec![];
                for path in &agent_config.documents {
                    if is_url(path) {
                        document_paths.push(path.to_string());
                    } else if is_loader_protocol(&loaders, path) {
                        let (protocol, document_path) = path
                            .split_once(':')
                            .with_context(|| "Invalid loader protocol path")?;
                        let resolved_path = resolve_home_dir(document_path);
                        let new_path = if Path::new(&resolved_path).is_relative() {
                            safe_join_path(&agent_data_dir, resolved_path)
                                .ok_or_else(|| anyhow!("Invalid document path: '{path}'"))?
                        } else {
                            PathBuf::from(&resolved_path)
                        };
                        document_paths.push(format!("{}:{}", protocol, new_path.display()));
                    } else if Path::new(&resolve_home_dir(path)).is_relative() {
                        let new_path = safe_join_path(&agent_data_dir, path)
                            .ok_or_else(|| anyhow!("Invalid document path: '{path}'"))?;
                        document_paths.push(new_path.display().to_string())
                    } else {
                        document_paths.push(path.to_string())
                    }
                }
                let rag =
                    Rag::init(config, "rag", &rag_path, &document_paths, abort_signal).await?;
                Some(Arc::new(rag))
            } else {
                None
            }
        } else {
            None
        };

        if agent_config.auto_continue {
            functions.append_todo_functions();
        }

        if agent_config.can_spawn_agents {
            functions.append_supervisor_functions();
        }

        agent_config.replace_tools_placeholder(&functions);

        Ok(Self {
            name: name.to_string(),
            config: agent_config,
            shared_variables: Default::default(),
            session_variables: None,
            shared_dynamic_instructions: None,
            session_dynamic_instructions: None,
            functions,
            rag,
            model,
            vault: Arc::clone(&config.read().vault),
            todo_list: TodoList::default(),
            continuation_count: 0,
            last_continuation_response: None,
        })
    }

    pub fn init_agent_variables(
        agent_variables: &[AgentVariable],
        pre_set_variables: Option<&AgentVariables>,
        no_interaction: bool,
    ) -> Result<AgentVariables> {
        let mut output = IndexMap::new();
        if agent_variables.is_empty() {
            return Ok(output);
        }
        let mut printed = false;
        let mut unset_variables = vec![];
        for agent_variable in agent_variables {
            let key = agent_variable.name.clone();
            if let Some(value) = pre_set_variables.and_then(|v| v.get(&key)) {
                output.insert(key, value.clone());
                continue;
            }
            if let Some(value) = agent_variable.default.clone() {
                output.insert(key, value);
                continue;
            }
            if no_interaction {
                continue;
            }
            if *IS_STDOUT_TERMINAL {
                if !printed {
                    println!("⚙ Init agent variables...");
                    printed = true;
                }
                let value = Text::new(&format!(
                    "{} ({}):",
                    agent_variable.name, agent_variable.description
                ))
                .with_validator(|input: &str| {
                    if input.trim().is_empty() {
                        Ok(Validation::Invalid("This field is required".into()))
                    } else {
                        Ok(Validation::Valid)
                    }
                })
                .prompt()?;
                output.insert(key, value);
            } else {
                unset_variables.push(agent_variable)
            }
        }
        if !unset_variables.is_empty() {
            bail!(
                "The following agent variables are required:\n{}",
                unset_variables
                    .iter()
                    .map(|v| format!("  - {}: {}", v.name, v.description))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
        Ok(output)
    }

    pub fn export(&self) -> Result<String> {
        let mut value = json!({});
        value["name"] = json!(self.name());
        let variables = self.variables();
        if !variables.is_empty() {
            value["variables"] = serde_json::to_value(variables)?;
        }
        value["config"] = json!(self.config);
        let mut config = self.config.clone();
        config.instructions = self.interpolated_instructions();
        value["definition"] = json!(config);
        value["data_dir"] = Config::agent_data_dir(&self.name)
            .display()
            .to_string()
            .into();
        value["config_file"] = Config::agent_config_file(&self.name)
            .display()
            .to_string()
            .into();
        let data = serde_yaml::to_string(&value)?;
        Ok(data)
    }

    pub fn banner(&self) -> String {
        self.config.banner(&self.conversation_starters())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn functions(&self) -> &Functions {
        &self.functions
    }

    pub fn rag(&self) -> Option<Arc<Rag>> {
        self.rag.clone()
    }

    pub fn conversation_starters(&self) -> Vec<String> {
        self.config
            .conversation_starters
            .iter()
            .map(|starter| self.interpolate_text(starter))
            .collect()
    }

    pub fn interpolated_instructions(&self) -> String {
        let mut output = self
            .session_dynamic_instructions
            .clone()
            .or_else(|| self.shared_dynamic_instructions.clone())
            .unwrap_or_else(|| self.config.instructions.clone());

        if self.config.auto_continue && self.config.inject_todo_instructions {
            output.push_str(DEFAULT_TODO_INSTRUCTIONS);
        }

        self.interpolate_text(&output)
    }

    fn interpolate_text(&self, text: &str) -> String {
        let mut output = text.to_string();
        for (k, v) in self.variables() {
            output = output.replace(&format!("{{{{{k}}}}}"), v)
        }
        interpolate_variables(&mut output);
        output
    }

    pub fn agent_session(&self) -> Option<&str> {
        self.config.agent_session.as_deref()
    }

    pub fn variables(&self) -> &AgentVariables {
        match &self.session_variables {
            Some(variables) => variables,
            None => &self.shared_variables,
        }
    }

    pub fn variable_envs(&self) -> HashMap<String, String> {
        self.variables()
            .iter()
            .map(|(k, v)| {
                (
                    format!("LLM_AGENT_VAR_{}", normalize_env_name(k)),
                    SECRET_RE
                        .replace(v, |caps: &Captures| {
                            self.vault
                                .get_secret(caps[1].trim(), false)
                                .unwrap_or(v.clone())
                        })
                        .to_string(),
                )
            })
            .collect()
    }

    pub fn shared_variables(&self) -> &AgentVariables {
        &self.shared_variables
    }

    pub fn set_shared_variables(&mut self, shared_variables: AgentVariables) {
        self.shared_variables = shared_variables;
    }

    pub fn set_session_variables(&mut self, session_variables: AgentVariables) {
        self.session_variables = Some(session_variables);
    }

    pub fn defined_variables(&self) -> &[AgentVariable] {
        &self.config.variables
    }

    pub fn exit_session(&mut self) {
        self.session_variables = None;
        self.session_dynamic_instructions = None;
    }

    pub fn auto_continue_enabled(&self) -> bool {
        self.config.auto_continue
    }

    pub fn max_auto_continues(&self) -> usize {
        self.config.max_auto_continues
    }

    pub fn can_spawn_agents(&self) -> bool {
        self.config.can_spawn_agents
    }

    pub fn max_concurrent_agents(&self) -> usize {
        self.config.max_concurrent_agents
    }

    pub fn max_agent_depth(&self) -> usize {
        self.config.max_agent_depth
    }

    pub fn summarization_model(&self) -> Option<&str> {
        self.config.summarization_model.as_deref()
    }

    pub fn summarization_threshold(&self) -> usize {
        self.config.summarization_threshold
    }

    pub fn continuation_count(&self) -> usize {
        self.continuation_count
    }

    pub fn increment_continuation(&mut self) {
        self.continuation_count += 1;
    }

    pub fn reset_continuation(&mut self) {
        self.continuation_count = 0;
        self.last_continuation_response = None;
    }

    pub fn is_stale_response(&self, response: &str) -> bool {
        self.last_continuation_response
            .as_ref()
            .is_some_and(|last| last == response)
    }

    pub fn set_last_continuation_response(&mut self, response: String) {
        self.last_continuation_response = Some(response);
    }

    pub fn todo_list(&self) -> &TodoList {
        &self.todo_list
    }

    pub fn init_todo_list(&mut self, goal: &str) {
        self.todo_list = TodoList::new(goal);
    }

    pub fn add_todo(&mut self, task: &str) -> usize {
        self.todo_list.add(task)
    }

    pub fn mark_todo_done(&mut self, id: usize) -> bool {
        self.todo_list.mark_done(id)
    }

    pub fn continuation_prompt(&self) -> String {
        self.config.continuation_prompt.clone().unwrap_or_else(|| {
            formatdoc! {"
                [SYSTEM REMINDER - TODO CONTINUATION]
                You have incomplete tasks. Rules:
                1. BEFORE marking a todo done: verify the work compiles/works. No premature completion.
                2. If a todo is broad (e.g. \"implement X and implement Y\"): break it into specific subtasks FIRST using todo__add, then work on those.\n\
                3. Each todo should be atomic and be \"single responsibility\" - completable in one focused action.
                4. Continue with the next pending item now. Call tools immediately."}
        })
    }

    pub fn compression_threshold(&self) -> Option<usize> {
        self.config.compression_threshold
    }

    pub fn is_dynamic_instructions(&self) -> bool {
        self.config.dynamic_instructions
    }

    pub fn update_shared_dynamic_instructions(&mut self, force: bool) -> Result<()> {
        if self.is_dynamic_instructions() && (force || self.shared_dynamic_instructions.is_none()) {
            self.shared_dynamic_instructions = Some(self.run_instructions_fn()?);
        }
        Ok(())
    }

    pub fn update_session_dynamic_instructions(&mut self, value: Option<String>) -> Result<()> {
        if self.is_dynamic_instructions() {
            let value = match value {
                Some(v) => v,
                None => self.run_instructions_fn()?,
            };
            self.session_dynamic_instructions = Some(value);
        }
        Ok(())
    }

    fn run_instructions_fn(&self) -> Result<String> {
        let value = run_llm_function(
            self.name().to_string(),
            vec!["_instructions".into(), "{}".into()],
            self.variable_envs(),
            Some(self.name().to_string()),
        )?;
        match value {
            Some(v) => Ok(v),
            _ => bail!("No return value from '_instructions' function"),
        }
    }
}

impl RoleLike for Agent {
    fn to_role(&self) -> Role {
        let prompt = self.interpolated_instructions();
        let mut role = Role::new("", &prompt);
        role.sync(self);
        role
    }

    fn model(&self) -> &Model {
        &self.model
    }

    fn temperature(&self) -> Option<f64> {
        self.config.temperature
    }

    fn top_p(&self) -> Option<f64> {
        self.config.top_p
    }

    fn enabled_tools(&self) -> Option<String> {
        self.config.global_tools.clone().join(",").into()
    }

    fn enabled_mcp_servers(&self) -> Option<String> {
        self.config.mcp_servers.clone().join(",").into()
    }

    fn set_model(&mut self, model: Model) {
        self.config.model_id = Some(model.id());
        self.model = model;
    }

    fn set_temperature(&mut self, value: Option<f64>) {
        self.config.temperature = value;
    }

    fn set_top_p(&mut self, value: Option<f64>) {
        self.config.top_p = value;
    }

    fn set_enabled_tools(&mut self, value: Option<String>) {
        match value {
            Some(tools) => {
                let tools = tools
                    .split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>();
                self.config.global_tools = tools;
            }
            None => {
                self.config.global_tools.clear();
            }
        }
    }

    fn set_enabled_mcp_servers(&mut self, value: Option<String>) {
        match value {
            Some(servers) => {
                let servers = servers
                    .split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>();
                self.config.mcp_servers = servers;
            }
            None => {
                self.config.mcp_servers.clear();
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentConfig {
    pub name: String,
    #[serde(rename(serialize = "model", deserialize = "model"))]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<String>,
    #[serde(default)]
    pub auto_continue: bool,
    #[serde(default)]
    pub can_spawn_agents: bool,
    #[serde(default = "default_max_concurrent_agents")]
    pub max_concurrent_agents: usize,
    #[serde(default = "default_max_agent_depth")]
    pub max_agent_depth: usize,
    #[serde(default = "default_max_auto_continues")]
    pub max_auto_continues: usize,
    #[serde(default = "default_true")]
    pub inject_todo_instructions: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression_threshold: Option<usize>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub global_tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_prompt: Option<String>,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub dynamic_instructions: bool,
    #[serde(default)]
    pub variables: Vec<AgentVariable>,
    #[serde(default)]
    pub conversation_starters: Vec<String>,
    #[serde(default)]
    pub documents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarization_model: Option<String>,
    #[serde(default = "default_summarization_threshold")]
    pub summarization_threshold: usize,
}

fn default_max_auto_continues() -> usize {
    10
}

fn default_max_concurrent_agents() -> usize {
    4
}

fn default_max_agent_depth() -> usize {
    3
}

fn default_true() -> bool {
    true
}

fn default_summarization_threshold() -> usize {
    4000
}

impl AgentConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = read_to_string(path)
            .with_context(|| format!("Failed to read agent config file at '{}'", path.display()))?;
        let agent_config: Self = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to load agent config at '{}'", path.display()))?;

        Ok(agent_config)
    }

    fn load_envs(&mut self, config: &Config) {
        let name = &self.name;
        let with_prefix = |v: &str| normalize_env_name(&format!("{name}_{v}"));

        if self.agent_session.is_none() {
            self.agent_session = config.agent_session.clone();
        }

        if let Some(v) = read_env_value::<String>(&with_prefix("model")) {
            self.model_id = v;
        }
        if let Some(v) = read_env_value::<f64>(&with_prefix("temperature")) {
            self.temperature = v;
        }
        if let Some(v) = read_env_value::<f64>(&with_prefix("top_p")) {
            self.top_p = v;
        }
        if let Ok(v) = env::var(with_prefix("global_tools"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.global_tools = v;
        }
        if let Ok(v) = env::var(with_prefix("mcp_servers"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.mcp_servers = v;
        }
        if let Some(v) = read_env_value::<String>(&with_prefix("agent_session")) {
            self.agent_session = v;
        }
        if let Ok(v) = env::var(with_prefix("variables"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.variables = v;
        }
    }

    fn banner(&self, conversation_starters: &[String]) -> String {
        let AgentConfig {
            name,
            description,
            version,
            ..
        } = self;
        let starters = if conversation_starters.is_empty() {
            String::new()
        } else {
            let starters = conversation_starters
                .iter()
                .map(|v| format!("- {v}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                r#"

## Conversation Starters
{starters}"#
            )
        };
        format!(
            r#"# {name} {version}
{description}{starters}"#
        )
    }

    fn replace_tools_placeholder(&mut self, functions: &Functions) {
        let tools_placeholder: &str = "{{__tools__}}";
        if self.instructions.contains(tools_placeholder) {
            let tools = functions
                .declarations()
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let description = match v.description.split_once('\n') {
                        Some((v, _)) => v,
                        None => &v.description,
                    };
                    format!("{}. {}: {description}", i + 1, v.name)
                })
                .collect::<Vec<String>>()
                .join("\n");
            self.instructions = self.instructions.replace(tools_placeholder, &tools);
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentVariable {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip_deserializing, default)]
    pub value: String,
}

pub fn list_agents() -> Vec<String> {
    let agents_data_dir = Config::agents_data_dir();
    if !agents_data_dir.exists() {
        return vec![];
    }

    let mut agents = Vec::new();
    if let Ok(entries) = read_dir(agents_data_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                agents.push(name.to_string());
            }
        }
    }

    agents
}

pub fn complete_agent_variables(agent_name: &str) -> Vec<(String, Option<String>)> {
    let config_path = Config::agent_config_file(agent_name);
    if !config_path.exists() {
        return vec![];
    }
    let Ok(config) = AgentConfig::load(&config_path) else {
        return vec![];
    };
    config
        .variables
        .iter()
        .map(|v| {
            let description = match &v.default {
                Some(default) => format!("{} [default: {default}]", v.description),
                None => v.description.clone(),
            };
            (format!("{}=", v.name), Some(description))
        })
        .collect()
}
