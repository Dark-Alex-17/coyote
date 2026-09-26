use super::*;

use crate::hooks::{HooksMap, RagSyncHooks};
use crate::{
    client::Model,
    config::memory,
    function::{
        Functions,
        jobs::{DEFAULT_MAX_CONCURRENT_JOBS, JOB_FUNCTION_PREFIX},
        run_llm_function,
    },
    graph, rag,
};

use super::rag_cache::RagKey;
use crate::config::builtin_manifest;
use crate::config::conflict::{self, InstallMode, StickyMode};
use crate::config::paths;
use crate::config::prompts::{
    DEFAULT_JOB_INSTRUCTIONS, DEFAULT_SPAWN_INSTRUCTIONS, DEFAULT_TEAMMATE_INSTRUCTIONS,
    DEFAULT_TODO_INSTRUCTIONS, DEFAULT_USER_INTERACTION_INSTRUCTIONS,
};
use crate::config::{
    BuiltinAgentUnavailable, RESERVED_AGENT_NAMES, builtin_agent_description, builtin_agent_dir,
    builtin_default_description, reserved_agent,
};
use crate::function::write_file_atomic;
use crate::graph::types::RagNode;
use crate::graph::{Graph, GraphParser, NodeType};
use crate::mcp::McpServerFeatures;
use crate::rag::RagInitConfig;
use crate::vault::SECRET_RE;
use anyhow::{Context, Result};
use fancy_regex::Captures;
use inquire::{Text, validator::Validation};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::{env, path::Path};

const DEFAULT_AGENT_NAME: &str = "rag";

const AGENT_DEFINITION_FILES: [&str; 6] = [
    "config.yaml",
    "graph.yaml",
    "tools.sh",
    "tools.py",
    "tools.ts",
    "tools.js",
];

pub type AgentVariables = IndexMap<String, String>;

#[derive(Embed)]
#[folder = "assets/agents/"]
struct AgentAssets;

/// Splits an embedded asset path of the form `<agent>/hooks/<file>` into
/// its agent and hook-file names. Only direct children of `hooks/` count:
/// they are the only shape the builtin manifest tracks.
fn parse_direct_hook_path(path: &str) -> Option<(&str, &str)> {
    let (agent, rest) = path.split_once('/')?;
    let hook = direct_hook_name(rest)?;
    Some((agent, hook))
}

fn direct_hook_name(rest: &str) -> Option<&str> {
    let name = rest.strip_prefix("hooks/")?;
    (!name.is_empty() && !name.contains('/')).then_some(name)
}

fn bundled_agent_assets() -> Result<Vec<(String, Vec<u8>)>> {
    AgentAssets::iter()
        .map(|file| {
            let embedded = AgentAssets::get(&file)
                .ok_or_else(|| anyhow!("Failed to load embedded agent file: {}", file.as_ref()))?;
            Ok((file.as_ref().to_string(), embedded.data.into_owned()))
        })
        .collect()
}

/// Drops every file whose top-level segment is a reserved agent name (both
/// `<agent>/<rest>` and a bare `<agent>` entry), warning once per canonical
/// name. The built-in never lives at `agents/<name>`, so no installer may
/// create or touch that path.
fn drop_reserved_agent_files(
    files: impl IntoIterator<Item = (String, Vec<u8>)>,
) -> Vec<(String, Vec<u8>)> {
    let mut warned: HashSet<&'static str> = HashSet::new();
    files
        .into_iter()
        .filter(|(file, _)| {
            let agent = file.split_once('/').map_or(file.as_str(), |(a, _)| a);
            let Some(canonical) = reserved_agent(agent) else {
                return true;
            };
            if warned.insert(canonical) {
                warn!(
                    "Ignoring bundled agent file {file}: the agent name '{agent}' is \
                     reserved for the built-in agent '{canonical}'"
                );
            }
            false
        })
        .collect()
}

/// Installs one bundled agent's hook scripts under `<agent>/hooks/` and
/// reconciles that directory through its builtin manifest, mirroring the
/// role-side hook installer.
pub(crate) fn install_and_reconcile_agent_hooks(
    agent: &str,
    shipped: &[(String, String)],
    mode: InstallMode,
    sticky: &mut StickyMode,
) -> Result<()> {
    let dir = paths::agents_data_dir().join(agent).join("hooks");
    let mut written = BTreeSet::new();
    for (name, content) in shipped {
        let path = dir.join(name);
        if path.exists()
            && !conflict::should_replace_existing(&path, content, "agents", mode, sticky)?
        {
            debug!(
                "Agent hook file already exists, skipping: {}",
                path.display()
            );
            continue;
        }
        ensure_parent_exists(&path)?;
        info!("Creating agent hook file: {}", path.display());
        write_file_atomic(&path, content, None)?;
        set_executable_bit_if_script(&path)?;
        written.insert(name.clone());
    }

    let names: BTreeSet<String> = shipped.iter().map(|(name, _)| name.clone()).collect();
    // Reconciliation is best-effort housekeeping: a failure here must not
    // abort the install, matching the role-side policy.
    if let Err(err) = builtin_manifest::reconcile_builtin_dir(&dir, &names, &written) {
        warn!("Failed to reconcile builtin hooks for agent '{agent}': {err}");
    }
    Ok(())
}

/// Installs the `<agent>/hooks/*` files of every bundled agent and
/// reconciles each hooks directory, including those of hookless agents.
fn install_agent_hook_files(
    files: impl IntoIterator<Item = (String, Vec<u8>)>,
    mode: InstallMode,
    sticky: &mut StickyMode,
) -> Result<()> {
    let mut shipped: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (file, data) in drop_reserved_agent_files(files) {
        let Some((agent, rest)) = file.split_once('/') else {
            continue;
        };
        let hooks = shipped.entry(agent.to_string()).or_default();
        let Some(hook) = direct_hook_name(rest) else {
            continue;
        };
        let content =
            std::str::from_utf8(&data).expect("bundled agent hook asset is not valid UTF-8");
        hooks.push((hook.to_string(), content.to_string()));
    }

    for (agent, files) in &shipped {
        install_and_reconcile_agent_hooks(agent, files, mode, sticky)?;
    }

    Ok(())
}

/// An agent the embed stopped bundling is never visited by the per-agent
/// reconcile loop, so its installed hooks and manifest would orphan forever.
/// Any agent directory that carries a builtin hooks manifest but is absent
/// from the current embed gets its manifest-owned hooks removed, each
/// deletion logged individually by the reconcile. Only the installer writes
/// manifests, but a manual copy of a builtin agent directory carries the
/// hidden manifest along: files it lists are treated as installer-owned and
/// swept, while files absent from the manifest always survive.
/// Best-effort: a failure here must not abort the install.
fn sweep_removed_agent_hooks(bundled_files: &HashMap<String, HashSet<String>>) {
    let entries = match read_dir(paths::agents_data_dir()) {
        Ok(entries) => entries,
        Err(err) => {
            warn!("Failed to scan agents dir for removed-agent hooks: {err}");
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if bundled_files.contains_key(name) {
            continue;
        }
        // A symlinked agent dir is user-arranged: never follow it into a
        // hooks tree that lives somewhere else.
        if !entry
            .path()
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_dir())
        {
            continue;
        }
        let hooks_dir = entry.path().join("hooks");
        if !hooks_dir
            .join(builtin_manifest::BUILTIN_MANIFEST_FILE)
            .is_file()
        {
            continue;
        }
        info!(
            "Removing hooks shipped by removed built-in agent '{name}' in {}",
            hooks_dir.display()
        );
        if let Err(err) =
            builtin_manifest::reconcile_builtin_dir(&hooks_dir, &BTreeSet::new(), &BTreeSet::new())
        {
            warn!("Failed to reconcile builtin hooks for removed agent '{name}': {err}");
            continue;
        }
        // remove_dir only succeeds on an empty directory, so a user file
        // left behind keeps the directory in place.
        let _ = fs::remove_dir(&hooks_dir);
    }
}

