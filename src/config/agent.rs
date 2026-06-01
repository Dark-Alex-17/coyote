use super::*;

use crate::{
    client::Model,
    function::{Functions, run_llm_function},
};

use super::rag_cache::RagKey;
use crate::config::paths;
use crate::config::prompts::{
    DEFAULT_SPAWN_INSTRUCTIONS, DEFAULT_TEAMMATE_INSTRUCTIONS, DEFAULT_TODO_INSTRUCTIONS,
    DEFAULT_USER_INTERACTION_INSTRUCTIONS,
};
use crate::graph::{Graph, GraphParser, NodeType};
use crate::rag::RagInitConfig;
use crate::vault::SECRET_RE;
use anyhow::{Context, Result};
use fancy_regex::Captures;
use inquire::{Text, validator::Validation};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use std::{ffi::OsStr, path::Path};

const DEFAULT_AGENT_NAME: &str = "rag";

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
    graph_rags: HashMap<String, Arc<Rag>>,
    model: Model,
    vault: GlobalVault,
}

impl Agent {
    pub fn install_builtin_agents(force: bool) -> Result<()> {
        info!(
            "Installing built-in agents in {}",
            paths::agents_data_dir().display()
        );

        for file in AgentAssets::iter() {
            debug!("Processing agent file: {}", file.as_ref());

            let embedded_file = AgentAssets::get(&file)
                .ok_or_else(|| anyhow!("Failed to load embedded agent file: {}", file.as_ref()))?;
            let content = unsafe { std::str::from_utf8_unchecked(&embedded_file.data) };
            let file_path = paths::agents_data_dir().join(file.as_ref());
            let file_extension = file_path
                .extension()
                .and_then(OsStr::to_str)
                .map(|s| s.to_lowercase());
            #[cfg_attr(not(unix), expect(unused))]
            let is_script = matches!(file_extension.as_deref(), Some("sh") | Some("py"));

            if file_path.exists() && !force {
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
        app: &AppConfig,
        app_state: &AppState,
        current_model: &Model,
        info_flag: bool,
        name: &str,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let agent_data_dir = paths::agent_data_dir(name);
        let loaders = app.document_loaders.clone();
        let rag_path = paths::agent_rag_file(name, DEFAULT_AGENT_NAME);
        let config_path = paths::agent_config_file(name);
        let graph_path = paths::agent_graph_file(name);
        let mut graph_for_rag: Option<Graph> = None;
        let mut agent_config = match (config_path.exists(), graph_path.exists()) {
            (true, true) => bail!(
                "Agent '{name}' has both config.yaml and graph.yaml. A graph agent \
                 is defined by graph.yaml alone; a normal agent by config.yaml alone. \
                 Remove one of the two files."
            ),
            (true, false) => AgentConfig::load(&config_path)?,
            (false, true) => {
                let parser = GraphParser::new(&agent_data_dir);
                let graph = parser
                    .load_from_file(&graph_path)
                    .with_context(|| format!("Failed to load graph.yaml for agent '{name}'"))?;
                let config = AgentConfig::from_graph(name, &graph);
                graph_for_rag = Some(graph);
                config
            }
            (false, false) => bail!(
                "Agent '{name}' has neither a config.yaml nor a graph.yaml at '{}'",
                agent_data_dir.display()
            ),
        };
        let mut functions = Functions::init_agent(name, &agent_config.global_tools)?;

        agent_config.load_envs(app);

        let model = match agent_config.model_id.as_ref() {
            Some(model_id) => Model::retrieve_model(app, model_id, ModelType::Chat)?,
            None => {
                if agent_config.temperature.is_none() {
                    agent_config.temperature = app.temperature;
                }
                if agent_config.top_p.is_none() {
                    agent_config.top_p = app.top_p;
                }
                current_model.clone()
            }
        };

        let rag = if rag_path.exists() {
            let key = RagKey::Agent(name.to_string());
            let app_clone = app.clone();
            let rag_path_clone = rag_path.clone();
            let rag = app_state
                .rag_cache
                .load_with(key, || async move {
                    Rag::load(&app_clone, DEFAULT_AGENT_NAME, &rag_path_clone)
                })
                .await?;
            Some(rag)
        } else if !agent_config.documents.is_empty() && !info_flag {
            let mut ans = false;
            if *IS_STDOUT_TERMINAL {
                ans = Confirm::new("The agent has documents attached, init RAG?")
                    .with_default(true)
                    .prompt()?;
            }
            if ans {
                let document_paths =
                    resolve_document_paths(&agent_config.documents, &loaders, &agent_data_dir)?;
                let key = RagKey::Agent(name.to_string());
                let app_clone = app.clone();
                let rag_path_clone = rag_path.clone();
                let abort = abort_signal.clone();
                let rag = app_state
                    .rag_cache
                    .load_with(key, || async move {
                        Rag::init(&app_clone, "rag", &rag_path_clone, &document_paths, abort).await
                    })
                    .await?;
                Some(rag)
            } else {
                None
            }
        } else {
            None
        };

        let graph_rags = match &graph_for_rag {
            Some(graph) => {
                init_graph_rags(
                    app,
                    app_state,
                    name,
                    graph,
                    &agent_data_dir,
                    &loaders,
                    info_flag,
                    abort_signal.clone(),
                )
                .await?
            }
            None => HashMap::new(),
        };

        if agent_config.auto_continue {
            functions.append_todo_functions();
        }

        if agent_config.can_spawn_agents {
            functions.append_supervisor_functions();
        }

        functions.append_teammate_functions();
        functions.append_user_interaction_functions();

        if app.function_calling_support
            && app.skills_enabled
            && !matches!(agent_config.skills_enabled, Some(false))
        {
            functions.append_skill_functions();
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
            graph_rags,
            model,
            vault: app_state.vault.clone(),
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
        value["data_dir"] = paths::agent_data_dir(&self.name)
            .display()
            .to_string()
            .into();
        let config_path = paths::agent_config_file(&self.name);
        let definition_file = if config_path.exists() {
            config_path
        } else {
            paths::agent_graph_file(&self.name)
        };
        value["config_file"] = definition_file.display().to_string().into();
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

    pub fn graph_rag(&self, node_id: &str) -> Option<Arc<Rag>> {
        self.graph_rags.get(node_id).cloned()
    }

    pub fn append_mcp_meta_functions(&mut self, mcp_servers: Vec<String>) {
        self.functions.append_mcp_meta_functions(mcp_servers);
    }

    pub fn mcp_server_names(&self) -> &[String] {
        &self.config.mcp_servers
    }

    pub fn skills_enabled(&self) -> Option<bool> {
        self.config.skills_enabled
    }

    pub fn enabled_skills(&self) -> Option<&[String]> {
        self.config.enabled_skills.as_deref()
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

        if self.config.can_spawn_agents && self.config.inject_spawn_instructions {
            output.push_str(DEFAULT_SPAWN_INSTRUCTIONS);
        }

        output.push_str(DEFAULT_TEAMMATE_INSTRUCTIONS);
        output.push_str(DEFAULT_USER_INTERACTION_INSTRUCTIONS);

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

    pub fn inject_todo_instructions(&self) -> bool {
        self.config.inject_todo_instructions
    }

    pub fn continuation_prompt_value(&self) -> Option<String> {
        self.config.continuation_prompt.clone()
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

    pub fn escalation_timeout(&self) -> u64 {
        self.config.escalation_timeout
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
        None
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
    #[serde(default = "default_true")]
    pub inject_spawn_instructions: bool,
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
    pub skills_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_skills: Option<Vec<String>>,
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
    #[serde(default = "default_escalation_timeout")]
    pub escalation_timeout: u64,
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

fn default_escalation_timeout() -> u64 {
    300
}

impl AgentConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = read_to_string(path)
            .with_context(|| format!("Failed to read agent config file at '{}'", path.display()))?;
        let agent_config: Self = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to load agent config at '{}'", path.display()))?;

        Ok(agent_config)
    }

    pub fn from_graph(dir_name: &str, graph: &Graph) -> Self {
        AgentConfig {
            name: dir_name.to_string(),
            model_id: graph.model.clone(),
            temperature: graph.temperature,
            top_p: graph.top_p,
            description: graph.description.clone(),
            global_tools: graph.global_tools.clone(),
            mcp_servers: graph.mcp_servers.clone(),
            conversation_starters: graph.conversation_starters.clone(),
            variables: graph.variables.clone(),
            can_spawn_agents: graph.has_agent_node(),
            max_concurrent_agents: default_max_concurrent_agents(),
            max_agent_depth: default_max_agent_depth(),
            escalation_timeout: default_escalation_timeout(),
            ..AgentConfig::default()
        }
    }

    fn load_envs(&mut self, app: &AppConfig) {
        let name = &self.name;
        let with_prefix = |v: &str| normalize_env_name(&format!("{name}_{v}"));

        if self.agent_session.is_none() {
            self.agent_session = app.agent_session.clone();
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

fn resolve_document_paths(
    documents: &[String],
    loaders: &HashMap<String, String>,
    agent_data_dir: &Path,
) -> Result<Vec<String>> {
    let mut document_paths = vec![];
    for path in documents {
        if is_url(path) {
            document_paths.push(path.to_string());
        } else if is_loader_protocol(loaders, path) {
            let (protocol, document_path) = path
                .split_once(':')
                .with_context(|| "Invalid loader protocol path")?;
            let resolved_path = resolve_home_dir(document_path);
            let new_path = if Path::new(&resolved_path).is_relative() {
                safe_join_path(agent_data_dir, resolved_path)
                    .ok_or_else(|| anyhow!("Invalid document path: '{path}'"))?
            } else {
                PathBuf::from(&resolved_path)
            };

            document_paths.push(format!("{}:{}", protocol, new_path.display()));
        } else if Path::new(&resolve_home_dir(path)).is_relative() {
            let new_path = safe_join_path(agent_data_dir, path)
                .ok_or_else(|| anyhow!("Invalid document path: '{path}'"))?;
            document_paths.push(new_path.display().to_string())
        } else {
            document_paths.push(path.to_string())
        }
    }
    Ok(document_paths)
}

#[allow(clippy::too_many_arguments)]
async fn init_graph_rags(
    app: &AppConfig,
    app_state: &AppState,
    agent_name: &str,
    graph: &Graph,
    agent_data_dir: &Path,
    loaders: &HashMap<String, String>,
    info_flag: bool,
    abort_signal: AbortSignal,
) -> Result<HashMap<String, Arc<Rag>>> {
    let mut rags = HashMap::new();
    if info_flag {
        return Ok(rags);
    }

    for (node_id, node) in &graph.nodes {
        let NodeType::Rag(rag_node) = &node.node_type else {
            continue;
        };
        let rag_path = paths::agent_rag_file(agent_name, node_id);
        let key = RagKey::GraphNode {
            agent: agent_name.to_string(),
            node: node_id.clone(),
        };
        let rag = if rag_path.exists() {
            let app_clone = app.clone();
            let path_clone = rag_path.clone();
            let name_clone = node_id.clone();
            app_state
                .rag_cache
                .load_with(key, || async move {
                    Rag::load(&app_clone, &name_clone, &path_clone)
                })
                .await?
        } else {
            let config = RagInitConfig {
                embedding_model: rag_node.embedding_model.clone(),
                chunk_size: rag_node.chunk_size,
                chunk_overlap: rag_node.chunk_overlap,
                reranker_model: rag_node.reranker_model.clone(),
                top_k: rag_node.top_k,
                batch_size: rag_node.batch_size,
            };
            let fully_specified = config.embedding_model.is_some()
                && config.chunk_size.is_some()
                && config.chunk_overlap.is_some();
            if !fully_specified {
                if !*IS_STDOUT_TERMINAL {
                    bail!(
                        "Agent '{agent_name}' requires RAG for rag node '{node_id}', but its \
                         knowledge base is not built and the node does not fully specify how \
                         to build it. Set `embedding_model`, `chunk_size`, and `chunk_overlap` \
                         on the node, or run the agent once interactively."
                    );
                }

                let ans = Confirm::new(&format!(
                    "Initialize RAG knowledge base for rag node '{node_id}'?"
                ))
                .with_default(true)
                .prompt()?;

                if !ans {
                    bail!(
                        "Agent '{agent_name}' has rag node '{node_id}' but its RAG was not \
                         initialized. RAG initialization is required for this agent."
                    );
                }
            }

            let document_paths =
                resolve_document_paths(&rag_node.documents, loaders, agent_data_dir)?;
            let app_clone = app.clone();
            let path_clone = rag_path.clone();
            let name_clone = node_id.clone();
            let abort = abort_signal.clone();
            app_state
                .rag_cache
                .load_with(key, || async move {
                    Rag::init_with_config(
                        &app_clone,
                        &name_clone,
                        &path_clone,
                        &document_paths,
                        &config,
                        abort,
                    )
                    .await
                })
                .await?
        };
        rags.insert(node_id.clone(), rag);
    }
    Ok(rags)
}

pub fn list_agents() -> Vec<String> {
    let agents_data_dir = paths::agents_data_dir();
    if !agents_data_dir.exists() {
        return vec![];
    }

    let mut agents = Vec::new();
    if let Ok(entries) = read_dir(agents_data_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
                && !name.starts_with('.')
            {
                agents.push(name.to_string());
            }
        }
    }

    agents
}

pub fn complete_agent_variables(agent_name: &str) -> Vec<(String, Option<String>)> {
    let config_path = paths::agent_config_file(agent_name);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_parses_from_yaml() {
        let yaml = r#"
name: test-agent
description: A test agent
instructions: You are helpful
auto_continue: true
max_auto_continues: 5
can_spawn_agents: true
max_concurrent_agents: 8
max_agent_depth: 2
mcp_servers:
  - github
  - jira
global_tools:
  - execute_command.sh
  - fs_read.sh
conversation_starters:
  - "Hello!"
  - "How are you?"
variables:
  - name: username
    description: Your name
"#;

        let config: AgentConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.name, "test-agent");
        assert_eq!(config.description, "A test agent");
        assert!(config.auto_continue);
        assert_eq!(config.max_auto_continues, 5);
        assert!(config.can_spawn_agents);
        assert_eq!(config.max_concurrent_agents, 8);
        assert_eq!(config.max_agent_depth, 2);
        assert_eq!(config.mcp_servers, vec!["github", "jira"]);
        assert_eq!(config.global_tools.len(), 2);
        assert_eq!(config.conversation_starters.len(), 2);
        assert_eq!(config.variables.len(), 1);
        assert_eq!(config.variables[0].name, "username");
    }

    #[test]
    fn agent_config_defaults() {
        let yaml = "name: minimal\ninstructions: hi\n";
        let config: AgentConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.name, "minimal");
        assert!(!config.auto_continue);
        assert!(!config.can_spawn_agents);
        assert_eq!(config.max_concurrent_agents, 4);
        assert_eq!(config.max_agent_depth, 3);
        assert_eq!(config.max_auto_continues, 10);
        assert!(config.mcp_servers.is_empty());
        assert!(config.global_tools.is_empty());
        assert!(config.conversation_starters.is_empty());
        assert!(config.variables.is_empty());
        assert!(config.model_id.is_none());
        assert!(config.temperature.is_none());
        assert!(config.top_p.is_none());
    }

    #[test]
    fn agent_config_with_model() {
        let yaml =
            "name: test\nmodel: openai:gpt-4\ntemperature: 0.7\ntop_p: 0.9\ninstructions: hi\n";
        let config: AgentConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.model_id, Some("openai:gpt-4".to_string()));
        assert_eq!(config.temperature, Some(0.7));
        assert_eq!(config.top_p, Some(0.9));
    }

    #[test]
    fn agent_config_inject_defaults_true() {
        let yaml = "name: test\ninstructions: hi\n";
        let config: AgentConfig = serde_yaml::from_str(yaml).unwrap();

        assert!(config.inject_todo_instructions);
        assert!(config.inject_spawn_instructions);
    }

    #[test]
    fn from_graph_maps_agent_level_fields() {
        let yaml = formatdoc! {r#"
            name: graph_name_ignored
            description: A graph agent
            model: claude:claude-sonnet-4-6
            temperature: 0.3
            top_p: 0.8
            global_tools:
              - fetch_pdf.sh
            mcp_servers:
              - pubmed-search
            conversation_starters:
              - "Start here"
            start: e
            nodes:
              e:
                id: e
                type: end
                output: done
            "#};
        let graph: Graph = serde_yaml::from_str(&yaml).unwrap();

        let config = AgentConfig::from_graph("my-agent-dir", &graph);

        assert_eq!(config.name, "my-agent-dir");
        assert_eq!(config.description, "A graph agent");
        assert_eq!(config.model_id.as_deref(), Some("claude:claude-sonnet-4-6"));
        assert_eq!(config.temperature, Some(0.3));
        assert_eq!(config.top_p, Some(0.8));
        assert_eq!(config.global_tools, vec!["fetch_pdf.sh"]);
        assert_eq!(config.mcp_servers, vec!["pubmed-search"]);
        assert_eq!(config.conversation_starters, vec!["Start here"]);
    }

    #[test]
    fn from_graph_derives_can_spawn_agents_from_agent_nodes() {
        let with_agent = formatdoc! {r#"
            name: g
            start: a
            nodes:
              a:
                id: a
                type: agent
                agent: helper
                prompt: hi
                next: e
              e:
                id: e
                type: end
                output: done
            "#};
        let graph: Graph = serde_yaml::from_str(&with_agent).unwrap();
        assert!(AgentConfig::from_graph("d", &graph).can_spawn_agents);

        let no_agent =
            "name: g\nstart: x\nnodes:\n  x:\n    id: x\n    type: end\n    output: ok\n";
        let graph: Graph = serde_yaml::from_str(no_agent).unwrap();
        assert!(!AgentConfig::from_graph("d", &graph).can_spawn_agents);
    }

    #[test]
    fn from_graph_keeps_defaults_for_llm_loop_fields() {
        let yaml = "name: g\nstart: x\nnodes:\n  x:\n    id: x\n    type: end\n    output: ok\n";
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();

        let config = AgentConfig::from_graph("d", &graph);

        assert!(!config.auto_continue);
        assert!(config.instructions.is_empty());
        assert!(config.documents.is_empty());
        assert!(!config.inject_todo_instructions);
        assert!(!config.inject_spawn_instructions);
        assert_eq!(config.max_auto_continues, 0);
        assert_eq!(config.summarization_threshold, 0);

        assert_eq!(
            config.max_concurrent_agents,
            default_max_concurrent_agents()
        );
        assert_eq!(config.max_agent_depth, default_max_agent_depth());
        assert_eq!(config.escalation_timeout, default_escalation_timeout());
    }
}