/// Installs bundled agent files (`<agent>/<rest>` paths) under the agents
/// data dir and reconciles stale definition files and hook manifests for
/// every agent in the bundle.
fn install_agent_files(
    files: impl IntoIterator<Item = (String, Vec<u8>)>,
    mode: InstallMode,
) -> Result<()> {
    let mut sticky = StickyMode::None;
    let mut written_hooks: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut bundled_files: HashMap<String, HashSet<String>> = HashMap::new();
    for (file, data) in drop_reserved_agent_files(files) {
        debug!("Processing agent file: {file}");

        if let Some((agent, rest)) = file.split_once('/') {
            bundled_files
                .entry(agent.to_string())
                .or_default()
                .insert(rest.to_string());
        }

        let content = std::str::from_utf8(&data).expect("bundled agent asset is not valid UTF-8");
        let file_path = paths::agents_data_dir().join(&file);

        if file_path.exists()
            && !conflict::should_replace_existing(&file_path, content, "agents", mode, &mut sticky)?
        {
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
        set_executable_bit_if_script(&file_path)?;
        if let Some((agent, hook)) = parse_direct_hook_path(&file) {
            written_hooks
                .entry(agent.to_string())
                .or_default()
                .insert(hook.to_string());
        }
    }

    for (agent, files) in &bundled_files {
        for candidate in AGENT_DEFINITION_FILES {
            if files.contains(candidate) {
                continue;
            }
            let stale_path = paths::agents_data_dir().join(agent).join(candidate);
            if stale_path.exists() {
                info!(
                    "Removing stale file no longer shipped by built-in agent '{agent}': {}",
                    stale_path.display()
                );
                if let Err(err) = std::fs::remove_file(&stale_path) {
                    warn!(
                        "Failed to remove stale agent file {}: {err}",
                        stale_path.display()
                    );
                }
            }
        }
    }

    // Hook filenames are open-ended (unlike AGENT_DEFINITION_FILES), so
    // each bundled agent's hooks/ directory reconciles through its
    // builtin manifest: only files the installer previously shipped are
    // removal candidates, and user-created files in the same directory
    // are never touched. Only direct children of hooks/ are tracked;
    // nested shipped files install but are not reconciled.
    for (agent, files) in &bundled_files {
        let shipped: BTreeSet<String> = files
            .iter()
            .filter_map(|rest| direct_hook_name(rest))
            .map(str::to_string)
            .collect();
        let written = written_hooks.remove(agent.as_str()).unwrap_or_default();
        let hooks_dir = paths::agents_data_dir().join(agent).join("hooks");
        if let Err(err) = builtin_manifest::reconcile_builtin_dir(&hooks_dir, &shipped, &written) {
            warn!("Failed to reconcile builtin hooks for agent '{agent}': {err}");
        }
    }

    sweep_removed_agent_hooks(&bundled_files);

    Ok(())
}

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
    is_graph: bool,
    enabled_tools: Option<Vec<String>>,
}

impl Agent {
    pub fn install_builtin_agents(mode: InstallMode) -> Result<()> {
        info!(
            "Installing built-in agents in {}",
            paths::agents_data_dir().display()
        );
        install_agent_files(bundled_agent_assets()?, mode)
    }

    pub fn install_builtin_agent_hooks(mode: InstallMode, sticky: &mut StickyMode) -> Result<()> {
        install_agent_hook_files(bundled_agent_assets()?, mode, sticky)
    }

    pub async fn init(
        app: &AppConfig,
        app_state: &AppState,
        current_model: &Model,
        info_flag: bool,
        name: &str,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        validate_agent_name(name)?;
        let (name, agent_data_dir, config_path, graph_path) =
            if let Some(canonical) = reserved_agent(name) {
                let dir = builtin_agent_dir(canonical).ok_or_else(|| BuiltinAgentUnavailable {
                    name: canonical.to_string(),
                })?;
                let user_agents_dir = paths::agents_data_dir();
                if dir.starts_with(&user_agents_dir) {
                    bail!(
                        "Agent '{canonical}' is built in but its registered dir '{}' is inside \
                         the user agents dir '{}'",
                        dir.display(),
                        user_agents_dir.display()
                    );
                }
                let config_path = dir.join(CONFIG_FILE_NAME);
                let graph_path = dir.join(AGENT_GRAPH_FILE_NAME);
                (canonical, dir, config_path, graph_path)
            } else {
                (
                    name,
                    paths::agent_data_dir(name),
                    paths::agent_config_file(name),
                    paths::agent_graph_file(name),
                )
            };
        let loaders = app.document_loaders.clone();
        let rag_path = paths::agent_rag_file(name, DEFAULT_AGENT_NAME);
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

        let rag_sync_hooks = RagSyncHooks::resolve_for_agent(
            &app.hooks,
            &agent_config.global_hooks,
            &agent_config.hooks,
            name,
        );

        let rag = if rag_path.exists() {
            let key = RagKey::Agent(name.to_string());
            let app_clone = app.clone();
            let vault_clone = app_state.vault.clone();
            let rag_path_clone = rag_path.clone();
            let rag = app_state
                .rag_cache
                .load_with(key, || async move {
                    Rag::load_async(
                        &app_clone,
                        &vault_clone,
                        DEFAULT_AGENT_NAME,
                        &rag_path_clone,
                    )
                    .await
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
                let sync_hooks = rag_sync_hooks.clone();
                let rag = app_state
                    .rag_cache
                    .load_with(key, || async move {
                        Rag::init(
                            &app_clone,
                            "rag",
                            &rag_path_clone,
                            &document_paths,
                            abort,
                            true,
                            sync_hooks,
                        )
                        .await
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
                    &rag_sync_hooks,
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

        if app.function_calling_support
            && agent_config
                .max_concurrent_jobs
                .or(app.max_concurrent_jobs)
                .unwrap_or(DEFAULT_MAX_CONCURRENT_JOBS)
                > 0
        {
            functions.append_job_functions();
        }
        if mesh_tools_available(app, &app_state.mesh) {
            functions.append_mesh_functions();
        }

        functions.append_teammate_functions();
        functions.append_user_interaction_functions();

        if app.function_calling_support
            && app.skills_enabled
            && !matches!(agent_config.skills_enabled, Some(false))
        {
            functions.append_skill_functions();
        }

        if app.function_calling_support
            && !matches!(agent_config.memory, Some(false))
            && !matches!(app.memory, Some(false))
        {
            let memory_exists = paths::global_memory_index_file().exists()
                || env::current_dir()
                    .ok()
                    .and_then(|cwd| memory::discover_workspace_memory(&cwd))
                    .is_some();
            if memory_exists {
                functions.append_memory_functions();
            }
        }

        if rag.is_some() && app.function_calling_support && graph_for_rag.is_none() {
            functions.append_rag_query_functions();
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
            is_graph: graph_for_rag.is_some(),
            enabled_tools: None,
        })
    }

    pub fn init_agent_variables(
        agent_variables: &[AgentVariable],
        pre_set_variables: Option<&AgentVariables>,
        no_interaction: bool,
        interactive: bool,
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
            if interactive && *IS_STDOUT_TERMINAL {
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

    pub fn is_graph(&self) -> bool {
        self.is_graph
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

    pub fn append_mcp_meta_functions(&mut self, mcp_servers: Vec<McpServerFeatures>) {
        self.functions.append_mcp_meta_functions(mcp_servers);
    }

    pub fn mcp_server_names(&self) -> &[String] {
        &self.config.mcp_servers
    }

    pub fn spawnable_agents(&self) -> Option<&[String]> {
        self.config.spawnable_agents.as_deref()
    }

    pub fn skills_enabled(&self) -> Option<bool> {
        self.config.skills_enabled
    }

    pub fn enabled_skills(&self) -> Option<&[String]> {
        self.config.enabled_skills.as_deref()
    }

    pub fn enabled_macros(&self) -> Option<&[String]> {
        self.config.enabled_macros.as_deref()
    }

    pub fn memory(&self) -> Option<bool> {
        self.config.memory
    }

    pub fn set_skills_enabled(&mut self, value: Option<bool>) {
        self.config.skills_enabled = value;
    }

    pub fn set_enabled_skills(&mut self, value: Option<Vec<String>>) {
        self.config.enabled_skills = value;
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

        if self
            .functions
            .declarations()
            .iter()
            .any(|f| f.name.starts_with(JOB_FUNCTION_PREFIX))
        {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(DEFAULT_JOB_INSTRUCTIONS);
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

    pub fn inject_skill_instructions(&self) -> bool {
        self.config.inject_skill_instructions
    }

    pub fn skill_instructions_value(&self) -> Option<String> {
        self.config.skill_instructions.clone()
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

    pub fn max_tool_result_chars(&self) -> Option<usize> {
        self.config.max_tool_result_chars
    }

    pub fn max_concurrent_jobs(&self) -> Option<usize> {
        self.config.max_concurrent_jobs
    }

    pub fn compression_keep_last(&self) -> Option<usize> {
        self.config.compression_keep_last
    }

    pub fn compression_model(&self) -> Option<&str> {
        self.config.compression_model.as_deref()
    }

    pub fn brief_model(&self) -> Option<&str> {
        self.config.brief_model.as_deref()
    }

    pub fn envoy_model(&self) -> Option<&str> {
        self.config.envoy_model.as_deref()
    }

    pub fn hooks(&self) -> &HooksMap {
        &self.config.hooks
    }

    pub fn global_hooks(&self) -> &[String] {
        &self.config.global_hooks
    }

    pub fn is_dynamic_instructions(&self) -> bool {
        self.config.dynamic_instructions
    }

    pub fn update_shared_dynamic_instructions(
        &mut self,
        force: bool,
        tool_timeout: Option<u64>,
    ) -> Result<()> {
        if self.is_dynamic_instructions() && (force || self.shared_dynamic_instructions.is_none()) {
            self.shared_dynamic_instructions = Some(self.run_instructions_fn(tool_timeout)?);
        }
        Ok(())
    }

    pub fn update_session_dynamic_instructions(
        &mut self,
        value: Option<String>,
        tool_timeout: Option<u64>,
    ) -> Result<()> {
        if self.is_dynamic_instructions() {
            let value = match value {
                Some(v) => v,
                None => self.run_instructions_fn(tool_timeout)?,
            };
            self.session_dynamic_instructions = Some(value);
        }
        Ok(())
    }

    fn run_instructions_fn(&self, tool_timeout: Option<u64>) -> Result<String> {
        let value = run_llm_function(
            self.name().to_string(),
            vec!["_instructions".into(), "{}".into()],
            self.variable_envs(),
            Some(self.name().to_string()),
            tool_timeout,
            false,
            None,
        )?;
        match value {
            Some(v) => Ok(v),
            _ => bail!("No return value from '_instructions' function"),
        }
    }

    #[cfg(test)]
    pub fn test_new(config: AgentConfig) -> Self {
        Self {
            name: config.name.clone(),
            config,
            shared_variables: Default::default(),
            session_variables: None,
            shared_dynamic_instructions: None,
            session_dynamic_instructions: None,
            functions: Functions::default(),
            rag: None,
            graph_rags: Default::default(),
            model: Model::default(),
            vault: std::sync::Arc::new(Vault::default()),
            is_graph: false,
            enabled_tools: None,
        }
    }

    pub fn functions_mut(&mut self) -> &mut Functions {
        &mut self.functions
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

    fn reasoning_effort(&self) -> Option<String> {
        self.config.reasoning_effort.clone()
    }

    fn enabled_tools(&self) -> Option<Vec<String>> {
        self.enabled_tools.clone()
    }

    fn enabled_mcp_servers(&self) -> Option<Vec<String>> {
        Some(self.config.mcp_servers.clone())
    }

    fn mcp_tools(&self) -> Option<IndexMap<String, Vec<String>>> {
        self.config.mcp_tools.clone()
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

    fn set_reasoning_effort(&mut self, value: Option<String>) {
        self.config.reasoning_effort = value;
    }

    fn set_enabled_tools(&mut self, value: Option<Vec<String>>) {
        self.enabled_tools = value.map(|tools| {
            tools
                .into_iter()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect::<Vec<_>>()
        });
    }

    fn set_enabled_mcp_servers(&mut self, value: Option<Vec<String>>) {
        match value {
            Some(servers) => {
                self.config.mcp_servers = servers
                    .into_iter()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>();
            }
            None => {
                self.config.mcp_servers.clear();
            }
        }
    }

    fn set_mcp_tools(&mut self, value: Option<IndexMap<String, Vec<String>>>) {
        self.config.mcp_tools = value;
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
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<String>,
    #[serde(default)]
    pub auto_continue: bool,
    #[serde(default)]
    pub can_spawn_agents: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawnable_agents: Option<Vec<String>>,
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
    #[serde(default = "default_true")]
    pub inject_skill_instructions: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression_threshold: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_result_chars: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_jobs: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression_keep_last: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brief_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envoy_model: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_tools: Option<IndexMap<String, Vec<String>>>,
    #[serde(default)]
    pub global_tools: Vec<String>,
    #[serde(default)]
    pub hooks: HooksMap,
    #[serde(default)]
    pub global_hooks: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_macros: Option<Vec<String>>,
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

pub(crate) fn default_max_agent_depth() -> usize {
    3
}

fn default_true() -> bool {
    true
}

fn default_summarization_threshold() -> usize {
    4000
}

fn default_escalation_timeout() -> u64 {
    0
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
            reasoning_effort: graph.reasoning_effort.clone(),
            description: graph.description.clone(),
            global_tools: graph.global_tools.clone(),
            hooks: graph.hooks.clone(),
            global_hooks: graph.global_hooks.clone(),
            mcp_servers: graph.mcp_servers.clone(),
            mcp_tools: graph.mcp_tools.clone(),
            skills_enabled: graph.skills_enabled,
            enabled_skills: graph.enabled_skills.clone(),
            inject_skill_instructions: graph.inject_skill_instructions.unwrap_or(true),
            skill_instructions: graph.skill_instructions.clone(),
            conversation_starters: graph.conversation_starters.clone(),
            variables: graph.variables.clone(),
            can_spawn_agents: graph
                .can_spawn_agents
                .unwrap_or_else(|| graph.has_agent_node()),
            max_concurrent_agents: graph
                .max_concurrent_agents
                .unwrap_or_else(default_max_concurrent_agents),
            max_concurrent_jobs: graph.max_concurrent_jobs,
            max_agent_depth: graph
                .max_agent_depth
                .unwrap_or_else(default_max_agent_depth),
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
        if let Some(v) = read_env_value::<String>(&with_prefix("reasoning_effort")) {
            self.reasoning_effort = v;
        }
        if let Ok(v) = env::var(with_prefix("global_tools")) {
            match serde_json::from_str(&v) {
                Ok(v) => self.global_tools = v,
                Err(err) => {
                    debug!("Ignoring malformed global_tools env override for agent '{name}': {err}")
                }
            }
        }
        if let Ok(v) = env::var(with_prefix("global_hooks")) {
            match serde_json::from_str(&v) {
                Ok(v) => self.global_hooks = v,
                Err(err) => {
                    debug!("Ignoring malformed global_hooks env override for agent '{name}': {err}")
                }
            }
        }
        if let Ok(v) = env::var(with_prefix("mcp_servers"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.mcp_servers = v;
        }
        if let Ok(v) = env::var(with_prefix("spawnable_agents"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.spawnable_agents = Some(v);
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

/// How a graph rag node describes the knowledge base it wants built.
///
/// `driver` is forwarded as-is: `None` means the node did not ask for one, which
/// `RagInitConfig` resolves to yaml, so workflows written before drivers existed
/// keep their current storage.
///
/// Every field is now named explicitly, so adding one to `RagInitConfig` breaks
/// this literal. That is deliberate: the new field then gets a decision about
/// whether a rag node can drive it, instead of silently taking its default.
fn rag_init_config(rag_node: &RagNode) -> RagInitConfig {
    RagInitConfig {
        embedding_model: rag_node.embedding_model.clone(),
        chunk_size: rag_node.chunk_size,
        chunk_overlap: rag_node.chunk_overlap,
        reranker_model: rag_node.reranker_model.clone(),
        top_k: rag_node.top_k,
        batch_size: rag_node.batch_size,
        extractor_model: rag_node.extractor_model.clone(),
        extractor_prompt: rag_node.extractor_prompt.clone(),
        graph_hops: rag_node.graph_hops,
        driver: rag_node.driver.clone(),
    }
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
    sync_hooks: &RagSyncHooks,
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
            let vault_clone = app_state.vault.clone();
            let path_clone = rag_path.clone();
            let name_clone = node_id.clone();
            app_state
                .rag_cache
                .load_with(key, || async move {
                    Rag::load_async(&app_clone, &vault_clone, &name_clone, &path_clone).await
                })
                .await?
        } else {
            // Checked before anything is built: an unknown driver would otherwise
            // fall through `Rag::create`'s catch-all to a yaml store, embed every
            // document, and persist the bogus driver string. The RAG would then be
            // rejected on every subsequent load, leaving the agent unstartable.
            // Graph validation catches this too, but it is skipped when
            // `validate_before_run` is off, so this guard is the load-bearing one.
            if let Some(driver) = &rag_node.driver
                && let Some(message) = graph::validator::rag_driver_error(driver)
            {
                bail!("rag node '{node_id}': {message}");
            }
            let mut config = rag_init_config(rag_node);
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

                if config.driver.is_none() {
                    config.driver = Some(rag::select_rag_driver()?);
                }
            }

            let document_paths =
                resolve_document_paths(&rag_node.documents, loaders, agent_data_dir)?;
            let app_clone = app.clone();
            let path_clone = rag_path.clone();
            let name_clone = node_id.clone();
            let abort = abort_signal.clone();
            let sync_hooks = sync_hooks.clone();
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
                        sync_hooks,
                    )
                    .await
                })
                .await?
        };
        rags.insert(node_id.clone(), rag);
    }
    Ok(rags)
}

/// Lists user agents on disk. Directories carrying a reserved name are
/// skipped so a shadow `agents/envoy/` never reaches the LLM-facing
/// listing; the built-in is surfaced to humans by `list_agents_for_humans`.
pub fn list_agents() -> Vec<String> {
    let agents_data_dir = paths::agents_data_dir();
    if !agents_data_dir.exists() {
        return vec![];
    }

    let mut agents = Vec::new();
    if let Ok(entries) = read_dir(agents_data_dir) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            if validate_agent_name(name).is_err() {
                continue;
            }
            if let Some(canonical) = reserved_agent(name) {
                debug!(
                    "Skipping agent directory {}: the name is reserved for the built-in agent '{canonical}'",
                    entry.path().display()
                );
                continue;
            }
            agents.push(name.to_string());
        }
    }

    agents
}

pub fn list_agents_with_descriptions() -> Vec<(String, String)> {
    list_agents()
        .into_iter()
        .map(|name| {
            let description = load_agent_description(&name);
            (name, description)
        })
        .collect()
}

pub struct AgentListing {
    pub name: String,
    pub description: String,
    pub builtin: bool,
}

impl AgentListing {
    pub fn help_text(&self) -> String {
        match (self.builtin, self.description.is_empty()) {
            (true, true) => "(built-in)".to_string(),
            (true, false) => format!("(built-in) {}", self.description),
            (false, _) => self.description.clone(),
        }
    }

    pub fn help_option(&self) -> Option<String> {
        Some(self.help_text()).filter(|help| !help.is_empty())
    }

    pub fn list_line(&self) -> String {
        if self.builtin {
            format!("{}  (built-in)", self.name)
        } else {
            self.name.clone()
        }
    }
}

pub fn list_agents_for_humans() -> Vec<AgentListing> {
    let mut listings: Vec<AgentListing> = list_agents_with_descriptions()
        .into_iter()
        .map(|(name, description)| AgentListing {
            name,
            description,
            builtin: false,
        })
        .collect();
    listings.extend(RESERVED_AGENT_NAMES.iter().map(|name| AgentListing {
        name: name.to_string(),
        description: load_agent_description(name),
        builtin: true,
    }));
    listings
}

/// Agent names are joined onto `paths::agents_data_dir()`, so anything that
/// is not a single normal path component (`./envoy`, `envoy/`, `x/../envoy`)
/// could escape the directory or slip past `reserved_agent`.
pub fn validate_agent_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    let single_normal = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if name.is_empty() || !single_normal || name.contains(['/', '\\']) {
        bail!(
            "Agent name '{name}' is invalid: it must be a single path component \
             (no '/', '\\', '.' or '..')"
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct AgentMetadataStub {
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct AgentVariablesStub {
    #[serde(default)]
    variables: Vec<AgentVariable>,
}

fn load_agent_description(name: &str) -> String {
    if let Some(canonical) = reserved_agent(name) {
        return builtin_agent_description(canonical)
            .unwrap_or_else(|| builtin_default_description(canonical).to_string());
    }

    if let Ok(config) = AgentConfig::load(&paths::agent_config_file(name)) {
        return config.description;
    }

    if let Ok(contents) = read_to_string(paths::agent_graph_file(name))
        && let Ok(meta) = serde_yaml::from_str::<AgentMetadataStub>(&contents)
    {
        return meta.description;
    }

    String::new()
}

pub fn load_agent_variables(name: &str) -> Vec<AgentVariable> {
    if let Some(canonical) = reserved_agent(name) {
        return builtin_agent_dir(canonical)
            .and_then(|dir| AgentConfig::load(&dir.join(CONFIG_FILE_NAME)).ok())
            .map(|config| config.variables)
            .unwrap_or_default();
    }

    if let Ok(config) = AgentConfig::load(&paths::agent_config_file(name)) {
        return config.variables;
    }

    if let Ok(contents) = read_to_string(paths::agent_graph_file(name))
        && let Ok(stub) = serde_yaml::from_str::<AgentVariablesStub>(&contents)
    {
        return stub.variables;
    }

    Vec::new()
}

/// Sessions dir for a session completer to list. A reserved name resolves
/// through the registered built-in (or to `None` while unregistered) so a
/// shadow `agents/<name>/sessions` never surfaces as a completion.
pub fn agent_sessions_dir(name: &str) -> Option<PathBuf> {
    let dir = match reserved_agent(name) {
        Some(canonical) => builtin_agent_dir(canonical)?,
        None => paths::agent_data_dir(name),
    };
    Some(dir.join(SESSIONS_DIR_NAME))
}

pub fn complete_agent_variables(agent_name: &str) -> Vec<(String, Option<String>)> {
    load_agent_variables(agent_name)
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
    use crate::testing;

    #[test]
    fn parse_direct_hook_path_accepts_only_direct_hook_children() {
        assert_eq!(parse_direct_hook_path("x/hooks/a.sh"), Some(("x", "a.sh")));
        assert_eq!(parse_direct_hook_path("x/hooks/nested/a.sh"), None);
        assert_eq!(parse_direct_hook_path("hooks/a.sh"), None);
        assert_eq!(parse_direct_hook_path("x/hooks/"), None);
        assert_eq!(parse_direct_hook_path("x/config.yaml"), None);
        assert_eq!(parse_direct_hook_path("config.yaml"), None);
    }

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
    fn agent_config_hooks_and_global_hooks_round_trip() {
        let yaml = "\
name: hooked
instructions: hi
hooks:
  tool.started:
    - name: notify
      command: ./hooks/notify.sh
global_hooks:
  - tool.started.notify
";
        let config: AgentConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.hooks["tool.started"][0].name, "notify");
        assert_eq!(config.hooks["tool.started"][0].command, "./hooks/notify.sh");
        assert_eq!(config.global_hooks, vec!["tool.started.notify"]);

        let serialized = serde_yaml::to_string(&config).unwrap();
        let reparsed: AgentConfig = serde_yaml::from_str(&serialized).unwrap();

        assert_eq!(reparsed.hooks, config.hooks);
        assert_eq!(reparsed.global_hooks, config.global_hooks);
    }

    #[test]
    fn load_envs_overrides_global_hooks() {
        let yaml = "name: hooks-env-probe\ninstructions: hi\nglobal_hooks:\n  - initial.hook\n";
        let mut config: AgentConfig = serde_yaml::from_str(yaml).unwrap();
        let env_name = normalize_env_name("hooks-env-probe_global_hooks");
        let prev = env::var_os(&env_name);

        unsafe {
            env::set_var(
                &env_name,
                r#"["tool.started.notify","turn.completed.webhook"]"#,
            )
        };
        config.load_envs(&AppConfig::default());
        assert_eq!(
            config.global_hooks,
            vec!["tool.started.notify", "turn.completed.webhook"]
        );

        unsafe { env::set_var(&env_name, "not json") };
        config.load_envs(&AppConfig::default());
        assert_eq!(
            config.global_hooks,
            vec!["tool.started.notify", "turn.completed.webhook"]
        );

        unsafe {
            match prev {
                Some(v) => env::set_var(&env_name, v),
                None => env::remove_var(&env_name),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn load_envs_ignores_malformed_global_tools_and_logs_debug() {
        testing::install_log_collector();
        let name = "gt-malformed-envtest";
        let yaml = format!("name: {name}\ninstructions: hi\nglobal_tools:\n  - keep_tool.sh\n");
        let mut config: AgentConfig = serde_yaml::from_str(&yaml).unwrap();
        let env_name = normalize_env_name(&format!("{name}_global_tools"));
        let _guard = testing::EnvVarGuard::set(&env_name, "{not-json");

        config.load_envs(&AppConfig::default());

        assert_eq!(
            config.global_tools,
            vec!["keep_tool.sh"],
            "a malformed override must leave the existing value untouched"
        );
        let debugs = testing::debug_snapshot();
        assert!(
            debugs.iter().any(|message| {
                message.contains("Ignoring malformed global_tools env override")
                    && message.contains(name)
            }),
            "expected a debug log for the malformed override: {debugs:?}"
        );
    }

    #[test]
    fn from_graph_carries_hooks_and_global_hooks() {
        let yaml = "\
name: g
start: e
hooks:
  turn.completed:
    - name: webhook
      command: curl -s https://example.com/hook
global_hooks:
  - turn.completed.webhook
nodes:
  e:
    id: e
    type: end
    output: done
";
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();

        let config = AgentConfig::from_graph("g", &graph);

        assert_eq!(config.hooks, graph.hooks);
        assert_eq!(config.global_hooks, graph.global_hooks);
        assert_eq!(config.hooks["turn.completed"][0].name, "webhook");
        assert_eq!(config.global_hooks, vec!["turn.completed.webhook"]);
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
        assert_eq!(config.escalation_timeout, 0);
        assert!(config.mcp_servers.is_empty());
        assert!(config.global_tools.is_empty());
        assert!(config.hooks.is_empty());
        assert!(config.global_hooks.is_empty());
        assert!(config.conversation_starters.is_empty());
        assert!(config.variables.is_empty());
        assert!(config.model_id.is_none());
        assert!(config.temperature.is_none());
        assert!(config.top_p.is_none());
    }

    #[test]
    fn agent_config_enabled_macros_absent_is_none() {
        let yaml = "name: minimal\ninstructions: hi\n";
        let config: AgentConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.enabled_macros, None);
    }

    #[test]
    fn agent_config_parses_mcp_tools() {
        let yaml =
            "name: minimal\ninstructions: hi\nmcp_tools:\n  github:\n    - get_*\n    - list_*\n";
        let config: AgentConfig = serde_yaml::from_str(yaml).unwrap();

        let mcp_tools = config.mcp_tools.unwrap();
        assert_eq!(
            mcp_tools.get("github"),
            Some(&vec!["get_*".to_string(), "list_*".to_string()])
        );
    }

    #[test]
    fn agent_mcp_tools_role_like_round_trip() {
        let config: AgentConfig =
            serde_yaml::from_str("name: minimal\ninstructions: hi\n").unwrap();
        let mut agent = Agent::test_new(config);
        assert_eq!(agent.mcp_tools(), None);

        let mut mcp_tools = IndexMap::new();
        mcp_tools.insert("github".to_string(), vec!["get_*".to_string()]);
        agent.set_mcp_tools(Some(mcp_tools.clone()));

        assert_eq!(agent.mcp_tools(), Some(mcp_tools));
    }

    #[test]
    fn agent_config_enabled_macros_empty_list_is_some_empty() {
        let yaml = "name: minimal\ninstructions: hi\nenabled_macros: []\n";
        let config: AgentConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.enabled_macros, Some(vec![]));
    }

    #[test]
    fn agent_config_enabled_macros_list() {
        let yaml = "name: minimal\ninstructions: hi\nenabled_macros:\n  - a\n";
        let config: AgentConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.enabled_macros, Some(vec!["a".to_string()]));
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
            max_concurrent_jobs: 2
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
        assert_eq!(config.max_concurrent_jobs, Some(2));
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

    #[test]
    fn from_graph_explicit_can_spawn_agents_enables_spawning_without_agent_nodes() {
        let yaml = formatdoc! {r#"
            name: g
            can_spawn_agents: true
            start: x
            nodes:
              x:
                id: x
                type: end
                output: ok
            "#};
        let graph: Graph = serde_yaml::from_str(&yaml).unwrap();

        assert!(AgentConfig::from_graph("d", &graph).can_spawn_agents);
    }

    #[test]
    fn from_graph_explicit_can_spawn_agents_false_wins_over_agent_nodes() {
        let yaml = formatdoc! {r#"
            name: g
            can_spawn_agents: false
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
        let graph: Graph = serde_yaml::from_str(&yaml).unwrap();

        assert!(!AgentConfig::from_graph("d", &graph).can_spawn_agents);
    }

    #[test]
    fn from_graph_propagates_explicit_agent_limits() {
        let yaml = formatdoc! {r#"
            name: g
            max_concurrent_agents: 7
            max_agent_depth: 3
            start: x
            nodes:
              x:
                id: x
                type: end
                output: ok
            "#};
        let graph: Graph = serde_yaml::from_str(&yaml).unwrap();

        let config = AgentConfig::from_graph("d", &graph);

        assert_eq!(config.max_concurrent_agents, 7);
        assert_eq!(config.max_agent_depth, 3);
    }

    #[test]
    fn agent_metadata_stub_extracts_description_from_graph_yaml() {
        let yaml = r#"
name: librarian
description: External-reference research agent.
version: "1.0"
start: triage
nodes: {}
"#;

        let meta: AgentMetadataStub = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(meta.description, "External-reference research agent.");
    }

    #[test]
    fn agent_metadata_stub_extracts_multiline_description() {
        let yaml = r#"
name: coder
description: |
  Implementation agent. Plans, implements, and runs build + tests in a
  bounded fix-loop until verified.
version: "1.0"
"#;

        let meta: AgentMetadataStub = serde_yaml::from_str(yaml).unwrap();

        assert!(meta.description.starts_with("Implementation agent."));
        assert!(meta.description.contains("bounded fix-loop"));
    }

    #[test]
    fn agent_metadata_stub_defaults_when_description_missing() {
        let yaml = "name: nameless\nversion: \"1.0\"\n";

        let meta: AgentMetadataStub = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(meta.description, "");
    }

    #[test]
    fn agent_variables_stub_extracts_variables_from_graph_yaml() {
        let yaml = r#"
name: coder
description: Implementation agent.
version: "1.0"
variables:
  - name: task
    description: The task to implement
  - name: scope
    description: Directory scope
    default: src/
start: plan
nodes: {}
"#;

        let stub: AgentVariablesStub = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(stub.variables.len(), 2);
        assert_eq!(stub.variables[0].name, "task");
        assert_eq!(stub.variables[0].description, "The task to implement");
        assert_eq!(stub.variables[0].default, None);
        assert_eq!(stub.variables[1].name, "scope");
        assert_eq!(stub.variables[1].default.as_deref(), Some("src/"));
    }

    #[test]
    fn agent_variables_stub_defaults_when_variables_missing() {
        let yaml = "name: coder\nversion: \"1.0\"\nstart: plan\nnodes: {}\n";

        let stub: AgentVariablesStub = serde_yaml::from_str(yaml).unwrap();

        assert!(stub.variables.is_empty());
    }

    #[test]
    fn rag_init_config_forwards_an_explicit_driver() {
        let node: RagNode =
            serde_yaml::from_str("documents: [\"./docs\"]\ndriver: duckdb\n").unwrap();

        assert_eq!(rag_init_config(&node).driver.as_deref(), Some("duckdb"));
    }

    /// A node that names no driver must forward `None`, which `RagInitConfig`
    /// documents as "yaml". Existing workflows therefore keep their yaml store.
    #[test]
    fn rag_init_config_leaves_the_driver_unset_by_default() {
        let node: RagNode = serde_yaml::from_str("documents: [\"./docs\"]\n").unwrap();

        assert_eq!(rag_init_config(&node).driver, None);
    }

    /// The driver must ride alongside the rest of the node's settings, not
    /// replace them.
    #[test]
    fn rag_init_config_forwards_the_other_settings_too() {
        let node: RagNode = serde_yaml::from_str(
            "documents: [\"./docs\"]\ndriver: duckdb\nchunk_size: 512\nchunk_overlap: 64\ntop_k: 7\nembedding_model: some:model\n",
        )
        .unwrap();

        let config = rag_init_config(&node);

        assert_eq!(config.driver.as_deref(), Some("duckdb"));
        assert_eq!(config.chunk_size, Some(512));
        assert_eq!(config.chunk_overlap, Some(64));
        assert_eq!(config.top_k, Some(7));
        assert_eq!(config.embedding_model.as_deref(), Some("some:model"));
    }

    #[test]
    fn interpolated_instructions_without_job_declarations_is_byte_identical_across_job_settings() {
        let agent = |max_concurrent_jobs| {
            Agent::test_new(AgentConfig {
                instructions: "hi".to_string(),
                max_concurrent_jobs,
                ..AgentConfig::default()
            })
        };

        let baseline = agent(None).interpolated_instructions();
        assert!(
            !baseline.contains(DEFAULT_JOB_INSTRUCTIONS),
            "no job guidance may be injected without job__ declarations"
        );
        assert_eq!(baseline, agent(Some(0)).interpolated_instructions());
        assert_eq!(baseline, agent(Some(7)).interpolated_instructions());

        let mut with_unrelated = agent(None);
        with_unrelated.functions.append_todo_functions();
        assert_eq!(
            baseline,
            with_unrelated.interpolated_instructions(),
            "job guidance injection must key strictly on the job__ prefix"
        );
    }

    #[test]
    fn interpolated_instructions_with_job_declarations_appends_job_guidance() {
        let config = AgentConfig {
            instructions: "hi".to_string(),
            ..AgentConfig::default()
        };
        let baseline = Agent::test_new(config.clone()).interpolated_instructions();

        let mut agent = Agent::test_new(config);
        agent.functions.append_job_functions();
        let output = agent.interpolated_instructions();

        assert!(output.contains(DEFAULT_JOB_INSTRUCTIONS));
        let expected = format!(
            "hi\n{DEFAULT_JOB_INSTRUCTIONS}{}",
            baseline.strip_prefix("hi").unwrap()
        );
        assert_eq!(output, expected);
    }

    use crate::testing::TestConfigDirGuard;

    fn fixture(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(name, content)| (name.to_string(), content.to_string()))
            .collect()
    }

    #[test]
    #[serial_test::serial]
    fn agent_hooks_prompt_mode_asks_per_conflict_and_honors_sticky() {
        use crate::config::conflict::prompt_script;

        let _guard = TestConfigDirGuard::new("agent-hooks-prompt");
        let shipped = fixture(&[("a.sh", "new content\n"), ("b.sh", "new content\n")]);
        let dir = paths::agents_data_dir().join("probe").join("hooks");
        create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.sh"), "local a").unwrap();
        fs::write(dir.join("b.sh"), "local b").unwrap();

        // First conflict answered per-file, second by the sticky replace-all.
        let script = prompt_script::install(&["replace-all"]);
        let mut sticky = StickyMode::None;
        install_and_reconcile_agent_hooks("probe", &shipped, InstallMode::Prompt, &mut sticky)
            .unwrap();
        assert_eq!(prompt_script::prompts_asked(), 1);
        assert_eq!(read_to_string(dir.join("a.sh")).unwrap(), "new content\n");
        assert_eq!(read_to_string(dir.join("b.sh")).unwrap(), "new content\n");
        assert_eq!(sticky, StickyMode::ReplaceAll);
        assert_eq!(
            read_to_string(dir.join(builtin_manifest::BUILTIN_MANIFEST_FILE)).unwrap(),
            "a.sh\nb.sh\n"
        );
        drop(script);

        // A sticky mode carried in from an earlier location suppresses all
        // prompting: `--install-builtins hooks` shares one sticky scope
        // across the global, role, and agent hook locations.
        fs::write(dir.join("a.sh"), "local again").unwrap();
        let script = prompt_script::install(&[]);
        let mut sticky = StickyMode::KeepAll;
        install_and_reconcile_agent_hooks("probe", &shipped, InstallMode::Prompt, &mut sticky)
            .unwrap();
        assert_eq!(prompt_script::prompts_asked(), 0);
        assert_eq!(read_to_string(dir.join("a.sh")).unwrap(), "local again");
        drop(script);
    }

    #[test]
    #[serial_test::serial]
    fn agent_hooks_reconcile_adds_updates_and_drops_within_a_shipped_agent() {
        let _guard = TestConfigDirGuard::new("agent-hooks-reconcile");
        let dir = paths::agents_data_dir().join("probe").join("hooks");

        install_and_reconcile_agent_hooks(
            "probe",
            &fixture(&[("keep.sh", "v1\n"), ("drop.sh", "#!/bin/sh\n")]),
            InstallMode::Skip,
            &mut StickyMode::None,
        )
        .unwrap();
        fs::write(dir.join("user.sh"), "user-owned").unwrap();

        install_and_reconcile_agent_hooks(
            "probe",
            &fixture(&[("keep.sh", "v2\n"), ("added.sh", "#!/bin/sh\n")]),
            InstallMode::Force,
            &mut StickyMode::None,
        )
        .unwrap();

        assert_eq!(
            read_to_string(dir.join("keep.sh")).unwrap(),
            "v2\n",
            "a still-shipped hook updates under Force"
        );
        assert_eq!(
            read_to_string(dir.join("added.sh")).unwrap(),
            "#!/bin/sh\n",
            "a newly shipped hook is added"
        );
        assert!(
            !dir.join("drop.sh").exists(),
            "a dropped hook reconciles away"
        );
        assert_eq!(
            read_to_string(dir.join("user.sh")).unwrap(),
            "user-owned",
            "user files in the same dir survive"
        );
        assert_eq!(
            read_to_string(dir.join(builtin_manifest::BUILTIN_MANIFEST_FILE)).unwrap(),
            "added.sh\nkeep.sh\n"
        );
    }

    #[test]
    #[serial_test::serial]
    fn install_builtin_agent_hooks_reconciles_hookless_bundled_agents() {
        let _guard = TestConfigDirGuard::new("agent-hooks-hookless");
        let agent = AgentAssets::iter()
            .filter_map(|file| {
                file.as_ref()
                    .split_once('/')
                    .map(|(agent, _)| agent.to_string())
            })
            .find(|agent| {
                !AgentAssets::iter().any(|file| {
                    parse_direct_hook_path(file.as_ref())
                        .is_some_and(|(hook_agent, _)| hook_agent == agent)
                })
            })
            .expect("at least one bundled agent ships no hooks");
        let dir = paths::agents_data_dir().join(&agent).join("hooks");
        // A manifest-owned hook left over from a release that shipped it.
        install_and_reconcile_agent_hooks(
            &agent,
            &fixture(&[("stale.sh", "#!/bin/sh\n")]),
            InstallMode::Skip,
            &mut StickyMode::None,
        )
        .unwrap();
        fs::write(dir.join("user.sh"), "user-owned").unwrap();

        Agent::install_builtin_agent_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();

        assert!(
            !dir.join("stale.sh").exists(),
            "a still-bundled agent with no hook assets must reconcile its hooks dir"
        );
        assert_eq!(read_to_string(dir.join("user.sh")).unwrap(), "user-owned");
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn sweep_removed_agent_hooks_never_follows_symlinked_agent_dirs() {
        let guard = TestConfigDirGuard::new("agent-sweep-symlink");
        let real = guard.path.join("agent-elsewhere");
        let hooks = real.join("hooks");
        create_dir_all(&hooks).unwrap();
        fs::write(
            hooks.join(builtin_manifest::BUILTIN_MANIFEST_FILE),
            "shipped.sh\n",
        )
        .unwrap();
        fs::write(hooks.join("shipped.sh"), "stale").unwrap();
        create_dir_all(paths::agents_data_dir()).unwrap();
        std::os::unix::fs::symlink(&real, paths::agents_data_dir().join("linked")).unwrap();

        sweep_removed_agent_hooks(&HashMap::new());

        assert!(
            hooks.join("shipped.sh").exists(),
            "the sweep must never follow a symlinked agent dir"
        );
        assert!(hooks.join(builtin_manifest::BUILTIN_MANIFEST_FILE).exists());
    }

    use crate::config::reserved_agents::{
        BuiltinAgentSource, BuiltinSourceGuard, ENVOY_AGENT_NAME, ENVOY_BUILTIN_DESCRIPTION,
        RESERVED_AGENT_NAMES,
    };
    use crate::testing::{EnvVarGuard, install_log_collector, warn_snapshot};

    const SHADOW_MARKER: &str = "shadow-model-XYZ";

    /// Unlike `reserved_agents::FixedDirSource`, this one also answers
    /// `description`, so tests can tell a source-provided description from
    /// the built-in default.
    struct FixedDirSource(PathBuf);

    impl BuiltinAgentSource for FixedDirSource {
        fn agent_dir(&self, _name: &str) -> Option<PathBuf> {
            Some(self.0.clone())
        }

        fn description(&self, _name: &str) -> Option<String> {
            Some("description from source".to_string())
        }
    }

    fn run_async<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn write_shadow_envoy_config(extra: &str) {
        let shadow = paths::agents_data_dir().join("envoy");
        create_dir_all(&shadow).unwrap();
        fs::write(
            shadow.join(CONFIG_FILE_NAME),
            format!("name: envoy\ninstructions: hi\nmodel: {SHADOW_MARKER}\n{extra}"),
        )
        .unwrap();
    }

    fn init_envoy() -> Result<Agent> {
        init_named("envoy")
    }

    fn init_named(name: &str) -> Result<Agent> {
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let app_config = Arc::clone(&ctx.app.config);
        let model = ctx.current_model().clone();
        run_async(Agent::init(
            app_config.as_ref(),
            ctx.app.as_ref(),
            &model,
            false,
            name,
            create_abort_signal(),
        ))
    }

    #[test]
    #[serial_test::serial]
    fn reserved_agent_shadow_config_is_refused() {
        let _guard = TestConfigDirGuard::new("reserved-shadow");
        let _data_dir = EnvVarGuard::unset("ENVOY_DATA_DIR");
        let _config_file = EnvVarGuard::unset("ENVOY_CONFIG_FILE");
        write_shadow_envoy_config("");

        let err = init_envoy().expect_err("shadow config must not load");

        assert!(err.downcast_ref::<BuiltinAgentUnavailable>().is_some());
        assert!(err.to_string().contains("built in"), "{err}");
        assert!(!format!("{err:?}").contains(SHADOW_MARKER));
    }

    #[test]
    #[serial_test::serial]
    fn path_shaped_agent_names_never_reach_shadow_config() {
        let _guard = TestConfigDirGuard::new("reserved-path-shaped");
        let _data_dir = EnvVarGuard::unset("ENVOY_DATA_DIR");
        let _config_file = EnvVarGuard::unset("ENVOY_CONFIG_FILE");
        write_shadow_envoy_config("");

        let err = init_named("envoy/").expect_err("trailing separator must be refused");
        assert!(!format!("{err:?}").contains(SHADOW_MARKER));

        let err = init_named("../evil").expect_err("parent traversal must be refused");
        assert!(err.to_string().contains("is invalid"), "{err}");
        assert!(!format!("{err:?}").contains(SHADOW_MARKER));
    }

    #[test]
    fn validate_agent_name_accepts_only_single_components() {
        for name in ["envoy", "my-agent", "agent_2", ".hidden"] {
            validate_agent_name(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        for name in [
            "",
            ".",
            "..",
            "./envoy",
            "envoy/",
            "envoy\\",
            "x/../envoy",
            "a/b",
        ] {
            assert!(
                validate_agent_name(name).is_err(),
                "{name:?} must be invalid"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn reserved_agent_resolves_through_registered_source() {
        let guard = TestConfigDirGuard::new("reserved-source");
        let dir = guard.path.join("builtin-envoy");
        create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(CONFIG_FILE_NAME),
            "name: envoy\ninstructions: from-builtin-source\n",
        )
        .unwrap();
        let _data_dir = EnvVarGuard::unset("ENVOY_DATA_DIR");
        let _config_file = EnvVarGuard::unset("ENVOY_CONFIG_FILE");
        let _source = BuiltinSourceGuard::new(Arc::new(FixedDirSource(dir.clone())));
        write_shadow_envoy_config("");

        let agent = init_envoy().unwrap();

        assert!(agent.config.instructions.contains("from-builtin-source"));
        assert!(!format!("{:?}", agent.config).contains(SHADOW_MARKER));

        let agent = init_named("En-Voy").unwrap();

        assert_eq!(agent.name(), ENVOY_AGENT_NAME);
        assert!(agent.config.instructions.contains("from-builtin-source"));
        assert!(!format!("{:?}", agent.config).contains(SHADOW_MARKER));
    }

    #[test]
    #[serial_test::serial]
    fn reserved_agent_registered_inside_user_agents_dir_is_refused() {
        let _guard = TestConfigDirGuard::new("reserved-inside-user-dir");
        let dir = paths::agents_data_dir().join("envoy");
        let _data_dir = EnvVarGuard::set("ENVOY_DATA_DIR", &dir);
        let _config_file = EnvVarGuard::unset("ENVOY_CONFIG_FILE");
        let _source = BuiltinSourceGuard::new(Arc::new(FixedDirSource(dir)));
        write_shadow_envoy_config("");

        let err = init_envoy().expect_err("a dir inside the user agents dir must be refused");

        assert!(
            err.to_string().contains("inside the user agents dir"),
            "{err}"
        );
        assert!(!format!("{err:?}").contains(SHADOW_MARKER));
    }

    #[test]
    #[serial_test::serial]
    fn reserved_agent_init_ignores_env_overrides() {
        let guard = TestConfigDirGuard::new("reserved-env-ignored");
        let dir = guard.path.join("builtin-envoy");
        create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(CONFIG_FILE_NAME),
            "name: envoy\ninstructions: from-builtin-source\n",
        )
        .unwrap();
        let shadow = paths::agents_data_dir().join("envoy");
        let _data_dir = EnvVarGuard::set("ENVOY_DATA_DIR", &shadow);
        let _config_file = EnvVarGuard::set("ENVOY_CONFIG_FILE", shadow.join(CONFIG_FILE_NAME));
        let _source = BuiltinSourceGuard::new(Arc::new(FixedDirSource(dir)));
        write_shadow_envoy_config("");

        let agent = init_envoy().unwrap();

        assert!(agent.config.instructions.contains("from-builtin-source"));
        assert!(!format!("{:?}", agent.config).contains(SHADOW_MARKER));
    }

    #[test]
    #[serial_test::serial]
    fn reserved_agent_description_never_reads_shadow_config() {
        let _guard = TestConfigDirGuard::new("reserved-description");
        let _data_dir = EnvVarGuard::unset("ENVOY_DATA_DIR");
        let _config_file = EnvVarGuard::unset("ENVOY_CONFIG_FILE");
        write_shadow_envoy_config("description: shadow description\n");

        assert_eq!(load_agent_description("envoy"), ENVOY_BUILTIN_DESCRIPTION);

        let _source = BuiltinSourceGuard::new(Arc::new(FixedDirSource(PathBuf::from("unused"))));
        assert_eq!(load_agent_description("envoy"), "description from source");
    }

    #[test]
    #[serial_test::serial]
    fn reserved_agent_variables_never_read_shadow_config() {
        let guard = TestConfigDirGuard::new("reserved-variables");
        let _data_dir = EnvVarGuard::unset("ENVOY_DATA_DIR");
        let _config_file = EnvVarGuard::unset("ENVOY_CONFIG_FILE");
        write_shadow_envoy_config("variables:\n  - name: leaked\n    description: leaked\n");

        assert!(load_agent_variables("envoy").is_empty());

        let dir = guard.path.join("builtin-envoy");
        create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(CONFIG_FILE_NAME),
            "name: envoy\ninstructions: hi\nvariables:\n  - name: peer\n    description: peer\n",
        )
        .unwrap();
        let _source = BuiltinSourceGuard::new(Arc::new(FixedDirSource(dir)));

        let names: Vec<String> = load_agent_variables("Envoy")
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(names, vec!["peer".to_string()]);
    }

    fn reserved_envoy_warnings() -> Vec<String> {
        warn_snapshot()
            .into_iter()
            .filter(|m| {
                m.starts_with("Ignoring bundled agent file")
                    && m.contains("'envoy'")
                    && m.contains("reserved")
            })
            .collect()
    }

    #[test]
    #[serial_test::serial]
    fn install_agent_files_skips_reserved_agent_names() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("install-reserved");
        let before = reserved_envoy_warnings().len();
        let files = vec![
            (
                "envoy/config.yaml".to_string(),
                b"name: envoy\ninstructions: hi\n".to_vec(),
            ),
            ("Envoy/hooks/pre.sh".to_string(), b"#!/bin/sh\n".to_vec()),
            ("envoy".to_string(), b"name: envoy\n".to_vec()),
            (
                "probe-agent/config.yaml".to_string(),
                b"name: probe-agent\ninstructions: hi\n".to_vec(),
            ),
        ];

        install_agent_files(files, InstallMode::Skip).unwrap();

        assert!(
            paths::agents_data_dir()
                .join("probe-agent")
                .join(CONFIG_FILE_NAME)
                .exists()
        );
        assert!(
            !paths::agents_data_dir().join("envoy").exists(),
            "neither an envoy dir nor a bare envoy file may be installed"
        );
        assert!(!paths::agents_data_dir().join("Envoy").exists());
        let warns = reserved_envoy_warnings();
        assert_eq!(
            warns.len(),
            before + 1,
            "one warning per reserved agent, not per file: {warns:?}"
        );
    }

    #[test]
    fn bundled_agent_assets_contain_no_reserved_names() {
        assert!(
            AgentAssets::iter().all(|f| f.split('/').next().and_then(reserved_agent).is_none()),
            "bundled agent assets must never ship under a reserved agent name"
        );
    }

    #[test]
    #[serial_test::serial]
    fn install_agent_hook_files_skips_reserved_agent_names() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("install-hooks-reserved");
        let before = reserved_envoy_warnings().len();
        let files = vec![
            ("envoy/hooks/pre.sh".to_string(), b"#!/bin/sh\n".to_vec()),
            ("Envoy/hooks/post.sh".to_string(), b"#!/bin/sh\n".to_vec()),
            (
                "probe-agent/hooks/pre.sh".to_string(),
                b"#!/bin/sh\n".to_vec(),
            ),
        ];

        install_agent_hook_files(files, InstallMode::Skip, &mut StickyMode::None).unwrap();

        assert!(
            paths::agents_data_dir()
                .join("probe-agent")
                .join("hooks")
                .join("pre.sh")
                .exists()
        );
        assert!(!paths::agents_data_dir().join("envoy").exists());
        assert!(!paths::agents_data_dir().join("Envoy").exists());
        let warns = reserved_envoy_warnings();
        assert_eq!(
            warns.len(),
            before + 1,
            "one warning per reserved agent, not per file: {warns:?}"
        );
    }

    fn write_agent_config(name: &str, extra: &str) {
        let dir = paths::agents_data_dir().join(name);
        create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(CONFIG_FILE_NAME),
            format!("name: {name}\ninstructions: hi\n{extra}"),
        )
        .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn list_agents_drops_reserved_directories() {
        let _guard = TestConfigDirGuard::new("list-reserved");
        write_agent_config("envoy", "");
        write_agent_config("Envoy", "");
        write_agent_config("other", "");

        assert_eq!(list_agents(), vec!["other".to_string()]);
        let names: Vec<String> = list_agents_with_descriptions()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["other".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn list_agents_skips_names_validate_agent_name_rejects() {
        let _guard = TestConfigDirGuard::new("list-invalid-names");
        let agents_dir = paths::agents_data_dir();
        create_dir_all(agents_dir.join("good")).unwrap();
        create_dir_all(agents_dir.join("bad\\name")).unwrap();

        assert_eq!(list_agents(), vec!["good".to_string()]);
    }

    #[test]
    #[serial_test::serial]
    fn list_agents_for_humans_appends_builtin_without_agents_dir() {
        let _guard = TestConfigDirGuard::new("humans-fresh");
        assert!(!paths::agents_data_dir().exists());

        let listings = list_agents_for_humans();

        assert_eq!(listings.len(), RESERVED_AGENT_NAMES.len());
        let envoy = listings
            .iter()
            .find(|l| l.name == ENVOY_AGENT_NAME)
            .unwrap();
        assert!(envoy.builtin);
        assert_eq!(envoy.description, ENVOY_BUILTIN_DESCRIPTION);
        assert_eq!(
            envoy.help_text(),
            format!("(built-in) {ENVOY_BUILTIN_DESCRIPTION}")
        );
        assert_eq!(envoy.list_line(), "envoy  (built-in)");
    }

    #[test]
    #[serial_test::serial]
    fn list_agents_for_humans_uses_registered_source_description() {
        let _guard = TestConfigDirGuard::new("humans-source");
        let _source = BuiltinSourceGuard::new(Arc::new(FixedDirSource(PathBuf::from("unused"))));

        let listings = list_agents_for_humans();

        assert_eq!(listings.len(), RESERVED_AGENT_NAMES.len());
        let envoy = listings
            .iter()
            .find(|l| l.name == ENVOY_AGENT_NAME)
            .unwrap();
        assert_eq!(envoy.description, "description from source");
    }

    #[test]
    #[serial_test::serial]
    fn list_agents_for_humans_ignores_shadow_reserved_dir() {
        let _guard = TestConfigDirGuard::new("humans-shadow");
        write_agent_config("envoy", "description: shadow-desc-XYZ\n");
        write_agent_config("other", "description: other-desc\n");

        let listings = list_agents_for_humans();

        let names: Vec<&str> = listings.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["other", "envoy"]);
        let envoy = listings.iter().find(|l| l.name == "envoy").unwrap();
        assert!(envoy.builtin);
        assert!(!envoy.description.contains("shadow-desc-XYZ"));
        let other = listings.iter().find(|l| l.name == "other").unwrap();
        assert!(!other.builtin);
        assert_eq!(other.help_text(), "other-desc");
        assert_eq!(other.list_line(), "other");
    }

    #[test]
    fn agent_listing_help_text_for_builtin_without_description() {
        let listing = AgentListing {
            name: "envoy".to_string(),
            description: String::new(),
            builtin: true,
        };
        assert_eq!(listing.help_text(), "(built-in)");
    }
}
