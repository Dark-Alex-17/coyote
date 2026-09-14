use super::bundles::installed_bundle_names;
use super::mcp_tool_policy::{McpToolPolicy, SkillMcpLayer, ToolFilter, expand_mcp_server_alias};
use super::rag_cache::{RagCache, RagKey};
use super::session::{INTERRUPTED_RESPONSE_TEXT, Session};
use super::skill::{SKILL_SCAFFOLD, Skill};
use super::skill_policy::SkillPolicy;
use super::skill_registry::SkillRegistry;
use super::todo::TodoList;
use super::tool_scope::{McpPromptCompletion, McpRuntime, ToolScope, format_prompt_arguments};
use super::{
    AGENTS_DIR_NAME, Agent, AgentVariables, AppConfig, AppState, AssetCategory, CREATE_TITLE_ROLE,
    Input, InstallFilter, LEFT_PROMPT, LastMessage, MESSAGES_FILE_NAME, MacroAllowlistLevel,
    MacroPolicy, MacroSource, MacroState, RESERVED_MACRO_NAMES, RIGHT_PROMPT, ResolvedMacro, Role,
    RoleLike, SESSIONS_DIR_NAME, SUMMARIZATION_PROMPT, SUMMARY_CONTEXT_PROMPT, StateFlags,
    TEMP_ROLE_NAME, TEMP_SESSION_NAME, WorkingMode, bundles, ensure_parent_exists,
    list_agents_with_descriptions, memory, paths,
};
use super::{MessageContentToolCalls, prompts};
use crate::client::{Model, ModelType, list_models};
use crate::function::{
    FunctionDeclaration, Functions, ToolCallTracker, ToolResult,
    agents::AGENT_FUNCTION_PREFIX,
    jobs::{DEFAULT_MAX_CONCURRENT_JOBS, JOB_FUNCTION_PREFIX, is_backgroundable_tool},
    memory::MEMORY_FUNCTION_PREFIX,
    rag_query::RAG_FUNCTION_PREFIX,
    skill::SKILL_FUNCTION_PREFIX,
    todo::TODO_FUNCTION_PREFIX,
    user_interaction::USER_FUNCTION_PREFIX,
};
use crate::mcp::{
    CatalogItem, MCP_SEARCH_META_FUNCTION_NAME_PREFIX, McpAuthReason, McpAuthRequired,
    McpServerFeatures, McpServersConfig, McpTransportType, is_auth_required_error,
    is_mcp_meta_function, mcp_meta_function_names,
};
use crate::rag::Rag;
use crate::supervisor::Supervisor;
use crate::supervisor::escalation::EscalationQueue;
use crate::supervisor::mailbox::{Inbox, PeerRegistry};
use crate::supervisor::notification::NotificationQueue;
use crate::utils::{
    AbortSignal, abortable_run_with_spinner, edit_file, fuzzy_filter, get_env_name,
    list_file_names, now, render_prompt, temp_file,
};
use comfy_table::{ContentArrangement, Table, presets::UTF8_FULL};

use super::install_remote::DEFAULT_GIT_HOST;
use super::instructions;
use super::macros::Macro;
use super::memory::{
    DEFAULT_MEMORY_CAP_WITH_TOOLS, DEFAULT_MEMORY_CAP_WITHOUT_TOOLS, MemoryStore, WorkspaceMemory,
};
use crate::graph;
use anyhow::{Context, Error, Result, bail};
use colored::Colorize;
use gman::providers::SupportedProvider;
use indexmap::IndexMap;
use indoc::formatdoc;
use inquire::{Confirm, MultiSelect, Text, list_option::ListOption, validator::Validation};
use log::warn;
use parking_lot::RwLock;
use prompts::DEFAULT_SKILL_INSTRUCTIONS;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{File, OpenOptions, read_dir, read_to_string, remove_dir_all, remove_file};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::{env, fs};

pub(crate) fn expand_enabled_mcp_server_ids(
    app: &AppConfig,
    mcp_config: &McpServersConfig,
    enabled_mcp_servers: &[String],
) -> Vec<String> {
    if enabled_mcp_servers.iter().any(|s| s.trim() == "all") {
        return mcp_config.mcp_servers.keys().cloned().collect();
    }

    let mut ids = Vec::new();
    for item in enabled_mcp_servers.iter().map(|s| s.trim()) {
        if mcp_config.mcp_servers.contains_key(item) {
            ids.push(item.to_string());
        } else {
            for mapped_id in expand_mcp_server_alias(&app.mapping_mcp_servers, item) {
                if mcp_config.mcp_servers.contains_key(&mapped_id) {
                    ids.push(mapped_id);
                }
            }
        }
    }

    ids
}

pub struct AutoContinueConfig {
    pub enabled: bool,
    pub max_continues: usize,
    pub inject_instructions: bool,
    pub continuation_prompt: Option<String>,
}

pub struct SkillInstructionsConfig {
    pub inject: bool,
    pub instructions: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub workspace: Option<WorkspaceMemory>,
}

impl MemoryConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            workspace: None,
        }
    }
}

/// Must stay in sync with the predicate that registers `skill__*` tools in `rebuild_tool_scope`
/// (and in `graph::llm::run_llm_node`). Telling the model to call tools that are not exposed
/// is a footgun. `compatible_enabled` is the post-filter universe that `skill__list` would
/// actually return (cascade-allowed AND surviving `Skill::is_compatible` for current
/// `mcp_server_support`), so an empty set means the hint has nothing to point at.
pub fn should_inject_skill_instructions(app: &AppConfig, policy: &SkillPolicy) -> bool {
    app.function_calling_support && policy.skills_enabled && !policy.compatible_enabled.is_empty()
}

pub fn effective_max_concurrent_jobs(agent: Option<&Agent>, app: &AppConfig) -> usize {
    agent
        .and_then(|a| a.max_concurrent_jobs())
        .or(app.max_concurrent_jobs)
        .unwrap_or(DEFAULT_MAX_CONCURRENT_JOBS)
}

pub fn jobs_enabled(agent: Option<&Agent>, app: &AppConfig) -> bool {
    app.function_calling_support && effective_max_concurrent_jobs(agent, app) > 0
}

fn print_asset_names(kind: &str, names: &[String]) -> Result<()> {
    if names.is_empty() {
        println!("No {kind} found.");
        return Ok(());
    }

    let mut header: Vec<char> = kind.chars().collect();
    header[0] = header[0].to_ascii_uppercase();
    let header: String = header.into_iter().collect();

    println!("{header}:");
    for name in names {
        println!("  • {name}");
    }

    Ok(())
}

pub(crate) fn asset_table(header: &[&str]) -> Table {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(header.to_vec());
    table
}

fn mcp_prompt_rows(items: &[CatalogItem]) -> Vec<[String; 4]> {
    items
        .iter()
        .map(|item| {
            [
                item.server.clone(),
                item.name.clone(),
                item.description.clone(),
                format_prompt_arguments(item.arguments.as_deref().unwrap_or_default()),
            ]
        })
        .collect()
}

fn complete_skills_with_descriptions(names: Vec<String>) -> Vec<(String, Option<String>)> {
    names
        .into_iter()
        .map(|name| {
            let description = Skill::load(&name)
                .ok()
                .map(|s| s.description().to_string())
                .filter(|d| !d.is_empty());
            (name, description)
        })
        .collect()
}

const SET_COMPLETION_KEYS: [&str; 27] = [
    "auto_continue",
    "continuation_prompt",
    "temperature",
    "top_p",
    "enabled_macros",
    "enabled_skills",
    "enabled_tools",
    "enabled_mcp_servers",
    "inject_todo_instructions",
    "inject_skill_instructions",
    "skill_instructions",
    "max_auto_continues",
    "mcp_tools",
    "memory",
    "save_session",
    "compression_threshold",
    "rag_reranker_model",
    "rag_top_k",
    "max_output_tokens",
    "dry_run",
    "function_calling_support",
    "mcp_server_support",
    "skills_enabled",
    "stream",
    "save",
    "highlight",
    "raw_markdown",
];

fn toggled_enabled_macros(
    current: Option<&[String]>,
    all_active: &[String],
    name: &str,
    enable: bool,
) -> Option<Vec<String>> {
    match (current, enable) {
        (None, true) => None,
        (Some(list), true) => {
            if list.iter().any(|v| v == name) {
                None
            } else {
                let mut list = list.to_vec();
                list.push(name.to_string());
                Some(list)
            }
        }
        (None, false) => Some(
            all_active
                .iter()
                .filter(|v| v.as_str() != name)
                .cloned()
                .collect(),
        ),
        (Some(list), false) => {
            if list.iter().any(|v| v == name) {
                Some(
                    list.iter()
                        .filter(|v| v.as_str() != name)
                        .cloned()
                        .collect(),
                )
            } else {
                None
            }
        }
    }
}

fn macro_state_display(
    row: &ResolvedMacro,
    lock_owner: impl Fn(MacroAllowlistLevel) -> String,
) -> String {
    match &row.state {
        MacroState::Enabled => "enabled".to_string(),
        MacroState::DisabledRuntime => "disabled (runtime)".to_string(),
        MacroState::Locked { level } => format!("locked ({} enabled_macros)", lock_owner(*level)),
        MacroState::Missing => "missing".to_string(),
        MacroState::ShadowedBuiltin => "shadowed (built-in)".to_string(),
        MacroState::Invalid { reason } => format!("invalid ({reason})"),
    }
}

fn macro_source_display(source: Option<MacroSource>) -> String {
    match source {
        Some(source) => source.to_string(),
        None => "-".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderMode {
    #[default]
    Streaming,
    Silent,
}

pub struct RequestContext {
    pub app: Arc<AppState>,

    pub macro_flag: bool,
    pub macro_non_isolated: bool,
    pub info_flag: bool,
    pub working_mode: WorkingMode,

    pub model: Model,
    pub agent_variables: Option<AgentVariables>,

    pub role: Option<Role>,
    pub session: Option<Session>,
    pub rag: Option<Arc<Rag>>,
    pub rag_key: Option<RagKey>,
    pub agent: Option<Agent>,

    pub last_message: Option<LastMessage>,

    pub tool_scope: ToolScope,

    pub declared_function_names: HashSet<String>,

    /// Ids of jobs started by the currently executing graph LLM node.
    /// `Some` only while a node runs: `job__start` records into it, the
    /// turn-end guardrail scopes its nag to it, and the node executor reaps
    /// whatever is left in it on exit. `None` outside graph nodes — there the
    /// context owns every job in its supervisor.
    pub node_job_scope: Option<Vec<String>>,

    /// Set while a graph LLM node with `mcp_tools` is executing; re-applied as
    /// the last filter layer by every `refresh_mcp_tool_filters` recompute.
    pub active_node_mcp_tools: Option<(String, IndexMap<String, Vec<String>>)>,

    pub supervisor: Option<Arc<RwLock<Supervisor>>>,
    pub parent_supervisor: Option<Arc<RwLock<Supervisor>>>,
    pub self_agent_id: Option<String>,
    pub inbox: Option<Arc<Inbox>>,
    pub parent_inbox: Option<Arc<Inbox>>,
    /// Directory of concurrent teammates for `teammates: true` graph fan-outs.
    /// Never inherited: fork sites and `run_agent_for_graph` propagate it
    /// explicitly so it cannot leak into nested fan-outs or spawned children.
    pub peer_registry: Option<Arc<PeerRegistry>>,
    /// Pre-provisioned (agent id, inbox) for the sub-agent this branch context
    /// will run, consumed by `run_agent_for_graph`.
    pub peer_assignment: Option<(String, Arc<Inbox>)>,
    pub escalation_queue: Option<Arc<EscalationQueue>>,
    pub notification_queue: Arc<NotificationQueue>,
    pub current_depth: usize,
    pub auto_continue_count: usize,
    pub pending_tasks_guardrail_count: u32,
    pub todo_list: TodoList,
    pub skill_registry: SkillRegistry,
    pub last_continuation_response: Option<String>,
    pub auto_continue_paused: Option<String>,
    pub pending_prefill: Option<String>,

    pub session_abort: Option<AbortSignal>,

    pub render_mode: RenderMode,
}

impl RequestContext {
    pub fn new(app: Arc<AppState>, working_mode: WorkingMode) -> Self {
        Self {
            app,
            macro_flag: false,
            macro_non_isolated: false,
            info_flag: false,
            working_mode,
            model: Default::default(),
            agent_variables: None,
            role: None,
            session: None,
            rag: None,
            rag_key: None,
            agent: None,
            last_message: None,
            tool_scope: ToolScope::default(),
            declared_function_names: Default::default(),
            node_job_scope: None,
            active_node_mcp_tools: None,
            supervisor: None,
            parent_supervisor: None,
            self_agent_id: None,
            inbox: None,
            parent_inbox: None,
            peer_registry: None,
            peer_assignment: None,
            escalation_queue: None,
            notification_queue: Arc::new(NotificationQueue::new()),
            current_depth: 0,
            auto_continue_count: 0,
            pending_tasks_guardrail_count: 0,
            todo_list: TodoList::default(),
            skill_registry: SkillRegistry::default(),
            last_continuation_response: None,
            auto_continue_paused: None,
            pending_prefill: None,
            session_abort: None,
            render_mode: RenderMode::default(),
        }
    }

    pub fn bootstrap(
        app: Arc<AppState>,
        working_mode: WorkingMode,
        info_flag: bool,
    ) -> Result<Self> {
        let model = Model::retrieve_model(&app.config, &app.config.model_id, ModelType::Chat)?;

        let mut functions = app.functions.clone();
        if working_mode.is_repl() {
            functions.append_user_interaction_functions();
        }

        if app.config.function_calling_support {
            let policy = SkillPolicy::effective(&app.config, None, None, None)?;
            if policy.skills_enabled {
                functions.append_skill_functions();
            }
        }

        let mut mcp_runtime = McpRuntime::default();
        if let Some(registry) = &app.mcp_registry {
            mcp_runtime.sync_from_registry(registry);
        }

        let mut ctx = Self {
            app,
            macro_flag: false,
            macro_non_isolated: false,
            info_flag,
            working_mode,
            model,
            agent_variables: None,
            role: None,
            session: None,
            rag: None,
            rag_key: None,
            agent: None,
            last_message: None,
            tool_scope: ToolScope {
                functions,
                mcp_runtime,
                tool_tracker: ToolCallTracker::default(),
            },
            declared_function_names: Default::default(),
            node_job_scope: None,
            active_node_mcp_tools: None,
            supervisor: None,
            parent_supervisor: None,
            self_agent_id: None,
            inbox: None,
            parent_inbox: None,
            peer_registry: None,
            peer_assignment: None,
            escalation_queue: None,
            notification_queue: Arc::new(NotificationQueue::new()),
            current_depth: 0,
            auto_continue_count: 0,
            pending_tasks_guardrail_count: 0,
            todo_list: TodoList::default(),
            skill_registry: SkillRegistry::default(),
            last_continuation_response: None,
            auto_continue_paused: None,
            pending_prefill: None,
            session_abort: None,
            render_mode: RenderMode::default(),
        };
        ctx.refresh_mcp_tool_filters();
        Ok(ctx)
    }

    /// Forks the context for one parallel branch of a graph super-step.
    ///
    /// Each branch gets a fresh, owned clone. Mutations (role swap,
    /// `before/after_chat_completion`, tool tracker, last_message, etc.) are
    /// scoped to the branch and discarded when the branch finishes. The
    /// user-visible state communication happens through the graph's
    /// `StateManager` (via `fork_for_branch_state` + `diff_against` +
    /// `apply_branch_writes` reducers), and not through `RequestContext`.
    ///
    /// Distinction from `new_for_child`: `new_for_child` builds a fresh context
    /// for a spawned sub-agent (different agent identity, different supervisor
    /// hierarchy, depth+1, fresh tool tracker). `fork_for_branch` keeps the
    /// caller's identity and supervisor hierarchy; it's a sibling clone of the
    /// same logical agent, running one of N parallel work items.
    pub fn fork_for_branch(&self) -> Self {
        Self {
            app: Arc::clone(&self.app),
            macro_flag: self.macro_flag,
            macro_non_isolated: self.macro_non_isolated,
            info_flag: self.info_flag,
            working_mode: self.working_mode,
            model: self.model.clone(),
            agent_variables: self.agent_variables.clone(),
            role: self.role.clone(),
            session: self.session.clone(),
            rag: self.rag.clone(),
            rag_key: self.rag_key.clone(),
            agent: self.agent.clone(),
            last_message: self.last_message.clone(),
            tool_scope: self.tool_scope.clone(),
            declared_function_names: self.declared_function_names.clone(),
            node_job_scope: None,
            active_node_mcp_tools: self.active_node_mcp_tools.clone(),
            supervisor: self.supervisor.clone(),
            parent_supervisor: self.parent_supervisor.clone(),
            self_agent_id: self.self_agent_id.clone(),
            inbox: self.inbox.clone(),
            parent_inbox: self.parent_inbox.clone(),
            peer_registry: None,
            peer_assignment: None,
            escalation_queue: self.escalation_queue.clone(),
            notification_queue: self.notification_queue.clone(),
            current_depth: self.current_depth,
            auto_continue_count: 0,
            pending_tasks_guardrail_count: 0,
            todo_list: self.todo_list.clone(),
            skill_registry: self.skill_registry.clone(),
            last_continuation_response: None,
            auto_continue_paused: self.auto_continue_paused.clone(),
            pending_prefill: None,
            session_abort: self.session_abort.clone(),
            render_mode: self.render_mode,
        }
    }

    pub fn new_for_child(
        app: Arc<AppState>,
        parent: &Self,
        current_depth: usize,
        inbox: Arc<Inbox>,
        self_agent_id: String,
    ) -> Self {
        let tool_call_tracker = ToolCallTracker::new(4, 10);

        Self {
            app,
            macro_flag: parent.macro_flag,
            macro_non_isolated: parent.macro_non_isolated,
            info_flag: parent.info_flag,
            working_mode: WorkingMode::Cmd,
            model: parent.model.clone(),
            agent_variables: parent.agent_variables.clone(),
            role: None,
            session: None,
            rag: None,
            rag_key: None,
            agent: None,
            last_message: None,
            tool_scope: ToolScope {
                functions: Functions::default(),
                mcp_runtime: McpRuntime::default(),
                tool_tracker: tool_call_tracker,
            },
            declared_function_names: Default::default(),
            node_job_scope: None,
            active_node_mcp_tools: None,
            supervisor: None,
            parent_supervisor: parent.supervisor.clone(),
            self_agent_id: Some(self_agent_id),
            inbox: Some(inbox),
            parent_inbox: parent.inbox.clone(),
            peer_registry: None,
            peer_assignment: None,
            escalation_queue: parent.escalation_queue.clone(),
            notification_queue: Arc::new(NotificationQueue::new()),
            current_depth,
            auto_continue_count: 0,
            pending_tasks_guardrail_count: 0,
            todo_list: TodoList::default(),
            skill_registry: SkillRegistry::default(),
            last_continuation_response: None,
            auto_continue_paused: None,
            pending_prefill: None,
            session_abort: None,
            render_mode: parent.render_mode,
        }
    }

    fn update_app_config(&mut self, update: impl FnOnce(&mut AppConfig)) {
        let mut app_config = (*self.app.config).clone();
        update(&mut app_config);

        let mut app_state = (*self.app).clone();
        app_state.config = Arc::new(app_config);
        self.app = Arc::new(app_state);
    }

    pub fn root_escalation_queue(&self) -> Option<&Arc<EscalationQueue>> {
        self.escalation_queue.as_ref()
    }

    pub fn ensure_root_escalation_queue(&mut self) -> Arc<EscalationQueue> {
        self.escalation_queue
            .get_or_insert_with(|| Arc::new(EscalationQueue::new()))
            .clone()
    }

    pub fn ensure_inbox(&mut self) -> Arc<Inbox> {
        self.inbox
            .get_or_insert_with(|| Arc::new(Inbox::new()))
            .clone()
    }

    pub fn rag_cache(&self) -> &Arc<RagCache> {
        &self.app.rag_cache
    }

    pub fn init_todo_list(&mut self, goal: &str) {
        self.todo_list = TodoList::new(goal);
        self.auto_continue_paused = None;
    }

    pub fn add_todo(&mut self, task: &str) -> usize {
        self.todo_list.add(task)
    }

    pub fn mark_todo_done(&mut self, id: usize) -> bool {
        self.todo_list.mark_done(id)
    }

    pub fn clear_todo_list(&mut self) {
        self.todo_list.clear();
        self.auto_continue_count = 0;
        self.auto_continue_paused = None;
    }

    pub fn increment_auto_continue_count(&mut self) {
        self.auto_continue_count += 1;
    }

    pub fn pause_auto_continue(&mut self, reason: &str) {
        self.auto_continue_paused = Some(reason.to_string());
    }

    pub fn resume_auto_continue(&mut self) {
        self.auto_continue_paused = None;
    }

    pub fn reset_continuation_count(&mut self) {
        self.auto_continue_count = 0;
        self.last_continuation_response = None;
    }

    pub fn set_last_continuation_response(&mut self, response: String) {
        self.last_continuation_response = Some(response);
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
        if self.app.config.function_calling_support {
            flags |= StateFlags::FUNCTION_CALLING;
        }
        if self.auto_continue_config().enabled {
            flags |= StateFlags::AUTO_CONTINUE;
        }
        if self.resolved_skills_enabled() {
            flags |= StateFlags::SKILLS_ENABLED;
        }
        flags
    }

    pub fn resolved_skills_enabled(&self) -> bool {
        if let Some(agent) = &self.agent
            && let Some(value) = agent.skills_enabled()
        {
            return value;
        }
        let app = &self.app.config;
        self.session
            .as_ref()
            .and_then(|s| s.skills_enabled())
            .or_else(|| self.role.as_ref().and_then(|r| r.skills_enabled()))
            .unwrap_or(app.skills_enabled)
    }

    pub fn messages_file(&self) -> PathBuf {
        match &self.agent {
            None => match env::var(get_env_name("messages_file")) {
                Ok(value) => PathBuf::from(value),
                Err(_) => paths::cache_dir().join(MESSAGES_FILE_NAME),
            },
            Some(agent) => paths::cache_dir()
                .join(AGENTS_DIR_NAME)
                .join(agent.name())
                .join(MESSAGES_FILE_NAME),
        }
    }

    pub fn sessions_dir(&self) -> PathBuf {
        match &self.agent {
            None => match env::var(get_env_name("sessions_dir")) {
                Ok(value) => PathBuf::from(value),
                Err(_) => paths::local_dir(SESSIONS_DIR_NAME),
            },
            Some(agent) => paths::agent_data_dir(agent.name()).join(SESSIONS_DIR_NAME),
        }
    }

    pub fn session_file(&self, name: &str) -> PathBuf {
        match name.split_once("/") {
            Some((dir, name)) => self.sessions_dir().join(dir).join(format!("{name}.yaml")),
            None => self.sessions_dir().join(format!("{name}.yaml")),
        }
    }

    pub fn rag_file(&self, name: &str) -> PathBuf {
        match &self.agent {
            Some(agent) => paths::agent_rag_file(agent.name(), name),
            None => paths::rags_dir().join(format!("{name}.yaml")),
        }
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

    pub fn rag_info(&self) -> Result<String> {
        if let Some(rag) = &self.rag {
            rag.export()
        } else {
            bail!("No RAG")
        }
    }

    pub fn todo_info(&self) -> Result<String> {
        if !self.auto_continue_config().enabled {
            bail!(
                "Auto-continuation is disabled. Enable it by setting `auto_continue: true` in your config or running `.set auto_continue true`."
            );
        }

        if self.todo_list.is_empty() {
            return Ok("No todos in the running list.\n".to_string());
        }

        let mut out = self.todo_list.render_for_model();
        out.push('\n');
        Ok(out)
    }

    pub fn tools_info(&self) -> Result<String> {
        if !self.app.config.function_calling_support {
            bail!(
                "Function calling is disabled. Enable it by setting `function_calling_support: true` in your config or running `.set function_calling_support true`."
            );
        }
        let role = self.extract_role(&self.app.config)?;
        match self.select_functions(&role) {
            None => Ok("No tools enabled for the next request.\n".to_string()),
            Some(functions) => {
                let mut names: Vec<&str> = functions.iter().map(|f| f.name.as_str()).collect();
                names.sort_unstable();
                let mut out = format!(
                    "Tools enabled for the next request: {}\n\n",
                    functions.len()
                );

                for name in names {
                    out.push_str("  ");
                    out.push_str(name);
                    out.push('\n');
                }

                Ok(out)
            }
        }
    }

    pub async fn mcp_server_info(&self, name: &str) -> Result<String> {
        let Some(spec) = self
            .app
            .mcp_config
            .as_ref()
            .and_then(|config| config.mcp_servers.get(name))
        else {
            bail!(
                "MCP server '{name}' is not configured. Run `.list mcp-servers` to see what's available"
            );
        };
        let Some(handle) = self.tool_scope.mcp_runtime.servers.get(name).cloned() else {
            bail!("MCP server '{name}' is not running. Enable it with `.mcp enable {name}`.");
        };

        let transport = match spec.transport_type {
            McpTransportType::Stdio => "stdio",
            McpTransportType::Http => "http",
            McpTransportType::Sse => "sse",
        };
        let info = handle.peer_info();
        let features =
            McpServerFeatures::from_capabilities(name, info.as_ref().map(|i| &i.capabilities));
        let mut capabilities: Vec<String> = vec![];
        if features.tools {
            capabilities.push("tools".to_string());
        }
        if features.resources {
            let resources = handle.list_all_resources().await;
            let templates = handle.list_all_resource_templates().await;
            let any_failed = resources.is_err() || templates.is_err();
            let count = resources.map(|r| r.len()).unwrap_or_default()
                + templates.map(|t| t.len()).unwrap_or_default();
            if count > 0 {
                capabilities.push(format!("resources ({count})"));
            } else if any_failed {
                capabilities.push("resources (declared, list failed)".to_string());
            }
        }
        if features.prompts {
            match handle.list_all_prompts().await {
                Ok(prompts) if prompts.is_empty() => {}
                Ok(prompts) => capabilities.push(format!("prompts ({})", prompts.len())),
                Err(_) => {
                    capabilities.push("prompts (declared, list failed)".to_string());
                }
            }
        }

        const INFO_LABEL_WIDTH: usize = 15;
        let mut out = String::new();
        out.push_str(&format!(
            "{:<INFO_LABEL_WIDTH$}{name} ({transport}, connected)\n",
            "server"
        ));
        out.push_str(&format!(
            "{:<INFO_LABEL_WIDTH$}{}\n",
            "capabilities",
            capabilities.join(", ")
        ));

        let filter = self.tool_scope.mcp_runtime.tool_filters.get(name);
        let layers: Vec<(String, String)> = filter
            .map(|f| {
                f.layers()
                    .map(|(source, patterns)| (format!("{source}:"), patterns.join(" | ")))
                    .collect()
            })
            .unwrap_or_default();
        if layers.is_empty() {
            out.push_str(&format!(
                "{:<INFO_LABEL_WIDTH$}(none — all tools allowed)\n",
                "filter layers"
            ));
        } else {
            let label_width = layers
                .iter()
                .map(|(label, _)| label.chars().count())
                .max()
                .unwrap_or_default()
                + 2;
            for (i, (label, patterns)) in layers.iter().enumerate() {
                if i == 0 {
                    out.push_str(&format!("{:<INFO_LABEL_WIDTH$}", "filter layers"));
                } else {
                    out.push_str(&" ".repeat(INFO_LABEL_WIDTH));
                }
                out.push_str(&format!("{label:<label_width$}{patterns}\n"));
            }
        }

        let tools = handle
            .list_all_tools()
            .await
            .with_context(|| format!("Failed to list tools on MCP server '{name}'"))?;
        let mut names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
        names.sort_unstable();
        let allowed = names
            .iter()
            .filter(|tool| filter.is_none_or(|f| f.allows(tool)))
            .count();
        out.push_str(&format!(
            "\ntools ({allowed} allowed / {} total)\n",
            names.len()
        ));
        let name_width = names
            .iter()
            .map(|tool| tool.chars().count())
            .max()
            .unwrap_or_default();
        let allowed_marker = "✓".green().bold().to_string();
        let hidden_marker = "✗".red().bold().to_string();
        for tool in &names {
            let explained = filter.map(|f| f.allows_explain(tool));
            match explained {
                None => out.push_str(&format!("  {allowed_marker} {tool}\n")),
                Some(Ok(matches)) => {
                    let chain: Vec<String> = matches
                        .iter()
                        .map(|(source, pattern)| format!("{pattern} ({})", source.short_label()))
                        .collect();
                    if chain.is_empty() {
                        out.push_str(&format!("  {allowed_marker} {tool}\n"));
                    } else {
                        out.push_str(&format!(
                            "  {allowed_marker} {tool:<name_width$}  {}\n",
                            chain.join(" ∧ ")
                        ));
                    }
                }
                Some(Err(source)) => out.push_str(&format!(
                    "  {hidden_marker} {tool:<name_width$}  hidden by {} layer\n",
                    source.short_label()
                )),
            }
        }
        if let Some(f) = filter {
            for (source, pattern) in f.dead_context_patterns(&names) {
                out.push_str(&format!(
                    "⚠ {} pattern '{pattern}' matches no allowed tools\n",
                    source.short_label()
                ));
            }
        }

        Ok(out)
    }

    pub fn list_sessions(&self) -> Vec<String> {
        list_file_names(self.sessions_dir(), ".yaml")
    }

    pub fn list_autoname_sessions(&self) -> Vec<String> {
        list_file_names(self.sessions_dir().join("_"), ".yaml")
    }

    pub fn is_compressing_session(&self) -> bool {
        self.session
            .as_ref()
            .map(|v| v.compressing())
            .unwrap_or_default()
    }

    pub fn compression_keep_last(&self) -> usize {
        self.agent
            .as_ref()
            .and_then(|a| a.compression_keep_last())
            .unwrap_or(self.app.config.compression_keep_last)
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

    pub fn exit_role(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.guard_empty()?;
            session.clear_role();
        } else if self.role.is_some() {
            self.role = None;
        }
        Ok(())
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

    pub fn fork_session(&mut self, fork_name: Option<&str>) -> Result<()> {
        let current_name = match &self.session {
            Some(s) => s.name().to_string(),
            None => bail!("No active session to fork"),
        };

        let fork_name: String = match fork_name {
            Some(name) => name.to_string(),
            None => {
                let base = fork_base_name(&current_name);
                let sessions_dir = self.sessions_dir();
                (1_u32..)
                    .map(|n| format!("{base}-fork-{n}"))
                    .find(|name| !sessions_dir.join(format!("{name}.yaml")).exists())
                    .unwrap()
            }
        };

        let fork_path = self.session_file(&fork_name);
        if fork_path.exists() {
            bail!("Session '{}' already exists", fork_name);
        }

        self.save_session(None)?;

        let session = self.session.as_ref().unwrap();
        let mut fork = session.clone();
        fork.set_name(fork_name.clone());
        fork.clear_autoname();
        fork.save(&fork_name, &fork_path, self.working_mode.is_repl())?;

        self.session = Some(fork);
        println!("Forked '{current_name}' → '{fork_name}'");

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

    pub fn undo_last_exchange(&mut self) -> Result<()> {
        let text = match self.session.as_mut() {
            Some(session) => session.pop_last_exchange(),
            None => bail!("No session"),
        };
        match text {
            Some(text) => {
                self.pending_prefill = Some(text);
                self.discontinuous_last_message();
                Ok(())
            }
            None => bail!("Nothing to undo"),
        }
    }

    pub fn set_save_session_this_time(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.set_save_session_this_time();
        } else {
            bail!("No session")
        }
        Ok(())
    }

    pub fn exit_rag(&mut self) -> Result<()> {
        self.rag.take();
        self.tool_scope.functions.remove_rag_query_functions();
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

    pub fn before_chat_completion(&mut self, input: &Input) -> Result<()> {
        // `job__start` validates against exactly what was declared to the
        // model for THIS request; refresh it every time.
        //
        // This is necessary to prevent the model from invoking functions it
        // otherwise wouldn't have access to by going through the free `tool`
        // argument of `job__start`. If a function is disabled, the model
        // shouldn't be able to invoke it at all in any way. This prevents
        // that backdoor.
        self.declared_function_names = input.declared_function_names();
        self.last_message = Some(LastMessage::new(input.clone(), String::new()));
        Ok(())
    }

    pub fn on_chat_completion_error(&mut self, app: &AppConfig, input: &Input) {
        self.last_message = Some(LastMessage::new(input.clone(), String::new()));
        if input.session(&self.session).is_none() {
            if let Some(lm) = self.last_message.as_mut() {
                lm.continuous = false;
            }

            return;
        }

        let mut i = input.clone();
        i.clear_patch();
        if let Some(session) = i.session_mut(&mut self.session) {
            let _ = session.add_message(&i, INTERRUPTED_RESPONSE_TEXT);
            if !app.dry_run && session.save_session() == Some(true) {
                let _ = session.flush();
            }
        }
    }

    pub fn has_recoverable_interruption(&self) -> bool {
        let live = self
            .last_message
            .as_ref()
            .map(|v| v.continuous && v.input.with_session())
            .unwrap_or(false);
        live || self
            .session
            .as_ref()
            .is_some_and(Session::has_interrupted_error_checkpoint)
    }

    pub fn discontinuous_last_message(&mut self) {
        if let Some(last_message) = self.last_message.as_mut() {
            last_message.continuous = false;
        }
    }

    pub fn init_agent_shared_variables(&mut self) -> Result<()> {
        let agent = match self.agent.as_mut() {
            Some(v) => v,
            None => return Ok(()),
        };
        if !agent.defined_variables().is_empty() && agent.shared_variables().is_empty() {
            let new_variables = Agent::init_agent_variables(
                agent.defined_variables(),
                self.agent_variables.as_ref(),
                self.info_flag,
                self.self_agent_id.is_none(),
            )?;
            agent.set_shared_variables(new_variables);
        }
        if !self.info_flag {
            agent.update_shared_dynamic_instructions(false, self.app.config.tool_timeout)?;
        }
        Ok(())
    }

    pub fn init_agent_session_variables(&mut self, new_session: bool) -> Result<()> {
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
                        self.self_agent_id.is_none(),
                    )?;
                    agent.set_shared_variables(new_variables.clone());
                    new_variables
                } else {
                    shared_variables
                };
            agent.set_session_variables(session_variables);
            if !self.info_flag {
                agent.update_session_dynamic_instructions(None, self.app.config.tool_timeout)?;
            }
            session.sync_agent(agent);
        } else {
            let variables = session.agent_variables();
            agent.set_session_variables(variables.clone());
            agent.update_session_dynamic_instructions(
                Some(session.agent_instructions().to_string()),
                self.app.config.tool_timeout,
            )?;
        }
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

    pub fn extract_role(&self, app: &AppConfig) -> Result<Role> {
        self.extract_role_impl(app, true)
    }

    fn extract_role_impl(&self, app: &AppConfig, inject_memory: bool) -> Result<Role> {
        let mut role = if let Some(session) = self.session.as_ref() {
            session.to_role()
        } else if let Some(agent) = self.agent.as_ref() {
            let mut role = agent.to_role();
            if role.reasoning_effort().is_none() {
                role.set_reasoning_effort(app.reasoning_effort.clone());
            }

            role
        } else if let Some(role) = self.role.as_ref() {
            role.clone()
        } else {
            let mut role = Role::default();
            role.batch_set(
                &self.model,
                app.reasoning_effort.clone(),
                app.temperature,
                app.top_p,
                app.enabled_tools.clone(),
                app.enabled_mcp_servers.clone(),
            );

            role
        };

        if self.agent.is_none() && self.app.config.function_calling_support {
            let config = self.auto_continue_config();
            if config.enabled && config.inject_instructions {
                role.append_to_prompt(prompts::DEFAULT_TODO_INSTRUCTIONS);
            }
        }

        let policy = SkillPolicy::effective(
            app,
            self.role.as_ref(),
            self.agent.as_ref(),
            self.session.as_ref(),
        )?;

        if app.workspace_instructions.unwrap_or(true)
            && let Ok(cwd) = env::current_dir()
        {
            let file_names = app
                .workspace_instructions_files
                .clone()
                .unwrap_or_else(instructions::default_workspace_instructions_files);
            if let Some(found) = instructions::discover_workspace_instructions(&cwd, &file_names) {
                let separator = if role.is_empty_prompt() { "" } else { "\n\n" };
                role.append_to_prompt(separator);
                role.append_to_prompt(&instructions::build_instructions_section(&found));
            }
        }

        if should_inject_skill_instructions(app, &policy) {
            let config = self.skill_instructions_config();

            if config.inject {
                let separator = if role.is_empty_prompt() { "" } else { "\n\n" };

                role.append_to_prompt(separator);
                role.append_to_prompt(
                    config
                        .instructions
                        .as_deref()
                        .unwrap_or(DEFAULT_SKILL_INSTRUCTIONS),
                );
            }
        }

        if inject_memory {
            let memory_config = self.memory_config();
            if memory_config.enabled {
                let store = MemoryStore {
                    global_dir: paths::global_memory_dir(),
                    workspace: memory_config.workspace,
                };
                let with_tools = app.function_calling_support;
                let cap = if with_tools {
                    app.memory_cap_with_tools
                        .unwrap_or(DEFAULT_MEMORY_CAP_WITH_TOOLS)
                } else {
                    app.memory_cap_without_tools
                        .unwrap_or(DEFAULT_MEMORY_CAP_WITHOUT_TOOLS)
                };
                match memory::build_memory_section(&store, with_tools, cap) {
                    Ok(Some(section)) => {
                        let separator = if role.is_empty_prompt() { "" } else { "\n\n" };
                        role.append_to_prompt(separator);
                        role.append_to_prompt(&section);
                        role.append_to_prompt("\n\n");
                        role.append_to_prompt(if with_tools {
                            prompts::DEFAULT_MEMORY_INSTRUCTIONS
                        } else {
                            prompts::DEFAULT_MEMORY_INSTRUCTIONS_READONLY
                        });
                    }
                    Ok(None) => {}
                    Err(e) => warn!("memory injection failed: {}", e),
                }
            }
        }

        Ok(self.skill_registry.effective_role(&role, &policy))
    }

    pub fn skill_instructions_config(&self) -> SkillInstructionsConfig {
        if let Some(agent) = &self.agent {
            return SkillInstructionsConfig {
                inject: agent.inject_skill_instructions(),
                instructions: agent.skill_instructions_value(),
            };
        }

        let app = &self.app.config;
        let inject = self
            .session
            .as_ref()
            .and_then(|s| s.inject_skill_instructions())
            .or_else(|| {
                self.role
                    .as_ref()
                    .and_then(|r| r.inject_skill_instructions())
            })
            .unwrap_or(app.inject_skill_instructions);
        let instructions = self
            .session
            .as_ref()
            .and_then(|s| s.skill_instructions().map(|v| v.to_string()))
            .or_else(|| {
                self.role
                    .as_ref()
                    .and_then(|r| r.skill_instructions().map(|v| v.to_string()))
            })
            .or_else(|| app.skill_instructions.clone());

        SkillInstructionsConfig {
            inject,
            instructions,
        }
    }

    pub fn memory_config(&self) -> MemoryConfig {
        if let Some(agent) = &self.agent
            && graph::agent_has_graph(agent.name())
        {
            return MemoryConfig::disabled();
        }

        let agent_pref = self.agent.as_ref().and_then(|a| a.memory());
        let session_pref = self.session.as_ref().and_then(|s| s.memory());
        let role_pref = self.role.as_ref().and_then(|r| r.memory());
        let app_pref = self.app.config.memory;

        let resolved = agent_pref
            .or(session_pref)
            .or(role_pref)
            .or(app_pref)
            .unwrap_or(true);
        if !resolved {
            return MemoryConfig::disabled();
        }

        let cwd = env::current_dir().ok();
        let store = cwd.as_deref().map(MemoryStore::new);
        let workspace = store.as_ref().and_then(|s| s.workspace.clone());

        let global_exists = paths::global_memory_index_file().exists();
        let workspace_exists = workspace.is_some();

        if !global_exists && !workspace_exists {
            return MemoryConfig::disabled();
        }

        MemoryConfig {
            enabled: true,
            workspace,
        }
    }

    pub fn should_inject_memory(&self) -> bool {
        self.memory_config().enabled
    }

    pub fn should_register_memory_tools(&self) -> bool {
        self.should_inject_memory() && self.app.config.function_calling_support
    }

    pub fn auto_continue_config(&self) -> AutoContinueConfig {
        if let Some(agent) = &self.agent {
            return AutoContinueConfig {
                enabled: agent.auto_continue_enabled(),
                max_continues: agent.max_auto_continues(),
                inject_instructions: agent.inject_todo_instructions(),
                continuation_prompt: agent.continuation_prompt_value(),
            };
        }
        let app = &self.app.config;
        let enabled = self
            .session
            .as_ref()
            .and_then(|s| s.auto_continue())
            .or_else(|| self.role.as_ref().and_then(|r| r.auto_continue()))
            .unwrap_or(app.auto_continue);
        let max = self
            .session
            .as_ref()
            .and_then(|s| s.max_auto_continues())
            .or_else(|| self.role.as_ref().and_then(|r| r.max_auto_continues()))
            .unwrap_or(app.max_auto_continues);
        let inject = self
            .session
            .as_ref()
            .and_then(|s| s.inject_todo_instructions())
            .or_else(|| {
                self.role
                    .as_ref()
                    .and_then(|r| r.inject_todo_instructions())
            })
            .unwrap_or(app.inject_todo_instructions);
        let prompt = self
            .session
            .as_ref()
            .and_then(|s| s.continuation_prompt().map(|v| v.to_string()))
            .or_else(|| {
                self.role
                    .as_ref()
                    .and_then(|r| r.continuation_prompt().map(|v| v.to_string()))
            })
            .or_else(|| app.continuation_prompt.clone());
        AutoContinueConfig {
            enabled,
            max_continues: max,
            inject_instructions: inject,
            continuation_prompt: prompt,
        }
    }

    pub fn set_temperature_on_role_like(&mut self, value: Option<f64>) -> bool {
        match self.role_like_mut() {
            Some(role_like) => {
                role_like.set_temperature(value);
                true
            }
            None => false,
        }
    }

    pub fn set_top_p_on_role_like(&mut self, value: Option<f64>) -> bool {
        match self.role_like_mut() {
            Some(role_like) => {
                role_like.set_top_p(value);
                true
            }
            None => false,
        }
    }

    pub fn set_reasoning_effort_on_role_like(&mut self, value: Option<String>) -> bool {
        match self.role_like_mut() {
            Some(role_like) => {
                role_like.set_reasoning_effort(value);
                true
            }
            None => false,
        }
    }

    pub fn set_enabled_tools_on_role_like(&mut self, value: Option<Vec<String>>) -> bool {
        match self.role_like_mut() {
            Some(role_like) => {
                role_like.set_enabled_tools(value);
                true
            }
            None => false,
        }
    }

    pub fn set_enabled_mcp_servers_on_role_like(&mut self, value: Option<Vec<String>>) -> bool {
        match self.role_like_mut() {
            Some(role_like) => {
                role_like.set_enabled_mcp_servers(value);
                true
            }
            None => false,
        }
    }

    pub fn set_mcp_tools_on_role_like(
        &mut self,
        value: Option<IndexMap<String, Vec<String>>>,
    ) -> bool {
        match self.role_like_mut() {
            Some(role_like) => {
                role_like.set_mcp_tools(value);
                true
            }
            None => false,
        }
    }

    fn concrete_tool_names(&self) -> Vec<String> {
        let declarations: Vec<&FunctionDeclaration> = self
            .tool_scope
            .functions
            .declarations()
            .iter()
            .chain(
                self.agent
                    .iter()
                    .flat_map(|agent| agent.functions().declarations()),
            )
            .collect();
        declarations
            .iter()
            .filter(|v| {
                !v.name.starts_with("user__")
                    && !v.name.starts_with("mcp_")
                    && !v.name.starts_with("todo__")
                    && !v.name.starts_with("agent__")
                    && !v.name.starts_with("memory__")
                    && !v.name.starts_with("skill__")
                    && !v.name.starts_with("rag__")
                    && !v.name.starts_with("job__")
            })
            .map(|v| v.name.clone())
            .collect()
    }

    fn tool_list_covers(&self, list: &[String], name: &str) -> bool {
        for item in list {
            let item = item.trim();
            if item == name {
                return true;
            }

            if let Some(values) = self.app.config.mapping_tools.get(item)
                && values.split(',').any(|v| v.trim() == name)
            {
                return true;
            }
        }

        false
    }

    fn mcp_list_covers(&self, list: &[String], name: &str) -> bool {
        for item in list {
            let item = item.trim();
            if item == name {
                return true;
            }

            if let Some(values) = self.app.config.mapping_mcp_servers.get(item)
                && values.split(',').any(|v| v.trim() == name)
            {
                return true;
            }
        }

        false
    }

    fn write_layer_label(&self) -> &'static str {
        if self.session.is_some() {
            "session"
        } else if self.agent.is_some() {
            "agent"
        } else if self.role.is_some() {
            "role"
        } else {
            "global, in-memory"
        }
    }

    pub fn toggle_tool(&mut self, action: &str, name: &str) -> Result<()> {
        let name = name.trim();
        let enable = match action {
            "enable" => true,
            "disable" => false,
            _ => bail!("Unknown action '{action}'. Usage: .tool <enable|disable> <name>"),
        };

        if self.agent.as_ref().is_some_and(|a| a.is_graph()) {
            bail!(
                "Graph agents define tools per-node via `tools:` in graph.yaml; agent-level enabled_tools has no effect"
            );
        }

        let pool = self.concrete_tool_names();
        let is_alias = self
            .app
            .config
            .mapping_tools
            .get(name)
            .is_some_and(|values| {
                values
                    .split(',')
                    .map(str::trim)
                    .any(|v| pool.iter().any(|p| p.as_str() == v))
            });
        if !pool.iter().any(|v| v == name) && !is_alias {
            bail!("Unknown tool '{name}'. Run `.list tools` to see what's available");
        }

        let current: Option<Vec<String>> = if let Some(session) = &self.session {
            session.enabled_tools()
        } else if let Some(agent) = &self.agent {
            agent.enabled_tools()
        } else if let Some(role) = &self.role {
            role.enabled_tools()
        } else {
            self.app.config.enabled_tools.clone()
        };
        let layer = self.write_layer_label();

        let new_list: Vec<String> = if enable {
            match current {
                Some(list) if list.iter().any(|s| s.trim() == "all") => {
                    println!("Tool '{name}' is already enabled ('all' is set).");
                    return Ok(());
                }
                Some(list) if self.tool_list_covers(&list, name) => {
                    println!("Tool '{name}' is already enabled.");
                    return Ok(());
                }
                Some(mut list) => {
                    list.push(name.to_string());
                    list
                }
                None => {
                    if self.agent.is_some() {
                        println!(
                            "Tool '{name}' is already enabled (no filter is set; the agent's full tool pool is active)."
                        );
                        return Ok(());
                    }
                    vec![name.to_string()]
                }
            }
        } else {
            match current {
                None => {
                    if self.agent.is_some() {
                        let mut materialized = pool;
                        materialized.retain(|v| v != name);
                        println!(
                            "Note: no filter was set; materialized the agent's pool into {} concrete tools.",
                            materialized.len()
                        );

                        materialized
                    } else {
                        println!("No tools are enabled in this context; nothing to disable.");
                        return Ok(());
                    }
                }
                Some(list) if list.is_empty() => {
                    println!("No tools are enabled in this context; nothing to disable.");
                    return Ok(());
                }
                Some(list) if list.iter().any(|s| s.trim() == "all") => {
                    let mut materialized = pool;
                    materialized.retain(|v| v != name);
                    println!(
                        "Note: expanded 'all' into {} concrete tools.",
                        materialized.len()
                    );

                    materialized
                }
                Some(mut list) => {
                    if list.iter().any(|s| s.trim() == name) {
                        list.retain(|s| s.trim() != name);
                        list
                    } else if self.tool_list_covers(&list, name) {
                        bail!(
                            "Tool '{name}' is enabled via an alias in 'mapping_tools'. \
                             Disable the alias instead, or set the list explicitly with `.set enabled_tools`."
                        );
                    } else {
                        println!("Tool '{name}' is not enabled; nothing to do.");
                        return Ok(());
                    }
                }
            }
        };

        let new_list = Some(new_list);

        if !self.set_enabled_tools_on_role_like(new_list.clone()) {
            self.update_app_config(|app| app.enabled_tools = new_list.clone());
        }

        let verb = if enable { "Enabled" } else { "Disabled" };
        println!("✓ {verb} tool '{name}' ({layer}).");

        Ok(())
    }

    pub async fn toggle_mcp_server(
        &mut self,
        action: &str,
        name: &str,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let name = name.trim();
        let enable = match action {
            "enable" => true,
            "disable" => false,
            _ => bail!("Unknown action '{action}'. Usage: .mcp <enable|disable> <server_name>"),
        };

        if self.agent.is_some() {
            bail!(
                "Agent MCP servers are defined by the agent's config ('mcp_servers'); \
                 edit the agent config with `.edit agent-config` instead."
            );
        }

        let configured_keys: Vec<String> = match &self.app.mcp_config {
            Some(mcp_config) if !mcp_config.mcp_servers.is_empty() => {
                mcp_config.mcp_servers.keys().cloned().collect()
            }
            _ => bail!("No MCP servers are configured. Please configure MCP servers first."),
        };
        let is_alias = self.app.config.mapping_mcp_servers.contains_key(name);
        if !configured_keys.iter().any(|v| v == name) && !is_alias {
            bail!(
                "MCP server '{name}' is not configured. Run `.list mcp-servers` to see what's available"
            );
        }

        let current: Option<Vec<String>> = if let Some(session) = &self.session {
            session.enabled_mcp_servers()
        } else if let Some(role) = &self.role {
            role.enabled_mcp_servers()
        } else {
            self.app.config.enabled_mcp_servers.clone()
        };
        let layer = self.write_layer_label();

        let new_list: Vec<String> = if enable {
            match current {
                Some(list) if list.iter().any(|s| s.trim() == "all") => {
                    println!("MCP server '{name}' is already enabled ('all' is set).");
                    return Ok(());
                }
                Some(list) if self.mcp_list_covers(&list, name) => {
                    println!("MCP server '{name}' is already enabled.");
                    return Ok(());
                }
                Some(mut list) => {
                    list.push(name.to_string());
                    list
                }
                None => vec![name.to_string()],
            }
        } else {
            match current {
                None => {
                    println!("No MCP servers are enabled in this context; nothing to disable.");
                    return Ok(());
                }
                Some(list) if list.is_empty() => {
                    println!("No MCP servers are enabled in this context; nothing to disable.");
                    return Ok(());
                }
                Some(list) if list.iter().any(|s| s.trim() == "all") => {
                    let mut materialized = configured_keys;
                    materialized.retain(|v| v != name);
                    materialized
                }
                Some(mut list) => {
                    if list.iter().any(|s| s.trim() == name) {
                        list.retain(|s| s.trim() != name);
                        list
                    } else if self.mcp_list_covers(&list, name) {
                        bail!(
                            "MCP server '{name}' is enabled via an alias in 'mapping_mcp_servers'. \
                             Disable the alias instead, or set the list explicitly with `.set enabled_mcp_servers`."
                        );
                    } else {
                        println!("MCP server '{name}' is not enabled; nothing to do.");
                        return Ok(());
                    }
                }
            }
        };

        if !enable && self.skill_registry.loaded_mcp_servers().contains(name) {
            println!(
                "Note: '{name}' is granted by a loaded skill and will keep running until that skill is unloaded."
            );
        }

        let new_list = Some(new_list);
        if !self.set_enabled_mcp_servers_on_role_like(new_list.clone()) {
            self.update_app_config(|app| app.enabled_mcp_servers = new_list.clone());
        }

        if self.app.config.mcp_server_support {
            let app = Arc::clone(&self.app.config);
            self.bootstrap_tools(app.as_ref(), true, abort_signal)
                .await?;
        }

        let verb = if enable { "Enabled" } else { "Disabled" };
        println!("✓ {verb} MCP server '{name}' ({layer}).");

        Ok(())
    }

    pub fn set_save_session_on_session(&mut self, value: Option<bool>) -> bool {
        match self.session.as_mut() {
            Some(session) => {
                session.set_save_session(value);
                true
            }
            None => false,
        }
    }

    pub fn set_compression_threshold_on_session(&mut self, value: Option<usize>) -> bool {
        match self.session.as_mut() {
            Some(session) => {
                session.set_compression_threshold(value);
                true
            }
            None => false,
        }
    }

    pub fn set_max_output_tokens_on_role_like(&mut self, value: Option<isize>) -> bool {
        match self.role_like_mut() {
            Some(role_like) => {
                let mut model = role_like.model().clone();
                model.set_max_tokens(value, true);
                role_like.set_model(model);
                true
            }
            None => false,
        }
    }

    pub fn save_message(&mut self, app: &AppConfig, input: &Input, output: &str) -> Result<()> {
        let mut input = input.clone();
        input.clear_patch();
        if let Some(session) = input.session_mut(&mut self.session) {
            session.add_message(&input, output)?;
            return Ok(());
        }

        if !app.save {
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

    fn open_message_file(&self) -> Result<File> {
        let path = self.messages_file();
        ensure_parent_exists(&path)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to create/append {}", path.display()))
    }

    pub fn after_chat_completion(
        &mut self,
        app: &AppConfig,
        input: &Input,
        output: &str,
        tool_results: &[ToolResult],
    ) -> Result<()> {
        if !tool_results.is_empty() {
            return Ok(());
        }
        self.last_message = Some(LastMessage::new(input.clone(), output.to_string()));
        if !app.dry_run {
            self.save_message(app, input, output)?;
        }
        self.skill_registry.sweep_auto_unload();
        Ok(())
    }

    pub fn sysinfo(&self, app: &AppConfig) -> Result<String> {
        let display_path = |path: &Path| path.display().to_string();
        let wrap = app
            .wrap
            .clone()
            .map_or_else(|| String::from("no"), |v| v.to_string());
        let (rag_reranker_model, rag_top_k) = match &self.rag {
            Some(rag) => rag.get_config(),
            None => (app.rag_reranker_model.clone(), app.rag_top_k),
        };
        let role = self.extract_role(app)?;
        let mut items = vec![
            ("model", role.model().id()),
            (
                "temperature",
                super::format_option_value(&role.temperature()),
            ),
            ("top_p", super::format_option_value(&role.top_p())),
            (
                "reasoning_effort",
                super::format_option_value(&role.reasoning_effort()),
            ),
            (
                "enabled_tools",
                super::format_option_value(&role.enabled_tools().map(|v| v.join(","))),
            ),
            (
                "enabled_mcp_servers",
                super::format_option_value(&role.enabled_mcp_servers().map(|v| v.join(","))),
            ),
            (
                "enabled_skills",
                super::format_option_value(&role.enabled_skills().map(|v| v.join(","))),
            ),
            (
                "enabled_macros",
                super::format_option_value(&role.enabled_macros().map(|v| v.join(","))),
            ),
            (
                "max_output_tokens",
                role.model()
                    .max_tokens_param()
                    .map(|v| format!("{v} (current model)"))
                    .unwrap_or_else(|| "null".into()),
            ),
            (
                "save_session",
                super::format_option_value(&app.save_session),
            ),
            (
                "compression_threshold",
                app.compression_threshold.to_string(),
            ),
            ("memory", super::format_option_value(&app.memory)),
            (
                "memory_cap_with_tools",
                super::format_option_value(&app.memory_cap_with_tools),
            ),
            (
                "memory_cap_without_tools",
                super::format_option_value(&app.memory_cap_without_tools),
            ),
            (
                "workspace_instructions",
                super::format_option_value(&app.workspace_instructions),
            ),
            (
                "workspace_instructions_file",
                env::current_dir()
                    .ok()
                    .and_then(|cwd| {
                        let file_names = app
                            .workspace_instructions_files
                            .clone()
                            .unwrap_or_else(instructions::default_workspace_instructions_files);
                        instructions::discover_workspace_instructions(&cwd, &file_names)
                    })
                    .map(|i| i.path.display().to_string())
                    .unwrap_or_else(|| "null".into()),
            ),
            (
                "rag_reranker_model",
                super::format_option_value(&rag_reranker_model),
            ),
            ("rag_top_k", rag_top_k.to_string()),
            ("dry_run", app.dry_run.to_string()),
            (
                "function_calling_support",
                app.function_calling_support.to_string(),
            ),
            ("mcp_server_support", app.mcp_server_support.to_string()),
            ("skills_enabled", app.skills_enabled.to_string()),
            ("auto_continue", app.auto_continue.to_string()),
            ("max_auto_continues", app.max_auto_continues.to_string()),
            ("stream", app.stream.to_string()),
            ("save", app.save.to_string()),
            ("keybindings", app.keybindings.clone()),
            ("wrap", wrap),
            ("wrap_code", app.wrap_code.to_string()),
            ("highlight", app.highlight.to_string()),
            ("raw_markdown", app.raw_markdown.to_string()),
            ("theme", super::format_option_value(&app.theme)),
            ("config_dir", display_path(&paths::config_dir())),
            ("config_file", display_path(&paths::config_file())),
            ("env_file", display_path(&paths::env_file())),
            ("agents_dir", display_path(&paths::agents_data_dir())),
            ("roles_dir", display_path(&paths::roles_dir())),
            ("skills_dir", display_path(&paths::skills_dir())),
            ("sessions_dir", display_path(&self.sessions_dir())),
            ("memory_dir", display_path(&paths::global_memory_dir())),
            ("rags_dir", display_path(&paths::rags_dir())),
            ("macros_dir", display_path(&paths::macros_dir())),
            ("functions_dir", display_path(&paths::functions_dir())),
            ("mcp_config_file", display_path(&paths::mcp_config_file())),
            ("sbx_kit_dir", display_path(&paths::sbx_kit_dir())),
            ("messages_file", display_path(&self.messages_file())),
        ];

        match &app.secrets_provider {
            None => {
                items.push(("secrets_provider", "local".to_string()));
                items.push((
                    "vault_password_file",
                    display_path(&app.vault_password_file()),
                ));
            }
            Some(provider) => {
                items.push(("secrets_provider", provider.to_string()));
                match provider {
                    SupportedProvider::Local { provider_def } => {
                        let path = provider_def
                            .password_file
                            .clone()
                            .unwrap_or_else(gman::config::Config::local_provider_password_file);
                        items.push(("vault_password_file", display_path(&path)));
                    }
                    SupportedProvider::AwsSecretsManager { provider_def } => {
                        if let Some(p) = &provider_def.aws_profile {
                            items.push(("aws_profile", p.clone()));
                        }
                        if let Some(r) = &provider_def.aws_region {
                            items.push(("aws_region", r.clone()));
                        }
                    }
                    SupportedProvider::GcpSecretManager { provider_def } => {
                        if let Some(id) = &provider_def.gcp_project_id {
                            items.push(("gcp_project_id", id.clone()));
                        }
                    }
                    SupportedProvider::AzureKeyVault { provider_def } => {
                        if let Some(n) = &provider_def.vault_name {
                            items.push(("azure_vault_name", n.clone()));
                        }
                    }
                    SupportedProvider::Gopass { provider_def } => {
                        if let Some(s) = &provider_def.store {
                            items.push(("gopass_store", s.clone()));
                        }
                    }
                    SupportedProvider::OnePassword { provider_def } => {
                        if let Some(v) = &provider_def.vault {
                            items.push(("op_vault", v.clone()));
                        }
                        if let Some(a) = &provider_def.account {
                            items.push(("op_account", a.clone()));
                        }
                    }
                }
            }
        }

        if let Ok((_, Some(log_path))) = paths::log_config() {
            items.push(("log_path", display_path(&log_path)));
        }
        let output = items
            .iter()
            .map(|(name, value)| format!("{name:<30}{value}\n"))
            .collect::<Vec<String>>()
            .join("");
        Ok(output)
    }

    pub fn info(&self, app: &AppConfig) -> Result<String> {
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
            self.sysinfo(app)
        }
    }

    pub fn session_info(&self, app: &AppConfig) -> Result<String> {
        if let Some(session) = &self.session {
            let render_options = app.render_options()?;
            let mut markdown_render = crate::render::MarkdownRender::init(render_options)?;
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

    pub fn generate_prompt_context(&self, app: &AppConfig) -> HashMap<&str, String> {
        let mut output = HashMap::new();
        let role = self.extract_role_impl(app, false).unwrap_or_else(|err| {
            warn!("failed to compute effective role for prompt rendering: {err}");
            Role::default()
        });
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
        let reasoning_effort = role.reasoning_effort().or_else(|| {
            role.model()
                .default_reasoning_effort()
                .map(|s| s.to_string())
        });
        if let Some(effort) = reasoning_effort {
            output.insert("reasoning_effort", effort.to_string());
        }

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
        if app.dry_run {
            output.insert("dry_run", "true".to_string());
        }
        if app.stream {
            output.insert("stream", "true".to_string());
        }
        if app.save {
            output.insert("save", "true".to_string());
        }
        if let Some(wrap) = &app.wrap
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

        if app.highlight {
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

    pub fn render_prompt_left(&self, app: &AppConfig) -> String {
        let variables = self.generate_prompt_context(app);
        let left_prompt = app.left_prompt.as_deref().unwrap_or(LEFT_PROMPT);
        render_prompt(left_prompt, &variables)
    }

    pub fn render_prompt_right(&self, app: &AppConfig) -> String {
        let variables = self.generate_prompt_context(app);
        let right_prompt = app.right_prompt.as_deref().unwrap_or(RIGHT_PROMPT);
        render_prompt(right_prompt, &variables)
    }

    pub fn select_enabled_functions(&self, role: &Role) -> Vec<FunctionDeclaration> {
        let app = self.app.config.as_ref();
        let mut functions = vec![];
        if app.function_calling_support {
            // Compute the set of tool names enabled by the role filter, drawn
            // from BOTH the tool_scope pool and the agent's pool so that an
            // explicit `enabled_tools` list (e.g. from a graph LLM node) can
            // narrow the agent's own custom tools too.
            let role_filter: Option<HashSet<String>> = role.enabled_tools().map(|enabled_tools| {
                let mut declaration_names: HashSet<String> = self
                    .tool_scope
                    .functions
                    .declarations()
                    .iter()
                    .filter(|v| !is_mcp_meta_function(&v.name))
                    .map(|v| v.name.to_string())
                    .collect();

                if let Some(agent) = &self.agent {
                    declaration_names.extend(
                        agent
                            .functions()
                            .declarations()
                            .iter()
                            .filter(|v| !is_mcp_meta_function(&v.name))
                            .map(|v| v.name.to_string()),
                    );
                }

                let mut tool_names: HashSet<String> = Default::default();
                if enabled_tools.iter().any(|s| s.trim() == "all") {
                    tool_names.extend(declaration_names);
                } else {
                    for item in enabled_tools.iter() {
                        let item = item.trim();
                        if item.is_empty() {
                            continue;
                        }

                        if let Some(values) = app.mapping_tools.get(item) {
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
                tool_names
            });

            if let Some(ref tool_names) = role_filter {
                functions = self
                    .tool_scope
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

            if self.agent.is_none() {
                let existing: HashSet<String> = functions.iter().map(|f| f.name.clone()).collect();
                let builtin_functions: Vec<FunctionDeclaration> = self
                    .tool_scope
                    .functions
                    .declarations()
                    .iter()
                    .filter(|v| {
                        (v.name.starts_with(USER_FUNCTION_PREFIX)
                            || (!matches!(role.skills_enabled(), Some(false))
                                && v.name.starts_with(SKILL_FUNCTION_PREFIX))
                            || (self.auto_continue_config().enabled
                                && v.name.starts_with(TODO_FUNCTION_PREFIX))
                            || v.name.starts_with(RAG_FUNCTION_PREFIX)
                            || v.name.starts_with(JOB_FUNCTION_PREFIX))
                            && !existing.contains(&v.name)
                    })
                    .cloned()
                    .collect();
                functions.extend(builtin_functions);
            }

            if let Some(agent) = &self.agent {
                let mut agent_functions: Vec<FunctionDeclaration> = agent
                    .functions()
                    .declarations()
                    .to_vec()
                    .into_iter()
                    .filter(|v| !is_mcp_meta_function(&v.name))
                    .collect();

                if let Some(ref tool_names) = role_filter {
                    agent_functions.retain(|v| {
                        tool_names.contains(&v.name)
                            || (!matches!(agent.skills_enabled(), Some(false))
                                && v.name.starts_with(SKILL_FUNCTION_PREFIX))
                            || v.name.starts_with(USER_FUNCTION_PREFIX)
                            || v.name.starts_with(TODO_FUNCTION_PREFIX)
                            || v.name.starts_with(AGENT_FUNCTION_PREFIX)
                            || v.name.starts_with(MEMORY_FUNCTION_PREFIX)
                            || v.name.starts_with(RAG_FUNCTION_PREFIX)
                            || v.name.starts_with(JOB_FUNCTION_PREFIX)
                    });
                }

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

    pub fn select_enabled_mcp_servers(&self, role: &Role) -> Vec<FunctionDeclaration> {
        let app = self.app.config.as_ref();
        let mut mcp_functions = vec![];
        if app.mcp_server_support {
            let role_filter: Option<HashSet<String>> =
                role.enabled_mcp_servers().map(|enabled_mcp_servers| {
                    let mut mcp_declaration_names: HashSet<String> = self
                        .tool_scope
                        .functions
                        .declarations()
                        .iter()
                        .filter(|v| is_mcp_meta_function(&v.name))
                        .map(|v| v.name.to_string())
                        .collect();
                    if let Some(agent) = &self.agent {
                        mcp_declaration_names.extend(
                            agent
                                .functions()
                                .declarations()
                                .iter()
                                .filter(|v| is_mcp_meta_function(&v.name))
                                .map(|v| v.name.to_string()),
                        );
                    }

                    let mut server_names: HashSet<String> = Default::default();
                    if enabled_mcp_servers.iter().any(|s| s.trim() == "all") {
                        server_names.extend(mcp_declaration_names);
                    } else {
                        for item in enabled_mcp_servers.iter() {
                            let item = item.trim();
                            if item.is_empty() {
                                continue;
                            }

                            let item_search_name =
                                format!("{}_{item}", MCP_SEARCH_META_FUNCTION_NAME_PREFIX);
                            if let Some(values) = app.mapping_mcp_servers.get(item) {
                                server_names.extend(
                                    values
                                        .split(',')
                                        .flat_map(mcp_meta_function_names)
                                        .filter(|v| mcp_declaration_names.contains(v)),
                                )
                            } else if mcp_declaration_names.contains(&item_search_name) {
                                server_names.extend(mcp_meta_function_names(item));
                            }
                        }
                    }
                    server_names
                });

            if let Some(ref server_names) = role_filter {
                mcp_functions = self
                    .tool_scope
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
                    .filter(|v| is_mcp_meta_function(&v.name))
                    .collect();

                if let Some(ref server_names) = role_filter {
                    agent_functions.retain(|v| server_names.contains(&v.name));
                }

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

    pub fn select_functions(&self, role: &Role) -> Option<Vec<FunctionDeclaration>> {
        let mut functions = vec![];
        functions.extend(self.select_enabled_functions(role));
        functions.extend(self.select_enabled_mcp_servers(role));
        self.apply_job_tool_visibility(&mut functions);

        if functions.is_empty() {
            None
        } else {
            Some(functions)
        }
    }

    /// Node-local job-ownership visibility rule: the `job__*` family is only
    /// declared where it can do something. `job__start` requires at least one
    /// backgroundable tool among this request's declarations; the lifecycle
    /// verbs (`check`/`collect`/`cancel`/`list`) additionally survive while
    /// the context still owns registered jobs, so a job started before a
    /// filter change stays reachable.
    fn apply_job_tool_visibility(&self, functions: &mut Vec<FunctionDeclaration>) {
        let has_backgroundable = functions.iter().any(|f| is_backgroundable_tool(&f.name));
        if has_backgroundable {
            return;
        }
        let owns_jobs = self.owns_active_jobs();
        let start_name = format!("{JOB_FUNCTION_PREFIX}start");
        functions.retain(|f| {
            !f.name.starts_with(JOB_FUNCTION_PREFIX) || (owns_jobs && f.name != start_name)
        });
    }

    /// Whether this context has registered jobs it is responsible for:
    /// inside a graph LLM node, only the jobs that node started; everywhere
    /// else, any job in the context's supervisor.
    pub fn owns_active_jobs(&self) -> bool {
        let Some(supervisor) = self.supervisor.as_ref() else {
            return false;
        };
        let sup = supervisor.read();
        match self.node_job_scope.as_ref() {
            Some(ids) => ids.iter().any(|id| sup.job(id).is_some()),
            None => sup.jobs().next().is_some(),
        }
    }

    pub fn retrieve_role(&self, app: &AppConfig, name: &str) -> Result<Role> {
        let names = paths::list_roles(false);
        let mut role = if names.contains(&name.to_string()) {
            let path = paths::role_file(name);
            let content = read_to_string(&path)?;
            Role::new(name, &content)
        } else {
            Role::builtin(name)?
        };
        let current_model = self.current_model().clone();
        match role.model_id() {
            Some(model_id) => {
                if current_model.id() != model_id {
                    let model = Model::retrieve_model(app, model_id, ModelType::Chat)?;
                    role.set_model(model);
                } else {
                    role.set_model(current_model);
                }
            }
            None => {
                role.set_model(current_model);
                if role.temperature().is_none() {
                    role.set_temperature(app.temperature);
                }
                if role.top_p().is_none() {
                    role.set_top_p(app.top_p);
                }
            }
        }
        Ok(role)
    }

    /// Returns `Ok(true)` if a role-like was mutated, `Ok(false)` if
    /// the model was set on `ctx.model` directly (no role-like active).
    pub fn set_model_on_role_like(&mut self, app: &AppConfig, model_id: &str) -> Result<bool> {
        let model = Model::retrieve_model(app, model_id, ModelType::Chat)?;
        match self.role_like_mut() {
            Some(role_like) => {
                role_like.set_model(model);
                Ok(true)
            }
            None => {
                self.model = model;
                Ok(false)
            }
        }
    }

    #[allow(dead_code)]
    pub fn reload_current_model(&mut self, app: &AppConfig, model_id: &str) -> Result<()> {
        let model = Model::retrieve_model(app, model_id, ModelType::Chat)?;
        self.model = model;
        Ok(())
    }

    pub fn use_temp_role(&mut self, _app: &AppConfig, prompt: &str) -> Result<()> {
        let mut role = Role::new(TEMP_ROLE_NAME, prompt);
        role.set_model(self.current_model().clone());
        self.use_role_obj(role)?;
        self.refresh_mcp_tool_filters();
        Ok(())
    }

    pub fn edit_config(&self) -> Result<()> {
        let config_path = paths::config_file();
        let editor = self.app.config.editor()?;
        edit_file(&editor, &config_path)?;
        println!(
            "NOTE: Remember to restart {} if there are changes made to '{}'",
            env!("CARGO_CRATE_NAME"),
            config_path.display(),
        );
        Ok(())
    }

    pub fn edit_mcp_config(&self) -> Result<()> {
        let mcp_path = paths::mcp_config_file();
        let editor = self.app.config.editor()?;
        edit_file(&editor, &mcp_path)?;
        println!(
            "NOTE: Remember to restart {} for changes to '{}' to take effect",
            env!("CARGO_CRATE_NAME"),
            mcp_path.display(),
        );

        Ok(())
    }

    pub fn new_role(&self, app: &AppConfig, name: &str) -> Result<()> {
        if self.macro_flag {
            bail!("No role");
        }
        let ans = Confirm::new("Create a new role?")
            .with_default(true)
            .prompt()?;
        if ans {
            self.upsert_role(app, name)?;
        } else {
            bail!("No role");
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
        let role_path = paths::role_file(&role_name);
        if let Some(role) = self.role.as_mut() {
            role.save(&role_name, &role_path, self.working_mode.is_repl())?;
        }
        Ok(())
    }

    pub fn edit_session(&mut self, app: &AppConfig) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.name().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);
        self.save_session(Some(&name))?;
        let editor = app.editor()?;
        edit_file(&editor, &session_path).with_context(|| {
            format!(
                "Failed to edit '{}' with '{}'",
                session_path.display(),
                editor
            )
        })?;
        self.session = Some(Session::load_from_ctx(self, app, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn edit_agent_config(&self, app: &AppConfig) -> Result<()> {
        let agent_name = match &self.agent {
            Some(agent) => agent.name(),
            None => bail!("No agent"),
        };
        let config_path = paths::agent_config_file(agent_name);
        let graph_path = paths::agent_graph_file(agent_name);
        let target_path = if !config_path.exists() && graph_path.exists() {
            graph_path
        } else {
            config_path
        };

        ensure_parent_exists(&target_path)?;
        if !target_path.exists() {
            fs::write(
                &target_path,
                "# see https://github.com/Dark-Alex-17/coyote/blob/main/config.agent.example.yaml\n",
            )
            .with_context(|| format!("Failed to write to '{}'", target_path.display()))?;
        }

        let editor = app.editor()?;
        edit_file(&editor, &target_path)?;
        println!(
            "NOTE: Remember to reload the agent if there are changes made to '{}'",
            target_path.display()
        );

        Ok(())
    }

    pub fn new_macro(&self, app: &AppConfig, name: &str) -> Result<()> {
        if RESERVED_MACRO_NAMES.contains(&name) {
            bail!("'{name}' is a reserved macro name");
        }
        if self.macro_flag {
            bail!("No macro");
        }
        let ans = Confirm::new("Create a new macro?")
            .with_default(true)
            .prompt()?;
        if ans {
            let macro_path = paths::macro_file(name);
            ensure_parent_exists(&macro_path)?;
            let editor = app.editor()?;
            edit_file(&editor, &macro_path)?;
        } else {
            bail!("No macro");
        }
        Ok(())
    }

    pub fn in_non_isolated_macro(&self) -> bool {
        self.macro_flag && self.macro_non_isolated
    }

    pub fn macro_policy(&self) -> MacroPolicy {
        MacroPolicy::effective(
            &self.app.config,
            self.role.as_ref(),
            self.agent.as_ref(),
            self.session.as_ref(),
            &crate::repl::builtin_command_names(),
            self.app.config.no_workspace_macros,
        )
    }

    fn macro_variable_completions(
        &self,
        cmd: &str,
        completed_args: &[&str],
    ) -> Vec<(String, Option<String>)> {
        let Some(name) = cmd.strip_prefix('.') else {
            return vec![];
        };
        if !self
            .visible_macro_completions()
            .iter()
            .any(|(macro_name, _)| macro_name == name)
        {
            return vec![];
        }
        match Macro::load(name, self.app.config.no_workspace_macros) {
            Ok(macro_value) => macro_value.variable_completions(completed_args),
            Err(_) => vec![],
        }
    }

    pub fn macro_lock_owner(&self, level: MacroAllowlistLevel) -> String {
        let name = match level {
            MacroAllowlistLevel::Session => self.session.as_ref().map(|s| s.name()),
            MacroAllowlistLevel::Agent => self.agent.as_ref().map(|a| a.name()),
            MacroAllowlistLevel::Role => self.role.as_ref().map(|r| r.name()),
            MacroAllowlistLevel::Global => return "global config".to_string(),
        };
        match name {
            Some(name) => format!("{level}:{name}"),
            None => level.to_string(),
        }
    }

    pub fn macro_toggle(&mut self, name: &str, enable: bool) -> Result<()> {
        let restricting_level = if self
            .session
            .as_ref()
            .and_then(|s| s.enabled_macros())
            .is_some()
        {
            Some(MacroAllowlistLevel::Session)
        } else if self
            .agent
            .as_ref()
            .and_then(|a| a.enabled_macros())
            .is_some()
        {
            Some(MacroAllowlistLevel::Agent)
        } else if self
            .role
            .as_ref()
            .and_then(|r| r.enabled_macros())
            .is_some()
        {
            Some(MacroAllowlistLevel::Role)
        } else {
            None
        };
        if let Some(level) = restricting_level {
            bail!(
                "Macro toggles are restricted by {} enabled_macros; edit enabled_macros there",
                self.macro_lock_owner(level)
            );
        }

        let policy = self.macro_policy();
        match policy.find(name).map(|row| &row.state) {
            None => bail!("Unknown macro '{name}'"),
            Some(MacroState::Invalid { reason }) => bail!("Macro '{name}' is invalid: {reason}"),
            Some(_) => {}
        }
        let all_active: Vec<String> = policy
            .macros
            .iter()
            .filter(|row| {
                row.source.is_some()
                    && !row.shadowed_by_workspace
                    && !matches!(row.state, MacroState::Missing | MacroState::Invalid { .. })
            })
            .map(|row| row.name.clone())
            .collect();

        let action = if enable { "enabled" } else { "disabled" };

        match toggled_enabled_macros(
            self.app.config.enabled_macros.as_deref(),
            &all_active,
            name,
            enable,
        ) {
            Some(list) => {
                self.update_app_config(|app| app.enabled_macros = Some(list));
                println!("Macro '{name}' {action}");
            }
            None => println!("Macro '{name}' is already {action}"),
        }

        Ok(())
    }

    pub fn visible_macro_completions(&self) -> Vec<(String, Option<String>)> {
        self.macro_policy()
            .macros
            .into_iter()
            .filter(|row| row.state == MacroState::Enabled && !row.shadowed_by_workspace)
            .map(|row| (row.name, row.description))
            .collect()
    }

    pub fn list_assets(&self, kind: &str) -> Result<()> {
        match kind {
            "roles" => print_asset_names("roles", &paths::list_roles(true)),
            "sessions" => print_asset_names("sessions", &self.list_sessions()),
            "rags" => print_asset_names("RAGs", &paths::list_rags()),
            "macros" => {
                let policy = self.macro_policy();
                if policy.macros.is_empty() {
                    println!("No macros found.");
                    return Ok(());
                }

                let mut table =
                    asset_table(&["name", "source", "isolated", "state", "description"]);

                for row in &policy.macros {
                    let source = macro_source_display(row.source);
                    let isolated = match row.isolated {
                        Some(true) => "yes",
                        Some(false) => "no",
                        None => "-",
                    };
                    let state = macro_state_display(row, |level| self.macro_lock_owner(level));
                    let description = row.description.as_deref().unwrap_or_default();
                    table.add_row(vec![
                        row.name.as_str(),
                        &source,
                        isolated,
                        &state,
                        description,
                    ]);
                }

                println!("Macros:");
                println!("{table}");
                Ok(())
            }
            "agents" => {
                let entries = list_agents_with_descriptions();
                if entries.is_empty() {
                    println!("No agents found.");
                    return Ok(());
                }

                let mut table = asset_table(&["name", "description"]);
                for (name, description) in entries {
                    table.add_row(vec![name, description]);
                }

                println!("Agents:");
                println!("{table}");
                Ok(())
            }
            "skills" => {
                let policy = SkillPolicy::effective(
                    &self.app.config,
                    self.role.as_ref(),
                    self.agent.as_ref(),
                    self.session.as_ref(),
                )?;

                if !policy.skills_enabled {
                    bail!("Skills are disabled in this context");
                }

                let visible_names: Vec<String> = match self.app.config.visible_skills.as_deref() {
                    Some(list) => list.to_vec(),
                    None => paths::list_skills(),
                };

                let mut entries = Vec::new();
                for name in visible_names {
                    if !policy.compatible_enabled.contains(&name) {
                        continue;
                    }
                    let skill = match Skill::load(&name) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("Failed to open skill '{name}' for listing: {e}");
                            continue;
                        }
                    };

                    let loaded = self.skill_registry.is_loaded(skill.name());
                    entries.push((
                        skill.name().to_string(),
                        skill.description().to_string(),
                        loaded,
                    ));
                }

                if entries.is_empty() {
                    println!("No skills found.");
                    return Ok(());
                }

                let mut table = asset_table(&["loaded", "name", "description"]);
                for (name, description, loaded) in entries {
                    let marker = if loaded {
                        "✓".green().bold().to_string()
                    } else {
                        "✗".red().bold().to_string()
                    };
                    table.add_row(vec![marker, name, description]);
                }

                println!("Skills:");
                println!("{table}");
                Ok(())
            }
            "tools" => {
                let mut names = self.concrete_tool_names();
                let aliases: Vec<String> = self
                    .app
                    .config
                    .mapping_tools
                    .iter()
                    .filter(|(_, expansion)| {
                        expansion
                            .split(',')
                            .map(str::trim)
                            .any(|v| names.iter().any(|p| p.as_str() == v))
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                names.extend(aliases);
                names.sort_unstable();
                names.dedup();

                let active: HashSet<String> = if self.app.config.function_calling_support {
                    let role = self.extract_role(&self.app.config)?;
                    match self.select_functions(&role) {
                        None => HashSet::new(),
                        Some(functions) => functions.iter().map(|f| f.name.clone()).collect(),
                    }
                } else {
                    HashSet::new()
                };

                if names.is_empty() {
                    println!("No tools found.");
                    return Ok(());
                }

                println!("Tools:");
                for name in &names {
                    let marker = if active.contains(name.as_str()) {
                        "✓".green().bold().to_string()
                    } else {
                        "✗".red().bold().to_string()
                    };
                    println!("  {marker} {name}");
                }
                Ok(())
            }
            "mcp-servers" => {
                match self.mcp_servers_listing() {
                    Some(listing) => print!("{listing}"),
                    None => println!("No MCP servers found."),
                }
                Ok(())
            }
            "bundles" => bundles::list_installed_bundles(),
            _ => bail!(
                "Unknown kind '{kind}'. Valid kinds: roles, sessions, agents, rags, macros, skills, prompts, tools, mcp-servers, bundles"
            ),
        }
    }

    fn mcp_server_is_filtered(&self, name: &str) -> bool {
        let filters = &self.tool_scope.mcp_runtime.tool_filters;
        let has_layers = |id: &str| filters.get(id).is_some_and(|f| f.layers().next().is_some());
        has_layers(name)
            || expand_mcp_server_alias(&self.app.config.mapping_mcp_servers, name)
                .iter()
                .any(|id| has_layers(id))
    }

    pub fn mcp_servers_listing(&self) -> Option<String> {
        let mut names: Vec<String> = vec![];
        if let Some(mcp_config) = &self.app.mcp_config {
            names.extend(mcp_config.mcp_servers.keys().map(|v| v.to_string()));
        }
        names.extend(
            self.app
                .config
                .mapping_mcp_servers
                .keys()
                .map(|v| v.to_string()),
        );
        names.sort_unstable();
        names.dedup();

        if names.is_empty() {
            return None;
        }

        let enabled: Option<Vec<String>> = if let Some(session) = &self.session {
            session.enabled_mcp_servers()
        } else if let Some(role) = &self.role {
            role.enabled_mcp_servers()
        } else {
            self.app.config.enabled_mcp_servers.clone()
        };
        let skill_mcps = self.skill_registry.loaded_mcp_servers();

        let mut out = String::from("MCP servers:\n");
        for name in &names {
            let active = skill_mcps.contains(name.as_str())
                || matches!(&enabled, Some(list) if list.iter().any(|s| s.trim() == "all") || self.mcp_list_covers(list, name));
            let marker = if active {
                "✓".green().bold().to_string()
            } else {
                "✗".red().bold().to_string()
            };
            let tag = if self.mcp_server_is_filtered(name) {
                " [filtered]"
            } else {
                ""
            };
            out.push_str(&format!("  {marker} {name}{tag}\n"));
        }

        Some(out)
    }

    pub fn delete(&self, kind: &str) -> Result<()> {
        let (dir, file_ext) = match kind {
            "role" => (paths::roles_dir(), Some(".md")),
            "session" => (self.sessions_dir(), Some(".yaml")),
            "rag" => (paths::rags_dir(), Some(".yaml")),
            "macro" => (paths::macros_dir(), Some(".yaml")),
            "skill" => (paths::skills_dir(), None),
            "agent-data" => (paths::agents_data_dir(), None),
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
                                // Sidecars are not independently deletable assets.
                                // Guarded on `kind == "rag"` because this scan is shared
                                // by all six kinds, and `session`/`macro` also use
                                // `.yaml`. The helper lives in paths.rs beside
                                // list_rags() so both filters cannot drift apart.
                                if kind == "rag" && paths::is_rag_sidecar_name(name) {
                                    continue;
                                }
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
                    // Sidecars FIRST. If this fails, the .yaml is still on disk, the
                    // RAG is still listed, and the user can retry. Unlinking the .yaml
                    // first would make the deletion unretryable while leaving an
                    // orphaned mixin whitelisting a host in every sandbox launch.
                    if kind == "rag" {
                        paths::remove_rag_sidecars(&dir, &name)?;
                    }
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

    pub fn rag_sources(&self) -> Result<String> {
        match self.rag.as_ref() {
            Some(rag) => match rag.get_last_sources() {
                Some(v) => Ok(v),
                None => bail!("No sources"),
            },
            None => bail!("No RAG"),
        }
    }

    pub async fn update(&mut self, data: &str, abort_signal: AbortSignal) -> Result<()> {
        let (key, raw_value) = match data.split_once(char::is_whitespace) {
            Some((k, v)) => (k, v.trim()),
            None => bail!("Usage: .set <key> <value>. If value is null, unset key."),
        };

        if raw_value.is_empty() {
            bail!("Usage: .set <key> <value>. If value is null, unset key.");
        }

        let value = match key {
            "continuation_prompt" | "skill_instructions" => raw_value,
            _ => {
                if raw_value.contains(char::is_whitespace) {
                    bail!("Usage: .set <key> <value>. If value is null, unset key.");
                }
                raw_value
            }
        };
        match key {
            "temperature" => {
                let value = super::parse_value(value)?;
                if !self.set_temperature_on_role_like(value) {
                    self.update_app_config(|app| app.temperature = value);
                }
            }
            "top_p" => {
                let value = super::parse_value(value)?;
                if !self.set_top_p_on_role_like(value) {
                    self.update_app_config(|app| app.top_p = value);
                }
            }
            "reasoning_effort" => {
                let value: Option<String> = super::parse_value(value)?;
                if let Some(ref level) = value {
                    let levels = self.current_model().reasoning_levels();
                    if levels.is_empty() {
                        bail!("The current model does not support reasoning effort configuration");
                    }
                    if !levels.iter().any(|l| l == level) {
                        bail!(
                            "Invalid reasoning effort '{level}'. Supported levels for this model: {}",
                            levels.join(", ")
                        );
                    }
                }
                if !self.set_reasoning_effort_on_role_like(value.clone()) {
                    self.update_app_config(|app| app.reasoning_effort = value);
                }
            }
            "enabled_tools" => {
                if self.agent.as_ref().is_some_and(|a| a.is_graph()) {
                    bail!(
                        "Graph agents define tools per-node via `tools:` in graph.yaml; agent-level enabled_tools has no effect"
                    );
                }
                let raw: Option<String> = super::parse_value(value)?;
                let parsed: Option<Vec<String>> = raw.map(|s| super::csv_to_vec(&s));
                if !self.set_enabled_tools_on_role_like(parsed.clone()) {
                    self.update_app_config(|app| app.enabled_tools = parsed.clone());
                }
            }
            "enabled_skills" => {
                let raw: Option<String> = super::parse_value(value)?;
                let parsed: Option<Vec<String>> = raw.map(|s| super::csv_to_vec(&s));
                if let Some(names) = parsed.as_ref() {
                    let visible = self.app.config.visible_skills.as_deref();
                    for name in names {
                        paths::validate_skill_name(name)?;
                        match visible {
                            Some(vs) => {
                                if !vs.iter().any(|s| s == name) {
                                    bail!(
                                        "skill '{name}' is not in the global 'visible_skills' allow-list"
                                    );
                                }
                            }
                            None => {
                                if !paths::has_skill(name) {
                                    bail!("skill '{name}' is not installed");
                                }
                            }
                        }
                    }
                }
                self.update_app_config(|app| app.enabled_skills = parsed.clone());
            }
            "enabled_macros" => {
                let raw: Option<String> = super::parse_value(value)?;
                let parsed: Option<Vec<String>> = raw.map(|s| super::csv_to_vec(&s));
                if let Some(names) = parsed.as_ref() {
                    let policy = self.macro_policy();
                    for name in names {
                        if !policy
                            .macros
                            .iter()
                            .any(|m| m.source.is_some() && &m.name == name)
                        {
                            bail!("macro '{name}' is not installed");
                        }
                    }
                }
                self.update_app_config(|app| app.enabled_macros = parsed.clone());
            }
            "skills_enabled" => {
                let value: Option<bool> = super::parse_value(value)?;
                if let Some(session) = self.session.as_mut() {
                    session.set_skills_enabled(value);
                } else {
                    self.update_app_config(|app| app.skills_enabled = value.unwrap_or(true));
                }
                self.refresh_tool_scope(abort_signal.clone()).await?;
            }
            "enabled_mcp_servers" => {
                let raw: Option<String> = super::parse_value(value)?;
                let parsed: Option<Vec<String>> = raw.map(|s| super::csv_to_vec(&s));
                if let Some(servers) = parsed.as_ref() {
                    let Some(mcp_config) = &self.app.mcp_config else {
                        bail!(
                            "No MCP servers are configured. Please configure MCP servers first before setting 'enabled_mcp_servers'."
                        );
                    };
                    if mcp_config.mcp_servers.is_empty() {
                        bail!(
                            "No MCP servers are configured. Please configure MCP servers first before setting 'enabled_mcp_servers'."
                        );
                    }

                    if !servers.iter().all(|s| {
                        let server = s.trim();
                        server == "all" || mcp_config.mcp_servers.contains_key(server)
                    }) {
                        bail!(
                            "Some of the specified MCP servers in 'enabled_mcp_servers' are not fully configured. Please check your MCP server configuration."
                        );
                    }
                }
                if !self.set_enabled_mcp_servers_on_role_like(parsed.clone()) {
                    self.update_app_config(|app| app.enabled_mcp_servers = parsed.clone());
                }
                if self.app.config.mcp_server_support {
                    let app = Arc::clone(&self.app.config);
                    self.bootstrap_tools(app.as_ref(), true, abort_signal.clone())
                        .await?;
                }
            }
            k if k.starts_with("mcp_tools.") => {
                if self.agent.as_ref().is_some_and(|a| a.is_graph()) {
                    bail!(
                        "Graph agents define MCP tool filters per-node via 'mcp_tools:' in graph.yaml"
                    );
                }
                let server = k.strip_prefix("mcp_tools.").expect("guarded by match arm");
                if server.is_empty() {
                    bail!(
                        "Usage: .set mcp_tools.<server> <patterns> to set patterns, .set mcp_tools.<server> null to remove an entry, or .set mcp_tools null to clear this layer's map"
                    );
                }
                let is_configured = self
                    .app
                    .mcp_config
                    .as_ref()
                    .is_some_and(|c| c.mcp_servers.contains_key(server));
                let is_alias = self.app.config.mapping_mcp_servers.contains_key(server);
                if !is_configured && !is_alias {
                    bail!(
                        "MCP server '{server}' is not configured. Run `.list mcp-servers` to see what's available"
                    );
                }
                let enabled: Option<Vec<String>> = if let Some(session) = &self.session {
                    session.enabled_mcp_servers()
                } else if let Some(role) = &self.role {
                    role.enabled_mcp_servers()
                } else {
                    self.app.config.enabled_mcp_servers.clone()
                };
                let is_enabled = self.skill_registry.loaded_mcp_servers().contains(server)
                    || matches!(&enabled, Some(list) if list.iter().any(|s| s.trim() == "all") || self.mcp_list_covers(list, server));
                if !is_enabled {
                    bail!(
                        "MCP server '{server}' is not enabled in this context. Run `.list mcp-servers` to see what's available"
                    );
                }

                let raw: Option<String> = super::parse_value(value)?;
                let patterns: Option<Vec<String>> = raw.map(|s| super::csv_to_vec(&s));

                let current: Option<IndexMap<String, Vec<String>>> =
                    if let Some(session) = &self.session {
                        session.mcp_tools()
                    } else if let Some(agent) = &self.agent {
                        agent.mcp_tools()
                    } else if let Some(role) = &self.role {
                        role.mcp_tools()
                    } else {
                        self.app.config.mcp_tools.clone()
                    };
                let mut map = current.unwrap_or_default();
                match &patterns {
                    Some(list) => {
                        map.insert(server.to_string(), list.clone());
                    }
                    None => {
                        map.shift_remove(server);
                    }
                }
                let new = (!map.is_empty()).then_some(map);
                if !self.set_mcp_tools_on_role_like(new.clone()) {
                    self.update_app_config(|app| app.mcp_tools = new.clone());
                }
                self.refresh_mcp_tool_filters();

                if let Some(set_patterns) = &patterns {
                    let ids: Vec<String> = if is_configured {
                        vec![server.to_string()]
                    } else {
                        expand_mcp_server_alias(&self.app.config.mapping_mcp_servers, server)
                    };
                    for id in ids {
                        let Some(handle) = self.tool_scope.mcp_runtime.servers.get(&id).cloned()
                        else {
                            continue;
                        };
                        let Ok(tools) = handle.list_all_tools().await else {
                            continue;
                        };
                        let Some(filter) = self.tool_scope.mcp_runtime.tool_filters.get(&id) else {
                            continue;
                        };
                        let advertised: Vec<String> =
                            tools.iter().map(|tool| tool.name.to_string()).collect();
                        for (_, pattern) in filter.dead_context_patterns(&advertised) {
                            if set_patterns.iter().any(|p| p == pattern) {
                                println!(
                                    "Note: pattern '{pattern}' matches no allowed tools on '{server}'."
                                );
                            }
                        }
                    }
                }
            }
            "mcp_tools" => {
                let raw: Option<String> = super::parse_value(value)?;
                if raw.is_some() {
                    bail!(
                        "Usage: .set mcp_tools.<server> <patterns> to set patterns, .set mcp_tools.<server> null to remove an entry, or .set mcp_tools null to clear this layer's map"
                    );
                }
                if self.agent.as_ref().is_some_and(|a| a.is_graph()) {
                    bail!(
                        "Graph agents define MCP tool filters per-node via 'mcp_tools:' in graph.yaml"
                    );
                }
                if !self.set_mcp_tools_on_role_like(None) {
                    self.update_app_config(|app| app.mcp_tools = None);
                }
                self.refresh_mcp_tool_filters();
            }
            "max_output_tokens" => {
                let value = super::parse_value(value)?;
                if !self.set_max_output_tokens_on_role_like(value) {
                    self.model.set_max_tokens(value, true);
                }
            }
            "save_session" => {
                let value = super::parse_value(value)?;
                if !self.set_save_session_on_session(value) {
                    self.update_app_config(|app| app.save_session = value);
                }
            }
            "compression_threshold" => {
                let value = super::parse_value(value)?;
                if !self.set_compression_threshold_on_session(value) {
                    self.update_app_config(|app| {
                        app.compression_threshold = value.unwrap_or_default();
                    });
                }
            }
            "rag_reranker_model" => {
                let value = super::parse_value(value)?;
                let app = Arc::clone(&self.app.config);
                if !self.set_rag_reranker_model(app.as_ref(), value.clone())? {
                    self.update_app_config(|app| app.rag_reranker_model = value);
                }
            }
            "rag_top_k" => {
                let value: usize = value.parse().with_context(|| "Invalid value")?;
                if value == 0 {
                    bail!(
                        "rag_top_k must be >= 1; a top_k of 0 makes every query return no results."
                    );
                }
                if !self.set_rag_top_k(value)? {
                    self.update_app_config(|app| app.rag_top_k = value);
                }
            }
            "dry_run" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.update_app_config(|app| app.dry_run = value);
            }
            "function_calling_support" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                if value && self.tool_scope.functions.is_empty() {
                    bail!("Function calling cannot be enabled because no functions are installed.")
                }
                self.update_app_config(|app| app.function_calling_support = value);
            }
            "mcp_server_support" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.update_app_config(|app| app.mcp_server_support = value);
                let app = Arc::clone(&self.app.config);
                self.bootstrap_tools(app.as_ref(), value, abort_signal.clone())
                    .await?;
            }
            "stream" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.update_app_config(|app| app.stream = value);
            }
            "save" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.update_app_config(|app| app.save = value);
            }
            "highlight" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.update_app_config(|app| app.highlight = value);
            }
            "raw_markdown" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.update_app_config(|app| app.raw_markdown = value);
            }
            "auto_continue" => {
                let value: bool = value.parse().with_context(|| "Invalid value")?;
                if value && !self.app.config.function_calling_support {
                    bail!(
                        "Cannot enable auto_continue: function calling is disabled. Set 'function_calling_support: true' first."
                    );
                }
                if let Some(session) = self.session.as_mut() {
                    session.set_auto_continue(Some(value));
                } else {
                    self.update_app_config(|app| app.auto_continue = value);
                }
                let should_register = self.agent.is_none()
                    && self.app.config.function_calling_support
                    && self.auto_continue_config().enabled;
                let already_registered = self.tool_scope.functions.contains("todo__init");

                if should_register && !already_registered {
                    self.tool_scope.functions.append_todo_functions();
                } else if !should_register && already_registered {
                    self.tool_scope.functions.remove_todo_functions();
                }
            }
            "max_auto_continues" => {
                let value: usize = value.parse().with_context(|| "Invalid value")?;
                if let Some(session) = self.session.as_mut() {
                    session.set_max_auto_continues(Some(value));
                } else {
                    self.update_app_config(|app| app.max_auto_continues = value);
                }
            }
            "inject_todo_instructions" => {
                let value: bool = value.parse().with_context(|| "Invalid value")?;
                if let Some(session) = self.session.as_mut() {
                    session.set_inject_todo_instructions(Some(value));
                } else {
                    self.update_app_config(|app| app.inject_todo_instructions = value);
                }
            }
            "continuation_prompt" => {
                let value: Option<String> = super::parse_value(value)?;
                if let Some(session) = self.session.as_mut() {
                    session.set_continuation_prompt(value);
                } else {
                    self.update_app_config(|app| app.continuation_prompt = value);
                }
            }
            "inject_skill_instructions" => {
                let value: bool = value.parse().with_context(|| "Invalid value")?;
                if let Some(session) = self.session.as_mut() {
                    session.set_inject_skill_instructions(Some(value));
                } else {
                    self.update_app_config(|app| app.inject_skill_instructions = value);
                }
            }
            "skill_instructions" => {
                let value: Option<String> = super::parse_value(value)?;
                if let Some(session) = self.session.as_mut() {
                    session.set_skill_instructions(value);
                } else {
                    self.update_app_config(|app| app.skill_instructions = value);
                }
            }
            "memory" => {
                let value: bool = value.parse().with_context(|| "Invalid value")?;

                if let Some(session) = self.session.as_mut() {
                    session.set_memory(Some(value));
                } else {
                    self.update_app_config(|app| app.memory = Some(value));
                }

                let should_register = self.should_register_memory_tools();
                let already_registered = self.tool_scope.functions.contains("memory__read");

                if should_register && !already_registered {
                    self.tool_scope.functions.append_memory_functions();
                } else if !should_register && already_registered {
                    self.tool_scope.functions.remove_memory_functions();
                }
            }
            _ => bail!("Unknown key '{key}'"),
        }
        Ok(())
    }

    /// Returns `Ok(true)` if the active RAG was mutated, `Ok(false)` if
    /// no RAG is active (caller should fall back to the `AppConfig` default).
    pub fn set_rag_reranker_model(
        &mut self,
        app: &AppConfig,
        value: Option<String>,
    ) -> Result<bool> {
        if let Some(id) = &value {
            Model::retrieve_model(app, id, ModelType::Reranker)?;
        }
        match &self.rag {
            Some(_) => {
                let mut rag = self.rag.as_ref().expect("checked above").as_ref().clone();
                rag.set_reranker_model(value)?;
                self.rag = Some(Arc::new(rag));
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn set_rag_top_k(&mut self, value: usize) -> Result<bool> {
        match &self.rag {
            Some(_) => {
                let mut rag = self.rag.as_ref().expect("checked above").as_ref().clone();
                rag.set_top_k(value)?;
                self.rag = Some(Arc::new(rag));
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn repl_complete(
        &self,
        cmd: &str,
        args: &[&str],
        _line: &str,
    ) -> Vec<(String, Option<String>)> {
        let app = self.app.config.as_ref();
        let mut values: Vec<(String, Option<String>)> = vec![];
        let filter = args.last().unwrap_or(&"");
        if args.len() == 1 {
            values = match cmd {
                ".role" => super::map_completion_values(paths::list_roles(true)),
                ".model" => list_models(app, ModelType::Chat)
                    .into_iter()
                    .map(|v| (v.id(), Some(v.description())))
                    .collect(),
                ".session" => {
                    if args[0].starts_with("_/") {
                        super::map_completion_values(
                            self.list_autoname_sessions()
                                .iter()
                                .rev()
                                .map(|v| format!("_/{v}"))
                                .collect::<Vec<String>>(),
                        )
                    } else {
                        super::map_completion_values(self.list_sessions())
                    }
                }
                ".rag" => super::map_completion_values(paths::list_rags()),
                ".agent" => list_agents_with_descriptions()
                    .into_iter()
                    .map(|(name, desc)| (name, if desc.is_empty() { None } else { Some(desc) }))
                    .collect(),
                ".install" => {
                    let mut names: Vec<String> =
                        AssetCategory::NAMES.iter().map(|s| s.to_string()).collect();
                    names.extend(installed_bundle_names());
                    let mut values = super::map_completion_values(names);
                    values.push((
                        "--filter".to_string(),
                        Some("Restrict a remote install to one category".to_string()),
                    ));
                    values.push((
                        "--force".to_string(),
                        Some("Overwrite all conflicts without prompting".to_string()),
                    ));
                    values.push((
                        "--git-host".to_string(),
                        Some("Host the owner/repo shorthand expands against".to_string()),
                    ));
                    values.push((
                        "--help".to_string(),
                        Some("Show usage for .install".to_string()),
                    ));
                    values
                }
                ".uninstall" => {
                    let mut values = super::map_completion_values(installed_bundle_names());
                    values.push((
                        "--yes".to_string(),
                        Some("Skip the uninstall confirmation".to_string()),
                    ));
                    values.push((
                        "--help".to_string(),
                        Some("Show usage for .uninstall".to_string()),
                    ));

                    values
                }
                ".macro" => {
                    let policy = self.macro_policy();
                    let mut values: Vec<(String, Option<String>)> = policy
                        .macros
                        .iter()
                        .filter(|row| {
                            row.source.is_some()
                                && !row.shadowed_by_workspace
                                && row.state.is_invocable()
                        })
                        .map(|row| (row.name.clone(), row.description.clone()))
                        .collect();
                    values.push((
                        "enable ".to_string(),
                        Some("Re-enable a runtime-disabled macro".to_string()),
                    ));
                    values.push((
                        "disable ".to_string(),
                        Some("Disable a macro for the rest of this process".to_string()),
                    ));
                    values
                }
                ".reasoning" => {
                    let levels = self.current_model().reasoning_levels();
                    levels.iter().map(|v| (v.clone(), None)).collect()
                }
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
                    let mut values: Vec<String> =
                        SET_COMPLETION_KEYS.iter().map(|v| v.to_string()).collect();
                    if !self.current_model().reasoning_levels().is_empty() {
                        values.push("reasoning_effort".to_string());
                    }
                    if let Some(mcp_config) = &self.app.mcp_config {
                        values.extend(
                            mcp_config
                                .mcp_servers
                                .keys()
                                .map(|name| format!("mcp_tools.{name}")),
                        );
                    }
                    values.extend(
                        self.app
                            .config
                            .mapping_mcp_servers
                            .keys()
                            .map(|name| format!("mcp_tools.{name}")),
                    );
                    values.sort_unstable();
                    values.dedup();
                    values
                        .into_iter()
                        .map(|v| (format!("{v} "), None))
                        .collect()
                }
                ".delete" => super::map_completion_values(vec![
                    "role",
                    "session",
                    "rag",
                    "macro",
                    "skill",
                    "agent-data",
                ]),
                ".list" => super::map_completion_values(vec![
                    "roles",
                    "sessions",
                    "agents",
                    "rags",
                    "macros",
                    "skills",
                    "prompts",
                    "tools",
                    "mcp-servers",
                    "bundles",
                ]),
                ".vault" => {
                    let mut values = vec!["add", "get", "update", "delete", "list"];
                    values.sort_unstable();
                    values
                        .into_iter()
                        .map(|v| (format!("{v} "), None))
                        .collect()
                }
                _ => self.macro_variable_completions(cmd, &[]),
            };
        } else if cmd == ".mcp" && args.first() == Some(&"auth") && args.len() == 2 {
            if let Some(mcp_config) = &self.app.mcp_config {
                values = super::map_completion_values(
                    mcp_config
                        .mcp_servers
                        .iter()
                        .filter(|(_, spec)| spec.is_remote())
                        .map(|(name, _)| name.clone())
                        .collect(),
                );
            }
        } else if cmd == ".info" && args.first() == Some(&"mcp-server") && args.len() == 2 {
            let mut names: Vec<String> = self
                .tool_scope
                .mcp_runtime
                .servers
                .keys()
                .cloned()
                .collect();
            names.sort_unstable();
            values = super::map_completion_values(names);
        } else if cmd == ".mcp"
            && (args.first() == Some(&"enable") || args.first() == Some(&"disable"))
            && args.len() == 2
        {
            let current = if let Some(session) = &self.session {
                session.enabled_mcp_servers()
            } else if let Some(role) = &self.role {
                role.enabled_mcp_servers()
            } else {
                self.app.config.enabled_mcp_servers.clone()
            }
            .unwrap_or_default();
            let has_all = current.iter().any(|s| s.trim() == "all");

            let candidates: Vec<String> = if args.first() == Some(&"enable") {
                if has_all {
                    vec![]
                } else {
                    let mut candidates: Vec<String> = vec![];
                    if let Some(mcp_config) = &self.app.mcp_config {
                        candidates.extend(mcp_config.mcp_servers.keys().cloned());
                    }
                    candidates.extend(self.app.config.mapping_mcp_servers.keys().cloned());
                    candidates.sort_unstable();
                    candidates.dedup();
                    candidates.retain(|v| !self.mcp_list_covers(&current, v));

                    candidates
                }
            } else if has_all {
                self.app
                    .mcp_config
                    .as_ref()
                    .map(|c| c.mcp_servers.keys().cloned().collect())
                    .unwrap_or_default()
            } else {
                current
                    .iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| s != "all" && !s.is_empty())
                    .collect()
            };
            values = super::map_completion_values(candidates);
        } else if cmd == ".tool"
            && (args.first() == Some(&"enable") || args.first() == Some(&"disable"))
            && args.len() == 2
        {
            let current = if let Some(session) = &self.session {
                session.enabled_tools()
            } else if let Some(agent) = &self.agent {
                agent.enabled_tools()
            } else if let Some(role) = &self.role {
                role.enabled_tools()
            } else {
                self.app.config.enabled_tools.clone()
            };
            let agent_unfiltered = self.agent.is_some() && current.is_none();
            let current = current.unwrap_or_default();
            let has_all = agent_unfiltered || current.iter().any(|s| s.trim() == "all");

            let candidates: Vec<String> = if args.first() == Some(&"enable") {
                if has_all {
                    vec![]
                } else {
                    let pool = self.concrete_tool_names();
                    let mut candidates = pool.clone();
                    candidates.extend(
                        self.app
                            .config
                            .mapping_tools
                            .iter()
                            .filter(|(_, expansion)| {
                                expansion
                                    .split(',')
                                    .map(str::trim)
                                    .any(|v| pool.iter().any(|p| p.as_str() == v))
                            })
                            .map(|(k, _)| k.clone()),
                    );
                    candidates.sort_unstable();
                    candidates.dedup();
                    candidates.retain(|v| !self.tool_list_covers(&current, v));

                    candidates
                }
            } else if has_all {
                self.concrete_tool_names()
            } else {
                current
                    .iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| s != "all" && !s.is_empty())
                    .collect()
            };
            values = super::map_completion_values(candidates);
        } else if cmd == ".macro"
            && (args.first() == Some(&"enable") || args.first() == Some(&"disable"))
            && args.len() == 2
        {
            let enable = args.first() == Some(&"enable");
            values = self
                .macro_policy()
                .macros
                .into_iter()
                .filter(|row| row.source.is_some() && !row.shadowed_by_workspace)
                .filter(|row| {
                    if enable {
                        row.state == MacroState::DisabledRuntime
                    } else {
                        row.state.is_invocable()
                    }
                })
                .map(|row| (row.name, row.description))
                .collect();
        } else if cmd == ".macro"
            && args.len() >= 2
            && args.first() != Some(&"enable")
            && args.first() != Some(&"disable")
        {
            if let Ok(macro_value) = Macro::load(args[0], app.no_workspace_macros) {
                values = macro_value.variable_completions(&args[1..args.len() - 1]);
            }
        } else if (cmd == ".edit" && args.first() == Some(&"skill") && args.len() == 2)
            || (cmd == ".skill" && args.first() == Some(&"load") && args.len() == 2)
        {
            values = complete_skills_with_descriptions(paths::list_skills());
        } else if cmd == ".skill" && args.first() == Some(&"unload") && args.len() == 2 {
            values = complete_skills_with_descriptions(self.skill_registry.loaded_names());
        } else if cmd == ".install" && args.len() >= 2 {
            let prev = args.get(args.len() - 2).copied().unwrap_or("");
            if prev == "--filter" {
                values = super::map_completion_values(
                    InstallFilter::NAMES.iter().map(|s| s.to_string()).collect(),
                );
            } else if prev == "--git-host" {
                values = super::map_completion_values(vec![DEFAULT_GIT_HOST.to_string()]);
            } else {
                let has_filter = args.iter().enumerate().any(|(i, a)| {
                    a.starts_with("--filter=") || (*a == "--filter" && i < args.len() - 1)
                });
                let has_force = args.contains(&"--force");
                let has_git_host = args.iter().enumerate().any(|(i, a)| {
                    a.starts_with("--git-host=") || (*a == "--git-host" && i < args.len() - 1)
                });
                let mut available: Vec<&str> = vec![];

                if !has_filter {
                    available.push("--filter");
                }
                if !has_force {
                    available.push("--force");
                }
                if !has_git_host {
                    available.push("--git-host");
                }
                if !args.contains(&"--help") {
                    available.push("--help");
                }

                values = super::map_completion_values(available);
            }
        } else if cmd == ".set" && args.len() == 2 {
            let candidates = match args[0] {
                "max_output_tokens" => match self.current_model().max_output_tokens() {
                    Some(v) => vec![v.to_string()],
                    None => vec![],
                },
                "dry_run" => super::complete_bool(app.dry_run),
                "stream" => super::complete_bool(app.stream),
                "save" => super::complete_bool(app.save),
                "function_calling_support" => super::complete_bool(app.function_calling_support),
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
                    values.extend(
                        self.tool_scope
                            .functions
                            .declarations()
                            .iter()
                            .filter(|v| {
                                !v.name.starts_with("user__")
                                    && !v.name.starts_with("mcp_")
                                    && !v.name.starts_with("todo__")
                                    && !v.name.starts_with("agent__")
                            })
                            .map(|v| v.name.clone()),
                    );
                    values.extend(app.mapping_tools.keys().map(|v| v.to_string()));
                    values
                        .into_iter()
                        .filter(|v| !ignores.contains(v.as_str()))
                        .map(|v| format!("{prefix}{v}"))
                        .collect()
                }
                "mcp_server_support" => super::complete_bool(app.mcp_server_support),
                "skills_enabled" => {
                    let current = if let Some(session) = &self.session {
                        session.skills_enabled()
                    } else {
                        Some(app.skills_enabled)
                    };
                    super::complete_option_bool(current)
                }
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

                    if let Some(mcp_config) = &self.app.mcp_config {
                        values.extend(mcp_config.mcp_servers.keys().map(|v| v.to_string()));
                    }
                    values.extend(app.mapping_mcp_servers.keys().map(|v| v.to_string()));
                    values.sort();
                    values.dedup();
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
                        app.save_session
                    };
                    super::complete_option_bool(save_session)
                }
                "rag_reranker_model" => list_models(app, ModelType::Reranker)
                    .iter()
                    .map(|v| v.id())
                    .collect(),
                "highlight" => super::complete_bool(app.highlight),
                "raw_markdown" => super::complete_bool(app.raw_markdown),
                "auto_continue" => {
                    let config = self.auto_continue_config();
                    super::complete_bool(config.enabled)
                }
                "max_auto_continues" => {
                    let config = self.auto_continue_config();
                    vec![config.max_continues.to_string()]
                }
                "inject_todo_instructions" => {
                    let config = self.auto_continue_config();
                    super::complete_bool(config.inject_instructions)
                }
                "continuation_prompt" => vec!["null".to_string()],
                "inject_skill_instructions" => {
                    let config = self.skill_instructions_config();
                    super::complete_bool(config.inject)
                }
                "skill_instructions" => vec!["null".to_string()],
                "memory" => super::complete_bool(self.should_inject_memory()),
                "reasoning_effort" => {
                    let levels = self.current_model().reasoning_levels();
                    levels.to_vec()
                }
                _ => vec![],
            };
            values = candidates.into_iter().map(|v| (v, None)).collect();
        } else if cmd == ".vault" && args.len() == 2 && args[0] != "list" {
            values = self
                .app
                .vault
                .list_secrets(false)
                .unwrap_or_default()
                .into_iter()
                .map(|v| (v, None))
                .collect();
        } else if cmd == ".agent" {
            if args.len() == 2 {
                let dir = paths::agent_data_dir(args[0]).join(SESSIONS_DIR_NAME);
                values = list_file_names(dir, ".yaml")
                    .into_iter()
                    .map(|v| (v, None))
                    .collect();
            }
            values.extend(super::complete_agent_variables(args[0]));
        } else if args.len() >= 2 {
            values = self.macro_variable_completions(cmd, &args[..args.len() - 1]);
        };
        fuzzy_filter(values, |v| v.0.as_str(), filter)
    }

    pub fn mcp_prompt_completion(&self, args: &[&str]) -> McpPromptCompletion {
        let app = self.app.config.as_ref();
        let enabled_ids = match &self.app.mcp_config {
            Some(mcp_config) => {
                let mut servers = self
                    .enabled_mcp_servers_for_current_scope(app, true)
                    .unwrap_or_default();
                servers.extend(self.skill_registry.loaded_mcp_servers());
                expand_enabled_mcp_server_ids(app, mcp_config, &servers)
            }
            None => vec![],
        };
        self.tool_scope
            .mcp_runtime
            .prompt_completion(&enabled_ids, args)
    }

    pub async fn list_mcp_prompts(&self) -> Result<()> {
        let items = self.tool_scope.mcp_runtime.prompt_catalog().await;
        if items.is_empty() {
            println!("No prompts found.");
            return Ok(());
        }

        let mut table = asset_table(&["server", "name", "description", "args"]);
        for row in mcp_prompt_rows(&items) {
            table.add_row(row.to_vec());
        }

        println!("Prompts:");
        println!("{table}");
        Ok(())
    }

    async fn rebuild_tool_scope(
        &mut self,
        app: &AppConfig,
        enabled_mcp_servers: Option<Vec<String>>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let policy = SkillPolicy::effective(
            app,
            self.role.as_ref(),
            self.agent.as_ref(),
            self.session.as_ref(),
        )?;

        let enabled_mcp_servers = if policy.skills_enabled && app.mcp_server_support {
            let skill_mcps = self.skill_registry.loaded_mcp_servers();
            let has_all = enabled_mcp_servers
                .as_ref()
                .map(|v| v.iter().any(|s| s.trim() == "all"))
                .unwrap_or(false);
            if has_all || skill_mcps.is_empty() {
                enabled_mcp_servers
            } else {
                let mut merged: BTreeSet<String> = skill_mcps;
                if let Some(servers) = &enabled_mcp_servers {
                    for token in servers {
                        let t = token.trim();
                        if !t.is_empty() {
                            merged.insert(t.to_string());
                        }
                    }
                }
                Some(merged.into_iter().collect())
            }
        } else {
            enabled_mcp_servers
        };

        let mut mcp_runtime = McpRuntime::new();

        if app.mcp_server_support
            && let Some(mcp_config) = &self.app.mcp_config
        {
            let server_ids: Vec<String> = match &enabled_mcp_servers {
                Some(servers) => expand_enabled_mcp_server_ids(app, mcp_config, servers),
                None => vec![],
            };

            if !server_ids.is_empty() {
                let app_ref = &self.app;
                let acquire_all = async {
                    let mut handles = Vec::new();
                    let mut auth_required = Vec::new();
                    for id in &server_ids {
                        if let Some(spec) = mcp_config.mcp_servers.get(id) {
                            match app_ref
                                .mcp_factory
                                .acquire(id, spec, app_ref.mcp_log_path.as_deref())
                                .await
                            {
                                Ok(handle) => handles.push((id.clone(), handle)),
                                Err(e) if is_auth_required_error(&e) => {
                                    let reason = e
                                        .downcast_ref::<McpAuthRequired>()
                                        .map(|a| a.reason)
                                        .unwrap_or(McpAuthReason::NotAuthenticated);
                                    auth_required.push((id.clone(), reason));
                                }
                                Err(e) => return Err(e),
                            }
                        }
                    }
                    Ok::<_, Error>((handles, auth_required))
                };
                let (handles, auth_required) = abortable_run_with_spinner(
                    acquire_all,
                    "Loading MCP servers",
                    abort_signal.clone(),
                )
                .await?;
                for (id, handle) in handles {
                    mcp_runtime.insert(id, handle);
                }
                for (id, reason) in auth_required {
                    eprintln!("Warning: {}", McpAuthRequired { server: id, reason });
                }
            }
        }

        let mut functions = Functions::init(app.visible_tools.as_deref())?;
        if self.working_mode.is_repl() {
            functions.append_user_interaction_functions();
        }
        if self.agent.is_none()
            && app.function_calling_support
            && self.auto_continue_config().enabled
        {
            functions.append_todo_functions();
        }
        if !mcp_runtime.is_empty() {
            functions.append_mcp_meta_functions(mcp_runtime.server_features());
        }
        if app.function_calling_support && policy.skills_enabled {
            functions.append_skill_functions();
        }
        if self.should_register_memory_tools() {
            functions.append_memory_functions();
        }
        if self.rag.is_some()
            && app.function_calling_support
            && !self.agent.as_ref().is_some_and(|a| a.is_graph())
        {
            functions.append_rag_query_functions();
        }
        if self.agent.is_none() && jobs_enabled(None, app) {
            functions.append_job_functions();
        }

        let tool_tracker = self.tool_scope.tool_tracker.clone();
        self.tool_scope = ToolScope {
            functions,
            mcp_runtime,
            tool_tracker,
        };
        self.refresh_mcp_tool_filters();
        Ok(())
    }

    /// In-place full recompute of `tool_scope.mcp_runtime.tool_filters` from
    /// the current declarative state: mcp.json `allowedTools`, app config,
    /// active role, agent, session, one layer per loaded skill, and the
    /// active graph-node layer (always applied last). Never incremental, so
    /// detach and unload paths need no layer-removal logic.
    pub fn refresh_mcp_tool_filters(&mut self) {
        self.tool_scope.mcp_runtime.tool_filters = self.compute_mcp_tool_filters();
    }

    fn compute_mcp_tool_filters(&self) -> HashMap<String, ToolFilter> {
        let Some(mcp_config) = self.app.mcp_config.as_ref() else {
            return HashMap::new();
        };
        let app = &self.app.config;
        let session_map = self.session.as_ref().and_then(|s| s.mcp_tools());
        let agent_map = self.agent.as_ref().and_then(|a| a.mcp_tools());
        let agent = self
            .agent
            .as_ref()
            .zip(agent_map.as_ref())
            .map(|(a, map)| (a.name(), map));
        let role_map = self.role.as_ref().and_then(|r| r.mcp_tools());
        let role = self
            .role
            .as_ref()
            .zip(role_map.as_ref())
            .map(|(r, map)| (r.name(), map));
        let skills: Vec<SkillMcpLayer> = self
            .skill_registry
            .loaded_skills()
            .filter_map(|skill| {
                let mcp_tools = skill.mcp_tools()?.clone();
                Some(SkillMcpLayer {
                    name: skill.name().to_string(),
                    enabled_servers: expand_enabled_mcp_server_ids(
                        app,
                        mcp_config,
                        skill.enabled_mcp_servers().unwrap_or_default(),
                    ),
                    mcp_tools,
                })
            })
            .collect();
        let node = self
            .active_node_mcp_tools
            .as_ref()
            .map(|(id, map)| (id.as_str(), map));

        McpToolPolicy::effective(
            mcp_config,
            session_map.as_ref(),
            agent,
            role,
            app.mcp_tools.as_ref(),
            &skills,
            node,
            &app.mapping_mcp_servers,
        )
    }

    pub async fn refresh_tool_scope(&mut self, abort_signal: AbortSignal) -> Result<()> {
        let app = (*self.app.config).clone();
        let base_mcps = if app.mcp_server_support {
            if let Some(session) = &self.session {
                session.enabled_mcp_servers()
            } else if let Some(agent) = &self.agent {
                let names = agent.mcp_server_names();
                if names.is_empty() {
                    None
                } else {
                    Some(names.to_vec())
                }
            } else if let Some(role) = &self.role {
                role.enabled_mcp_servers()
            } else {
                app.enabled_mcp_servers.clone()
            }
        } else {
            None
        };

        self.rebuild_tool_scope(&app, base_mcps, abort_signal).await
    }

    pub async fn use_role(
        &mut self,
        app: &AppConfig,
        name: &str,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let role = self.retrieve_role(app, name)?;
        if let Some(session) = self.session.as_mut() {
            session.guard_empty()?;
        }

        let mcp_servers = if app.mcp_server_support {
            role.enabled_mcp_servers()
        } else {
            if role.enabled_mcp_servers().is_some() {
                eprintln!(
                    "{}",
                    formatdoc!(
                        "
                        This role uses MCP servers, but MCP support is disabled.
                        To enable it, exit the role and set 'mcp_server_support: true', then try again
                        "
                    )
                );
            }
            None
        };

        if let Some(ref effort) = role.reasoning_effort() {
            let levels = role.model().reasoning_levels();
            if levels.is_empty() {
                bail!(
                    "Role has reasoning_effort '{}' configured but the model does not support reasoning effort",
                    effort
                );
            }
            if !levels.iter().any(|l| l == effort) {
                bail!(
                    "Role's reasoning_effort '{}' is not valid for the model. Supported levels: {}",
                    effort,
                    levels.join(", ")
                );
            }
        }
        let prev_role = self.role.clone();
        let prev_session = self.session.clone();
        self.use_role_obj(role)?;
        if let Err(e) = self
            .rebuild_tool_scope(app, mcp_servers, abort_signal)
            .await
        {
            self.role = prev_role;
            self.session = prev_session;
            return Err(e);
        }
        Ok(())
    }

    pub async fn use_session(
        &mut self,
        app: &AppConfig,
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
                session = Some(Session::new_from_ctx(self, app, TEMP_SESSION_NAME)?);
            }
            Some(name) => {
                let session_path = self.session_file(name);
                if !session_path.exists() {
                    session = Some(Session::new_from_ctx(self, app, name)?);
                } else {
                    session = Some(Session::load_from_ctx(self, app, name, &session_path)?);
                }
            }
        }
        let mut new_session = false;
        if let Some(session) = session.as_mut() {
            let mcp_servers = if app.mcp_server_support {
                session.enabled_mcp_servers()
            } else {
                if session.enabled_mcp_servers().is_some() {
                    eprintln!(
                        "{}",
                        formatdoc!(
                            "
                            This session uses MCP servers, but MCP support is disabled.
                            To enable it, exit the session and set 'mcp_server_support: true', then try again
                            "
                        )
                    );
                }
                None
            };

            if let Some(ref effort) = session.reasoning_effort() {
                let levels = session.model().reasoning_levels();
                if levels.is_empty() {
                    bail!(
                        "Session has reasoning_effort '{}' configured but the model does not support reasoning effort",
                        effort
                    );
                }
                if !levels.iter().any(|l| l == effort) {
                    bail!(
                        "Session's reasoning_effort '{}' is not valid for the model. Supported levels: {}",
                        effort,
                        levels.join(", ")
                    );
                }
            }

            self.rebuild_tool_scope(app, mcp_servers, abort_signal.clone())
                .await?;

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
        self.refresh_mcp_tool_filters();
        self.init_agent_session_variables(new_session)?;
        Ok(())
    }

    pub async fn use_agent(
        &mut self,
        app: &AppConfig,
        agent_name: &str,
        session_name: Option<&str>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        if !app.function_calling_support {
            bail!("Please enable function calling support before using the agent.");
        }
        if self.agent.is_some() {
            bail!("Already in an agent, please run '.exit agent' first to exit the current agent.");
        }

        let current_model = self.current_model().clone();
        let agent = Agent::init(
            app,
            &self.app,
            &current_model,
            self.info_flag,
            agent_name,
            abort_signal.clone(),
        )
        .await?;

        if let Some(ref effort) = agent.reasoning_effort() {
            let levels = agent.model().reasoning_levels();
            if levels.is_empty() {
                bail!(
                    "Agent has reasoning_effort '{}' configured but the model does not support reasoning effort",
                    effort
                );
            }
            if !levels.iter().any(|l| l == effort) {
                bail!(
                    "Agent's reasoning_effort '{}' is not valid for the model. Supported levels: {}",
                    effort,
                    levels.join(", ")
                );
            }
        }

        let is_graph_agent = graph::agent_has_graph(agent_name);
        if is_graph_agent && session_name.is_some() {
            bail!(
                "Graph-based agent '{agent_name}' does not support sessions. \
                 The graph manages its own state; re-run without a session."
            );
        }

        let mcp_servers = if app.mcp_server_support {
            (!agent.mcp_server_names().is_empty()).then(|| agent.mcp_server_names().to_vec())
        } else {
            if !agent.mcp_server_names().is_empty() {
                bail!(
                    "This agent uses MCP servers, but MCP support is disabled.\nTo enable it, set 'mcp_server_support: true', then try again."
                );
            }
            None
        };

        self.rebuild_tool_scope(app, mcp_servers, abort_signal.clone())
            .await?;

        if !agent.model().supports_function_calling() {
            eprintln!(
                "Warning: The model '{}' does not support function calling. Agent tools (including todo, spawning, and user interaction) will not be available.",
                agent.model().id()
            );
        }

        // Graph agents manage their own state; never engage a session,
        // not even an inherited app-level `agent_session` default.
        // Isolated macros suppress an inherited default too: their forked
        // context has no session to return to. A non-isolated macro's `.agent`
        // step engages it exactly as if the user had typed the command.
        let session_name = session_name.map(|v| v.to_string()).or_else(|| {
            if (self.macro_flag && !self.macro_non_isolated) || is_graph_agent {
                None
            } else {
                agent.agent_session().map(|v| v.to_string())
            }
        });

        if self.session.is_some() {
            bail!(
                "Already in a session, please run '.exit session' first to exit the current session."
            );
        }

        let jobs_enabled = jobs_enabled(Some(&agent), app);
        let should_init_supervisor = agent.can_spawn_agents() || jobs_enabled;
        let max_concurrent_agents = if agent.can_spawn_agents() {
            agent.max_concurrent_agents()
        } else {
            0
        };
        let max_depth = agent.max_agent_depth();
        let max_jobs = effective_max_concurrent_jobs(Some(&agent), app);
        let supervisor = should_init_supervisor.then(|| {
            Arc::new(RwLock::new(
                Supervisor::new(max_concurrent_agents, max_depth)
                    .with_max_concurrent_jobs(max_jobs),
            ))
        });

        self.rag = agent.rag();
        // Keep `rag_key` in lockstep with `rag`. Agent RAGs are cached under
        // `RagKey::Agent(<agent name>)` (see `Agent::init`), so mirror that key exactly;
        // leaving the previous key in place would let `.rebuild rag` invalidate an
        // unrelated RAG's cache entry, and leaving it `None` would invalidate nothing.
        self.rag_key = self
            .rag
            .is_some()
            .then(|| RagKey::Agent(agent.name().to_string()));
        self.agent = Some(agent);
        self.refresh_mcp_tool_filters();
        if let Some(old) = self.supervisor.as_ref() {
            old.read().cancel_recursive();
        }
        self.supervisor = supervisor;
        self.inbox = None;
        self.parent_inbox = None;
        self.escalation_queue = None;
        self.notification_queue = Arc::new(NotificationQueue::new());
        self.self_agent_id = None;
        self.parent_supervisor = None;
        self.current_depth = 0;
        self.auto_continue_count = 0;
        self.todo_list = TodoList::default();
        self.auto_continue_paused = None;

        if let Some(session_name) = session_name.as_deref() {
            self.use_session(app, Some(session_name), abort_signal)
                .await?;
        } else {
            self.init_agent_shared_variables()?;
        }
        self.agent_variables = None;

        Ok(())
    }

    pub fn exit_agent(&mut self, app: &AppConfig) -> Result<()> {
        self.exit_session()?;
        let mut functions = Functions::init(app.visible_tools.as_deref())?;
        if self.working_mode.is_repl() {
            functions.append_user_interaction_functions();
        }
        if jobs_enabled(None, app) {
            functions.append_job_functions();
        }
        let tool_tracker = self.tool_scope.tool_tracker.clone();
        self.tool_scope = ToolScope {
            functions,
            mcp_runtime: McpRuntime::default(),
            tool_tracker,
        };

        if self.agent.take().is_some() {
            if let Some(supervisor) = self.supervisor.clone() {
                supervisor.read().cancel_recursive();
            }
            self.supervisor = None;
            self.parent_supervisor = None;
            self.self_agent_id = None;
            self.inbox = None;
            self.parent_inbox = None;
            self.escalation_queue = None;
            self.notification_queue = Arc::new(NotificationQueue::new());
            self.current_depth = 0;
            self.auto_continue_count = 0;
            self.pending_tasks_guardrail_count = 0;
            self.todo_list = TodoList::default();
            self.auto_continue_paused = None;
            self.rag.take();
            // Cleared alongside `rag` so the pair never disagrees: an agent RAG is
            // cached under `RagKey::Agent(<agent name>)`, and leaving that key behind
            // would outlive the RAG it names. Latent rather than live today only
            // because `rebuild_rag`/`edit_rag_docs` bail on `rag.is_none()` first.
            self.rag_key = None;
            self.discontinuous_last_message();
        }
        Ok(())
    }

    pub async fn edit_role(&mut self, app: &AppConfig, abort_signal: AbortSignal) -> Result<()> {
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
        let name = role_name.ok_or_else(|| anyhow::anyhow!("No role"))?;
        self.upsert_role(app, &name)?;
        self.use_role(app, &name, abort_signal).await
    }

    fn upsert_role(&self, app: &AppConfig, name: &str) -> Result<()> {
        let role_path = paths::role_file(name);
        ensure_parent_exists(&role_path)?;
        let editor = app.editor()?;
        edit_file(&editor, &role_path)?;
        if self.working_mode.is_repl() {
            println!("✓ Saved the role to '{}'.", role_path.display());
        }
        Ok(())
    }

    pub fn upsert_skill(&self, app: &AppConfig, name: &str) -> Result<()> {
        paths::validate_skill_name(name)?;
        let path = paths::skill_file(name);
        ensure_parent_exists(&path)?;
        let is_new = !path.exists();
        if is_new {
            fs::write(&path, SKILL_SCAFFOLD)
                .with_context(|| format!("Failed to scaffold skill at {}", path.display()))?;
        }
        let editor = app.editor()?;
        edit_file(&editor, &path)?;
        if is_new {
            println!("✓ Created skill at '{}'.", path.display());
        } else {
            println!("✓ Saved skill at '{}'.", path.display());
        }
        Ok(())
    }

    pub async fn load_skill_repl(&mut self, name: &str, abort_signal: AbortSignal) -> Result<()> {
        paths::validate_skill_name(name)?;
        if !self.app.config.function_calling_support {
            bail!(
                "Skills require function calling, which is disabled. Enable function calling in your config then try again."
            );
        }

        if !paths::has_skill(name) {
            bail!(
                "Skill '{name}' is not installed (expected at {})",
                paths::skill_file(name).display()
            );
        }

        let policy = SkillPolicy::effective(
            &self.app.config,
            self.role.as_ref(),
            self.agent.as_ref(),
            self.session.as_ref(),
        )?;

        if !policy.skills_enabled {
            bail!("Skills are disabled in this context");
        }

        if !policy.allows(name) {
            bail!("Skill '{name}' is not enabled in this context");
        }

        let skill = Skill::load(name)?;
        let needs_mcps = skill
            .enabled_mcp_servers()
            .map(|v| !v.is_empty())
            .unwrap_or(false);

        if needs_mcps && !self.app.config.mcp_server_support {
            bail!("Skill '{name}' requires MCP servers, which are disabled");
        }

        self.skill_registry.insert(skill)?;
        if let Err(e) = self.refresh_tool_scope(abort_signal).await {
            if let Err(unload_err) = self.skill_registry.unload(name) {
                warn!("Failed to unload skill '{name}' during error recovery: {unload_err}");
            }
            bail!("Loaded skill '{name}' but failed to refresh tool scope: {e}");
        }

        println!("✓ Loaded skill '{name}'.");
        Ok(())
    }

    pub async fn unload_skill_repl(&mut self, name: &str, abort_signal: AbortSignal) -> Result<()> {
        let skill = self.skill_registry.unload(name)?;

        if let Err(e) = self.refresh_tool_scope(abort_signal).await {
            if let Err(restore_err) = self.skill_registry.insert(skill) {
                warn!(
                    "Failed to restore skill '{name}' after tool-scope refresh failure: {restore_err}"
                );
            }
            bail!("Unloaded skill '{name}' but failed to refresh tool scope; restored: {e}");
        }

        println!("✓ Unloaded skill '{name}'.");
        Ok(())
    }

    pub fn list_loaded_skills(&self) {
        let names = self.skill_registry.loaded_names();

        if names.is_empty() {
            println!("No skills loaded.");
        } else {
            println!("Loaded skills:");
            for name in names {
                println!("  • {name}");
            }
        }
    }

    pub async fn apply_prelude(
        &mut self,
        app: &AppConfig,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        if self.macro_flag || !self.state().is_empty() {
            return Ok(());
        }
        let prelude = match self.working_mode {
            WorkingMode::Repl => app.repl_prelude.as_ref(),
            WorkingMode::Cmd => app.cmd_prelude.as_ref(),
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
                self.use_role(app, name, abort_signal)
                    .await
                    .with_context(err_msg)?;
            }
            Some(("session", name)) => {
                self.use_session(app, Some(name), abort_signal)
                    .await
                    .with_context(err_msg)?;
            }
            Some((session_name, role_name)) => {
                self.use_session(app, Some(session_name), abort_signal.clone())
                    .await
                    .with_context(err_msg)?;
                if let Some(true) = self.session.as_ref().map(|v| v.is_empty()) {
                    self.use_role(app, role_name, abort_signal)
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

    pub fn maybe_autoname_session(&mut self) -> bool {
        if let Some(session) = self.session.as_mut()
            && session.need_autoname()
        {
            session.set_autonaming(true);
            true
        } else {
            false
        }
    }

    fn enabled_mcp_servers_for_current_scope(
        &self,
        app: &AppConfig,
        start_mcp_servers: bool,
    ) -> Option<Vec<String>> {
        if !start_mcp_servers || !app.mcp_server_support {
            return None;
        }
        if let Some(agent) = self.agent.as_ref() {
            return (!agent.mcp_server_names().is_empty())
                .then(|| agent.mcp_server_names().to_vec());
        }
        if let Some(session) = self.session.as_ref() {
            return session.enabled_mcp_servers();
        }
        if let Some(role) = self.role.as_ref() {
            return role.enabled_mcp_servers();
        }
        app.enabled_mcp_servers.clone()
    }

    pub async fn bootstrap_tools(
        &mut self,
        app: &AppConfig,
        start_mcp_servers: bool,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let enabled_mcp_servers =
            self.enabled_mcp_servers_for_current_scope(app, start_mcp_servers);

        self.rebuild_tool_scope(app, enabled_mcp_servers, abort_signal)
            .await
    }

    pub async fn compress_session(&mut self) -> Result<()> {
        match self.session.as_ref() {
            Some(session) => {
                if !session.has_user_messages() {
                    bail!("No need to compress since there are no messages in the session")
                }
            }
            None => bail!("No session"),
        }

        let prompt = self
            .app
            .config
            .summarization_prompt
            .clone()
            .unwrap_or_else(|| SUMMARIZATION_PROMPT.into());
        let input = Input::from_str(self, &prompt, None)?;
        let summary = tokio::time::timeout(Duration::from_secs(120), input.fetch_chat_text())
            .await
            .map_err(|_| anyhow::anyhow!("Compression LLM call timed out after 120 s"))??;
        let summary_context_prompt = self
            .app
            .config
            .summary_context_prompt
            .clone()
            .unwrap_or_else(|| SUMMARY_CONTEXT_PROMPT.into());

        let todo_prefix = if self.auto_continue_config().enabled && !self.todo_list.is_empty() {
            format!(
                "[ACTIVE TODO LIST]\n{}\n\n",
                self.todo_list.render_for_model()
            )
        } else {
            String::new()
        };

        let keep_last = self.compression_keep_last();
        if let Some(session) = self.session.as_mut() {
            session.compress(
                format!("{todo_prefix}{summary_context_prompt}{summary}"),
                keep_last,
            );
        }
        self.discontinuous_last_message();
        Ok(())
    }

    pub async fn autoname_session(&mut self, app: &AppConfig) -> Result<()> {
        let text = match self
            .session
            .as_ref()
            .and_then(|session| session.chat_history_for_autonaming())
        {
            Some(v) => v,
            None => bail!("No chat history"),
        };
        let role = self.retrieve_role(app, CREATE_TITLE_ROLE)?;
        let input = Input::from_str(self, &text, Some(role))?;
        let text = input.fetch_chat_text().await?;
        if let Some(session) = self.session.as_mut() {
            session.set_autoname(&text);
        }
        Ok(())
    }

    pub async fn use_rag(&mut self, rag: Option<&str>, abort_signal: AbortSignal) -> Result<()> {
        if self.agent.is_some() {
            bail!("Cannot perform this operation because you are using a agent")
        }

        let app = self.app.config.clone();
        let vault = self.app.vault.clone();
        let rag_cache = self.rag_cache();
        let working_mode = self.working_mode;

        let (rag, rag_key): (Arc<Rag>, Option<RagKey>) = match rag {
            None => {
                let rag_path = self.rag_file(super::TEMP_RAG_NAME);
                if rag_path.exists() {
                    remove_file(&rag_path).with_context(|| {
                        format!("Failed to cleanup previous '{}' rag", super::TEMP_RAG_NAME)
                    })?;
                }
                (
                    Arc::new(
                        Rag::init(
                            &app,
                            super::TEMP_RAG_NAME,
                            &rag_path,
                            &[],
                            abort_signal.clone(),
                            false,
                        )
                        .await?,
                    ),
                    None,
                )
            }
            Some(name) => {
                let rag_path = self.rag_file(name);
                let key = RagKey::Named(name.to_string());

                let loaded = rag_cache
                    .load_with(key.clone(), || {
                        let app = app.clone();
                        let vault = vault.clone();
                        let rag_path = rag_path.clone();
                        let abort_signal = abort_signal.clone();
                        async move {
                            if !rag_path.exists() {
                                if working_mode.is_cmd() {
                                    bail!("Unknown RAG '{name}'");
                                }
                                Rag::init(&app, name, &rag_path, &[], abort_signal.clone(), true)
                                    .await
                            } else {
                                Rag::load_async(&app, &vault, name, &rag_path).await
                            }
                        }
                    })
                    .await?;
                (loaded, Some(key))
            }
        };
        self.rag = Some(rag);
        self.rag_key = rag_key;
        self.refresh_tool_scope(abort_signal).await?;
        Ok(())
    }

    pub async fn attach_rag(&mut self, name: &str, abort_signal: AbortSignal) -> Result<()> {
        let rag_path = self.rag_file(name);
        if rag_path.exists() {
            bail!(
                "RAG '{name}' already exists at '{}'. \
                 Use a different name, or delete the existing file first.",
                rag_path.display()
            );
        }
        let app = self.app.config.as_ref();
        let vault = self.app.vault.clone();
        let rag = Rag::attach(app, &vault, name, &rag_path).await?;
        let rag = Arc::new(rag);
        let key = RagKey::Named(name.to_string());
        self.rag_cache().insert(key.clone(), &rag);
        self.rag = Some(rag);
        self.rag_key = Some(key);
        self.refresh_tool_scope(abort_signal).await?;
        Ok(())
    }

    pub async fn edit_rag_docs(&mut self, abort_signal: AbortSignal) -> Result<()> {
        let mut rag = match self.rag.clone() {
            Some(v) => v.as_ref().clone(),
            None => bail!("No RAG"),
        };

        if rag.is_attached() {
            bail!(
                "Cannot edit documents on an attached RAG; Coyote does not own its source documents."
            );
        }

        let document_paths = rag.document_paths();
        let temp_file = temp_file(&format!("-rag-{}", rag.name()), ".txt");
        tokio::fs::write(&temp_file, &document_paths.join("\n"))
            .await
            .with_context(|| format!("Failed to write to '{}'", temp_file.display()))?;
        let editor = self.app.config.editor()?;
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

        if let Some(key) = self.rag_key.clone() {
            self.rag_cache().invalidate(&key);
        }

        rag.refresh_document_paths(
            &new_document_paths,
            false,
            false,
            &self.app.config,
            abort_signal,
        )
        .await?;
        self.rag = Some(Arc::new(rag));
        Ok(())
    }

    pub async fn rebuild_rag(&mut self, abort_signal: AbortSignal) -> Result<()> {
        let mut rag = match self.rag.clone() {
            Some(v) => v.as_ref().clone(),
            None => bail!("No RAG"),
        };

        if rag.is_attached() {
            bail!(
                "Cannot rebuild an attached RAG; Coyote does not own its source documents. \
                 Re-index from the system that originally created '{}'.",
                rag.name()
            );
        }

        if let Some(key) = self.rag_key.clone() {
            self.rag_cache().invalidate(&key);
        }

        let document_paths = rag.document_paths().to_vec();
        println!(
            "Rebuilding re-embeds every document ({} files). \
             This will call the embedding API and may take a while.",
            rag.file_count()
        );
        rag.refresh_document_paths(&document_paths, true, true, &self.app.config, abort_signal)
            .await?;
        self.rag = Some(Arc::new(rag));
        Ok(())
    }
}

fn fork_base_name(name: &str) -> &str {
    if let Some(pos) = name.rfind("-fork-") {
        let suffix = &name[pos + 6..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            return &name[..pos];
        }
    }

    name
}

#[cfg(test)]
mod tests {
    use super::super::mcp_factory::McpFactory;
    use super::*;
    use crate::config::AppState;
    use crate::config::agent::AgentConfig;
    use crate::config::bundles::BundleStore;
    use crate::config::mcp_tool_policy::LayerSource;
    use crate::config::tool_scope::test_fixtures::{FixtureServer, fixture_runtime};
    use crate::function::jobs::RingBuf;
    use crate::function::{ToolCall, skill};
    use crate::mcp::{McpServer, McpServerFeatures, McpServersConfig, McpTransportType};
    use crate::supervisor::{
        AgentExitStatus, AgentHandle, AgentResult, JobHandle, JobResult, JobState, JobStatus,
    };
    use crate::utils;
    use crate::utils::get_env_name;
    use crate::vault::Vault;
    use rmcp::model::PromptArgument;
    use serde_json::json;
    use serial_test::serial;
    use std::fs::{create_dir_all, remove_dir_all, write};
    use std::path::PathBuf;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};
    use std::{env, mem};

    // `list_client_names` / `list_all_models` cache the first AppConfig they
    // see in process-wide OnceLocks. Several tests reach them through configs
    // with an empty client list, which would permanently pin every later
    // model lookup in this process to "unknown model" and make any test that
    // needs a resolvable model dependent on test ordering. Seed the caches
    // before any test runs with a client, "test-seeded", exposing a single
    // embedding model. Deliberately NO chat model: some tests assert that no
    // chat model is available, and chat lookups don't need one — a
    // "test-seeded:<anything>" chat id resolves through the create-from-name
    // fallback because the client name is registered.
    //
    // `unsafe` is ctor's required acknowledgment that this runs before main;
    // the body only allocates and initializes OnceLocks, both of which are
    // sound pre-main.
    #[ctor::ctor(unsafe)]
    fn seed_model_registries() {
        use crate::client::{ClientConfig, ModelData, list_all_models, list_client_names};

        let mut client = ClientConfig::default();
        if let ClientConfig::OpenAIConfig(config) = &mut client {
            config.name = Some("test-seeded".to_string());
            let mut embedder = ModelData::new("test-embedder");
            embedder.model_type = "embedding".to_string();
            config.models = vec![embedder];
        }
        // A claude client is seeded alongside it so tests can exercise
        // provider-quirk inheritance through the create-from-name fallback.
        // It deliberately carries one embedding model: an empty `models` list
        // would fall back to the full embedded claude catalog and register
        // real chat models, breaking the no-chat-model invariant above.
        // The test-dual-model pair shares one id across two types, mirroring
        // jina:jina-colbert-v2, for the type-aware lookup tests.
        let claude: ClientConfig = serde_yaml::from_str(
            "type: claude\nmodels:\n  - name: test-claude-embedder\n    type: embedding\n  - name: test-dual-model\n    type: embedding\n  - name: test-dual-model\n    type: reranker\n",
        )
        .unwrap();
        let config = AppConfig {
            clients: vec![client, claude],
            ..AppConfig::default()
        };
        let _ = list_client_names(&config);
        let _ = list_all_models(&config);
    }

    struct TestConfigDirGuard {
        key: String,
        previous: Option<std::ffi::OsString>,
        path: PathBuf,
    }

    impl TestConfigDirGuard {
        fn new() -> Self {
            let key = get_env_name("config_dir");
            let previous = env::var_os(&key);
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("coyote-request-context-tests-{unique}"));
            create_dir_all(&path).unwrap();
            unsafe {
                env::set_var(&key, &path);
            }
            Self {
                key,
                previous,
                path,
            }
        }
    }

    impl Drop for TestConfigDirGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                unsafe {
                    env::set_var(&self.key, previous);
                }
            } else {
                unsafe {
                    env::remove_var(&self.key);
                }
            }
            let _ = remove_dir_all(&self.path);
        }
    }

    fn default_app_state() -> Arc<AppState> {
        Arc::new(AppState::test_default())
    }

    fn create_test_ctx() -> RequestContext {
        RequestContext::new(default_app_state(), WorkingMode::Cmd)
    }

    fn test_decl(name: &str) -> FunctionDeclaration {
        FunctionDeclaration {
            name: name.to_string(),
            description: String::new(),
            parameters: Default::default(),
            agent: false,
        }
    }

    fn tools_only_features(name: &str) -> McpServerFeatures {
        McpServerFeatures {
            name: name.to_string(),
            tools: true,
            resources: false,
            prompts: false,
        }
    }

    #[test]
    fn new_creates_clean_state() {
        let ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);

        assert!(ctx.role.is_none());
        assert!(ctx.session.is_none());
        assert!(ctx.agent.is_none());
        assert!(ctx.rag.is_none());
        assert!(ctx.supervisor.is_none());
        assert!(ctx.tool_scope.mcp_runtime.is_empty());
        assert_eq!(ctx.current_depth, 0);
    }

    #[test]
    fn update_app_config_persists_changes() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let previous = Arc::clone(&ctx.app.config);

        ctx.update_app_config(|app| {
            app.save = true;
            app.compression_threshold = 1234;
        });

        assert!(ctx.app.config.save);
        assert_eq!(ctx.app.config.compression_threshold, 1234);
        assert!(!Arc::ptr_eq(&ctx.app.config, &previous));
    }

    #[test]
    fn memory_config_app_some_false_disables_via_cascade() {
        let mut ctx = create_test_ctx();

        ctx.update_app_config(|app| app.memory = Some(false));

        assert!(
            !ctx.should_inject_memory(),
            "AppConfig.memory=Some(false) must disable memory regardless of on-disk content (this is the --no-memory CLI path)"
        );
    }

    #[test]
    fn memory_config_role_false_beats_app_true_in_cascade() {
        let mut ctx = create_test_ctx();
        ctx.update_app_config(|app| app.memory = Some(true));
        let role = Role::new("memory_off_role", "---\nmemory: false\n---\n");
        assert_eq!(role.memory(), Some(false), "metadata parser sanity check");
        ctx.role = Some(role);
        assert!(
            !ctx.should_inject_memory(),
            "Role::memory=Some(false) must win over AppConfig::memory=Some(true)"
        );
    }

    #[test]
    fn should_register_memory_tools_false_when_function_calling_off() {
        let mut ctx = create_test_ctx();

        ctx.update_app_config(|app| {
            app.memory = Some(true);
            app.function_calling_support = false;
        });

        assert!(
            !ctx.should_register_memory_tools(),
            "memory tools must require function_calling_support even when memory itself would otherwise be enabled"
        );
    }

    #[test]
    fn use_role_obj_sets_role() {
        let mut ctx = create_test_ctx();
        let role = Role::new("test", "test prompt");
        ctx.use_role_obj(role).unwrap();
        assert!(ctx.role.is_some());
        assert_eq!(ctx.role.as_ref().unwrap().name(), "test");
    }

    #[test]
    fn exit_role_clears_role() {
        let mut ctx = create_test_ctx();
        let role = Role::new("test", "prompt");
        ctx.use_role_obj(role).unwrap();
        assert!(ctx.role.is_some());
        ctx.exit_role().unwrap();
        assert!(ctx.role.is_none());
    }

    #[test]
    fn use_temp_role_creates_temp_role() {
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        ctx.use_temp_role(&app, "you are a pirate").unwrap();
        assert!(ctx.role.is_some());
        assert_eq!(ctx.role.as_ref().unwrap().name(), "temp");
        assert!(
            ctx.role
                .as_ref()
                .unwrap()
                .prompt()
                .contains("you are a pirate")
        );
    }

    #[test]
    fn mcp_prompt_rows_assembles_columns() {
        let items = vec![
            CatalogItem {
                name: "summarize".to_string(),
                server: "docs".to_string(),
                description: "Summarize a document".to_string(),
                arguments: Some(vec![
                    PromptArgument::new("path").with_required(true),
                    PromptArgument::new("style"),
                ]),
                ..Default::default()
            },
            CatalogItem {
                name: "greet".to_string(),
                server: "misc".to_string(),
                ..Default::default()
            },
        ];

        let rows = mcp_prompt_rows(&items);

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            [
                "docs",
                "summarize",
                "Summarize a document",
                "path (required), style"
            ]
        );
        assert_eq!(rows[1], ["misc", "greet", "", ""]);
    }

    #[test]
    fn extract_role_returns_standalone_role() {
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let role = Role::new("myrole", "my prompt");
        ctx.use_role_obj(role).unwrap();
        let extracted = ctx.extract_role(&app).unwrap();
        assert_eq!(extracted.name(), "myrole");
    }

    #[test]
    fn extract_role_returns_default_when_nothing_active() {
        let ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let extracted = ctx.extract_role(&app).unwrap();
        assert_eq!(extracted.name(), "");
    }

    #[test]
    fn extract_role_agent_without_reasoning_effort_inherits_app_config() {
        let mut ctx = create_test_ctx();
        ctx.agent = Some(Agent::test_new(AgentConfig {
            name: "test-agent".to_string(),
            reasoning_effort: None,
            ..AgentConfig::default()
        }));
        let app = AppConfig {
            reasoning_effort: Some("max".to_string()),
            ..AppConfig::default()
        };

        let extracted = ctx.extract_role(&app).unwrap();

        assert_eq!(extracted.reasoning_effort(), Some("max".to_string()));
    }

    #[test]
    fn extract_role_agent_with_explicit_reasoning_effort_takes_priority_over_app_config() {
        let mut ctx = create_test_ctx();
        ctx.agent = Some(Agent::test_new(AgentConfig {
            name: "test-agent".to_string(),
            reasoning_effort: Some("low".to_string()),
            ..AgentConfig::default()
        }));
        let app = AppConfig {
            reasoning_effort: Some("max".to_string()),
            ..AppConfig::default()
        };

        let extracted = ctx.extract_role(&app).unwrap();

        assert_eq!(extracted.reasoning_effort(), Some("low".to_string()));
    }

    #[test]
    fn should_inject_skill_instructions_requires_function_calling() {
        let app = AppConfig {
            function_calling_support: false,
            ..AppConfig::default()
        };

        let policy = SkillPolicy {
            skills_enabled: true,
            enabled: ["a".to_string()].into_iter().collect(),
            compatible_enabled: ["a".to_string()].into_iter().collect(),
        };

        assert!(!should_inject_skill_instructions(&app, &policy));
    }

    #[test]
    fn should_inject_skill_instructions_requires_skills_enabled() {
        let app = AppConfig {
            function_calling_support: true,
            ..AppConfig::default()
        };

        let policy = SkillPolicy {
            skills_enabled: false,
            enabled: ["a".to_string()].into_iter().collect(),
            compatible_enabled: ["a".to_string()].into_iter().collect(),
        };

        assert!(!should_inject_skill_instructions(&app, &policy));
    }

    #[test]
    fn should_inject_skill_instructions_suppresses_when_no_compatible_skills() {
        let app = AppConfig {
            function_calling_support: true,
            ..AppConfig::default()
        };

        // `enabled` has names, but none survive the compatibility filter — hint must suppress.
        let policy = SkillPolicy {
            skills_enabled: true,
            enabled: ["a".to_string()].into_iter().collect(),
            compatible_enabled: Default::default(),
        };

        assert!(!should_inject_skill_instructions(&app, &policy));
    }

    #[test]
    fn should_inject_skill_instructions_when_all_conditions_met() {
        let app = AppConfig {
            function_calling_support: true,
            ..AppConfig::default()
        };

        let policy = SkillPolicy {
            skills_enabled: true,
            enabled: ["a".to_string()].into_iter().collect(),
            compatible_enabled: ["a".to_string()].into_iter().collect(),
        };

        assert!(should_inject_skill_instructions(&app, &policy));
    }

    #[test]
    fn skill_instructions_config_falls_back_to_app_default() {
        let ctx = create_test_ctx();

        let cfg = ctx.skill_instructions_config();

        assert!(cfg.inject);
        assert!(cfg.instructions.is_none());
    }

    #[test]
    fn skill_instructions_config_respects_role_disable() {
        let mut ctx = create_test_ctx();
        let role = Role::new("r", "---\ninject_skill_instructions: false\n---\nhello");
        ctx.use_role_obj(role).unwrap();

        let cfg = ctx.skill_instructions_config();

        assert!(!cfg.inject);
    }

    #[test]
    fn skill_instructions_config_session_overrides_role() {
        let mut ctx = create_test_ctx();
        let role = Role::new("r", "---\ninject_skill_instructions: false\n---\nhello");
        ctx.use_role_obj(role).unwrap();
        let mut session = Session::default();
        session.set_inject_skill_instructions(Some(true));
        session.set_skill_instructions(Some("custom hint".into()));
        ctx.session = Some(session);

        let cfg = ctx.skill_instructions_config();

        assert!(cfg.inject);
        assert_eq!(cfg.instructions.as_deref(), Some("custom hint"));
    }

    #[test]
    fn exit_session_clears_session() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        assert!(ctx.session.is_some());
        ctx.exit_session().unwrap();
        assert!(ctx.session.is_none());
    }

    #[test]
    fn empty_session_clears_messages() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        ctx.empty_session().unwrap();
        assert!(ctx.session.is_some());
        assert!(ctx.session.as_ref().unwrap().is_empty());
    }

    #[test]
    fn maybe_autoname_session_returns_false_when_no_session() {
        let mut ctx = create_test_ctx();
        assert!(!ctx.maybe_autoname_session());
    }

    #[test]
    #[serial]
    fn repl_complete_uninstall_offers_installed_bundle_names() {
        let _guard = TestConfigDirGuard::new();
        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle(
                "omc",
                bundles::InstallMetadata {
                    source: "https://github.com/x/omc".to_string(),
                    git_ref: None,
                    commit: "abc123".to_string(),
                    version: None,
                    description: None,
                    homepage: None,
                },
            )
            .unwrap();
        let ctx = create_test_ctx();

        let values = ctx.repl_complete(".uninstall", &[""], "");

        assert!(
            values.iter().any(|(name, _)| name == "omc"),
            "got: {values:?}"
        );
    }

    #[test]
    #[serial]
    fn repl_complete_install_offers_categories_and_bundles() {
        let _guard = TestConfigDirGuard::new();
        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle(
                "omc",
                bundles::InstallMetadata {
                    source: "https://github.com/x/omc".to_string(),
                    git_ref: None,
                    commit: "abc123".to_string(),
                    version: None,
                    description: None,
                    homepage: None,
                },
            )
            .unwrap();
        let ctx = create_test_ctx();

        let values = ctx.repl_complete(".install", &[""], "");

        for expected in ["agents", "omc"] {
            assert!(
                values.iter().any(|(name, _)| name == expected),
                "missing '{expected}'; got: {values:?}"
            );
        }
    }

    #[test]
    #[serial]
    fn exit_agent_clears_all_agent_state() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                ctx.use_agent(&app, &agent_name, None, utils::create_abort_signal())
                    .await
                    .unwrap();
            });

        assert!(ctx.agent.is_some());

        ctx.exit_agent(&app).unwrap();

        assert!(ctx.agent.is_none());
        assert!(ctx.rag.is_none());
        assert_eq!(ctx.rag_key, None);
    }

    #[test]
    #[serial]
    fn use_agent_does_not_carry_stale_rag_key() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        ctx.rag_key = Some(RagKey::Named("docs".to_string()));

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                ctx.use_agent(&app, &agent_name, None, utils::create_abort_signal())
                    .await
                    .unwrap();
            });

        assert!(ctx.rag.is_none());
        assert_eq!(ctx.rag_key, None);
    }

    #[test]
    fn effective_max_concurrent_jobs_resolution_precedence() {
        let mut app = AppConfig::default();
        assert_eq!(effective_max_concurrent_jobs(None, &app), 5);

        app.max_concurrent_jobs = Some(9);
        assert_eq!(effective_max_concurrent_jobs(None, &app), 9);

        let agent = Agent::test_new(AgentConfig {
            max_concurrent_jobs: Some(2),
            ..AgentConfig::default()
        });
        assert_eq!(effective_max_concurrent_jobs(Some(&agent), &app), 2);
    }

    #[test]
    fn jobs_enabled_requires_function_calling_and_nonzero_capacity() {
        let mut app = AppConfig::default();
        assert!(jobs_enabled(None, &app));

        app.max_concurrent_jobs = Some(0);
        assert!(!jobs_enabled(None, &app));

        app.max_concurrent_jobs = None;
        app.function_calling_support = false;
        assert!(!jobs_enabled(None, &app));

        app.function_calling_support = true;
        let agent = Agent::test_new(AgentConfig {
            max_concurrent_jobs: Some(0),
            ..AgentConfig::default()
        });
        assert!(!jobs_enabled(Some(&agent), &app));
    }

    #[test]
    #[serial]
    fn use_agent_cancels_previous_supervisor() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        let old_sig = utils::create_abort_signal();

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let join_handle = tokio::spawn(async {
                    Ok(AgentResult {
                        id: "a1".into(),
                        agent_name: "explore".into(),
                        output: String::new(),
                        exit_status: AgentExitStatus::Completed,
                    })
                });
                let handle = AgentHandle {
                    id: "a1".to_string(),
                    agent_name: "explore".to_string(),
                    depth: 1,
                    inbox: Arc::new(Inbox::new()),
                    abort_signal: old_sig.clone(),
                    join_handle,
                    child_supervisor: None,
                };
                let old_sup = Arc::new(RwLock::new(Supervisor::new(4, 3)));
                old_sup.write().register(handle).unwrap();
                ctx.supervisor = Some(old_sup);

                ctx.use_agent(&app, &agent_name, None, utils::create_abort_signal())
                    .await
                    .unwrap();
            });

        assert!(old_sig.aborted());
        assert!(ctx.supervisor.is_some());
    }

    #[test]
    #[serial]
    fn use_agent_inits_job_capable_supervisor_without_spawning() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                ctx.use_agent(&app, &agent_name, None, utils::create_abort_signal())
                    .await
                    .unwrap();
            });

        let supervisor = ctx.supervisor.as_ref().expect("supervisor for jobs");
        let supervisor = supervisor.read();
        assert_eq!(supervisor.max_concurrent(), 0);
        assert_eq!(supervisor.max_concurrent_jobs(), 5);
    }

    #[test]
    #[serial]
    fn use_agent_skips_supervisor_when_jobs_disabled() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let mut app = ctx.app.config.as_ref().clone();
        app.max_concurrent_jobs = Some(0);
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                ctx.use_agent(&app, &agent_name, None, utils::create_abort_signal())
                    .await
                    .unwrap();
            });

        assert!(ctx.supervisor.is_none());
    }

    #[test]
    fn current_depth_default_is_zero() {
        let ctx = create_test_ctx();
        assert_eq!(ctx.current_depth, 0);
    }

    #[test]
    fn current_depth_can_be_set() {
        let mut ctx = create_test_ctx();
        ctx.current_depth = 3;
        assert_eq!(ctx.current_depth, 3);
    }

    #[test]
    fn supervisor_defaults_to_none() {
        let ctx = create_test_ctx();
        assert!(ctx.supervisor.is_none());
    }

    #[test]
    fn inbox_defaults_to_none() {
        let ctx = create_test_ctx();
        assert!(ctx.inbox.is_none());
    }

    #[test]
    fn parent_inbox_defaults_to_none() {
        let ctx = create_test_ctx();

        assert!(ctx.parent_inbox.is_none());
    }

    #[test]
    fn ensure_inbox_allocates_once_and_is_stable() {
        let mut ctx = create_test_ctx();
        let first = ctx.ensure_inbox();
        let second = ctx.ensure_inbox();

        assert!(Arc::ptr_eq(&first, &second));
        assert!(ctx.inbox.is_some());
    }

    #[test]
    fn new_for_child_inherits_parent_inbox() {
        let mut parent = create_test_ctx();
        let parent_inbox = parent.ensure_inbox();
        let child = RequestContext::new_for_child(
            Arc::clone(&parent.app),
            &parent,
            1,
            Arc::new(Inbox::new()),
            "agent_test_1".to_string(),
        );

        let child_parent_inbox = child.parent_inbox.expect("child should see parent's inbox");

        assert!(Arc::ptr_eq(&parent_inbox, &child_parent_inbox));
    }

    #[test]
    fn fork_for_branch_resets_peer_fields() {
        let mut ctx = create_test_ctx();
        ctx.peer_registry = Some(Arc::new(PeerRegistry::new()));
        ctx.peer_assignment = Some(("graph_agent_a_1".to_string(), Arc::new(Inbox::new())));

        let branch = ctx.fork_for_branch();

        assert!(
            branch.peer_registry.is_none() && branch.peer_assignment.is_none(),
            "branches must not inherit peer identity; the executor provisions per-branch assignments"
        );
    }

    #[test]
    fn new_for_child_resets_peer_fields() {
        let mut parent = create_test_ctx();
        parent.peer_registry = Some(Arc::new(PeerRegistry::new()));
        parent.peer_assignment = Some(("graph_agent_a_1".to_string(), Arc::new(Inbox::new())));

        let child = RequestContext::new_for_child(
            Arc::clone(&parent.app),
            &parent,
            1,
            Arc::new(Inbox::new()),
            "agent_test_1".to_string(),
        );

        assert!(
            child.peer_registry.is_none() && child.peer_assignment.is_none(),
            "children must not inherit the parent frontier's peer roster"
        );
    }

    #[test]
    fn escalation_queue_defaults_to_none() {
        let ctx = create_test_ctx();
        assert!(ctx.root_escalation_queue().is_none());
    }

    #[test]
    fn new_for_child_gets_fresh_notification_queue() {
        let parent = create_test_ctx();
        let child = RequestContext::new_for_child(
            Arc::clone(&parent.app),
            &parent,
            1,
            Arc::new(Inbox::new()),
            "agent_test_1".to_string(),
        );
        assert!(
            !Arc::ptr_eq(&parent.notification_queue, &child.notification_queue),
            "each child owns its notifications; a shared queue would race drains"
        );
    }

    #[test]
    fn fork_for_branch_shares_notification_queue() {
        let ctx = create_test_ctx();
        let branch = ctx.fork_for_branch();
        assert!(Arc::ptr_eq(
            &ctx.notification_queue,
            &branch.notification_queue
        ));
    }

    fn app_state_with_mcp_config(mcp_server_support: bool, server_names: &[&str]) -> Arc<AppState> {
        app_state_with_mcp_command(mcp_server_support, server_names, "echo")
    }

    fn app_state_with_mcp_command(
        mcp_server_support: bool,
        server_names: &[&str],
        command: &str,
    ) -> Arc<AppState> {
        let app_config = AppConfig {
            mcp_server_support,
            ..AppConfig::default()
        };

        let mcp_config = if server_names.is_empty() {
            None
        } else {
            let mut servers = IndexMap::new();
            for name in server_names {
                servers.insert(
                    name.to_string(),
                    McpServer {
                        transport_type: McpTransportType::Stdio,
                        command: Some(command.to_string()),
                        args: None,
                        env: None,
                        cwd: None,
                        url: None,
                        headers: None,
                        oauth: None,
                        allowed_tools: None,
                    },
                );
            }
            Some(McpServersConfig {
                mcp_servers: servers,
            })
        };

        Arc::new(AppState {
            config: Arc::new(app_config),
            vault: Arc::new(Vault::default()),
            mcp_factory: Arc::new(McpFactory::default()),
            rag_cache: Arc::new(RagCache::default()),
            mcp_config,
            mcp_log_path: None,
            mcp_registry: None,
            functions: Functions::default(),
        })
    }

    fn run_async<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    #[serial]
    fn rebuild_tool_scope_mcp_disabled_skips_servers() {
        let _guard = TestConfigDirGuard::new();
        let app_state = app_state_with_mcp_config(false, &["github", "slack"]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        run_async(ctx.rebuild_tool_scope(&app, Some(vec!["all".to_string()]), abort)).unwrap();

        assert!(ctx.tool_scope.mcp_runtime.is_empty());
    }

    #[test]
    #[serial]
    fn use_role_rolls_back_when_mcp_startup_fails() {
        let _guard = TestConfigDirGuard::new();
        let roles_dir = paths::roles_dir();
        create_dir_all(&roles_dir).unwrap();
        write(
            roles_dir.join("broken_mcp.md"),
            "---\nenabled_mcp_servers: failing\n---\nYou use MCP servers.",
        )
        .unwrap();

        let app_state =
            app_state_with_mcp_command(true, &["failing"], "/nonexistent/coyote-test-mcp-binary");
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        let result = run_async(ctx.use_role(&app, "broken_mcp", abort));

        assert!(result.is_err());
        assert!(
            ctx.role.is_none(),
            "role must be rolled back when MCP startup fails"
        );
    }

    #[test]
    #[serial]
    fn rebuild_tool_scope_no_enabled_servers_yields_empty_runtime() {
        let _guard = TestConfigDirGuard::new();
        let app_state = app_state_with_mcp_config(true, &["github"]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        run_async(ctx.rebuild_tool_scope(&app, None, abort)).unwrap();

        assert!(ctx.tool_scope.mcp_runtime.is_empty());
    }

    #[test]
    #[serial]
    fn rebuild_tool_scope_no_mcp_config_yields_empty_runtime() {
        let _guard = TestConfigDirGuard::new();
        let app_state = app_state_with_mcp_config(true, &[]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        run_async(ctx.rebuild_tool_scope(&app, Some(vec!["all".to_string()]), abort)).unwrap();

        assert!(ctx.tool_scope.mcp_runtime.is_empty());
    }

    #[test]
    #[serial]
    fn rebuild_tool_scope_preserves_tool_tracker() {
        let _guard = TestConfigDirGuard::new();
        let app_state = app_state_with_mcp_config(false, &[]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let dummy = ToolCall {
            name: "test_tool".to_string(),
            ..Default::default()
        };
        ctx.tool_scope.tool_tracker.record_call(dummy);

        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();
        run_async(ctx.rebuild_tool_scope(&app, None, abort)).unwrap();

        let check_call = ToolCall {
            name: "test_tool".to_string(),
            ..Default::default()
        };
        assert!(
            ctx.tool_scope
                .tool_tracker
                .check_loop(&check_call)
                .is_none()
        );
    }

    #[test]
    #[serial]
    fn rebuild_tool_scope_repl_mode_appends_user_interaction_functions() {
        let _guard = TestConfigDirGuard::new();
        let app_state = app_state_with_mcp_config(false, &[]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Repl);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        run_async(ctx.rebuild_tool_scope(&app, None, abort)).unwrap();

        let names: Vec<String> = ctx
            .tool_scope
            .functions
            .declarations()
            .iter()
            .map(|f| f.name.clone())
            .collect();
        assert!(
            names.iter().any(|n| n.starts_with("user__")),
            "REPL mode should include user interaction functions, got: {names:?}"
        );
    }

    #[test]
    #[serial]
    fn rebuild_tool_scope_cmd_mode_no_user_interaction_functions() {
        let _guard = TestConfigDirGuard::new();
        let app_state = app_state_with_mcp_config(false, &[]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        run_async(ctx.rebuild_tool_scope(&app, None, abort)).unwrap();

        let names: Vec<String> = ctx
            .tool_scope
            .functions
            .declarations()
            .iter()
            .map(|f| f.name.clone())
            .collect();
        assert!(
            !names.iter().any(|n| n.starts_with("user__")),
            "CMD mode should NOT include user interaction functions, got: {names:?}"
        );
    }

    #[test]
    #[serial]
    fn update_skills_enabled_false_removes_skill_meta_tools_from_scope() {
        let _guard = TestConfigDirGuard::new();
        let app_state = app_state_with_mcp_config(false, &[]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Repl);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        run_async(ctx.rebuild_tool_scope(&app, None, abort.clone())).unwrap();

        let names_before: Vec<String> = ctx
            .tool_scope
            .functions
            .declarations()
            .iter()
            .map(|f| f.name.clone())
            .collect();
        assert!(
            names_before.iter().any(|n| n.starts_with("skill__")),
            "expected skill__* functions before toggle, got: {names_before:?}"
        );

        run_async(ctx.update("skills_enabled false", abort)).unwrap();

        let names_after: Vec<String> = ctx
            .tool_scope
            .functions
            .declarations()
            .iter()
            .map(|f| f.name.clone())
            .collect();
        assert!(
            !names_after.iter().any(|n| n.starts_with("skill__")),
            "expected skill__* functions to be removed after `.set skills_enabled false`, got: {names_after:?}"
        );
    }

    #[test]
    fn select_functions_returns_none_when_no_tools_enabled() {
        let ctx = create_test_ctx();
        let role = Role::default();
        assert!(ctx.select_functions(&role).is_none());
    }

    #[test]
    fn select_functions_returns_none_when_function_calling_disabled() {
        let app_state = {
            let config = AppConfig {
                function_calling_support: false,
                ..AppConfig::default()
            };
            Arc::new(AppState {
                config: Arc::new(config),
                vault: Arc::new(Vault::default()),
                mcp_factory: Arc::new(McpFactory::default()),
                rag_cache: Arc::new(RagCache::default()),
                mcp_config: None,
                mcp_log_path: None,
                mcp_registry: None,
                functions: Functions::default(),
            })
        };
        let ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["all".to_string()]));
        assert!(ctx.select_functions(&role).is_none());
    }

    #[test]
    fn select_functions_hides_job_functions_without_backgroundable_tools() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_job_functions();

        assert!(
            ctx.select_functions(&Role::default()).is_none(),
            "job__ tools must not be declared when nothing backgroundable is declared"
        );
    }

    #[test]
    fn select_functions_keeps_job_tools_when_filter_includes_backgroundable_tool() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_job_functions();
        ctx.tool_scope
            .functions
            .append_declaration(test_decl("my_build_tool"));

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["my_build_tool".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        assert!(
            fns.iter().any(|f| f.name == "job__start"),
            "job__ tools must survive a role tool filter that declares a backgroundable tool"
        );
    }

    #[test]
    fn select_functions_hides_job_tools_when_filter_has_only_non_backgroundable_tools() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_job_functions();
        ctx.tool_scope
            .functions
            .append_declaration(test_decl("fs_cat"));

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["fs_cat".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        assert!(
            !fns.iter().any(|f| f.name.starts_with("job__")),
            "job__ tools must be hidden when no declared tool is backgroundable"
        );
    }

    #[test]
    fn concrete_tool_names_excludes_job_functions() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_job_functions();

        assert!(ctx.concrete_tool_names().is_empty());
    }

    #[test]
    fn before_chat_completion_refreshes_declared_function_names() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_job_functions();
        ctx.tool_scope
            .functions
            .append_declaration(test_decl("echo"));
        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["echo".to_string()]));
        let input = Input::from_str(&ctx, "hello", Some(role)).unwrap();
        ctx.before_chat_completion(&input).unwrap();

        assert_eq!(ctx.declared_function_names.len(), 6);
        assert!(ctx.declared_function_names.contains("job__start"));
        assert!(ctx.declared_function_names.contains("echo"));

        ctx.tool_scope = ToolScope::default();
        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["echo".to_string()]));
        let input = Input::from_str(&ctx, "hello again", Some(role)).unwrap();
        ctx.before_chat_completion(&input).unwrap();

        assert!(
            ctx.declared_function_names.is_empty(),
            "stash must be refreshed on every request"
        );
    }

    #[test]
    #[serial]
    fn rebuild_tool_scope_gates_job_functions_on_jobs_enabled() {
        let _guard = TestConfigDirGuard::new();
        let app_state = app_state_with_mcp_config(false, &[]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        run_async(ctx.rebuild_tool_scope(&app, None, abort.clone())).unwrap();
        assert!(ctx.tool_scope.functions.contains("job__start"));

        let jobs_off = AppConfig {
            max_concurrent_jobs: Some(0),
            ..(*app).clone()
        };
        run_async(ctx.rebuild_tool_scope(&jobs_off, None, abort.clone())).unwrap();
        assert!(
            !ctx.tool_scope
                .functions
                .declarations()
                .iter()
                .any(|f| f.name.starts_with("job__"))
        );

        let fc_off = AppConfig {
            function_calling_support: false,
            ..(*app).clone()
        };
        run_async(ctx.rebuild_tool_scope(&fc_off, None, abort)).unwrap();
        assert!(
            !ctx.tool_scope
                .functions
                .declarations()
                .iter()
                .any(|f| f.name.starts_with("job__"))
        );
    }

    #[test]
    #[serial]
    fn exit_agent_rebuild_retains_job_functions_when_enabled() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();

        ctx.exit_agent(&app).unwrap();
        assert!(ctx.tool_scope.functions.contains("job__start"));

        let jobs_off = AppConfig {
            max_concurrent_jobs: Some(0),
            ..(*app).clone()
        };
        ctx.exit_agent(&jobs_off).unwrap();
        assert!(!ctx.tool_scope.functions.contains("job__start"));
    }

    #[test]
    fn select_functions_all_enabled_tools_returns_all_non_mcp() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_todo_functions();
        ctx.tool_scope.functions.append_user_interaction_functions();

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["all".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"todo__init"));
        assert!(names.contains(&"user__select"));
    }

    #[test]
    fn select_functions_comma_separated_filters() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_todo_functions();

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec![
            "todo__init".to_string(),
            "todo__add".to_string(),
        ]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"todo__init"));
        assert!(names.contains(&"todo__add"));
        assert!(!names.contains(&"todo__done"));
    }

    #[test]
    fn select_functions_re_adds_skill_tools_when_role_skills_enabled_unset() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_skill_functions();

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["foo".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"skill__list"));
        assert!(names.contains(&"skill__load"));
        assert!(names.contains(&"skill__unload"));
    }

    #[test]
    fn select_functions_suppresses_skill_tools_when_role_skills_enabled_false() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_skill_functions();
        ctx.tool_scope.functions.append_todo_functions();

        let mut role = Role::new("r", "---\nskills_enabled: false\n---\np");
        role.set_enabled_tools(Some(vec!["todo__init".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"todo__init"));
        assert!(!names.contains(&"skill__list"));
        assert!(!names.contains(&"skill__load"));
        assert!(!names.contains(&"skill__unload"));
    }

    #[test]
    fn select_functions_still_re_adds_user_tools_when_role_skills_enabled_false() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_user_interaction_functions();
        ctx.tool_scope.functions.append_skill_functions();

        let mut role = Role::new("r", "---\nskills_enabled: false\n---\np");
        role.set_enabled_tools(Some(vec!["foo".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"user__select"));
        assert!(!names.contains(&"skill__list"));
    }

    #[test]
    #[serial]
    fn select_functions_re_adds_skill_tools_when_agent_skills_enabled_not_false() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_skill_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\nversion: \"1.0\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: ok\n"
            ),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        run_async(ctx.use_agent(&app, &agent_name, None, abort)).unwrap();
        ctx.tool_scope.functions.append_skill_functions();

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["foo".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"skill__list"));
        assert!(names.contains(&"skill__load"));
        assert!(names.contains(&"skill__unload"));
    }

    #[test]
    #[serial]
    fn select_functions_preserves_infra_tools_under_agent_filter() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_infra_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!(
                "name: {agent_name}\ninstructions: hi\nauto_continue: true\ncan_spawn_agents: true\n"
            ),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        run_async(ctx.use_agent(&app, &agent_name, None, abort)).unwrap();

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["foo".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(
            names.contains(&"todo__init"),
            "todo__ tools must survive an agent tool filter, got: {names:?}"
        );
        assert!(
            names.contains(&"agent__spawn"),
            "agent__ tools must survive an agent tool filter, got: {names:?}"
        );
        assert!(
            names.contains(&"agent__send_message"),
            "teammate tools must survive an agent tool filter, got: {names:?}"
        );
        assert!(
            names.contains(&"user__select"),
            "user__ tools must survive an agent tool filter, got: {names:?}"
        );
    }

    #[test]
    #[serial]
    fn select_functions_preserves_job_tools_under_agent_filter() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_job_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        run_async(ctx.use_agent(&app, &agent_name, None, abort)).unwrap();
        ctx.tool_scope
            .functions
            .append_declaration(test_decl("foo"));

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["foo".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(
            names.contains(&"job__start"),
            "job__ tools must survive an agent tool filter that declares a backgroundable tool, got: {names:?}"
        );
        assert!(names.contains(&"job__collect"));
    }

    #[test]
    fn fork_for_branch_clones_skill_registry() {
        let mut ctx = create_test_ctx();
        let skill = Skill::new("shared", "---\nauto_unload: false\n---\nbody");
        ctx.skill_registry.insert(skill).unwrap();

        let fork = ctx.fork_for_branch();

        assert!(
            fork.skill_registry.is_loaded("shared"),
            "Parallel branches must share loaded skills with parent"
        );
        assert!(ctx.skill_registry.is_loaded("shared"));
    }

    #[test]
    fn handle_skill_tool_returns_error_when_skills_disabled() {
        let mut ctx = create_test_ctx();
        let role = Role::new("r", "---\nskills_enabled: false\n---\np");
        ctx.use_role_obj(role).unwrap();

        let result = run_async(skill::handle_skill_tool(
            &mut ctx,
            "skill__list",
            &json!({}),
        ))
        .unwrap();

        assert!(
            result.get("error").is_some(),
            "Expected error when skills are disabled, got: {result:?}"
        );
    }

    #[test]
    fn handle_unload_returns_error_when_skill_not_loaded() {
        let mut ctx = create_test_ctx();

        let result = run_async(skill::handle_skill_tool(
            &mut ctx,
            "skill__unload",
            &json!({"name": "ghost"}),
        ))
        .unwrap();

        assert!(
            result.get("error").is_some(),
            "Expected error when unloading unloaded skill, got: {result:?}"
        );
    }

    #[test]
    #[serial]
    fn select_functions_suppresses_skill_tools_when_agent_skills_enabled_false() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_skill_agent_off_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\nversion: \"1.0\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: ok\n"
            ),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        run_async(ctx.use_agent(&app, &agent_name, None, abort)).unwrap();
        ctx.agent
            .as_mut()
            .expect("agent loaded")
            .set_skills_enabled(Some(false));
        ctx.tool_scope.functions.append_skill_functions();

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["foo".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(!names.contains(&"skill__list"));
        assert!(!names.contains(&"skill__load"));
        assert!(!names.contains(&"skill__unload"));
    }

    #[test]
    fn select_enabled_mcp_servers_returns_empty_when_mcp_disabled() {
        let app_state = {
            let config = AppConfig {
                mcp_server_support: false,
                ..AppConfig::default()
            };
            Arc::new(AppState {
                config: Arc::new(config),
                vault: Arc::new(Vault::default()),
                mcp_factory: Arc::new(McpFactory::default()),
                rag_cache: Arc::new(RagCache::default()),
                mcp_config: None,
                mcp_log_path: None,
                mcp_registry: None,
                functions: Functions::default(),
            })
        };
        let ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let mut role = Role::new("r", "p");
        role.set_enabled_mcp_servers(Some(vec!["all".to_string()]));
        let result = ctx.select_enabled_mcp_servers(&role);
        assert!(result.is_empty());
    }

    #[test]
    fn select_enabled_mcp_servers_all_returns_all_mcp_functions() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_mcp_meta_functions(vec![
            tools_only_features("github"),
            tools_only_features("slack"),
        ]);

        let mut role = Role::new("r", "p");
        role.set_enabled_mcp_servers(Some(vec!["all".to_string()]));

        let fns = ctx.select_enabled_mcp_servers(&role);
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"mcp_invoke_github"));
        assert!(names.contains(&"mcp_search_github"));
        assert!(names.contains(&"mcp_invoke_slack"));
    }

    #[test]
    fn select_enabled_mcp_servers_comma_filters() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_mcp_meta_functions(vec![
            tools_only_features("github"),
            tools_only_features("slack"),
        ]);

        let mut role = Role::new("r", "p");
        role.set_enabled_mcp_servers(Some(vec!["github".to_string()]));

        let fns = ctx.select_enabled_mcp_servers(&role);
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"mcp_invoke_github"));
        assert!(!names.contains(&"mcp_invoke_slack"));
    }

    #[test]
    fn select_enabled_mcp_servers_keeps_resources_only_server() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope
            .functions
            .append_mcp_meta_functions(vec![McpServerFeatures {
                name: "res".to_string(),
                tools: false,
                resources: true,
                prompts: false,
            }]);

        let mut role = Role::new("r", "p");
        role.set_enabled_mcp_servers(Some(vec!["res".to_string()]));

        let fns = ctx.select_enabled_mcp_servers(&role);
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"mcp_search_res"));
        assert!(names.contains(&"mcp_describe_res"));
        assert!(!names.contains(&"mcp_invoke_res"));
    }

    #[test]
    fn state_empty_context_has_no_context_flags() {
        let ctx = create_test_ctx();

        let state = ctx.state();

        assert!(!state.contains(StateFlags::ROLE));
        assert!(!state.contains(StateFlags::SESSION));
        assert!(!state.contains(StateFlags::SESSION_EMPTY));
        assert!(!state.contains(StateFlags::AGENT));
        assert!(!state.contains(StateFlags::RAG));
    }

    #[test]
    fn state_includes_function_calling_when_app_enables_it() {
        let ctx = create_test_ctx();

        assert!(ctx.state().contains(StateFlags::FUNCTION_CALLING));
    }

    #[test]
    fn state_includes_skills_enabled_when_app_enables_it() {
        let ctx = create_test_ctx();

        assert!(ctx.state().contains(StateFlags::SKILLS_ENABLED));
    }

    #[test]
    fn state_omits_skills_enabled_when_app_disables_it() {
        let mut ctx = create_test_ctx();

        ctx.update_app_config(|app| app.skills_enabled = false);

        assert!(!ctx.state().contains(StateFlags::SKILLS_ENABLED));
    }

    #[test]
    fn state_skills_enabled_respects_session_override() {
        let mut ctx = create_test_ctx();
        let mut session = Session::default();
        session.set_skills_enabled(Some(false));

        ctx.session = Some(session);

        assert!(!ctx.state().contains(StateFlags::SKILLS_ENABLED));
    }

    #[test]
    fn state_skills_enabled_respects_role_override() {
        let mut ctx = create_test_ctx();
        let role = Role::new("r", "---\nskills_enabled: false\n---\nbody");

        ctx.role = Some(role);

        assert!(!ctx.state().contains(StateFlags::SKILLS_ENABLED));
    }

    #[test]
    fn state_omits_function_calling_when_app_disables_it() {
        let app_state = {
            let config = AppConfig {
                function_calling_support: false,
                ..AppConfig::default()
            };
            Arc::new(AppState {
                config: Arc::new(config),
                vault: Arc::new(Vault::default()),
                mcp_factory: Arc::new(McpFactory::default()),
                rag_cache: Arc::new(RagCache::default()),
                mcp_config: None,
                mcp_log_path: None,
                mcp_registry: None,
                functions: Functions::default(),
            })
        };

        let ctx = RequestContext::new(app_state, WorkingMode::Cmd);

        assert!(!ctx.state().contains(StateFlags::FUNCTION_CALLING));
    }

    #[test]
    fn state_with_role_only() {
        let mut ctx = create_test_ctx();
        ctx.role = Some(Role::new("r", "p"));
        assert!(ctx.state().contains(StateFlags::ROLE));
        assert!(!ctx.state().contains(StateFlags::SESSION));
    }

    #[test]
    fn state_with_empty_session() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        assert!(ctx.state().contains(StateFlags::SESSION_EMPTY));
        assert!(!ctx.state().contains(StateFlags::SESSION));
    }

    #[test]
    fn state_flags_combine_role_and_session() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        ctx.role = Some(Role::new("r", "p"));
        let state = ctx.state();
        assert!(state.contains(StateFlags::SESSION_EMPTY));
    }

    #[test]
    fn todo_info_errors_when_auto_continue_disabled() {
        let ctx = create_test_ctx();
        let err = ctx.todo_info().unwrap_err();

        let msg = err.to_string();

        assert!(
            msg.contains("Auto-continuation is disabled"),
            "expected error to mention auto-continuation, got: {msg}"
        );
    }

    #[test]
    fn todo_info_returns_empty_message_when_list_is_empty() {
        let mut ctx = create_test_ctx();

        ctx.update_app_config(|app| app.auto_continue = true);

        let info = ctx.todo_info().unwrap();
        assert!(
            info.contains("No todos in the running list"),
            "expected 'No todos' message, got: {info}"
        );
    }

    #[test]
    fn todo_info_renders_running_list() {
        let mut ctx = create_test_ctx();
        ctx.update_app_config(|app| app.auto_continue = true);
        ctx.init_todo_list("Map Labs");
        ctx.add_todo("Discover columns");
        ctx.add_todo("Write report");

        ctx.mark_todo_done(1);

        let info = ctx.todo_info().unwrap();
        assert!(
            info.contains("Goal: Map Labs"),
            "expected goal in output, got: {info}"
        );
        assert!(
            info.contains("Progress: 1/2 completed"),
            "expected progress line, got: {info}"
        );
        assert!(
            info.contains("Discover columns"),
            "expected first task, got: {info}"
        );
        assert!(
            info.contains("Write report"),
            "expected second task, got: {info}"
        );
    }

    #[test]
    fn tools_info_returns_message_when_no_tools_enabled() {
        let ctx = create_test_ctx();

        let info = ctx.tools_info().unwrap();

        assert!(
            info.contains("No tools enabled"),
            "expected 'No tools enabled' message, got: {info}"
        );
    }

    #[test]
    fn tools_info_lists_enabled_tool_names_alphabetically() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_todo_functions();
        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["all".to_string()]));
        ctx.role = Some(role);

        let info = ctx.tools_info().unwrap();

        assert!(
            info.contains("Tools enabled for the next request:"),
            "expected count line, got: {info}"
        );
        assert!(
            info.contains("todo__init"),
            "expected todo__init in output, got: {info}"
        );

        let positions: Vec<usize> = info
            .lines()
            .filter(|line| line.trim().starts_with("todo__"))
            .enumerate()
            .map(|(i, _)| i)
            .collect();
        assert!(
            !positions.is_empty(),
            "expected at least one todo__ entry, got: {info}"
        );

        let todo_lines: Vec<&str> = info
            .lines()
            .filter(|line| line.trim().starts_with("todo__"))
            .collect();
        let mut sorted = todo_lines.clone();
        sorted.sort_unstable();
        assert_eq!(
            todo_lines, sorted,
            "expected todo__ entries to be alphabetically sorted, got: {todo_lines:?}"
        );
    }

    #[test]
    fn tools_info_errors_when_function_calling_disabled() {
        let app_state = {
            let config = AppConfig {
                function_calling_support: false,
                ..AppConfig::default()
            };
            Arc::new(AppState {
                config: Arc::new(config),
                vault: Arc::new(Vault::default()),
                mcp_factory: Arc::new(McpFactory::default()),
                rag_cache: Arc::new(RagCache::default()),
                mcp_config: None,
                mcp_log_path: None,
                mcp_registry: None,
                functions: Functions::default(),
            })
        };
        let ctx = RequestContext::new(app_state, WorkingMode::Cmd);

        let err = ctx.tools_info().unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("Function calling is disabled"),
            "expected error to mention function calling, got: {msg}"
        );
    }

    #[test]
    fn role_info_errors_when_no_role() {
        let ctx = create_test_ctx();
        assert!(ctx.role_info().is_err());
    }

    #[test]
    fn role_info_succeeds_with_role() {
        let mut ctx = create_test_ctx();
        ctx.role = Some(Role::new("test", "be helpful"));
        let info = ctx.role_info().unwrap();
        assert!(info.contains("be helpful"));
    }

    #[test]
    fn agent_info_errors_when_no_agent() {
        let ctx = create_test_ctx();
        assert!(ctx.agent_info().is_err());
    }

    #[test]
    fn rag_info_errors_when_no_rag() {
        let ctx = create_test_ctx();
        assert!(ctx.rag_info().is_err());
    }

    #[test]
    #[serial]
    fn use_role_obj_errors_when_agent_active() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                ctx.use_agent(&app, &agent_name, None, crate::utils::create_abort_signal())
                    .await
                    .unwrap();
            });

        let result = ctx.use_role_obj(Role::new("r", "p"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("using a agent"));
    }

    #[test]
    fn exit_rag_clears_rag() {
        let mut ctx = create_test_ctx();
        assert!(ctx.rag.is_none());
        ctx.exit_rag().unwrap();
        assert!(ctx.rag.is_none());
    }

    #[test]
    fn discontinuous_last_message_sets_continuous_false() {
        let mut ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "test", None).unwrap();
        ctx.last_message = Some(LastMessage::new(input, "reply".to_string()));
        assert!(ctx.last_message.as_ref().unwrap().continuous);
        ctx.discontinuous_last_message();
        assert!(!ctx.last_message.as_ref().unwrap().continuous);
    }

    #[test]
    fn discontinuous_last_message_noop_when_none() {
        let mut ctx = create_test_ctx();
        assert!(ctx.last_message.is_none());
        ctx.discontinuous_last_message();
        assert!(ctx.last_message.is_none());
    }

    #[test]
    fn before_chat_completion_sets_last_message() {
        let mut ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "hello", None).unwrap();
        ctx.before_chat_completion(&input).unwrap();
        assert!(ctx.last_message.is_some());
        let lm = ctx.last_message.as_ref().unwrap();
        assert_eq!(lm.output, "");
        assert!(lm.continuous);
    }

    #[test]
    fn on_chat_completion_error_without_session_sets_last_message_discontinuous() {
        let mut ctx = create_test_ctx();
        let app = Arc::clone(&ctx.app.config);
        let input = Input::from_str(&ctx, "hello", None).unwrap();

        ctx.on_chat_completion_error(app.as_ref(), &input);

        let lm = ctx.last_message.as_ref().unwrap();
        assert_eq!(lm.output, "");
        assert!(!lm.continuous, "no session means recovery is not possible");
    }

    #[test]
    fn on_chat_completion_error_with_session_sets_last_message_continuous() {
        let mut ctx = create_test_ctx();
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                dry_run: true,
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });
        ctx.session = Some(Session::default());
        let app = Arc::clone(&ctx.app.config);
        let input = Input::from_str(&ctx, "hello", None).unwrap();

        ctx.on_chat_completion_error(app.as_ref(), &input);

        let lm = ctx.last_message.as_ref().unwrap();
        assert_eq!(lm.output, "");
        assert!(lm.continuous, "session present means .recover is available");
    }

    #[test]
    fn on_chat_completion_error_with_session_checkpoints_session_messages() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        assert!(ctx.session.as_ref().unwrap().is_empty());
        let app = Arc::clone(&ctx.app.config);
        let input = Input::from_str(&ctx, "hello", None).unwrap();
        ctx.on_chat_completion_error(app.as_ref(), &input);
        assert!(
            !ctx.session.as_ref().unwrap().is_empty(),
            "session should have the interrupted turn checkpointed"
        );
    }

    #[test]
    fn has_recoverable_interruption_false_by_default() {
        let ctx = create_test_ctx();
        assert!(!ctx.has_recoverable_interruption());
    }

    #[test]
    fn has_recoverable_interruption_true_for_live_interrupted_turn() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let app = Arc::clone(&ctx.app.config);
        let input = Input::from_str(&ctx, "hello", None).unwrap();

        ctx.on_chat_completion_error(app.as_ref(), &input);

        assert!(ctx.has_recoverable_interruption());
    }

    #[test]
    fn has_recoverable_interruption_false_after_normal_exchange() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let input = Input::from_str(&ctx, "hello", None).unwrap();
        ctx.session
            .as_mut()
            .unwrap()
            .add_message(&input, "all done")
            .unwrap();

        assert!(!ctx.has_recoverable_interruption());
    }

    #[test]
    fn has_recoverable_interruption_survives_session_save_and_reload() {
        // Simulates the full user flow: a turn crashes mid-tool-loop (the
        // checkpoint lands in the session), the user runs `.save session`,
        // exits, and a fresh process resumes the session. The in-memory
        // last_message is gone; only the persisted checkpoint can mark the
        // session recoverable for `.recover`.
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let app = Arc::clone(&ctx.app.config);
        let input = Input::from_str(&ctx, "hello", None).unwrap();
        ctx.on_chat_completion_error(app.as_ref(), &input);

        let yaml = serde_yaml::to_string(ctx.session.as_ref().unwrap()).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();

        let mut resumed_ctx = create_test_ctx();
        resumed_ctx.session = Some(reloaded);
        assert!(resumed_ctx.last_message.is_none());
        assert!(
            resumed_ctx.has_recoverable_interruption(),
            ".recover must work on a session resumed after an interrupted turn"
        );
    }

    #[test]
    fn after_chat_completion_sweeps_auto_unload_skills_at_turn_end() {
        let mut ctx = create_test_ctx();
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                dry_run: true,
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });

        let ephemeral = Skill::new("ephemeral", "---\nauto_unload: true\n---\nbody");
        let persistent = Skill::new("persistent", "---\nauto_unload: false\n---\nbody");
        ctx.skill_registry.insert(ephemeral).unwrap();
        ctx.skill_registry.insert(persistent).unwrap();

        let input = Input::from_str(&ctx, "hello", None).unwrap();
        let app = Arc::clone(&ctx.app.config);
        ctx.after_chat_completion(app.as_ref(), &input, "response", &[])
            .unwrap();

        assert!(!ctx.skill_registry.is_loaded("ephemeral"));
        assert!(ctx.skill_registry.is_loaded("persistent"));
    }

    #[test]
    fn after_chat_completion_preserves_auto_unload_during_tool_loop() {
        let mut ctx = create_test_ctx();
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                dry_run: true,
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });

        let ephemeral = Skill::new("ephemeral", "---\nauto_unload: true\n---\nbody");
        ctx.skill_registry.insert(ephemeral).unwrap();

        let input = Input::from_str(&ctx, "hello", None).unwrap();
        let app = Arc::clone(&ctx.app.config);
        let tool_result = ToolResult::new(crate::function::ToolCall::default(), json!({}));
        ctx.after_chat_completion(app.as_ref(), &input, "", &[tool_result])
            .unwrap();

        assert!(
            ctx.skill_registry.is_loaded("ephemeral"),
            "auto_unload skills must persist through tool-using rounds"
        );
    }

    #[test]
    fn role_like_mut_returns_none_when_empty() {
        let mut ctx = create_test_ctx();
        assert!(ctx.role_like_mut().is_none());
    }

    #[test]
    fn role_like_mut_returns_role_when_only_role() {
        let mut ctx = create_test_ctx();
        ctx.role = Some(Role::new("r", "p"));
        assert!(ctx.role_like_mut().is_some());
    }

    #[test]
    fn role_like_mut_prefers_session_over_role() {
        let mut ctx = create_test_ctx();
        ctx.role = Some(Role::new("r", "p"));
        ctx.session = Some(Session::default());
        let rl = ctx.role_like_mut().unwrap();
        rl.set_temperature(Some(0.5));
        assert_eq!(ctx.session.as_ref().unwrap().temperature(), Some(0.5));
    }

    #[test]
    fn working_mode_cmd() {
        let ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        assert!(ctx.working_mode.is_cmd());
        assert!(!ctx.working_mode.is_repl());
    }

    #[test]
    fn working_mode_repl() {
        let ctx = RequestContext::new(default_app_state(), WorkingMode::Repl);
        assert!(ctx.working_mode.is_repl());
        assert!(!ctx.working_mode.is_cmd());
    }

    #[test]
    fn fork_base_name_strips_fork_suffix() {
        assert_eq!(fork_base_name("my-session-fork-1"), "my-session");
        assert_eq!(fork_base_name("my-session-fork-42"), "my-session");
    }

    #[test]
    fn fork_base_name_leaves_plain_names_unchanged() {
        assert_eq!(fork_base_name("my-session"), "my-session");
        assert_eq!(fork_base_name("research"), "research");
    }

    #[test]
    fn fork_base_name_ignores_non_numeric_suffix() {
        assert_eq!(fork_base_name("my-session-fork-abc"), "my-session-fork-abc");
        assert_eq!(fork_base_name("my-session-fork-"), "my-session-fork-");
    }

    #[test]
    fn fork_base_name_flattens_nested_forks() {
        assert_eq!(
            fork_base_name("my-session-fork-1-fork-2"),
            "my-session-fork-1"
        );
    }

    #[test]
    fn session_file_returns_yaml_path() {
        let ctx = create_test_ctx();
        let path = ctx.session_file("my-session");
        assert!(path.to_string_lossy().ends_with("my-session.yaml"));
    }

    #[test]
    fn session_file_with_subdir() {
        let ctx = create_test_ctx();
        let path = ctx.session_file("subdir/my-session");
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("subdir"));
        assert!(path_str.ends_with("my-session.yaml"));
    }

    #[test]
    fn is_compressing_session_false_when_no_session() {
        let ctx = create_test_ctx();
        assert!(!ctx.is_compressing_session());
    }

    #[test]
    fn is_compressing_session_false_with_default_session() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        assert!(!ctx.is_compressing_session());
    }

    #[test]
    #[serial]
    fn retrieve_role_from_markdown_file() {
        let _guard = TestConfigDirGuard::new();
        let roles_dir = paths::roles_dir();
        create_dir_all(&roles_dir).unwrap();
        write(
            roles_dir.join("pirate.md"),
            "You are a pirate. Speak only in pirate language.",
        )
        .unwrap();

        let ctx = create_test_ctx();
        let role = ctx.retrieve_role(&ctx.app.config, "pirate").unwrap();
        assert_eq!(role.name(), "pirate");
        assert!(role.prompt().contains("pirate"));
    }

    #[test]
    #[serial]
    fn retrieve_role_builtin_exists() {
        let _guard = TestConfigDirGuard::new();
        let ctx = create_test_ctx();
        let names = paths::list_roles(true);
        if !names.is_empty() {
            let role = ctx.retrieve_role(&ctx.app.config, &names[0]);
            assert!(role.is_ok());
        }
    }

    #[test]
    #[serial]
    fn retrieve_role_nonexistent_errors() {
        let _guard = TestConfigDirGuard::new();
        let ctx = create_test_ctx();
        let result = ctx.retrieve_role(&ctx.app.config, "definitely_not_a_real_role_xyz");
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn retrieve_role_no_model_id_inherits_current_model() {
        let _guard = TestConfigDirGuard::new();
        let roles_dir = paths::roles_dir();
        create_dir_all(&roles_dir).unwrap();
        write(roles_dir.join("simple.md"), "You are helpful.").unwrap();

        let ctx = create_test_ctx();
        let role = ctx.retrieve_role(&ctx.app.config, "simple").unwrap();
        assert_eq!(role.model().id(), ctx.current_model().id());
    }

    #[test]
    #[serial]
    fn list_roles_finds_markdown_files() {
        let _guard = TestConfigDirGuard::new();
        let roles_dir = paths::roles_dir();
        create_dir_all(&roles_dir).unwrap();
        write(roles_dir.join("alpha.md"), "Alpha role").unwrap();
        write(roles_dir.join("beta.md"), "Beta role").unwrap();
        write(roles_dir.join("not_a_role.txt"), "ignored").unwrap();

        let names = paths::list_roles(false);
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"beta".to_string()));
        assert!(!names.contains(&"not_a_role".to_string()));
    }

    #[test]
    #[serial]
    fn list_roles_empty_dir() {
        let _guard = TestConfigDirGuard::new();
        let roles_dir = paths::roles_dir();
        create_dir_all(&roles_dir).unwrap();
        let names = paths::list_roles(false);
        assert!(names.is_empty());
    }

    #[test]
    #[serial]
    fn session_new_from_ctx_captures_state() {
        let _guard = TestConfigDirGuard::new();
        let ctx = create_test_ctx();
        let session = Session::new_from_ctx(&ctx, &ctx.app.config, "test-session").unwrap();
        assert_eq!(session.name(), "test-session");
        assert!(session.is_empty());
    }

    #[test]
    #[serial]
    fn session_save_creates_file() {
        let _guard = TestConfigDirGuard::new();
        let ctx = create_test_ctx();
        let mut session = Session::new_from_ctx(&ctx, &ctx.app.config, "save-test").unwrap();
        let session_path = ctx.session_file("save-test");
        ensure_parent_exists(&session_path).unwrap();

        session.save("save-test", &session_path, false).unwrap();
        assert!(session_path.exists());
    }

    #[test]
    #[serial]
    fn use_session_errors_when_already_in_session() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());

        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();
        let result = run_async(ctx.use_session(&app, Some("new"), abort));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Already in a session")
        );
    }

    #[test]
    #[serial]
    fn use_session_creates_temp_session() {
        let _guard = TestConfigDirGuard::new();
        let sessions_dir = paths::local_dir("sessions");
        create_dir_all(&sessions_dir).unwrap();

        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();
        run_async(ctx.use_session(&app, None, abort)).unwrap();

        assert!(ctx.session.is_some());
        assert_eq!(ctx.session.as_ref().unwrap().name(), TEMP_SESSION_NAME);
    }

    #[test]
    #[serial]
    fn use_session_creates_named_session() {
        let _guard = TestConfigDirGuard::new();
        let sessions_dir = paths::local_dir("sessions");
        create_dir_all(&sessions_dir).unwrap();

        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();
        run_async(ctx.use_session(&app, Some("my-session"), abort)).unwrap();

        assert!(ctx.session.is_some());
        assert_eq!(ctx.session.as_ref().unwrap().name(), "my-session");
    }

    #[test]
    #[serial]
    fn exit_session_roundtrip() {
        let _guard = TestConfigDirGuard::new();
        let sessions_dir = paths::local_dir("sessions");
        create_dir_all(&sessions_dir).unwrap();

        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();
        run_async(ctx.use_session(&app, Some("roundtrip"), abort.clone())).unwrap();
        assert!(ctx.session.is_some());

        ctx.exit_session().unwrap();
        assert!(ctx.session.is_none());
    }

    #[test]
    #[serial]
    fn use_role_obj_and_exit_role_full_cycle() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();

        ctx.use_role_obj(Role::new("test-role", "test prompt"))
            .unwrap();
        assert!(ctx.role.is_some());
        assert_eq!(ctx.role.as_ref().unwrap().name(), "test-role");

        let _ = ctx.exit_role();
        assert!(ctx.role.is_none());
    }

    #[test]
    #[serial]
    fn use_role_obj_twice_replaces_role() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();

        ctx.use_role_obj(Role::new("first", "prompt 1")).unwrap();
        assert_eq!(ctx.role.as_ref().unwrap().name(), "first");

        ctx.use_role_obj(Role::new("second", "prompt 2")).unwrap();
        assert_eq!(ctx.role.as_ref().unwrap().name(), "second");
    }

    #[test]
    #[serial]
    fn list_macros_finds_yaml_files() {
        let _guard = TestConfigDirGuard::new();
        let macros_dir = paths::macros_dir();
        create_dir_all(&macros_dir).unwrap();
        write(macros_dir.join("greet.yaml"), "steps:\n  - \".help\"").unwrap();
        write(macros_dir.join("build.yaml"), "steps:\n  - \".help\"").unwrap();

        let names = paths::list_macros();
        assert!(names.contains(&"greet".to_string()));
        assert!(names.contains(&"build".to_string()));
    }

    #[test]
    #[serial]
    fn list_rags_finds_yaml_files() {
        let _guard = TestConfigDirGuard::new();
        let rags_dir = paths::rags_dir();
        create_dir_all(&rags_dir).unwrap();
        write(rags_dir.join("docs.yaml"), "embedding_model: test").unwrap();

        let names = paths::list_rags();
        assert!(names.contains(&"docs".to_string()));
    }

    #[test]
    #[serial]
    fn list_rags_empty_dir() {
        let _guard = TestConfigDirGuard::new();
        let rags_dir = paths::rags_dir();
        create_dir_all(&rags_dir).unwrap();
        assert!(paths::list_rags().is_empty());
    }

    #[test]
    #[serial]
    fn list_rags_skips_sbx_mixin_sidecars() {
        let _guard = TestConfigDirGuard::new();
        let rags_dir = paths::rags_dir();
        create_dir_all(&rags_dir).unwrap();
        write(rags_dir.join("docs.yaml"), "embedding_model: test").unwrap();
        write(rags_dir.join("docs.sbx-mixin.yaml"), "kind: mixin").unwrap();
        write(rags_dir.join("v2.docs.yaml"), "embedding_model: test").unwrap();

        let names = paths::list_rags();
        assert!(names.contains(&"docs".to_string()));
        assert!(
            names.contains(&"v2.docs".to_string()),
            "a dotted RAG name must still be listed: {names:?}"
        );
        assert!(
            !names.contains(&"docs.sbx-mixin".to_string()),
            "the sandbox mixin sidecar must not appear as a RAG: {names:?}"
        );
    }

    #[test]
    #[serial]
    fn use_agent_errors_when_already_in_session() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());

        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        let result = run_async(ctx.use_agent(&app, &agent_name, Some("test_session"), abort));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Already in a session")
        );
        assert!(
            ctx.agent.is_none(),
            "Agent should not be set when session check fails"
        );
    }

    #[test]
    #[serial]
    fn use_agent_errors_when_already_in_session_even_without_session_name() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());

        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        let result = run_async(ctx.use_agent(&app, &agent_name, None, abort));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Already in a session")
        );
        assert!(
            ctx.agent.is_none(),
            "Agent should not be set when session check fails"
        );
    }

    #[test]
    #[serial]
    fn use_agent_errors_when_graph_agent_given_explicit_session() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();

        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_graph_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\nversion: \"1.0\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: ok\n"
            ),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        let result = run_async(ctx.use_agent(&app, &agent_name, Some("test_session"), abort));

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("does not support sessions")
        );
        assert!(
            ctx.agent.is_none(),
            "Agent should not be set when the graph-agent session guard fails"
        );
    }

    #[test]
    #[serial]
    fn use_agent_skips_inherited_session_for_graph_agent() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        ctx.update_app_config(|app| app.agent_session = Some("inherited".to_string()));

        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_graph_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\nversion: \"1.0\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: ok\n"
            ),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        run_async(ctx.use_agent(&app, &agent_name, None, abort)).unwrap();

        assert!(ctx.agent.is_some(), "Graph agent should load successfully");
        assert!(
            ctx.session.is_none(),
            "Graph agent must not engage a session, not even an inherited default"
        );
    }

    #[test]
    #[serial]
    fn use_agent_suppresses_inherited_session_in_isolated_macro() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        ctx.macro_flag = true;
        ctx.update_app_config(|app| app.agent_session = Some("inherited".to_string()));

        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        run_async(ctx.use_agent(&app, &agent_name, None, abort)).unwrap();

        assert!(
            ctx.session.is_none(),
            "an isolated macro must keep suppressing the agent's default session"
        );
    }

    #[test]
    #[serial]
    fn use_agent_engages_inherited_session_in_non_isolated_macro() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        ctx.macro_flag = true;
        ctx.macro_non_isolated = true;
        ctx.update_app_config(|app| app.agent_session = Some("inherited".to_string()));

        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        run_async(ctx.use_agent(&app, &agent_name, None, abort)).unwrap();

        assert!(
            ctx.session.is_some(),
            "a non-isolated macro's agent step must engage the default session as if typed"
        );
    }

    fn first_file(dir: &Path) -> Option<PathBuf> {
        for entry in read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = first_file(&path) {
                    return Some(found);
                }
            } else {
                return Some(path);
            }
        }
        None
    }

    #[test]
    fn asset_category_parse_maps_known_names() {
        assert_eq!(AssetCategory::parse("agents"), Some(AssetCategory::Agents));
        assert_eq!(AssetCategory::parse("macros"), Some(AssetCategory::Macros));
        assert_eq!(
            AssetCategory::parse("functions"),
            Some(AssetCategory::Functions)
        );
        assert_eq!(
            AssetCategory::parse("mcp-config"),
            Some(AssetCategory::McpConfig)
        );
        assert_eq!(
            AssetCategory::parse("mcp_config"),
            Some(AssetCategory::McpConfig)
        );
        assert_eq!(AssetCategory::parse("roles"), None);
        assert_eq!(AssetCategory::parse(""), None);
    }

    #[test]
    #[serial]
    fn install_builtin_agents_force_overwrites_only_with_force() {
        let _guard = TestConfigDirGuard::new();

        Agent::install_builtin_agents(false).unwrap();
        let file =
            first_file(&paths::agents_data_dir()).expect("bundled agents should be installed");

        write(&file, "SENTINEL").unwrap();
        Agent::install_builtin_agents(false).unwrap();
        assert_eq!(
            read_to_string(&file).unwrap(),
            "SENTINEL",
            "non-force install must not overwrite an existing file"
        );

        Agent::install_builtin_agents(true).unwrap();
        assert_ne!(
            read_to_string(&file).unwrap(),
            "SENTINEL",
            "force install must overwrite the existing file"
        );
    }

    #[test]
    #[serial]
    fn install_builtin_agents_removes_stale_definition_files() {
        let _guard = TestConfigDirGuard::new();

        Agent::install_builtin_agents(false).unwrap();

        let agents_dir = paths::agents_data_dir();
        // Simulate an upgrade from an older install: coder used to ship
        // config.yaml + tools.py (now graph.yaml + tools.sh) and demo used
        // to ship tools.sh (now tools.py + tools.sh.bak).
        write(agents_dir.join("coder").join("config.yaml"), "stale").unwrap();
        write(agents_dir.join("coder").join("tools.py"), "stale").unwrap();
        write(agents_dir.join("demo").join("tools.sh"), "stale").unwrap();
        // Reconciliation must never touch subdirectories, non-definition
        // root files, or agents that aren't part of the bundle.
        create_dir_all(agents_dir.join("coder").join("sessions")).unwrap();
        write(
            agents_dir.join("coder").join("sessions").join("keep.yaml"),
            "keep",
        )
        .unwrap();
        write(agents_dir.join("coder").join(".env"), "keep").unwrap();
        create_dir_all(agents_dir.join("myagent")).unwrap();
        write(agents_dir.join("myagent").join("config.yaml"), "keep").unwrap();

        Agent::install_builtin_agents(false).unwrap();

        assert!(
            !agents_dir.join("coder").join("config.yaml").exists(),
            "stale config.yaml must be removed for a graph-based bundled agent"
        );
        assert!(
            !agents_dir.join("coder").join("tools.py").exists(),
            "stale tools script must be removed when the bundle no longer ships it"
        );
        assert!(
            !agents_dir.join("demo").join("tools.sh").exists(),
            "stale tools.sh must be removed when the bundle only ships tools.py"
        );
        assert!(agents_dir.join("coder").join("graph.yaml").exists());
        assert!(agents_dir.join("coder").join("tools.sh").exists());
        assert!(agents_dir.join("demo").join("tools.py").exists());
        assert!(
            agents_dir
                .join("coder")
                .join("sessions")
                .join("keep.yaml")
                .exists(),
            "files in agent subdirectories must survive reconciliation"
        );
        assert!(
            agents_dir.join("coder").join(".env").exists(),
            "non-definition root files must survive reconciliation"
        );
        assert!(
            agents_dir.join("myagent").join("config.yaml").exists(),
            "custom agents outside the bundle must survive reconciliation"
        );
    }

    #[test]
    #[serial]
    fn install_builtin_agents_removes_stale_graph_for_config_agent() {
        let _guard = TestConfigDirGuard::new();

        Agent::install_builtin_agents(false).unwrap();

        let agents_dir = paths::agents_data_dir();
        write(agents_dir.join("architect").join("graph.yaml"), "stale").unwrap();

        Agent::install_builtin_agents(false).unwrap();

        assert!(
            !agents_dir.join("architect").join("graph.yaml").exists(),
            "stale graph.yaml must be removed for a config-based bundled agent"
        );
        assert!(agents_dir.join("architect").join("config.yaml").exists());
    }

    #[test]
    #[serial]
    fn install_builtin_skills_force_overwrites_only_with_force() {
        let _guard = TestConfigDirGuard::new();

        Skill::install_builtin_skills(false).unwrap();
        let file = paths::skill_file("git-master");
        assert!(file.exists(), "git-master skill should be installed");

        write(&file, "SENTINEL").unwrap();
        Skill::install_builtin_skills(false).unwrap();
        assert_eq!(
            read_to_string(&file).unwrap(),
            "SENTINEL",
            "non-force install must not overwrite an existing skill"
        );

        Skill::install_builtin_skills(true).unwrap();
        assert_ne!(
            read_to_string(&file).unwrap(),
            "SENTINEL",
            "force install must overwrite the existing skill"
        );
    }

    #[test]
    #[serial]
    fn install_builtin_skills_installs_all_bundled() {
        let _guard = TestConfigDirGuard::new();

        Skill::install_builtin_skills(false).unwrap();
        assert!(paths::skill_file("git-master").exists());
        assert!(paths::skill_file("ai-slop-remover").exists());
        assert!(paths::skill_file("code-review").exists());
        assert!(paths::skill_file("frontend-ui-ux").exists());
    }

    #[test]
    #[serial]
    fn bundled_assets_pin_task_queue_guidance() {
        let _guard = TestConfigDirGuard::new();
        Agent::install_builtin_agents(false).unwrap();

        // 1. Architect config: the parallel-mode "Task-queue mirroring"
        //    section with all five HARD RULES.
        let architect = read_to_string(
            paths::agents_data_dir()
                .join("architect")
                .join("config.yaml"),
        )
        .unwrap();
        for anchor in [
            "Task-queue mirroring (optional).",
            "it is NEVER the source of truth",
            "Root tasks (no blockers) are NOT auto-dispatched on create — spawn them yourself.",
            "Call `agent__task_complete` ONLY after this task's verification gate",
            "Collect every auto-dispatched agent via `agent__collect`",
            "strands the queue task InProgress with no reset — recreate",
        ] {
            assert!(
                architect.contains(anchor),
                "architect config lost task-queue mirroring anchor: {anchor:?}"
            );
        }

        // 2. Sisyphus config: the task-queue chain note in the
        //    parallel-research section.
        let sisyphus = read_to_string(
            paths::agents_data_dir()
                .join("sisyphus")
                .join("config.yaml"),
        )
        .unwrap();
        for anchor in [
            "`agent__task_create` chains MAY encode the",
            "Todos remain the tracking source of truth.",
        ] {
            assert!(
                sisyphus.contains(anchor),
                "sisyphus config lost task-queue chain anchor: {anchor:?}"
            );
        }

        // 3. Spawn instructions: dispatch/collect/failure semantics plus the
        //    corrected numeric-string task IDs (no `task_1`-style remnants).
        let spawn = crate::config::prompts::DEFAULT_SPAWN_INSTRUCTIONS;
        for anchor in [
            "is NEVER auto-dispatched",
            "agent__collect",
            "recreate the chain rather than retrying",
            "--blocked_by [\"1\"]",
        ] {
            assert!(
                spawn.contains(anchor),
                "DEFAULT_SPAWN_INSTRUCTIONS lost task-queue semantics anchor: {anchor:?}"
            );
        }
        assert!(
            !spawn.contains("task_1"),
            "DEFAULT_SPAWN_INSTRUCTIONS must not reference task_1-style IDs"
        );
    }

    // A gauntlet output carrying the review-incomplete line is an infrastructure
    // fault: both callers must escalate it with the exact same three options
    // instead of routing it through their findings-handling rules.
    #[test]
    #[serial]
    fn bundled_assets_pin_review_incomplete_escalation() {
        let _guard = TestConfigDirGuard::new();
        Agent::install_builtin_agents(false).unwrap();

        // Every anchor is a single-line prefix so YAML re-wrapping cannot
        // break the pin; clauses that wrap differently per config are split
        // at the wrap point.
        const SHARED_ANCHORS: [&str; 10] = [
            "GAUNTLET_REVIEW_INCOMPLETE:",
            "\"Retry the review again\"",
            "\"Accept NEEDS-HUMAN: proceed without the <lane or pipeline> review (recorded in the report / PR body)\"",
            "\"Abort this task\"",
            "NEVER route it through the findings-handling rules",
            "NEVER claim done while it stands",
            "is NOT a code failure",
            // Findings AND incomplete on the same output: findings first.
            "fix the findings first, then re-run the gauntlet (the re-run",
            // The autonomous ladder still governs this escalation.
            "Under `escalation_policy: autonomous`",
            "the standard ladder applies (a standing",
        ];

        // The pin must never leak run-local provenance into the shipped
        // assets: task IDs, plan branch names, or plan-repo paths.
        const PROVENANCE_LEAKS: [&str; 3] = ["TASK-030", "PLAN-review-retry", "plans/tasks/"];

        let mut configs = std::collections::HashMap::new();
        for name in ["architect", "sisyphus"] {
            // Normalize line endings: the ordering slices below span line
            // breaks and Windows checkouts carry CRLF.
            let config = read_to_string(paths::agents_data_dir().join(name).join("config.yaml"))
                .unwrap()
                .replace("\r\n", "\n");
            for anchor in SHARED_ANCHORS {
                assert!(
                    config.contains(anchor),
                    "{name} config lost review-incomplete escalation anchor: {anchor:?}"
                );
            }
            assert!(
                config.contains("user__select"),
                "{name} config must escalate review-incomplete via user__select"
            );
            for leak in PROVENANCE_LEAKS {
                assert!(
                    !config.contains(leak),
                    "{name} config must not carry run-local provenance: {leak:?}"
                );
            }
            configs.insert(name, config);
        }

        // Sisyphus PLACEMENT: the incomplete rule is its own bullet directly
        // between the BLOCKED and PASS handling bullets, so a follower reading
        // the BLOCKED rule meets "the next rule owns it" immediately.
        let sisyphus = &configs["sisyphus"];
        let find = |needle: &str| {
            sisyphus
                .find(needle)
                .unwrap_or_else(|| panic!("sisyphus config lost placement anchor: {needle:?}"))
        };
        let blocked = find("- `GAUNTLET: BLOCKED` blocks completion");
        let incomplete =
            find("- `GAUNTLET_REVIEW_INCOMPLETE: <lane>[, <lane>]` (the line directly under");
        let pass = find("- `GAUNTLET: PASS` with a");
        assert!(
            blocked < incomplete && incomplete < pass,
            "sisyphus config must order the gauntlet rules BLOCKED < REVIEW_INCOMPLETE < PASS \
             (got {blocked} / {incomplete} / {pass})"
        );
        assert!(
            !sisyphus[blocked + 1..incomplete].contains("\n  - "),
            "sisyphus config must keep the REVIEW_INCOMPLETE bullet directly after the BLOCKED \
             bullet (no other same-level bullet in between)"
        );

        // Architect PLACEMENT: the incomplete contract lives inside Phase E's
        // divergence check, under the preferred gauntlet path and before the
        // fallback adversary spawn.
        let architect = &configs["architect"];
        let find = |needle: &str| {
            architect
                .find(needle)
                .unwrap_or_else(|| panic!("architect config lost placement anchor: {needle:?}"))
        };
        let phase_e = find("### Phase E");
        let verify = find("4. **Verify against the plan");
        let preferred = find("**Preferred: run the conformance checks through `review-gauntlet`**");
        let incomplete = find("GAUNTLET_REVIEW_INCOMPLETE:");
        let adversary = find("- **Spawn `adversary`**");
        assert!(
            phase_e < verify
                && verify < preferred
                && preferred < incomplete
                && incomplete < adversary,
            "architect config must place the REVIEW_INCOMPLETE contract inside Phase E's verify \
             step, under the preferred gauntlet path and before the adversary fallback \
             (got {phase_e} / {verify} / {preferred} / {incomplete} / {adversary})"
        );

        // The architect also owns the no-gauntlet fallback path: a
        // fault-only adversary/probe result is retried, never treated as
        // a divergence.
        for anchor in [
            "an adversary/probe result that is ONLY a `PIPELINE-FAULT`",
            "is likewise an infrastructure fault to retry, never a divergence",
        ] {
            assert!(
                architect.contains(anchor),
                "architect config lost the fallback adversary/probe pipeline-fault sentence: {anchor:?}"
            );
        }

        // The agent READMEs describe the same contract to humans: the line is
        // an infrastructure fault escalated with the three caller options,
        // never a finding (Sisyphus) / never a divergence (Architect).
        let readmes = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents");
        for (name, own_claim) in [
            ("sisyphus", "never a finding to fix"),
            ("architect", "never a divergence"),
        ] {
            let readme = read_to_string(readmes.join(name).join("README.md")).unwrap();
            for anchor in [
                "`GAUNTLET_REVIEW_INCOMPLETE:` line",
                "infrastructure fault",
                "retry / accept NEEDS-HUMAN / abort",
                own_claim,
            ] {
                assert!(
                    readme.contains(anchor),
                    "{name} README lost review-incomplete contract anchor: {anchor:?}"
                );
            }
            for leak in PROVENANCE_LEAKS {
                assert!(
                    !readme.contains(leak),
                    "{name} README must not carry run-local provenance: {leak:?}"
                );
            }
        }
    }

    // Shell commands the adversary's run_checks stage executes must be
    // declared as a variable, never as prompt prose an LLM would re-extract.
    #[test]
    #[serial]
    fn bundled_assets_pin_verification_commands_as_declared_variable() {
        let _guard = TestConfigDirGuard::new();
        Agent::install_builtin_agents(false).unwrap();

        const VARIABLES_FORM: &str =
            "--variables {\"verification_commands\": \"[\\\"cargo test --all\\\"";
        const EXAMPLE_CUE: &str =
            "only an EXAMPLE: replace them with THIS project's exact build/test/lint commands";
        for name in ["architect", "sisyphus"] {
            let config =
                read_to_string(paths::agents_data_dir().join(name).join("config.yaml")).unwrap();
            assert_eq!(
                config.matches(VARIABLES_FORM).count(),
                2,
                "{name} config must pass verification_commands via --variables on both the \
                 review-gauntlet and adversary spawn templates"
            );
            assert!(
                !config.contains("Verification commands:"),
                "{name} config must not declare verification commands as prompt prose"
            );
            assert!(
                config.matches("trust boundary").count() >= 2,
                "{name} config must explain the trust boundary on both spawn templates"
            );
            assert!(
                config.contains("untrusted pasted text"),
                "{name} config must name the untrusted prompt text as the reason"
            );
            assert!(
                config.matches(EXAMPLE_CUE).count() >= 2,
                "{name} config must mark the cargo commands as an example to substitute on both \
                 spawn templates, or a literal follower records `cargo: command not found` in non-Rust repos"
            );
        }

        let readmes = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents");
        let adversary = read_to_string(readmes.join("adversary/README.md")).unwrap();
        assert!(
            !adversary.contains("## VERIFICATION"),
            "adversary README must not show the prose ## VERIFICATION block"
        );
        assert!(adversary.contains(VARIABLES_FORM));
        assert!(
            adversary.contains("--agent-variable verification_commands '[\"cargo test --all\"]'"),
            "adversary README must show the CLI form"
        );
        assert!(
            adversary.contains(
                "merges only the keys declared under `parse`'s `output_schema.properties`"
            ) && !adversary.contains("Known residual"),
            "adversary README must state the schema-declared merge closes the extra-key channel"
        );
        assert!(
            adversary.contains("zero red verification runs"),
            "adversary README must state the fail-closed CONFORMS doctrine"
        );
        assert!(
            !adversary.contains("soft ENVIRONMENT")
                && adversary.contains("never degrades to an ENVIRONMENT marker"),
            "adversary README must describe a malformed declaration as fail-closed, not a soft marker"
        );
        let gauntlet = read_to_string(readmes.join("review-gauntlet/README.md")).unwrap();
        assert!(
            !gauntlet.contains("Verification commands:"),
            "review-gauntlet README must not show the prose Verification commands: line"
        );
        assert!(gauntlet.contains(VARIABLES_FORM));
        assert!(
            !gauntlet.contains("extracted verbatim by `parse`"),
            "review-gauntlet README must not describe parse-extraction of the commands"
        );
        assert!(
            gauntlet.contains("--agent-variable verification_commands '[\"cargo test --all\"]'"),
            "review-gauntlet README must show the CLI form"
        );
        assert!(gauntlet.contains("plus probe on consumer surface *with a local-run recipe*"));
        assert!(gauntlet.contains("critical count from the report's summary line"));
        assert!(gauntlet.contains("raw 🔴 count if absent"));
        assert!(
            gauntlet
                .contains("records a pipeline fault blocks (a degraded review is never a pass)")
        );
        assert!(
            !gauntlet.contains("any 🔴 in the code-review report"),
            "review-gauntlet README must not describe the retired raw-🔴 gate"
        );

        // Normalize line endings: the assertion below spans a line break and
        // Windows checkouts carry CRLF.
        let runner = read_to_string(readmes.join("adversary/scripts/run_checks.py"))
            .unwrap()
            .replace("\r\n", "\n");
        assert!(
            runner.contains("Trust boundary:") && runner.contains("never a field an LLM"),
            "run_checks.py docstring must state the declared-variable trust boundary"
        );
        assert!(
            !runner.contains("soft ENVIRONMENT")
                && runner.contains("never\ndegrades to an ENVIRONMENT marker"),
            "run_checks.py docstring must describe a malformed declaration as fail-closed, not a soft marker"
        );
        assert!(
            !runner.contains("Known residual") && !gauntlet.contains("residual:"),
            "run_checks.py and the review-gauntlet README must not disclose the closed residual"
        );
    }

    #[test]
    fn review_gauntlet_readme_states_retry_and_incomplete_contract() {
        let readmes = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents");
        let gauntlet = read_to_string(readmes.join("review-gauntlet/README.md")).unwrap();
        for edge in [
            "mcr & madv & msec & mpb --> rgate[\"retry_gate",
            "rgate --> gate[\"verdict_gate",
            "rgate -. \"_next: retry faulted lanes\" .-> build",
        ] {
            assert!(
                gauntlet.contains(edge),
                "review-gauntlet README mermaid must route the lane maps through the retry gate: {edge}"
            );
        }
        assert!(gauntlet.contains("the retry gate on equal-length paths"));
        for claim in [
            "when any lane or the pipeline was incomplete",
            "Whenever at least one lane (or the pipeline) could not complete",
            "GAUNTLET_REVIEW_INCOMPLETE: <lane>[, <lane>]",
            "`pipeline` pseudo-lane",
            "the named lanes never completed",
            // Caller guidance mirrors the sisyphus/architect contract: the
            // caller escalates; a fresh run is only the "retry" choice.
            "escalate to the user with three options — retry the review again (a **fresh**",
            "accept NEEDS-HUMAN and proceed without that lane",
            "never narrows selection",
            "up to 3 attempts",
            "MAX_RETRY_ELAPSED_SECS",
            "preserves the prior `lanes_summary`",
            "append to an existing `signals_error`",
            "structurally implies",
            "not as a DIVERGES finding",
            "never dropped",
            "| adversary | `ADVERSARIAL_REVIEW: CONFORMS \\| DIVERGES` | DIVERGES; own degraded-run header (pipeline fault, died criterion checks, or its verdict script's crash stub); missing sentinel |",
        ] {
            assert!(
                gauntlet.contains(claim),
                "review-gauntlet README must state the retry/incomplete contract: {claim}"
            );
        }
        assert!(
            gauntlet.contains("never dropped.\n\nWhenever at least one lane"),
            "review-gauntlet README must keep the incomplete-line contract in its own paragraph"
        );
        for stale in [
            "on fault-only BLOCKED",
            "there is nothing to fix in the code yet",
            "3h whole-gauntlet",
            "| DIVERGES; missing sentinel |",
        ] {
            assert!(
                !gauntlet.contains(stale),
                "review-gauntlet README must not carry the stale claim: {stale}"
            );
        }
    }

    // The spawner's own `max_agent_depth` is checked against the child's
    // absolute depth (root = 0), so every hop along a chain needs its own
    // limit >= child depth; +1 leaves one level of headroom.
    #[test]
    #[serial]
    fn bundled_review_suite_depth_limits_cover_documented_chain() {
        use crate::graph::GraphParser;

        fn bundled_agent_config(name: &str) -> AgentConfig {
            let dir = paths::agents_data_dir().join(name);
            let graph_path = dir.join("graph.yaml");
            if graph_path.exists() {
                let graph = GraphParser::new(&dir)
                    .load_from_file(&graph_path)
                    .unwrap_or_else(|e| panic!("graph.yaml for '{name}' failed to parse: {e}"));
                return AgentConfig::from_graph(name, &graph);
            }
            AgentConfig::load(&dir.join("config.yaml"))
                .unwrap_or_else(|e| panic!("config.yaml for '{name}' failed to load: {e}"))
        }

        let _guard = TestConfigDirGuard::new();
        Agent::install_builtin_agents(false).unwrap();

        for (name, expected) in [
            ("architect", 10),
            ("sisyphus", 5),
            ("review-gauntlet", 4),
            ("step-runner", 4),
            ("code-reviewer", 5),
            ("domain-reviewer", 6),
            ("architecture-reviewer", 4),
        ] {
            assert_eq!(
                bundled_agent_config(name).max_agent_depth,
                expected,
                "bundled '{name}' max_agent_depth drifted"
            );
        }

        // User decision: domain-reviewer's file-reviewer fan-out stays unwired
        // for now. Remove it from this list when `can_spawn_agents: true` is added.
        const DOCUMENTED_BUT_UNWIRED: &[&str] = &["domain-reviewer"];

        let chains: [&[&str]; 4] = [
            &[
                "architect",
                "sisyphus",
                "review-gauntlet",
                "code-reviewer",
                "domain-reviewer",
                "file-reviewer",
            ],
            &[
                "architect",
                "sisyphus",
                "step-runner",
                "code-reviewer",
                "domain-reviewer",
                "file-reviewer",
            ],
            &[
                "architect",
                "sisyphus",
                "review-gauntlet",
                "code-reviewer",
                "finding-verifier",
            ],
            &["architect", "sisyphus", "architecture-reviewer", "explore"],
        ];
        for chain in chains {
            for (i, pair) in chain.windows(2).enumerate() {
                let (spawner, child) = (pair[0], pair[1]);
                let cfg = bundled_agent_config(spawner);
                assert!(
                    cfg.max_agent_depth >= i + 2,
                    "{spawner} (depth {i}) needs max_agent_depth >= {} to spawn {child} at depth {} with one level of headroom",
                    i + 2,
                    i + 1
                );
                if !DOCUMENTED_BUT_UNWIRED.contains(&spawner) {
                    assert!(
                        cfg.can_spawn_agents,
                        "{spawner} must have can_spawn_agents to reach {child}"
                    );
                }
                if let Some(list) = &cfg.spawnable_agents {
                    assert!(
                        list.iter().any(|s| s == child),
                        "{spawner} spawnable_agents must include {child}, got {list:?}"
                    );
                }
            }
        }
    }

    #[test]
    #[serial]
    fn bundled_graph_agents_parse_and_validate() {
        use crate::graph::GraphParser;
        use crate::graph::validator::{GraphValidator, ValidationResult};
        use std::collections::BTreeMap;

        const UNREACHABLE: &str = "Node is unreachable from the start node via declared edges \
                                   (script `_next` routing is not analyzed)";

        fn warning_lines(result: &ValidationResult) -> Vec<String> {
            let mut lines: Vec<String> = result
                .warnings
                .iter()
                .map(|w| format!("{}: {}", w.node_id.as_deref().unwrap_or("-"), w.message))
                .collect();
            lines.sort();
            lines
        }

        fn unreachable(node_ids: &[&str]) -> Vec<String> {
            node_ids
                .iter()
                .map(|id| format!("{id}: {UNREACHABLE}"))
                .collect()
        }

        fn shadowed(names: &[&str]) -> Vec<String> {
            names
                .iter()
                .map(|name| {
                    format!(
                        "-: declared variable '{name}' is shadowed by an explicit \
                         `initial_state` key: the `initial_state` value wins and the \
                         variable's resolved value (spawn-provided/CLI/default) is never \
                         seeded into graph state; rename one of them, or drop the \
                         `initial_state` key to let the variable seed it (script nodes' \
                         `LLM_AGENT_VAR_<NAME>` env is unaffected)"
                    )
                })
                .collect()
        }

        let _guard = TestConfigDirGuard::new();

        Agent::install_builtin_agents(false).unwrap();
        Skill::install_builtin_skills(false).unwrap();

        let mut checked = Vec::new();
        let mut warnings_by_agent = BTreeMap::new();
        for entry in std::fs::read_dir(paths::agents_data_dir()).unwrap() {
            let dir = entry.unwrap().path();
            let graph_path = dir.join("graph.yaml");
            if !graph_path.exists() {
                continue;
            }
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            let graph = GraphParser::new(&dir)
                .load_from_file(&graph_path)
                .unwrap_or_else(|e| panic!("graph.yaml for '{name}' failed to parse: {e}"));
            let result = GraphValidator::new(&dir).validate(&graph);
            assert!(
                result.errors.is_empty(),
                "graph.yaml for '{name}' failed validation: {:#?}",
                result.errors
            );
            warnings_by_agent.insert(name.clone(), warning_lines(&result));
            checked.push(name);
        }
        checked.sort();
        for expected in ["coder", "librarian", "step-runner"] {
            assert!(
                checked.iter().any(|n| n == expected),
                "expected bundled graph agent '{expected}' to be checked; found {checked:?}"
            );
        }

        // Warning baseline for the shipped graphs: `_next`-routed unreachable
        // nodes, plus variable-shadowing on `coder` and `step-runner` (they
        // declare variables whose names are also `initial_state` keys). A new
        // rule that fires on a shipped asset (or a new bundled graph) must be
        // recorded here deliberately.
        let expected_warnings = BTreeMap::from([
            ("adversary".to_string(), Vec::new()),
            ("code-reviewer".to_string(), Vec::new()),
            (
                "coder".to_string(),
                [
                    shadowed(&["project_dir"]),
                    unreachable(&[
                        "analyze_request",
                        "end_rejected",
                        "end_success",
                        "fix_loop_gate",
                        "gate_approval",
                        "implement",
                        "route_complexity",
                        "route_review_result",
                        "self_review",
                        "verify_build",
                        "verify_tests",
                    ]),
                ]
                .concat(),
            ),
            ("deep-research".to_string(), unreachable(&["ask_topic"])),
            ("finding-verifier".to_string(), Vec::new()),
            ("librarian".to_string(), Vec::new()),
            ("review-gauntlet".to_string(), Vec::new()),
            (
                "step-runner".to_string(),
                [
                    shadowed(&["plans_dir", "project_dir"]),
                    unreachable(&[
                        "check_handoff",
                        "edge_case_sweep",
                        "end_blocked",
                        "end_rejected",
                        "end_success",
                        "fix_loop_gate",
                        "gate_blocked",
                        "gate_deviation",
                        "gate_user_review",
                        "get_revision",
                        "independent_review",
                        "revise_from_choice",
                        "route_review",
                        "route_sweep",
                        "verify_build",
                        "verify_format_lint",
                        "verify_tests",
                        "write_handoff",
                    ]),
                ]
                .concat(),
            ),
        ]);
        assert_eq!(
            warnings_by_agent, expected_warnings,
            "bundled graph validator warnings drifted from the recorded baseline"
        );

        // The shipped reference graph must load through the real parser and
        // validate cleanly too; its `scripts/synthesize.py` only has to exist.
        let base = _guard.path.join("example-base");
        create_dir_all(base.join("scripts")).unwrap();
        write(base.join("scripts").join("synthesize.py"), "").unwrap();
        let example = GraphParser::new(&base)
            .load_from_string(include_str!("../../graph.example.yaml"))
            .unwrap_or_else(|e| panic!("graph.example.yaml failed to parse: {e}"));
        let result = GraphValidator::new(&base).validate(&example);
        assert!(
            result.errors.is_empty(),
            "graph.example.yaml failed validation: {:#?}",
            result.errors
        );
        let example_warnings = warning_lines(&result);
        assert!(
            example_warnings.iter().all(|w| !w.contains("output_key")),
            "graph.example.yaml's map must declare a writer for its output_key: {example_warnings:#?}"
        );
        assert_eq!(
            example_warnings,
            unreachable(&[
                "aggregate_subjects",
                "deep_dive",
                "research_subject",
                "subjects_map",
            ]),
            "graph.example.yaml validator warnings drifted from the recorded baseline"
        );
    }

    // `state_updates::apply` merges only the keys an llm/agent node's
    // `output_schema.properties` declares, so a bundled graph that relied on
    // an undeclared model-emitted key reaching state would now break at
    // runtime. Every key a graph consumes must have a declared writer, and a
    // schema node's field list must not name a state key its schema omits.
    #[test]
    #[serial]
    fn bundled_graph_output_schemas_declare_every_key_relied_on() {
        use crate::graph::types::{ConcurrencyCap, Node};
        use crate::graph::{GraphParser, NodeType};
        use fancy_regex::Regex;
        use std::collections::BTreeMap;

        fn idents(re: &Regex, text: &str) -> BTreeSet<String> {
            re.captures_iter(text)
                .flatten()
                .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
                .collect()
        }

        fn state_updates_of(node: &Node) -> Option<&HashMap<String, String>> {
            match &node.node_type {
                NodeType::Llm(n) => n.state_updates.as_ref(),
                NodeType::Agent(n) => n.state_updates.as_ref(),
                NodeType::Rag(n) => n.state_updates.as_ref(),
                NodeType::Approval(n) => n.state_updates.as_ref(),
                NodeType::Input(n) => n.state_updates.as_ref(),
                NodeType::Script(n) => n.state_updates.as_ref(),
                NodeType::End(n) => n.state_updates.as_ref(),
                NodeType::Map(_) => None,
            }
        }

        fn templated_fields(node: &Node) -> Vec<&str> {
            let mut fields: Vec<&str> = match &node.node_type {
                NodeType::Llm(n) => {
                    let mut v = vec![n.prompt.as_str()];
                    v.extend(n.instructions.as_deref());
                    v
                }
                NodeType::Agent(n) => {
                    let mut v = vec![n.prompt.as_str()];
                    v.extend(n.inputs.iter().flat_map(|m| m.values().map(String::as_str)));
                    v
                }
                NodeType::Rag(n) => {
                    let mut v: Vec<&str> = n.documents.iter().map(String::as_str).collect();
                    v.extend(n.query.as_deref());
                    v.extend(n.extractor_prompt.as_deref());
                    v
                }
                NodeType::Approval(n) => vec![n.question.as_str()],
                NodeType::Input(n) => {
                    let mut v = vec![n.question.as_str()];
                    v.extend(n.default.as_deref());
                    v
                }
                NodeType::End(n) => vec![n.output.as_str()],
                NodeType::Map(n) => {
                    let mut v = vec![n.over.as_str()];
                    if let Some(ConcurrencyCap::Template(t)) = &n.max_concurrency {
                        v.push(t);
                    }
                    v
                }
                NodeType::Script(_) => Vec::new(),
            };
            fields.extend(
                state_updates_of(node)
                    .into_iter()
                    .flat_map(|m| m.values().map(String::as_str)),
            );
            fields
        }

        fn schema_node_text(node: &Node) -> Option<(Option<&serde_json::Value>, String)> {
            match &node.node_type {
                NodeType::Llm(n) => Some((
                    n.output_schema.as_ref(),
                    format!("{}\n{}", n.instructions.as_deref().unwrap_or(""), n.prompt),
                )),
                NodeType::Agent(n) => Some((n.output_schema.as_ref(), n.prompt.clone())),
                _ => None,
            }
        }

        let template_root = Regex::new(r"\{\{\s*([A-Za-z_][A-Za-z0-9_]*)").unwrap();
        let script_read =
            Regex::new(r#"state(?:\.get\(|\[)\s*["']([A-Za-z_][A-Za-z0-9_]*)["']"#).unwrap();
        let script_dict_key = Regex::new(r#"["']([A-Za-z_][A-Za-z0-9_]*)["']\s*:"#).unwrap();
        let script_subscript_assign =
            Regex::new(r#"\[["']([A-Za-z_][A-Za-z0-9_]*)["']\]\s*=[^=]"#).unwrap();
        let backticked_field = Regex::new(r"(?m)^\s*-\s*`([a-z][a-z0-9_]*)`").unwrap();

        // Seeded by the engine rather than any graph author: `initial_prompt`
        // (dispatch) and the per-node scoped `output`/`choice`/`input` bindings.
        let engine_seeded = ["initial_prompt", "output", "choice", "input"];

        let _guard = TestConfigDirGuard::new();
        Agent::install_builtin_agents(false).unwrap();

        let mut checked = Vec::new();
        for entry in std::fs::read_dir(paths::agents_data_dir()).unwrap() {
            let dir = entry.unwrap().path();
            let graph_path = dir.join("graph.yaml");
            if !graph_path.exists() {
                continue;
            }
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            let graph = GraphParser::new(&dir)
                .load_from_file(&graph_path)
                .unwrap_or_else(|e| panic!("graph.yaml for '{name}' failed to parse: {e}"));

            let mut scripts = String::new();
            if let Ok(entries) = std::fs::read_dir(dir.join("scripts")) {
                for script in entries.map(|e| e.unwrap().path()) {
                    if script.extension().is_some_and(|ext| ext == "py") {
                        scripts.push_str(&read_to_string(&script).unwrap());
                        scripts.push('\n');
                    }
                }
            }

            let mut writers: BTreeSet<String> =
                engine_seeded.iter().map(|k| k.to_string()).collect();
            writers.extend(graph.initial_state.keys().cloned());
            writers.extend(graph.variables.iter().map(|v| v.name.clone()));
            writers.extend(idents(&script_dict_key, &scripts));
            writers.extend(idents(&script_subscript_assign, &scripts));

            let mut consumed = idents(&script_read, &scripts);
            let mut declared: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
            for (id, node) in &graph.nodes {
                for field in templated_fields(node) {
                    consumed.extend(idents(&template_root, field));
                }
                writers.extend(
                    state_updates_of(node)
                        .into_iter()
                        .flat_map(|m| m.keys().cloned()),
                );
                if let NodeType::Map(m) = &node.node_type {
                    writers.extend([
                        m.as_name.clone(),
                        m.output_key.clone(),
                        m.collect_into.clone(),
                    ]);
                }
                if let Some((Some(schema), _)) = schema_node_text(node) {
                    let properties = schema
                        .get("properties")
                        .and_then(serde_json::Value::as_object)
                        .unwrap_or_else(|| {
                            panic!(
                                "{name}/{id}: output_schema has no object `properties`, so the \
                                 engine would merge nothing from its output: {schema}"
                            )
                        });
                    declared.insert(id, properties.keys().cloned().collect());
                }
            }
            let schema_keys: BTreeSet<&String> = declared.values().flatten().collect();

            let unwritten: Vec<&String> = consumed
                .iter()
                .filter(|k| !writers.contains(*k) && !schema_keys.contains(k))
                .collect();
            assert!(
                unwritten.is_empty(),
                "{name}: state keys consumed with no declared writer (initial_state, variable, \
                 state_updates target, map key, script-emitted key, or output_schema property): \
                 {unwritten:?}"
            );

            for (id, properties) in &declared {
                let (_, text) = schema_node_text(&graph.nodes[*id]).unwrap();
                let omitted: Vec<String> = idents(&backticked_field, &text)
                    .into_iter()
                    .filter(|k| {
                        (consumed.contains(k) || schema_keys.contains(&k))
                            && !properties.contains(k)
                            && !writers.contains(k)
                    })
                    .collect();
                assert!(
                    omitted.is_empty(),
                    "{name}/{id}: the node's field list names state keys its output_schema does \
                     not declare, so the engine would drop them when the model emits them: \
                     {omitted:?}"
                );
            }
            checked.push(name);
        }
        for expected in ["adversary", "review-gauntlet"] {
            assert!(
                checked.iter().any(|n| n == expected),
                "expected bundled graph agent '{expected}' to be checked; found {checked:?}"
            );
        }
    }

    // ---- adversary suite-script regression tests ----
    //
    // Fault paths of the adversary's verdict/gate scripts, exercised by
    // invoking `python3 <script>` with a synthetic GRAPH_STATE env — the same
    // contract the graph's script executor uses. Guarded on python3 being
    // available, mirroring src/graph/script.rs's local helper.

    fn cmd_available(name: &str) -> bool {
        which::which(name).is_ok()
    }

    fn run_adversary_script(script: &str, state: &serde_json::Value) -> serde_json::Value {
        run_adversary_script_env(script, state, &[])
    }

    // Scripts' load_state() prefers GRAPH_STATE_FILE over GRAPH_STATE, so an
    // inherited live state file (adversary verifying this repo) must not win.
    fn adversary_script_command(script: &str, state: &serde_json::Value) -> std::process::Command {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/adversary/scripts")
            .join(script);
        let mut cmd = std::process::Command::new("python3");
        cmd.arg(&path)
            .env("GRAPH_STATE", state.to_string())
            .env_remove("GRAPH_STATE_FILE");
        cmd
    }

    fn run_adversary_script_env(
        script: &str,
        state: &serde_json::Value,
        envs: &[(&str, &str)],
    ) -> serde_json::Value {
        let mut cmd = adversary_script_command(script, state);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("failed to invoke python3 {script}: {e}"));
        assert!(
            out.status.success(),
            "{script} exited nonzero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{script} stdout is not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    #[test]
    fn adversary_verdict_pipeline_fault_forces_diverges() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": ["PIPELINE-FAULT: parse failed — cannot review: LLM node failed: boom"],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""},
                {"id": "c2", "text": "does Y", "status": "UNMET",
                 "evidence": "", "complaint": "nothing in the diff does Y"}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": ""
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "a pipeline fault must force DIVERGES: {report}"
        );
        assert!(
            report.contains("1. PIPELINE-FAULT: parse failed — cannot review"),
            "the fault must be complaint #1: {report}"
        );
        assert!(
            report.contains("2. Acceptance criterion \"does Y\""),
            "per-criterion results must still be reported after the fault: {report}"
        );
        assert!(
            report.contains("Verification runs:"),
            "the report must carry the Verification runs section: {report}"
        );
    }

    #[test]
    fn adversary_verdict_invalid_declaration_fault_forces_diverges() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        const FAULT: &str = "PIPELINE-FAULT: verification_commands declaration invalid — \
                             expected a JSON array of strings, got dict";
        let state = json!({
            "pipeline_faults": [FAULT],
            "crit_verdicts": [
                {"id": "c1", "text": "tests pass", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": FAULT
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "an invalid declaration must force DIVERGES even with every criterion MET: {report}"
        );
        assert!(
            report.contains(&format!("1. {FAULT}")),
            "the declaration fault must be complaint #1: {report}"
        );
    }

    #[test]
    fn adversary_verdict_holistic_failure_is_a_pipeline_fault() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "holistic_failure": "LLM node failed: provider exploded",
            "exec_results": ""
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "a holistic-pass failure must force DIVERGES: {report}"
        );
        assert!(
            report.contains(
                "PIPELINE-FAULT: holistic pass failed — criterion verdicts stand \
                 but cross-cutting hunt did not run"
            ),
            "the holistic failure must become a PIPELINE-FAULT complaint: {report}"
        );
    }

    #[test]
    fn adversary_verdict_conforms_is_unaffected_without_faults() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // holistic_failure carries the rendered SUCCESS output here — it must
        // not be mistaken for a failure, and a recorded green run must render
        // as [PASS] in the Verification runs section.
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "holistic_failure": "{\"extra_complaints\": [], \"observations\": \"\"}",
            "exec_results": [{"cmd": "cargo test --all", "exit": 0, "duration_s": 42.0, "tail": "ok"}]
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: CONFORMS\nCriteria: 1/1 met"),
            "non-degraded all-MET semantics must be unchanged: {report}"
        );
        assert!(
            report.contains("Verification runs:") && report.contains("- [PASS] `cargo test --all`"),
            "a recorded green run must render as PASS evidence: {report}"
        );
    }

    #[test]
    fn adversary_run_checks_none_declared_marker() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_adversary_script("run_checks.py", &json!({}));
        let marker = out["exec_results"].as_str().unwrap();
        assert!(
            marker.contains("none declared"),
            "no verification_commands must degrade to a 'none declared' marker: {marker}"
        );
    }

    #[test]
    fn adversary_run_checks_records_green_and_failing_runs() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({"verification_commands": ["echo ok", "false"]});
        let out = run_adversary_script("run_checks.py", &state);
        let results = out["exec_results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            2,
            "one record per declared command: {results:?}"
        );
        assert_eq!(results[0]["cmd"], "echo ok");
        assert_eq!(results[0]["exit"], 0, "green command must record exit 0");
        assert!(
            results[0]["tail"].as_str().unwrap().contains("ok"),
            "the output tail must be recorded: {results:?}"
        );
        assert!(results[0]["duration_s"].is_number());
        assert_ne!(
            results[1]["exit"], 0,
            "failing command must record its nonzero exit: {results:?}"
        );
    }

    // Graph variables land in state as strings, so the gauntlet's `inputs:`
    // passthrough and `--agent-variable` both deliver a JSON-encoded list.
    #[test]
    fn adversary_run_checks_accepts_json_string_declaration() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({"verification_commands": "[\"echo ok\", \"false\"]"});
        let out = run_adversary_script("run_checks.py", &state);
        let results = out["exec_results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            2,
            "one record per declared command: {results:?}"
        );
        assert_eq!(results[0]["cmd"], "echo ok");
        assert_eq!(results[0]["exit"], 0, "green command must record exit 0");
        assert!(
            results[0]["tail"].as_str().unwrap().contains("ok"),
            "the output tail must be recorded: {results:?}"
        );
        assert!(results[0]["duration_s"].is_number());
        assert_ne!(
            results[1]["exit"], 0,
            "failing command must record its nonzero exit: {results:?}"
        );
    }

    #[test]
    fn adversary_run_checks_empty_declarations_mean_none_declared() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        for state in [
            json!({"verification_commands": "[]"}),
            json!({"verification_commands": []}),
            json!({"verification_commands": null}),
            json!({}),
        ] {
            let out = run_adversary_script("run_checks.py", &state);
            let marker = out["exec_results"].as_str().unwrap();
            assert!(
                marker.starts_with("none declared"),
                "{state} must degrade to the 'none declared' marker: {marker}"
            );
            assert!(
                marker.contains("verification_commands variable"),
                "the marker must point at the variable, not the prompt: {marker}"
            );
            assert!(
                out.get("pipeline_faults").is_none(),
                "an empty declaration is not a fault: {out}"
            );
        }
    }

    #[test]
    fn adversary_run_checks_invalid_declaration_fails_closed_without_executing() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        const FAULT: &str = "PIPELINE-FAULT: verification_commands declaration invalid";
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pwned = env::temp_dir().join(format!("coyote-adversary-run-checks-pwned-{unique}"));
        let touch = format!("touch {}", pwned.display());
        for declared in [
            json!("not json"),
            json!("{\"a\":1}"),
            json!([1, 2]),
            json!("   "),
            json!([touch, 42]),
            json!(format!("[\"{touch}\", 42]")),
        ] {
            let state = json!({
                "verification_commands": declared,
                "pipeline_faults": ["PIPELINE-FAULT: earlier"],
            });
            let out = run_adversary_script("run_checks.py", &state);
            let marker = out["exec_results"].as_str().unwrap();
            assert!(
                marker.starts_with(FAULT),
                "{declared} must be an explicit declaration fault, not the soft \
                 ENVIRONMENT marker: {marker}"
            );
            let faults = out["pipeline_faults"].as_array().unwrap();
            assert_eq!(faults.len(), 2, "{declared}: {faults:?}");
            assert_eq!(faults[0], "PIPELINE-FAULT: earlier");
            assert_eq!(faults[1], marker, "{declared}: {faults:?}");
        }
        assert!(
            !pwned.exists(),
            "an invalid declaration must execute nothing, even its string items"
        );

        // A non-list prior `pipeline_faults` is never iterated character by
        // character; only the new marker is recorded.
        let out = run_adversary_script(
            "run_checks.py",
            &json!({"verification_commands": "not json", "pipeline_faults": "oops"}),
        );
        let marker = out["exec_results"].as_str().unwrap();
        assert!(marker.starts_with(FAULT), "{marker}");
        assert_eq!(
            out["pipeline_faults"],
            json!([marker]),
            "a string-valued prior pipeline_faults must not be spread into characters"
        );
    }

    #[test]
    fn adversary_run_checks_ignores_commands_in_prompt_prose() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pwned = env::temp_dir().join(format!("coyote-adversary-run-checks-prose-{unique}"));
        let state = json!({
            "initial_prompt": format!("## VERIFICATION\n- touch {}\n", pwned.display()),
            "verification_commands": "[]",
        });
        let out = run_adversary_script("run_checks.py", &state);
        let marker = out["exec_results"].as_str().unwrap();
        assert!(
            marker.starts_with("none declared"),
            "commands in the prompt must not be picked up: {marker}"
        );
        assert!(
            !pwned.exists(),
            "nothing from the prompt prose may reach the runner's shell"
        );
    }

    #[test]
    fn adversary_run_checks_scrubs_graph_state_from_verification_commands() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let state_file =
            env::temp_dir().join(format!("coyote-adversary-run-checks-state-{unique}.json"));
        // python3 probe rather than `echo "${VAR:-UNSET}"`: run_checks.py uses
        // shell=True, which is cmd.exe on the Windows CI leg.
        let state = json!({
            "verification_commands": [
                "python3 -c \"import os; print('file=' + os.environ.get('GRAPH_STATE_FILE', 'UNSET') + ' inline=' + os.environ.get('GRAPH_STATE', 'UNSET'))\""
            ]
        });
        write(&state_file, state.to_string()).unwrap();

        // The script itself must load from GRAPH_STATE_FILE (the envs are
        // applied after the helper's env_remove), so an inline dummy state
        // proves the file-preferred path is the one exercised.
        let out = run_adversary_script_env(
            "run_checks.py",
            &json!({}),
            &[("GRAPH_STATE_FILE", state_file.to_str().unwrap())],
        );
        let _ = std::fs::remove_file(&state_file);

        let results = out["exec_results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            1,
            "state must come from the file: {results:?}"
        );
        assert_eq!(results[0]["exit"], 0, "{results:?}");
        assert!(
            results[0]["tail"]
                .as_str()
                .unwrap()
                .contains("file=UNSET inline=UNSET"),
            "verification command must not inherit GRAPH_STATE*: {results:?}"
        );
    }

    #[test]
    fn adversary_run_checks_runner_error_degrades_to_environment_marker() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A dead project_dir makes every spawn raise — the outer except must
        // still exit 0 (asserted inside the helper) and degrade the whole run
        // to the ENVIRONMENT marker instead of failing the node.
        let state = json!({
            "verification_commands": ["echo hi"],
            "project_dir": "/nonexistent/xyz"
        });
        let out = run_adversary_script("run_checks.py", &state);
        let marker = out["exec_results"].as_str().unwrap();
        assert!(
            marker.starts_with("ENVIRONMENT"),
            "a runner error must degrade to an ENVIRONMENT marker: {marker}"
        );
    }

    #[test]
    fn adversary_run_checks_deadline_skips_remaining_commands() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Deadline seam: a 1s total budget. The hung first command must be cut
        // off at min(per-command, remaining)≈1s and recorded as a timeout; the
        // second must be recorded as skipped — the in-script handling stays
        // authoritative instead of the NODE timeout killing the script from
        // outside (which would bypass the ENVIRONMENT degradation entirely).
        let state = json!({"verification_commands": [
            r#"python3 -c "import time; time.sleep(5)""#,
            "echo never"
        ]});
        let out = run_adversary_script_env(
            "run_checks.py",
            &state,
            &[("ADVERSARY_RUN_CHECKS_DEADLINE_SECS", "1")],
        );
        let results = out["exec_results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            2,
            "every declared command gets a record: {results:?}"
        );
        assert_eq!(
            results[0]["exit"], -1,
            "a command outliving its budget must record a timeout: {results:?}"
        );
        assert!(
            results[0]["tail"].as_str().unwrap().contains("TIMEOUT"),
            "the timeout must be named in the tail: {results:?}"
        );
        assert_eq!(
            results[1]["skipped"], "deadline",
            "commands past the deadline must be recorded as skipped: {results:?}"
        );
    }

    #[test]
    fn adversary_run_checks_tolerates_non_utf8_output() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({"verification_commands": [
            r#"python3 -c "import sys; sys.stdout.buffer.write(b'ok \xff\xfe bytes\n')""#
        ]});
        let out = run_adversary_script("run_checks.py", &state);
        let results = out["exec_results"].as_array().unwrap_or_else(|| {
            panic!("invalid UTF-8 output must not collapse the record to a marker: {out}")
        });
        assert_eq!(
            results[0]["exit"], 0,
            "the command itself succeeded: {results:?}"
        );
        let tail = results[0]["tail"].as_str().unwrap();
        assert!(
            tail.contains("ok") && tail.contains("bytes"),
            "decodable parts of the output must survive with replacement: {results:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn adversary_run_checks_timeout_kills_the_whole_process_group() {
        if !cmd_available("python3") || !cmd_available("sh") {
            eprintln!("skipping: python3 or sh not available");
            return;
        }
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = env::temp_dir().join(format!(
            "coyote-adversary-run-checks-pgroup-{}-{unique}",
            std::process::id()
        ));
        create_dir_all(&tmp).unwrap();
        let pid_file = tmp.join("grandchild.pid");

        // run_one() starts each command in its own session and SIGKILLs the
        // process group on timeout. A plain p.kill() would only take the
        // shell, leaving a backgrounded `sleep 30` holding the stdout pipe —
        // and, in the real graph, a leaked cargo/pytest tree.
        let cmd = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
        let state = json!({"verification_commands": [cmd]});
        let out = run_adversary_script_env(
            "run_checks.py",
            &state,
            &[("ADVERSARY_RUN_CHECKS_DEADLINE_SECS", "1")],
        );
        let results = out["exec_results"].as_array().unwrap();
        assert_eq!(
            results[0]["exit"], -1,
            "the hung shell must record a timeout: {results:?}"
        );
        assert!(
            results[0]["tail"].as_str().unwrap().contains("TIMEOUT"),
            "the timeout must be named in the tail: {results:?}"
        );

        let mut pid = String::new();
        for _ in 0..20 {
            pid = read_to_string(&pid_file)
                .unwrap_or_default()
                .trim()
                .to_string();
            if !pid.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = remove_dir_all(&tmp);
        assert!(
            !pid.is_empty(),
            "the shell must have recorded its grandchild pid at {}",
            pid_file.display()
        );

        let alive = |pid: &str| {
            std::process::Command::new("kill")
                .args(["-0", pid])
                .output()
                .unwrap()
                .status
                .success()
        };
        let mut polls = 0;
        let mut still_alive = alive(&pid);
        while still_alive && polls < 40 {
            std::thread::sleep(Duration::from_millis(50));
            polls += 1;
            still_alive = alive(&pid);
        }
        assert!(
            !still_alive,
            "grandchild {pid} must die with the process group, still alive after {polls} polls: {results:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn adversary_run_checks_timeout_drain_is_bounded_by_session_escaped_grandchild() {
        if !cmd_available("python3") || !cmd_available("sh") {
            eprintln!("skipping: python3 or sh not available");
            return;
        }
        // The grandchild starts its own session, so the SIGKILL of the
        // command's process group misses it — but it inherited the stdout
        // pipe, so an unbounded post-kill drain would block until it exits
        // (~40s). DRAIN_TIMEOUT_SECS (10) must cap that. The orphaned `sleep
        // 40` exits on its own.
        let cmd = "python3 -c \"import subprocess,time; \
                   subprocess.Popen(['sleep','40'], start_new_session=True); time.sleep(60)\"";
        let state = json!({"verification_commands": [cmd]});
        let started = Instant::now();
        let out = run_adversary_script_env(
            "run_checks.py",
            &state,
            &[("ADVERSARY_RUN_CHECKS_DEADLINE_SECS", "1")],
        );
        let elapsed = started.elapsed();
        let results = out["exec_results"].as_array().unwrap();
        assert_eq!(
            results[0]["exit"], -1,
            "the hung command must record a timeout: {results:?}"
        );
        assert!(
            elapsed < Duration::from_secs(30),
            "the post-kill drain must be bounded by DRAIN_TIMEOUT_SECS, took {elapsed:?}: {results:?}"
        );
    }

    /// TOTAL_DEADLINE_SECS in run_checks.py and the run_checks node's
    /// `timeout:` in graph.yaml live in different files; if the script's
    /// deadline ever creeps past the node timeout, the executor kills the
    /// script from outside and the in-script skipped/ENVIRONMENT degradation
    /// never gets to run.
    #[test]
    fn adversary_run_checks_deadline_stays_inside_the_node_timeout() {
        use crate::graph::NodeType;
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/adversary/scripts/run_checks.py");
        let source = read_to_string(&script).unwrap();
        let line = source
            .lines()
            .find(|l| l.starts_with("TOTAL_DEADLINE_SECS = "))
            .expect("run_checks.py must define TOTAL_DEADLINE_SECS");
        let deadline: u64 = line
            .rsplit_once(" or ")
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| panic!("TOTAL_DEADLINE_SECS default no longer parses: {line}"));

        let graph = load_bundled_graph("adversary");
        let node = graph
            .get_node("run_checks")
            .expect("adversary graph must have a run_checks node");
        let NodeType::Script(s) = &node.node_type else {
            panic!("run_checks must be a script node");
        };
        assert!(
            deadline < s.timeout,
            "run_checks.py TOTAL_DEADLINE_SECS ({deadline}) must stay below the run_checks node timeout ({}) in assets/agents/adversary/graph.yaml",
            s.timeout
        );
    }

    /// The adversary's graph-level timeout must leave room for run_checks to
    /// burn its whole node timeout AND for the rest of the pipeline to still
    /// finish — a graph timeout kills from outside, so no fallback fires and
    /// the DIVERGES sentinel is lost.
    #[test]
    fn adversary_graph_timeout_covers_run_checks_budget() {
        use crate::graph::NodeType;
        const OTHER_STAGES_ENVELOPE_SECS: u64 = 5400;
        let graph = load_bundled_graph("adversary");
        let node = graph
            .get_node("run_checks")
            .expect("adversary graph must have a run_checks node");
        let NodeType::Script(s) = &node.node_type else {
            panic!("run_checks must be a script node");
        };
        let graph_timeout = graph
            .settings
            .timeout
            .expect("adversary graph must set settings.timeout");
        assert!(
            graph_timeout >= s.timeout + OTHER_STAGES_ENVELOPE_SECS,
            "assets/agents/adversary/graph.yaml settings.timeout ({graph_timeout}) must cover the run_checks node timeout ({}) plus the {OTHER_STAGES_ENVELOPE_SECS}s envelope for the other stages",
            s.timeout
        );
    }

    /// Every gauntlet lane that runs a graph-backed agent must outlive that
    /// graph's own timeout so the child's own graph timeout, not the lane
    /// timeout, is the binding bound; the child's internal retry envelope is
    /// deliberately not derived here (follow-up). And because agent-node
    /// retries each get a fresh per-attempt budget (src/graph/agent.rs
    /// retry_transient/bounded_attempt), the gauntlet itself must outlive
    /// `max_attempts × timeout` for every lane, with headroom for the
    /// surrounding stages.
    #[test]
    fn gauntlet_lane_timeouts_cover_child_graphs_and_retries() {
        use crate::graph::NodeType;
        const OTHER_STAGES_MARGIN_SECS: u64 = 1200;
        let gauntlet = load_bundled_graph("review-gauntlet");
        let gauntlet_timeout = gauntlet
            .settings
            .timeout
            .expect("review-gauntlet must set settings.timeout");
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents");
        let gauntlet_yaml = read_to_string(assets.join("review-gauntlet/graph.yaml")).unwrap();
        assert!(
            gauntlet_yaml.contains("internal retry envelope")
                && gauntlet_yaml.contains("not derived here"),
            "review-gauntlet graph.yaml must keep the NOTE that the child's internal retry envelope is not derived from the lane timeout"
        );
        assert!(
            !gauntlet_yaml.contains("is what arrives"),
            "review-gauntlet graph.yaml must not claim the lane timeout guarantees the child's verdict arrives"
        );
        assert!(
            gauntlet_yaml.contains("AND a probe_context (local-run recipe) is present"),
            "review-gauntlet graph.yaml default_lanes description must state the probe_context guard"
        );
        let mut seen = 0;
        let mut graph_backed = 0;
        for (id, node) in &gauntlet.nodes {
            let NodeType::Agent(a) = &node.node_type else {
                continue;
            };
            seen += 1;
            let lane_timeout = a
                .timeout
                .unwrap_or_else(|| panic!("review-gauntlet lane {id} must set a timeout"));
            if assets.join(&a.agent).join("graph.yaml").exists() {
                graph_backed += 1;
                let child_timeout = load_bundled_graph(&a.agent)
                    .settings
                    .timeout
                    .unwrap_or_else(|| panic!("{} graph must set settings.timeout", a.agent));
                assert!(
                    lane_timeout > child_timeout,
                    "assets/agents/review-gauntlet/graph.yaml lane {id} timeout ({lane_timeout}) must exceed assets/agents/{}/graph.yaml settings.timeout ({child_timeout})",
                    a.agent
                );
            }
            let worst_case = u64::from(a.max_attempts) * lane_timeout;
            assert!(
                gauntlet_timeout >= worst_case + OTHER_STAGES_MARGIN_SECS,
                "review-gauntlet settings.timeout ({gauntlet_timeout}) must cover lane {id}'s max_attempts × timeout ({} × {lane_timeout} = {worst_case}) plus a {OTHER_STAGES_MARGIN_SECS}s margin for the other stages",
                a.max_attempts
            );
        }
        assert!(
            seen >= 4,
            "review-gauntlet must have at least 4 agent lanes, saw {seen}"
        );
        assert!(
            graph_backed >= 2,
            "at least the adversary and code-reviewer lanes must be graph-backed so the child-graph timeout check runs, saw {graph_backed}"
        );
    }

    #[test]
    fn adversary_pipeline_fault_parse_stage_attribution() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_adversary_script(
            "pipeline_fault.py",
            &json!({"parse_failure": "LLM node 'parse' failed: provider exploded"}),
        );
        let faults = out["pipeline_faults"].as_array().unwrap();
        let fault = faults[0].as_str().unwrap();
        assert!(
            fault.contains("PIPELINE-FAULT: parse failed — cannot review"),
            "an llm-node failure string must attribute to the parse stage: {fault}"
        );
        assert!(
            fault.contains("provider exploded"),
            "the failure detail must be carried into the fault: {fault}"
        );
    }

    #[test]
    fn adversary_pipeline_fault_facts_and_run_checks_stage_attribution() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A script node's fallback writes NOTHING into state (executor.rs
        // script arm), so attribution keys off which stage outputs are
        // present. facts stage: parse succeeded (schema JSON in
        // parse_failure) but diff_text is still the initial '' — diff
        // resolution itself died.
        let out = run_adversary_script(
            "pipeline_fault.py",
            &json!({"parse_failure": "{\"criteria\": []}", "diff_text": ""}),
        );
        let faults = out["pipeline_faults"].as_array().unwrap();
        assert!(
            faults[0]
                .as_str()
                .unwrap()
                .contains("PIPELINE-FAULT: diff resolution failed"),
            "empty diff_text must attribute to the facts stage: {faults:?}"
        );

        // run_checks stage: facts completed (diff_facts.py unconditionally
        // writes a nonempty diff_text) but the runner died at the node level
        // before recording exec_results.
        let out = run_adversary_script(
            "pipeline_fault.py",
            &json!({
                "parse_failure": "{\"criteria\": []}",
                "diff_text": "diff --git a/x b/x",
                "exec_results": ""
            }),
        );
        let faults = out["pipeline_faults"].as_array().unwrap();
        assert!(
            faults[0]
                .as_str()
                .unwrap()
                .contains("PIPELINE-FAULT: verification runner killed"),
            "nonempty diff_text must attribute to the run_checks stage: {faults:?}"
        );
    }

    #[test]
    fn adversary_check_criterion_prompt_cites_exec_results() {
        use crate::graph::{GraphParser, NodeType};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/adversary");
        let graph = GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .expect("adversary graph.yaml must parse");
        let node = graph
            .nodes
            .get("check_criterion")
            .expect("adversary graph must have a check_criterion node");
        let NodeType::Llm(llm) = &node.node_type else {
            panic!("check_criterion must be an llm node");
        };
        assert!(
            llm.prompt.contains("{{exec_results}}"),
            "check_criterion's prompt must cite the recorded verification runs: {}",
            llm.prompt
        );
    }

    #[test]
    fn adversary_verdict_renders_skipped_runs() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": [{"cmd": "cargo test --all", "skipped": "deadline"}]
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.contains("- [SKIPPED] `cargo test --all`"),
            "a skipped record must render as SKIPPED: {report}"
        );
        assert!(
            !report.contains("[FAIL] `cargo test --all`"),
            "a skipped record must not render as FAIL: {report}"
        );
    }

    #[test]
    fn adversary_verdict_green_runs_keep_conforms() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": [{"cmd": "cargo test", "exit": 0, "duration_s": 1.0, "tail": "ok"}]
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: CONFORMS"),
            "an all-green run record must not block CONFORMS: {report}"
        );
    }

    #[test]
    fn adversary_verdict_red_run_forces_diverges() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A criterion judged MET cannot outrank a recorded failing run of a
        // declared command — the verdict is fail-closed on exec_results.
        let mut state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": [{"cmd": "cargo test", "exit": 101, "duration_s": 1.0, "tail": "FAILED"}]
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "a recorded red run must force DIVERGES: {report}"
        );
        assert!(
            report.contains("1. Verification run `cargo test` — exit 101"),
            "the red run must be a numbered complaint naming the command: {report}"
        );
        assert!(
            report.contains("Criteria: 1/1 met, 0 partial, 0 unmet/diverged — 1 red verification run(s) (fail-closed)."),
            "a reds-only DIVERGES header must count the red runs: {report}"
        );

        state["exec_results"] = json!([{"cmd": "cargo clippy", "skipped": "deadline"}]);
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "a skipped (unproven) run must force DIVERGES: {report}"
        );
        assert!(
            report.contains("1. Verification run `cargo clippy` — never ran"),
            "the skipped run must be a numbered complaint naming the command: {report}"
        );
    }

    #[test]
    fn adversary_verdict_none_declared_marker_is_not_red() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": "none declared — the caller supplied no verification_commands; criteria relying on tests are unproven"
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: CONFORMS"),
            "the none-declared marker must not block CONFORMS: {report}"
        );
        assert!(
            !report.contains("Verification run `"),
            "the none-declared marker must not yield a run complaint: {report}"
        );
    }

    #[test]
    fn adversary_verdict_environment_marker_with_declared_commands_blocks() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Commands were declared and the runner died before running them:
        // the criteria are unproven, and an all-MET set of criterion
        // judgments must not turn that into CONFORMS.
        let state = json!({
            "pipeline_faults": [],
            "verification_commands": ["cargo test"],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": "ENVIRONMENT: verification runner error: x — the declared commands could not be executed"
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "an ENVIRONMENT marker with declared commands must block CONFORMS: {report}"
        );
        assert!(
            report.contains("1. Verification runner error"),
            "the runner error must be a numbered complaint: {report}"
        );
        assert!(
            report.contains("0 unmet/diverged — 1 red verification run(s) (fail-closed)."),
            "the header must count the runner error as a red run: {report}"
        );
    }

    #[test]
    fn adversary_verdict_environment_marker_without_declaration_is_not_red() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "verification_commands": "[]",
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": "ENVIRONMENT: verification runner error: x — the declared commands could not be executed"
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: CONFORMS"),
            "with nothing declared the ENVIRONMENT marker is doctrine-PARTIAL, not a red run: {report}"
        );
        assert!(
            !report.contains("Verification runner error"),
            "the ENVIRONMENT marker must not yield a run complaint: {report}"
        );
    }

    #[test]
    fn adversary_verdict_timeout_run_named_as_timeout() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": [{"cmd": "cargo test", "exit": -1, "duration_s": 900.0, "tail": "TIMEOUT after 900s"}]
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "a timed-out run must force DIVERGES: {report}"
        );
        assert!(
            report.contains("1. Verification run `cargo test` — TIMEOUT (exit -1)"),
            "the -1 sentinel must be named as a TIMEOUT, not a bare exit code: {report}"
        );
    }

    #[test]
    fn adversary_verdict_red_runs_number_after_faults_and_criteria() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": ["PIPELINE-FAULT: x"],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "UNMET",
                 "evidence": "", "complaint": "no impl found"}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": [{"cmd": "cargo test", "exit": 101, "duration_s": 1.0, "tail": "FAILED"}]
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        let fault = report
            .find("1. PIPELINE-FAULT")
            .expect("the pipeline fault must be complaint 1");
        let criterion = report
            .find("2. Acceptance criterion")
            .expect("the unmet criterion must be complaint 2");
        let red = report
            .find("3. Verification run")
            .expect("the red run must be complaint 3");
        assert!(
            fault < criterion && criterion < red,
            "complaints must be ordered fault, criterion, red run: {report}"
        );
    }

    #[test]
    fn adversary_verdict_no_criteria_still_lists_red_runs() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [],
            "extra_complaints": [],
            "observations": "",
            "exec_results": [{"cmd": "cargo test", "exit": 101, "duration_s": 1.0, "tail": "FAILED"}]
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "no criteria must fail closed: {report}"
        );
        assert!(
            report.contains("Criteria: none provided."),
            "the no-criteria header must be kept: {report}"
        );
        assert!(
            report.contains("2. Verification run `cargo test`"),
            "the red run must be numbered after the no-criteria complaint: {report}"
        );
    }

    #[test]
    fn adversary_verdict_environment_marker_with_string_declaration_blocks() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Production path: graph variables land in state as strings, so the
        // declaration is a JSON-encoded list, not a list.
        let mut state = json!({
            "pipeline_faults": [],
            "verification_commands": "[\"cargo test\"]",
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": "ENVIRONMENT: verification runner error: x — the declared commands could not be executed"
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "a JSON-string declaration must count as declared: {report}"
        );
        assert!(
            report.contains("1. Verification runner error"),
            "the runner error must be a numbered complaint: {report}"
        );

        state["verification_commands"] = json!("[\"  \"]");
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: CONFORMS"),
            "blank items are not a declaration, so the ENVIRONMENT marker is not a red run: {report}"
        );
    }

    #[test]
    fn adversary_verdict_died_and_red_header_has_single_period() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "UNMET",
                 "evidence": "PIPELINE-FAULT: criterion x", "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": [{"cmd": "cargo test", "exit": 101, "duration_s": 1.0, "tail": "FAILED"}]
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.contains("died (fail-closed) — 1 red verification run(s) (fail-closed)."),
            "both header notes must be joined with a single em dash: {report}"
        );
        assert!(
            !report.contains(".."),
            "the header must end with exactly one period: {report}"
        );
    }

    /// The fail-closed CONFORMS block on unrun declared commands is described
    /// in three places that must keep agreeing: the runner's ENVIRONMENT
    /// marker, the node descriptions, and check_criterion's instructions.
    #[test]
    fn adversary_fail_closed_wording_pins() {
        use crate::graph::NodeType;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/adversary");
        let runner = read_to_string(dir.join("scripts/run_checks.py")).unwrap();
        assert!(
            runner.contains("the verdict blocks CONFORMS when"),
            "run_checks.py's ENVIRONMENT marker must point at the verdict's fail-closed block"
        );
        let graph = load_bundled_graph("adversary");
        let run_checks = graph
            .get_node("run_checks")
            .expect("adversary graph must have a run_checks node");
        assert!(
            run_checks.description.contains("blocks CONFORMS"),
            "run_checks description must state the verdict blocks CONFORMS: {}",
            run_checks.description
        );
        let verdict = graph
            .get_node("verdict")
            .expect("adversary graph must have a verdict node");
        assert!(
            verdict
                .description
                .contains("zero recorded red verification runs"),
            "verdict description must list red runs among the CONFORMS requirements: {}",
            verdict.description
        );
        let check = graph
            .get_node("check_criterion")
            .expect("adversary graph must have a check_criterion node");
        assert!(
            check.description.contains(
                "the deterministic verdict blocks CONFORMS when declared commands did not run"
            ),
            "check_criterion description must hand the fail-closed block to the verdict: {}",
            check.description
        );
        let NodeType::Llm(llm) = &check.node_type else {
            panic!("check_criterion must be an llm node");
        };
        let instructions = llm
            .instructions
            .as_deref()
            .expect("check_criterion must have instructions");
        assert!(
            instructions.contains("deterministic verdict additionally blocks CONFORMS"),
            "check_criterion instructions must hand the fail-closed block to the verdict"
        );
    }

    #[test]
    fn adversary_crit_gate_contract_regression() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Valid MET verdict passes through with the id stamped from the criterion.
        let out = run_adversary_script(
            "crit_gate.py",
            &json!({
                "criterion": {"id": "c7", "text": "the spec"},
                "gate_attempts": 0,
                "crit_verdict": "{\"id\": \"wrong\", \"status\": \"MET\", \
                 \"evidence\": \"src/a.rs:1 + test tests/a.rs:5\", \"complaint\": \"\"}"
            }),
        );
        assert_eq!(out["crit_verdict"]["id"], "c7", "id must be stamped: {out}");
        assert_eq!(out["crit_verdict"]["status"], "MET");

        // Malformed output with retry budget left → reject back to check_criterion.
        let out = run_adversary_script(
            "crit_gate.py",
            &json!({
                "criterion": {"id": "c7", "text": "the spec"},
                "gate_attempts": 0,
                "crit_verdict": "not json at all"
            }),
        );
        assert_eq!(
            out["_next"], "check_criterion",
            "first failure must retry: {out}"
        );
        assert_eq!(out["gate_attempts"], 1);

        // Malformed output with the retry exhausted → recorded PARTIAL (unproven).
        let out = run_adversary_script(
            "crit_gate.py",
            &json!({
                "criterion": {"id": "c7", "text": "the spec"},
                "gate_attempts": 1,
                "crit_verdict": "still not json"
            }),
        );
        assert_eq!(out["crit_verdict"]["status"], "PARTIAL");
        assert!(
            out["crit_verdict"]["complaint"]
                .as_str()
                .unwrap()
                .contains("failed machine validation"),
            "exhausted retries must record the unproven complaint: {out}"
        );
    }

    #[test]
    fn adversary_criterion_fault_happy_path() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "criterion": {"id": "c7", "text": "the spec"},
            "crit_verdict": "LLM node failed: LLM call failed: llm node hit max_iterations (10) before LLM concluded"
        });
        let out = run_adversary_script("criterion_fault.py", &state);
        let v = &out["crit_verdict"];
        assert_eq!(v["status"], "UNMET", "a dead check must fail closed: {out}");
        assert_eq!(
            v["id"], "c7",
            "id must be stamped from the criterion: {out}"
        );
        assert_eq!(v["text"], "the spec");
        assert!(
            v["evidence"]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: criterion check failed — LLM node failed:"),
            "evidence must carry the fault marker and the captured chain: {out}"
        );
        assert!(
            v["complaint"].as_str().unwrap().contains("DIED"),
            "the complaint must say the check died rather than judged: {out}"
        );
    }

    #[test]
    fn adversary_criterion_fault_degraded_inputs() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // No captured failure text at all.
        let out = run_adversary_script(
            "criterion_fault.py",
            &json!({"criterion": {"id": "c1", "text": "x"}}),
        );
        assert_eq!(out["crit_verdict"]["status"], "UNMET");
        assert!(
            out["crit_verdict"]["evidence"]
                .as_str()
                .unwrap()
                .ends_with("no failure text captured"),
            "{out}"
        );

        // crit_verdict present but not an engine failure string.
        let out = run_adversary_script(
            "criterion_fault.py",
            &json!({"criterion": {"id": "c1", "text": "x"}, "crit_verdict": "{\"status\": \"MET\"}"}),
        );
        assert_eq!(out["crit_verdict"]["status"], "UNMET");
        assert!(
            out["crit_verdict"]["evidence"]
                .as_str()
                .unwrap()
                .ends_with("no failure text captured"),
            "non-failure text must not be echoed as a failure: {out}"
        );

        // Criterion missing → id falls back to "unknown".
        let out = run_adversary_script(
            "criterion_fault.py",
            &json!({"crit_verdict": "LLM node failed: boom"}),
        );
        assert_eq!(out["crit_verdict"]["id"], "unknown", "{out}");
        assert_eq!(out["crit_verdict"]["status"], "UNMET");

        // Overlong, multi-line failure text is flattened and truncated.
        let long = format!("LLM node failed: {}\nline two", "x".repeat(600));
        let out = run_adversary_script(
            "criterion_fault.py",
            &json!({"criterion": {"id": "c1", "text": "x"}, "crit_verdict": long}),
        );
        let evidence = out["crit_verdict"]["evidence"].as_str().unwrap();
        assert!(
            evidence.ends_with('…'),
            "must truncate with an ellipsis: {evidence}"
        );
        assert!(
            !evidence.contains('\n'),
            "must flatten newlines: {evidence}"
        );
        assert!(
            evidence.chars().count() < 600,
            "{}",
            evidence.chars().count()
        );
    }

    #[test]
    fn adversary_criterion_fault_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Unparseable state makes load_state raise before main runs; the
        // fault marker itself must still emit a schema-shaped UNMET verdict.
        let out = run_adversary_script_env(
            "criterion_fault.py",
            &json!({}),
            &[("GRAPH_STATE", "not json")],
        );
        let v = &out["crit_verdict"];
        assert_eq!(
            v["status"], "UNMET",
            "a crashed fault marker must fail closed: {out}"
        );
        assert_eq!(v["id"], "unknown");
        assert!(
            v["evidence"]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: criterion "),
            "the crash path must emit the exact prefix verdict.py's died() keys on: {out}"
        );
        assert!(
            v["complaint"]
                .as_str()
                .unwrap()
                .contains("fault-marker script error"),
            "the crash must be named: {out}"
        );
    }

    #[test]
    fn adversary_script_helper_ignores_inherited_graph_state_file() {
        let cmd = adversary_script_command("criterion_fault.py", &json!({}));
        let removed = cmd
            .get_envs()
            .any(|(k, v)| k == "GRAPH_STATE_FILE" && v.is_none());
        assert!(
            removed,
            "GRAPH_STATE_FILE must be explicitly removed so an inherited live state file cannot shadow GRAPH_STATE: {:?}",
            cmd.get_envs().collect::<Vec<_>>()
        );
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == "GRAPH_STATE" && v.is_some()),
            "GRAPH_STATE must still carry the synthetic state"
        );
    }

    #[test]
    fn adversary_verdict_renders_died_criterion_and_never_conforms() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""},
                {"id": "c2", "text": "does Y", "status": "UNMET",
                 "evidence": "PIPELINE-FAULT: criterion check failed — LLM node failed: boom",
                 "complaint": "criterion check DIED (pipeline fault) — LLM node failed: boom; the criterion was NOT verified and is treated as unmet (fail-closed)"}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": []
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "a died criterion must never conform: {report}"
        );
        assert!(
            report.contains("Criteria: 1/2 met, 0 partial, 1 unmet/diverged — degraded run: 1 criterion check(s) died (fail-closed)."),
            "the died criterion counts as unmet and the header flags the degraded run: {report}"
        );
        assert!(
            report.contains(
                "Acceptance criterion \"does Y\" — criterion check DIED (pipeline fault) — "
            ),
            "a died criterion must render as died rather than judged: {report}"
        );
        assert!(
            !report.contains("— Unmet —"),
            "a died criterion must not render as a judged Unmet: {report}"
        );
        assert_eq!(
            report
                .matches("criterion check DIED (pipeline fault)")
                .count(),
            1,
            "the died marker must not be doubled: {report}"
        );
    }

    #[test]
    fn adversary_verdict_met_with_fault_evidence_never_conforms() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""},
                {"id": "c2", "text": "does Y", "status": "MET",
                 "evidence": "PIPELINE-FAULT: criterion check failed — LLM node failed: boom",
                 "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": []
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "a fault-marked verdict must never conform, whatever status it carries: {report}"
        );
        assert!(
            report.contains("Criteria: 1/2 met, 0 partial, 1 unmet/diverged — degraded run: 1 criterion check(s) died (fail-closed)."),
            "the fault-marked verdict counts as unmet, not met, and the header flags the degraded run: {report}"
        );
        let died_line = report
            .lines()
            .find(|l| l.contains("criterion check DIED (pipeline fault)"))
            .unwrap_or_else(|| panic!("a fault-marked verdict must render as died: {report}"));
        assert!(
            !died_line.ends_with("— "),
            "an empty complaint must not leave a dangling em-dash: {died_line:?}"
        );
        assert!(
            died_line.ends_with("criterion check DIED (pipeline fault)"),
            "with no complaint the died marker stands alone: {died_line:?}"
        );
        let met_section = report
            .split("Met criteria (evidence):")
            .nth(1)
            .expect("the genuine MET criterion produces an evidence appendix");
        assert!(
            !met_section.contains("does Y"),
            "the fault-marked verdict must not be listed as met: {report}"
        );
    }

    #[test]
    fn adversary_verdict_quoted_marker_in_met_evidence_is_not_died() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Free-text evidence that merely quotes the marker (not the exact
        // prefix criterion_fault emits) is a genuine MET, not a died check.
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "renders the fault marker", "status": "MET",
                 "evidence": "PIPELINE-FAULT: is emitted by criterion_fault.py per verdict.py:70",
                 "complaint": ""}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": []
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: CONFORMS"),
            "evidence quoting the marker must not be reclassified as died: {report}"
        );
        assert!(
            !report.contains("DIED"),
            "no died rendering for a genuine MET: {report}"
        );
    }

    #[test]
    fn adversary_verdict_died_criterion_under_pipeline_fault_banner() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": ["PIPELINE-FAULT: parse failed — cannot review: LLM node failed: boom"],
            "crit_verdicts": [
                {"id": "c1", "text": "does X", "status": "MET",
                 "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""},
                {"id": "c2", "text": "does Y", "status": "UNMET",
                 "evidence": "PIPELINE-FAULT: criterion check failed — LLM node failed: boom",
                 "complaint": "criterion check DIED (pipeline fault) — LLM node failed: boom; the criterion was NOT verified and is treated as unmet (fail-closed)"}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": []
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "a pipeline fault plus a died criterion must never conform: {report}"
        );
        assert!(
            report.contains("Criteria: 1/2 met, 0 partial, 1 unmet/diverged — degraded run: pipeline fault recorded (fail-closed)."),
            "the pipeline-fault banner takes precedence in the header: {report}"
        );
        assert!(
            report
                .contains("1. PIPELINE-FAULT: parse failed — cannot review: LLM node failed: boom"),
            "the pipeline fault is complaint #1: {report}"
        );
        assert!(
            report.contains(
                "2. Acceptance criterion \"does Y\" — criterion check DIED (pipeline fault) — "
            ),
            "the died criterion is complaint #2: {report}"
        );
        let met_section = report
            .split("Met criteria (evidence):")
            .nth(1)
            .expect("the genuine MET criterion produces an evidence appendix");
        assert!(
            !met_section.contains("does Y"),
            "the died criterion must not be listed as met: {report}"
        );
    }

    #[test]
    fn adversary_verdict_prepends_died_marker_once_when_complaint_lacks_it() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "pipeline_faults": [],
            "crit_verdicts": [
                {"id": "c1", "text": "does Y", "status": "UNMET",
                 "evidence": "PIPELINE-FAULT: criterion check failed — LLM node failed: boom",
                 "complaint": "LLM node failed: boom"}
            ],
            "extra_complaints": [],
            "observations": "",
            "exec_results": []
        });
        let out = run_adversary_script("verdict.py", &state);
        let report = out["adv_report"].as_str().unwrap();
        assert!(
            report.contains(
                "1. Acceptance criterion \"does Y\" — criterion check DIED (pipeline fault) — LLM node failed: boom"
            ),
            "the died marker is prepended to a bare complaint: {report}"
        );
        assert_eq!(
            report
                .matches("criterion check DIED (pipeline fault)")
                .count(),
            1,
            "the died marker must appear exactly once: {report}"
        );
    }

    #[test]
    fn adversary_crit_gate_passes_fault_shaped_verdict_through() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let evidence = "PIPELINE-FAULT: criterion check failed — LLM node failed: boom";
        let fault = json!({
            "id": "c1", "text": "x", "status": "UNMET", "evidence": evidence,
            "complaint": "criterion check DIED (pipeline fault) — LLM node failed: boom"
        });
        let out = run_adversary_script(
            "crit_gate.py",
            &json!({
                "criterion": {"id": "c1", "text": "x"},
                "gate_attempts": 0,
                "crit_verdict": fault.to_string()
            }),
        );
        assert!(
            out.get("_next").is_none(),
            "a fault-shaped verdict must not retry: {out}"
        );
        assert_eq!(out["crit_verdict"]["id"], "c1", "{out}");
        assert_eq!(out["crit_verdict"]["text"], "x", "{out}");
        assert_eq!(out["crit_verdict"]["status"], "UNMET");
        assert_eq!(out["crit_verdict"]["evidence"], evidence, "{out}");
        assert_eq!(
            out["crit_verdict"]["complaint"], fault["complaint"],
            "{out}"
        );
    }

    #[test]
    fn adversary_check_criterion_fails_closed_per_criterion() {
        use crate::graph::{GraphParser, NodeType};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/adversary");
        let graph = GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .expect("adversary graph.yaml must parse");

        let node = graph
            .nodes
            .get("check_criterion")
            .expect("adversary graph must have a check_criterion node");
        let NodeType::Llm(llm) = &node.node_type else {
            panic!("check_criterion must be an llm node");
        };
        assert_eq!(
            llm.fallback.as_deref(),
            Some("criterion_fault"),
            "a dead criterion check must fall back instead of sinking the map"
        );
        assert_eq!(llm.max_iterations, 30);
        assert_eq!(llm.max_attempts, 2);
        assert!(
            llm.state_updates
                .as_ref()
                .is_some_and(|u| u.contains_key("crit_verdict")),
            "the failure text must land in crit_verdict for criterion_fault to read"
        );
        assert_eq!(node.next_target(), Some("crit_gate"));
        assert!(
            llm.instructions
                .as_deref()
                .is_some_and(|s| s.contains("bounded tool budget")),
            "check_criterion must be told its tool budget is bounded"
        );
        assert!(
            llm.instructions
                .as_deref()
                .is_some_and(|s| s.contains("Never begin `evidence`")),
            "check_criterion must be told the PIPELINE-FAULT evidence prefix is reserved"
        );
        assert_eq!(
            llm.tools.as_deref(),
            Some(&["fs_read", "fs_cat", "fs_grep", "ast_grep"].map(String::from)[..]),
            "check_criterion must have read-only tools only: no execute_command, so it can never run checks itself"
        );
        assert!(
            llm.instructions
                .as_deref()
                .is_some_and(|s| s.contains("judged ONLY against the recorded verification runs")),
            "check_criterion must be told execution criteria are judged from the recorded runs, never by running anything"
        );
        let instructions = llm.instructions.as_deref().unwrap_or_default();
        assert!(
            instructions.contains("declare via the `verification_commands` variable")
                && instructions.contains("never declared in the"),
            "check_criterion must point the caller at the declared variable, not prompt prose"
        );
        assert!(
            !instructions.contains("should declare via verification_commands (e.g."),
            "check_criterion must not carry the old prompt-declaration wording"
        );

        let fault = graph
            .nodes
            .get("criterion_fault")
            .expect("adversary graph must have a criterion_fault node");
        let NodeType::Script(script) = &fault.node_type else {
            panic!("criterion_fault must be a script node");
        };
        assert!(
            script.script.ends_with("criterion_fault.py"),
            "{}",
            script.script
        );
        assert_eq!(
            fault.next_target(),
            None,
            "criterion_fault must terminate the branch so the map collects it"
        );

        let holistic = graph
            .nodes
            .get("holistic")
            .expect("adversary graph must have a holistic node");
        let NodeType::Llm(holistic) = &holistic.node_type else {
            panic!("holistic must be an llm node");
        };
        assert_eq!(holistic.max_iterations, 20);
    }

    const BUNDLED_GRAPHS: [&str; 6] = [
        "adversary",
        "review-gauntlet",
        "code-reviewer",
        "finding-verifier",
        "deep-research",
        "step-runner",
    ];

    fn load_bundled_graph(name: &str) -> crate::graph::Graph {
        use crate::graph::GraphParser;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents")
            .join(name);
        GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .unwrap_or_else(|e| panic!("{name} graph.yaml must parse: {e}"))
    }

    fn node_fallback(node_type: &crate::graph::NodeType) -> Option<&str> {
        use crate::graph::NodeType;
        match node_type {
            NodeType::Llm(l) => l.fallback.as_deref(),
            NodeType::Agent(a) => a.fallback.as_deref(),
            NodeType::Script(s) => s.fallback.as_deref(),
            _ => None,
        }
    }

    /// `default_max_attempts()` is 1, so a `max_attempts:` line silently
    /// dropped from a graph.yaml would turn a retried agent lane into a
    /// one-shot without any validator complaint. Every bundled agent node
    /// retries exactly once, except step-runner's coder: rerunning a coder
    /// that died mid-edit against the mutated tree is unsafe.
    #[test]
    fn bundled_graph_agent_nodes_retry_exactly_once_except_step_runner_implement() {
        use crate::graph::NodeType;

        let mut seen = Vec::new();
        for name in BUNDLED_GRAPHS {
            let graph = load_bundled_graph(name);
            for (id, node) in &graph.nodes {
                let NodeType::Agent(agent) = &node.node_type else {
                    continue;
                };
                let label = format!("{name}/{id}");
                let expected = if label == "step-runner/implement" {
                    1
                } else {
                    2
                };
                assert_eq!(
                    agent.max_attempts, expected,
                    "{label} must have max_attempts {expected}"
                );
                seen.push(label);
            }
        }
        seen.sort();
        assert_eq!(
            seen,
            [
                "code-reviewer/review_domain",
                "code-reviewer/verify",
                "deep-research/synthesize",
                "review-gauntlet/run_adversary",
                "review-gauntlet/run_code_review",
                "review-gauntlet/run_probe",
                "review-gauntlet/run_security",
                "step-runner/implement",
                "step-runner/independent_review",
            ],
            "the sweep must cover every bundled agent node; update this list when one is added or removed"
        );
    }

    /// An llm/agent node without a fallback sinks its whole map or graph
    /// when the model dies after retries. Every such node in the bundled
    /// graphs currently declares one; any future exemption must be listed
    /// here by `<graph>/<node_id>` with the reason it is safe to sink.
    #[test]
    fn bundled_graph_llm_and_agent_nodes_all_declare_fallbacks() {
        use crate::graph::NodeType;

        let mut graphs = std::collections::BTreeMap::new();
        for name in BUNDLED_GRAPHS {
            let graph = load_bundled_graph(name);
            for (id, node) in &graph.nodes {
                if !matches!(node.node_type, NodeType::Llm(_) | NodeType::Agent(_)) {
                    continue;
                }
                let target = node_fallback(&node.node_type)
                    .unwrap_or_else(|| panic!("{name}/{id} must declare a fallback"));
                assert!(
                    graph.nodes.contains_key(target),
                    "{name}/{id} fallback target {target} does not exist"
                );
            }
            graphs.insert(name, graph);
        }

        let wiring = [
            ("review-gauntlet", "parse", "parse_fault"),
            ("review-gauntlet", "select_lanes", "default_lanes"),
            ("review-gauntlet", "run_code_review", "lane_fault"),
            ("review-gauntlet", "run_adversary", "lane_fault"),
            ("review-gauntlet", "run_security", "lane_fault"),
            ("review-gauntlet", "run_probe", "lane_fault"),
            ("code-reviewer", "parse", "parse_fault"),
            ("code-reviewer", "refine_groups", "cover_gate"),
            ("code-reviewer", "review_domain", "domain_fault"),
            ("code-reviewer", "aux_lanes", "aux_fault"),
            ("adversary", "parse", "pipeline_fault"),
            ("adversary", "facts", "pipeline_fault"),
            ("adversary", "run_checks", "pipeline_fault"),
            ("adversary", "holistic", "verdict"),
        ];
        for (name, id, target) in wiring {
            let graph = &graphs[name];
            let node = graph
                .nodes
                .get(id)
                .unwrap_or_else(|| panic!("{name} graph must have a {id} node"));
            assert_eq!(
                node_fallback(&node.node_type),
                Some(target),
                "{name}/{id} must fall back to {target}"
            );
            assert!(
                graph.nodes.contains_key(target),
                "{name}/{id} fallback target {target} does not exist"
            );
        }
    }

    // ---- review-gauntlet suite-script regression tests ----
    //
    // Degradation paths of the gauntlet's lane/builder/gate scripts,
    // exercised the same way as the adversary suite above: `python3 <script>`
    // with a synthetic GRAPH_STATE env.

    fn run_gauntlet_script(script: &str, state: &serde_json::Value) -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/review-gauntlet/scripts")
            .join(script);
        let out = std::process::Command::new("python3")
            .arg(&path)
            .env("GRAPH_STATE", state.to_string())
            .env_remove("GRAPH_STATE_FILE")
            .output()
            .unwrap_or_else(|e| panic!("failed to invoke python3 {script}: {e}"));
        assert!(
            out.status.success(),
            "{script} exited nonzero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{script} stdout is not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    #[test]
    fn gauntlet_verdict_gate_blocks_lane_fault_distinctly() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // One run exercising all three failure classes at once — a faulted
        // lane, an unselected lane, and a lane that returned no sentinel —
        // each must be reported distinctly.
        let state = json!({
            "code_review_results": ["...**Verdict: MERGE-READY**..."],
            "adversary_results": ["PIPELINE-FAULT: adversary lane failed after retries — Agent node failed: boom"],
            "security_results": [],
            "probe_results": ["some text with no sentinel"]
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("adversary: PIPELINE-FAULT — the lane failed after retries"),
            "the faulted lane must be named as a blocker: {report}"
        );
        assert!(
            report.contains("| adversary | BLOCKED | PIPELINE-FAULT (lane failed) |"),
            "the lane table must carry the fault detail: {report}"
        );
        assert!(
            report.contains("| security | SKIPPED | not selected |"),
            "an unselected lane must stay SKIPPED: {report}"
        );
        assert!(
            report.contains("| probe | BLOCKED | missing verdict sentinel |"),
            "a sentinel-less lane must stay a missing-sentinel failure: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_signals_error_fault_blocks() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "code_review_results": [],
            "adversary_results": [],
            "security_results": [],
            "probe_results": [],
            "signals_error": "PIPELINE-FAULT: lane builder crashed — boom; no lanes were run"
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains(
                "## Blockers\n- pipeline: PIPELINE-FAULT: lane builder crashed — boom; no lanes were run"
            ),
            "the pipeline fault must appear in the Blockers section: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_happy_path_regression() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "code_review_results": ["**Verdict: MERGE-READY**"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(
            out["gauntlet_verdict"], "PASS",
            "non-degraded semantics must be unchanged: {out}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_reds_block_despite_merge_ready() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "code_review_results": ["🔴 [correctness] broken invariant\n\n**Verdict: MERGE-READY**"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("code-review: 1 🔴 CRITICAL finding(s)"),
            "🔴 findings must block regardless of the verdict line: {report}"
        );
        assert!(
            report.contains("| code-review | BLOCKED | 1 🔴 finding(s) |"),
            "the lane table must carry the 🔴 count: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_needs_human_without_reds_passes_with_attention() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "code_review_results": ["🟡 [convention] minor nit\n\n**Verdict: NEEDS-HUMAN**"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(
            out["gauntlet_verdict"], "PASS",
            "NEEDS-HUMAN without 🔴 must pass: {out}"
        );
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("## Human attention required"),
            "the attention section must be surfaced: {report}"
        );
        assert!(
            report.contains("| code-review | GREEN (attention) | NEEDS-HUMAN, no 🔴 |"),
            "the lane table must record the attention status: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_probe_inconclusive_blocks_with_environment_note() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "code_review_results": ["**Verdict: MERGE-READY**"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": ["USAGE_PROBE: INCONCLUSIVE — could not boot the stack"]
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("the ENVIRONMENT could not be established"),
            "INCONCLUSIVE must carry the environment note: {report}"
        );
        assert!(
            report.contains("| probe | BLOCKED | INCONCLUSIVE (environment) |"),
            "the lane table must record the environment block: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A truthy non-list lane result makes lane_report index into an int,
        // raising inside main; the top-level guard must emit BLOCKED (exit 0
        // is asserted in the helper) — never a crash into a silent pass.
        let out = run_gauntlet_script("verdict_gate.py", &json!({"code_review_results": 42}));
        assert_eq!(out["gauntlet_verdict"], "BLOCKED");
        assert!(
            out["gauntlet_report"]
                .as_str()
                .unwrap()
                .contains("verdict gate error"),
            "the internal error must be named in the report: {out}"
        );
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: pipeline",
            "a crashed gate is an incomplete review, never a code finding: {out}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_quoted_fault_marker_not_misrouted() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A real review that merely QUOTES the marker mid-text (e.g. a code
        // review of the gauntlet itself) must flow to the normal verdict
        // rules: the 🔴 count blocks it, NOT the lane-fault rule.
        let state = json!({
            "code_review_results": ["🔴 [correctness] gate mishandles reports quoting 'PIPELINE-FAULT:' mid-text\n\n**Verdict: MERGE-READY**"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| code-review | BLOCKED | 1 🔴 finding(s) |"),
            "the quoting report must be routed to the 🔴 rule: {report}"
        );
        assert!(
            !report.contains("code-review: PIPELINE-FAULT — the lane failed"),
            "a quoting report must NOT be treated as a lane fault: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_quoted_red_marker_uses_summary_count() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A 🔴 quoted inside a Changes-table row is not a finding: the
        // critical count comes from render.py's summary line, not a raw
        // marker count over the whole report.
        let state = json!({
            "code_review_results": ["| `route_review.sh` | Fault guard before the 🔴 grep | — |\n\n**Verdict: NEEDS-HUMAN**\n\n*Reviewed 3 files, found 0 critical, 1 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "PASS", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| code-review | GREEN (attention) | NEEDS-HUMAN, no 🔴 |"),
            "the summary line's zero critical count must win over a quoted 🔴: {report}"
        );
        assert!(
            !report.contains("🔴 CRITICAL finding(s)"),
            "a quoted 🔴 must not produce a blocker: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_summary_count_blocks_naming_count() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "code_review_results": ["**Verdict: MERGE-READY**\n\n*Reviewed 3 files, found 2 critical, 0 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("code-review: 2 🔴 CRITICAL finding(s)"),
            "the summary line's critical count must block and be named: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_missing_summary_falls_back_to_raw_count() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // No summary line → the raw 🔴 count stays authoritative (fail closed).
        let state = json!({
            "code_review_results": ["🔴 [correctness] broken invariant\n\n**Verdict: MERGE-READY**"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| code-review | BLOCKED | 1 🔴 finding(s) |"),
            "without a summary line the raw 🔴 count must block: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_last_summary_line_wins() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // render.py emits its summary line AFTER every finding body, so a
        // full-shape look-alike quoted earlier in the report must lose to the
        // real (last) line.
        let state = json!({
            "code_review_results": ["#### quoted\n*Reviewed 1 files, found 9 critical, 0 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*\n\n**Verdict: MERGE-READY**\n\n---\n*Reviewed 3 files, found 0 critical, 0 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "PASS", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            !report.contains("🔴 CRITICAL finding(s)"),
            "the last summary line must win over an earlier look-alike: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_nested_code_review_fault_blocks() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // code-reviewer's verdict.py forces NEEDS-HUMAN on its own internal
        // faults; the nested graph completes with a clean-looking zero
        // critical count, so the gate must anchor on the verdict line's
        // degraded-run wording rather than trust the count.
        let state = json!({
            "code_review_results": ["# Code Review Summary\n\n**Verdict: NEEDS-HUMAN** — pipeline fault(s) recorded — degraded run; always-human trigger(s) fired\n\n## Walkthrough\n(no walkthrough provided)\n\n---\n*Reviewed 3 files, found 0 critical, 0 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(
            out["gauntlet_verdict"], "BLOCKED",
            "a degraded code-review lane must never pass: {out}"
        );
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains(
                "code-review: the lane reported an internal PIPELINE-FAULT (degraded review) — a degraded lane is never a pass"
            ),
            "the nested fault must be named as the blocker: {report}"
        );
        assert!(
            report.contains("| code-review | BLOCKED | PIPELINE-FAULT (degraded lane) |"),
            "the lane table must record the degraded status: {report}"
        );
        assert!(
            !report.contains("GREEN (attention)"),
            "a degraded lane must not be recorded as attention-only: {report}"
        );

        // A degraded lane that ALSO carries 🔴 findings must name both
        // blockers — neither may shadow the other.
        let state = json!({
            "code_review_results": ["# Code Review Summary\n\n**Verdict: NEEDS-HUMAN** — 2 🔴 CRITICAL finding(s); pipeline fault(s) recorded — degraded run\n\n---\n*Reviewed 3 files, found 2 critical, 0 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains(
                "code-review: the lane reported an internal PIPELINE-FAULT (degraded review) — a degraded lane is never a pass"
            ),
            "the fault blocker must survive alongside the reds: {report}"
        );
        assert!(
            report.contains("code-review: 2 🔴 CRITICAL finding(s) — fix before claiming done"),
            "the reds blocker must survive alongside the fault: {report}"
        );
        assert!(
            report.contains("| code-review | BLOCKED | PIPELINE-FAULT (degraded lane); 2 🔴 |"),
            "the lane table must record both: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_attempt_counts_on_faulted_rows() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let degraded = "# Code Review Summary\n\n**Verdict: NEEDS-HUMAN** — pipeline fault(s) recorded — degraded run; always-human trigger(s) fired\n\n## Walkthrough\n(no walkthrough provided)\n\n---\n*Reviewed 3 files, found 0 critical, 0 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*";
        let state = json!({
            "lane_attempts": {"adversary": 3, "code-review": 2},
            "code_review_results": [degraded],
            "adversary_results": [GAUNTLET_ADVERSARY_FAULT],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | BLOCKED | PIPELINE-FAULT (lane failed ×3) |"),
            "a re-run faulted lane must show its attempt count: {report}"
        );
        assert!(
            report.contains("| code-review | BLOCKED | PIPELINE-FAULT (degraded lane ×2) |"),
            "a re-run degraded lane must show its attempt count: {report}"
        );

        let state = json!({
            "lane_attempts": {"code-review": 2},
            "code_review_results": ["# Code Review Summary\n\n**Verdict: NEEDS-HUMAN** — 2 🔴 CRITICAL finding(s); pipeline fault(s) recorded — degraded run\n\n---\n*Reviewed 3 files, found 2 critical, 0 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*"],
            "adversary_results": [],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| code-review | BLOCKED | PIPELINE-FAULT (degraded lane ×2); 2 🔴 |"),
            "the attempt count must sit inside the degraded detail, before the reds: {report}"
        );

        let state = json!({
            "lane_attempts": {"adversary": 1},
            "code_review_results": [],
            "adversary_results": [GAUNTLET_ADVERSARY_FAULT],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | BLOCKED | PIPELINE-FAULT (lane failed) |"),
            "a single attempt must render without a suffix: {report}"
        );

        let state = json!({
            "lane_attempts": {"code-review": 1},
            "code_review_results": [degraded],
            "adversary_results": [],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| code-review | BLOCKED | PIPELINE-FAULT (degraded lane) |"),
            "a single-attempt degraded lane must render without a suffix: {report}"
        );

        let state = json!({
            "lane_attempts": {"code-review": 1},
            "code_review_results": ["# Code Review Summary\n\n**Verdict: NEEDS-HUMAN** — 2 🔴 CRITICAL finding(s); pipeline fault(s) recorded — degraded run\n\n---\n*Reviewed 3 files, found 2 critical, 0 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*"],
            "adversary_results": [],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| code-review | BLOCKED | PIPELINE-FAULT (degraded lane); 2 🔴 |"),
            "a single-attempt degraded lane with reds must render without a suffix: {report}"
        );

        let state = json!({
            "lane_attempts": {"adversary": 2},
            "code_review_results": [],
            "adversary_results": ["ADVERSARIAL_REVIEW: DIVERGES"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | BLOCKED | DIVERGES |"),
            "a real verdict must render its own detail: {report}"
        );
        assert!(
            !report.contains("×2"),
            "the attempt suffix must stay on PIPELINE-FAULT rows only: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_incomplete_line_names_exhausted_lanes() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "review_incomplete": ["probe", "adversary"],
            "lane_attempts": {"adversary": 3, "probe": 3},
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": [GAUNTLET_ADVERSARY_FAULT],
            "security_results": [],
            "probe_results": ["PIPELINE-FAULT: probe lane failed after retries — Agent node failed: boom"],
            "signals_error": "git diff unavailable"
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: adversary, probe",
            "the machine line must name every exhausted lane, sorted: {out}"
        );
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains(
                "## Review incomplete\n- adversary: no completed review after 3 attempt(s)\n- probe: no completed review after 3 attempt(s)\n\n## Full lane reports"
            ),
            "the section must list the exhausted lanes sorted, with attempt counts, directly above the full lane reports: {report}"
        );
        assert!(
            report.find("> git diff unavailable").unwrap()
                < report.find("## Review incomplete").unwrap(),
            "the section must follow the signals_error blockquote: {report}"
        );

        // No lane_attempts at all (retry_gate never ran, or crashed before
        // recording): the section still names the lane, without a count.
        let state = json!({
            "review_incomplete": ["probe"],
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": ["PIPELINE-FAULT: probe lane failed after retries — Agent node failed: boom"]
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: probe",
            "{out}"
        );
        assert!(
            out["gauntlet_report"].as_str().unwrap().contains(
                "## Review incomplete\n- probe: no completed review\n\n## Full lane reports"
            ),
            "a lane with no recorded attempts must render without a count: {out}"
        );

        // The incomplete line must coexist with real findings, never
        // suppress them.
        let state = json!({
            "review_incomplete": ["adversary"],
            "lane_attempts": {"adversary": 3},
            "code_review_results": ["# Code Review Summary\n\n**Verdict: NEEDS-HUMAN** — 2 🔴 CRITICAL finding(s)\n\n---\n*Reviewed 3 files, found 2 critical, 0 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*"],
            "adversary_results": [GAUNTLET_ADVERSARY_FAULT],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: adversary",
            "{out}"
        );
        assert!(
            out["gauntlet_report"]
                .as_str()
                .unwrap()
                .contains("code-review: 2 🔴 CRITICAL finding(s) — fix before claiming done"),
            "a real finding must still be named as a blocker alongside the incomplete line: {out}"
        );

        let state = json!({
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "PASS", "{out}");
        assert_eq!(
            out["gauntlet_incomplete_line"], "",
            "a complete review must emit an empty line, not a missing key: {out}"
        );
        assert!(
            !out["gauntlet_report"]
                .as_str()
                .unwrap()
                .contains("## Review incomplete"),
            "a complete review must not render the section: {out}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_pipeline_incomplete_alongside_real_lanes() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // default_lanes' own crash guard fired (the ordinary degraded fallback
        // emits no PIPELINE-FAULT and is not incomplete): real lanes ran to a
        // verdict, but the selection stage faulted, so the review is incomplete
        // at the pipeline level even though no lane row is faulted.
        let fault = run_gauntlet_script(
            "default_lanes.py",
            &json!({"forced_lanes": 42, "touches_auth": true}),
        )["signals_error"]
            .clone();
        let fault = fault
            .as_str()
            .expect("default_lanes crash guard must fire on a non-list forced_lanes");
        assert!(
            fault.starts_with("PIPELINE-FAULT: lane-selection fallback crashed"),
            "{fault}"
        );
        let state = json!({
            "signals_error": fault,
            "code_review_results": ["**Verdict: MERGE-READY**"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": ["SECURITY_REVIEW: PASS"],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: pipeline",
            "a pipeline-level fault must name the pipeline pseudo-lane: {out}"
        );
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| code-review | GREEN | MERGE-READY |"),
            "real lane verdicts must still render: {report}"
        );
        assert!(
            report.contains(
                "## Review incomplete\n- pipeline: PIPELINE-FAULT: lane-selection fallback crashed"
            ),
            "the section must carry the pipeline fault text: {report}"
        );

        let state = json!({
            "signals_error": fault,
            "review_incomplete": ["adversary"],
            "lane_attempts": {"adversary": 3},
            "code_review_results": ["**Verdict: MERGE-READY**"],
            "adversary_results": [GAUNTLET_ADVERSARY_FAULT],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: adversary, pipeline",
            "exhausted lanes and the pipeline pseudo-lane must sort together: {out}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_incomplete_lane_forces_blocked() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // retry_gate restores an exhausted lane's LAST report, which can be
        // an earlier clean verdict; the incomplete lane must block anyway.
        let state = json!({
            "review_incomplete": ["adversary"],
            "lane_attempts": {"adversary": 3},
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: adversary",
            "{out}"
        );
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | BLOCKED | review incomplete after 3 attempt(s) |"),
            "an incomplete lane must render as BLOCKED with its attempt count: {report}"
        );
        assert!(
            report.contains(
                "adversary: review incomplete after 3 attempt(s) — the lane never produced a usable verdict"
            ),
            "an incomplete lane must be named as a blocker: {report}"
        );
        assert!(
            !report.contains("| adversary | GREEN | CONFORMS |"),
            "a restored clean verdict must not render as GREEN for an incomplete lane: {report}"
        );
        assert!(
            !(out["gauntlet_verdict"] == "PASS" && out["gauntlet_incomplete_line"] != ""),
            "PASS must never co-occur with an incomplete line: {out}"
        );

        let mut state = state;
        state.as_object_mut().unwrap().remove("lane_attempts");
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | BLOCKED | review incomplete |"),
            "no recorded attempts must render without a count: {report}"
        );
        assert!(
            report.contains(
                "adversary: review incomplete — the lane never produced a usable verdict"
            ),
            "{report}"
        );
        assert!(
            !(out["gauntlet_verdict"] == "PASS" && out["gauntlet_incomplete_line"] != ""),
            "PASS must never co-occur with an incomplete line: {out}"
        );

        state.as_object_mut().unwrap().remove("review_incomplete");
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "PASS", "{out}");
        assert_eq!(out["gauntlet_incomplete_line"], "", "{out}");
        assert!(
            out["gauntlet_report"]
                .as_str()
                .unwrap()
                .contains("| adversary | GREEN | CONFORMS |"),
            "a complete clean lane must still render GREEN: {out}"
        );
        assert!(
            !(out["gauntlet_verdict"] == "PASS" && out["gauntlet_incomplete_line"] != ""),
            "PASS must never co-occur with an incomplete line: {out}"
        );

        // A bare `pipeline` entry with no fault text must still block and
        // must never render Python's None into the report.
        let state = json!({
            "review_incomplete": ["pipeline"],
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: pipeline",
            "{out}"
        );
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("pipeline: review incomplete"),
            "a pipeline entry without fault text must still be named as a blocker: {report}"
        );
        assert!(
            !report.contains("pipeline: None"),
            "missing fault text must not render as None: {report}"
        );

        // An incomplete lane whose map collected nothing must not fall back
        // to the SKIPPED row.
        let state = json!({
            "review_incomplete": ["security"],
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| security | BLOCKED | review incomplete |"),
            "an incomplete lane with no report must render BLOCKED: {report}"
        );
        assert!(
            !report.contains("| security | SKIPPED | not selected |"),
            "an incomplete lane must never read as SKIPPED: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_adversary_degraded_header_is_fault_not_finding() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "lane_attempts": {"adversary": 2},
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": [GAUNTLET_ADVERSARY_DEGRADED],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | BLOCKED | PIPELINE-FAULT (degraded lane ×2) |"),
            "the adversary's own degraded-run header is a pipeline fault with its attempt count: {report}"
        );
        assert!(
            report.contains(
                "adversary: PIPELINE-FAULT — degraded run, pipeline fault recorded inside the adversary"
            ),
            "{report}"
        );
        assert!(
            !report.contains("adversary: DIVERGES — the implementation does not conform"),
            "a degraded run must not be reported as a DIVERGES finding: {report}"
        );

        let mut state = state;
        state.as_object_mut().unwrap().remove("lane_attempts");
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert!(
            out["gauntlet_report"]
                .as_str()
                .unwrap()
                .contains("| adversary | BLOCKED | PIPELINE-FAULT (degraded lane) |"),
            "a first-pass degraded run renders without a count: {out}"
        );

        // The predicate is anchored to the line under the sentinel: a clean
        // report that quotes the wording later stays a real verdict.
        state["adversary_results"] = json!([GAUNTLET_ADVERSARY_QUOTED_DEGRADED]);
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "PASS", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | GREEN | CONFORMS |"),
            "a quoted degraded-run phrase must not trip the fault predicate: {report}"
        );
        assert!(!report.contains("adversary: PIPELINE-FAULT"), "{report}");

        // Every degraded header the adversary's verdict.py can emit is a
        // pipeline fault, not a DIVERGES finding.
        for degraded in [
            GAUNTLET_ADVERSARY_DEGRADED_COUNTED,
            GAUNTLET_ADVERSARY_DEGRADED_DIED,
            GAUNTLET_ADVERSARY_CRASH_STUB,
        ] {
            state["adversary_results"] = json!([degraded]);
            let out = run_gauntlet_script("verdict_gate.py", &state);
            assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{degraded:?}: {out}");
            let report = out["gauntlet_report"].as_str().unwrap();
            assert!(
                report.contains("| adversary | BLOCKED | PIPELINE-FAULT (degraded lane) |"),
                "{degraded:?} must render as a degraded lane: {report}"
            );
            assert!(
                !report.contains("adversary: DIVERGES — the implementation does not conform"),
                "{degraded:?} must not be reported as a DIVERGES finding: {report}"
            );
        }

        // A plain DIVERGES header is a real finding.
        state["adversary_results"] = json!([GAUNTLET_ADVERSARY_DIVERGES]);
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | BLOCKED | DIVERGES |"),
            "a real DIVERGES must stay a finding: {report}"
        );
        assert!(!report.contains("adversary: PIPELINE-FAULT"), "{report}");
    }

    #[test]
    fn gauntlet_verdict_gate_malformed_attempts_keeps_lane_rows() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Malformed bookkeeping must not crash the gate into the error stub
        // that loses every lane row: a non-dict lane_attempts only affects
        // counts, while a non-list review_incomplete is itself a pipeline
        // fault — a bare string is never adopted as a lane name.
        let state = json!({
            "lane_attempts": 42,
            "review_incomplete": "adversary",
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": [GAUNTLET_ADVERSARY_FAULT],
            "security_results": [],
            "probe_results": [],
            "signals_error": "PIPELINE-FAULT: lane-selection fallback crashed — boom"
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            !report.contains("verdict gate error"),
            "malformed bookkeeping must not reach the crash stub: {report}"
        );
        assert!(
            report.contains("| code-review | GREEN | MERGE-READY |"),
            "{report}"
        );
        assert!(
            report.contains("| adversary | BLOCKED | PIPELINE-FAULT (lane failed) |"),
            "unreadable attempts must render without a count: {report}"
        );
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: pipeline",
            "a string-valued review_incomplete is a pipeline fault, not a lane name; the pipeline is named: {out}"
        );

        // Without any upstream fault text, the malformed bookkeeping alone
        // must block as a pipeline fault.
        let state = json!({
            "lane_attempts": 42,
            "review_incomplete": "adversary",
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: pipeline",
            "{out}"
        );
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("pipeline: PIPELINE-FAULT — malformed review_incomplete bookkeeping"),
            "malformed bookkeeping must be named as a pipeline fault: {report}"
        );
        assert!(
            report.contains("| code-review | GREEN | MERGE-READY |"),
            "{report}"
        );
        assert!(!report.contains("verdict gate error"), "{report}");

        // A list with a non-str entry keeps its str lanes and still records
        // the fault.
        let state = json!({
            "review_incomplete": ["adversary", 7],
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: adversary, pipeline",
            "{out}"
        );
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | BLOCKED | review incomplete |"),
            "the str lane entries must still force their rows: {report}"
        );
        assert!(
            report.contains("pipeline: PIPELINE-FAULT — malformed review_incomplete bookkeeping"),
            "{report}"
        );

        // Per-lane values that are not ints count as zero: the row renders
        // without a ×N suffix and the incomplete line without "after N".
        let state = json!({
            "lane_attempts": {"adversary": "three", "code-review": [1]},
            "review_incomplete": ["adversary"],
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": [GAUNTLET_ADVERSARY_FAULT],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(!report.contains("verdict gate error"), "{report}");
        assert!(
            report.contains("| adversary | BLOCKED | PIPELINE-FAULT (lane failed) |"),
            "a non-int per-lane count must render without a count: {report}"
        );
        assert!(!report.contains('×'), "{report}");
        assert!(
            report.contains("## Review incomplete\n- adversary: no completed review\n"),
            "a non-int per-lane count must not render an attempt count: {report}"
        );

        let state = json!({
            "lane_attempts": {"security": "x"},
            "review_incomplete": ["security"],
            "code_review_results": [GAUNTLET_MERGE_READY],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(!report.contains("verdict gate error"), "{report}");
        assert!(
            report.contains("| security | BLOCKED | review incomplete |"),
            "{report}"
        );
        assert!(!report.contains("review incomplete after"), "{report}");
    }

    #[test]
    fn gauntlet_verdict_gate_non_string_lane_result_blocks() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // The map collected an object instead of the lane's rendered text;
        // it cannot carry a sentinel and must never read as a pass.
        let state = json!({
            "code_review_results": [{"verdict": "MERGE-READY"}],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("code-review: malformed lane result (non-string item) — never a pass"),
            "{report}"
        );
        assert!(
            report.contains("| code-review | BLOCKED | malformed lane result (non-string) |"),
            "{report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_non_fault_needs_human_reason_passes() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // The fault anchor must not over-match: a rendered NEEDS-HUMAN reason
        // made of ordinary findings still passes with attention.
        let state = json!({
            "code_review_results": ["# Code Review Summary\n\n**Verdict: NEEDS-HUMAN** — 1 🟡 [correctness] finding(s) outside the deferred section\n\n---\n*Reviewed 2 files, found 0 critical, 1 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*"],
            "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
            "security_results": [],
            "probe_results": []
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "PASS", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| code-review | GREEN (attention) | NEEDS-HUMAN, no 🔴 |"),
            "a non-fault NEEDS-HUMAN must surface as attention: {report}"
        );
        assert!(
            !report.contains("PIPELINE-FAULT"),
            "no fault may be inferred from an ordinary reason: {report}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_parses_real_render_output() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Pins the render.py ↔ verdict_gate.py contract end to end: the
        // gate's summary-line regex must match what render.py actually emits,
        // and a 🔴 quoted inside a finding body must not count.
        let finding = json!({
            "id": "F1",
            "file": "src/lib.rs",
            "icon": "🟡",
            "section": "blocking",
            "marker": "correctness",
            "title": "Fault guard removed",
            "block": "#### 🟡 [correctness] Fault guard removed\nThe guard before the 🔴 grep was dropped; a quoted marker now leaks through."
        });
        let rendered = run_code_reviewer_script(
            "render.py",
            &json!({
                "changed_files": ["src/lib.rs"],
                "resolved_rigor": "production",
                "walkthrough": "Removes a guard.",
                "changes_rows": [{"file": "src/lib.rs", "desc": "guard removal"}],
                "verdict_out": {
                    "verdict": "NEEDS-HUMAN",
                    "reason": "1 🟡 [correctness] finding(s) outside the deferred section; always-human trigger(s) fired",
                    "counts": {"🔴": 0, "🟡": 1, "🟢": 0, "💡": 0},
                    "deferred_count": 0,
                    "dropped_count": 0,
                    "dropped_titles": [],
                    "attention": ["x"],
                    "findings_final": [finding]
                }
            }),
        );
        let report = rendered["final_report"].as_str().unwrap();
        assert!(
            report.contains("found 0 critical, 1 warnings"),
            "render.py must emit the summary line: {report}"
        );
        let out = run_gauntlet_script(
            "verdict_gate.py",
            &json!({
                "code_review_results": [report],
                "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
                "security_results": [],
                "probe_results": []
            }),
        );
        assert_eq!(out["gauntlet_verdict"], "PASS", "{out}");
        let gauntlet = out["gauntlet_report"].as_str().unwrap();
        assert!(
            !gauntlet.contains("🔴 CRITICAL finding(s)"),
            "the rendered summary line must win over the quoted 🔴: {gauntlet}"
        );
        assert!(
            gauntlet.contains("| code-review | GREEN (attention) | NEEDS-HUMAN, no 🔴 |"),
            "the gate must parse render.py's real output: {gauntlet}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_blocks_real_render_of_degraded_code_review() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Pins the verdict.py → render.py → verdict_gate.py fault contract end
        // to end: the degraded-run reason wording render.py emits on the
        // verdict line must be what the gate's fault regex anchors on.
        let rendered = run_code_reviewer_script(
            "render.py",
            &json!({
                "changed_files": ["a.rs"],
                "verdict_out": {
                    "verdict": "NEEDS-HUMAN",
                    "reason": "pipeline fault(s) recorded — degraded run",
                    "counts": {"🔴": 0, "🟡": 0, "🟢": 0, "💡": 0},
                    "attention": ["PIPELINE-FAULT: x"],
                    "findings_final": []
                }
            }),
        );
        let report = rendered["final_report"].as_str().unwrap();
        let out = run_gauntlet_script(
            "verdict_gate.py",
            &json!({
                "code_review_results": [report],
                "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
                "security_results": [],
                "probe_results": []
            }),
        );
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        assert!(
            out["gauntlet_report"]
                .as_str()
                .unwrap()
                .contains("internal PIPELINE-FAULT (degraded review)"),
            "a rendered degraded run must block as a fault, not pass on 🔴 = 0: {out}"
        );
    }

    #[test]
    fn gauntlet_verdict_gate_blocks_real_render_crash_report() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A non-dict verdict_out makes render.py's main() raise; its crash
        // guard emits a bare verdict line that the gate must still block on.
        let rendered = run_code_reviewer_script(
            "render.py",
            &json!({"changed_files": ["a.rs"], "verdict_out": "not a dict"}),
        );
        let report = rendered["final_report"].as_str().unwrap();
        assert!(
            report.starts_with(
                "# Code Review Summary\n\n**Verdict: NEEDS-HUMAN** — report rendering error:"
            ),
            "the crash guard must emit the anchored verdict line: {report}"
        );
        let out = run_gauntlet_script(
            "verdict_gate.py",
            &json!({
                "code_review_results": [report],
                "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
                "security_results": [],
                "probe_results": []
            }),
        );
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        assert!(
            out["gauntlet_report"]
                .as_str()
                .unwrap()
                .contains("internal PIPELINE-FAULT (degraded review)"),
            "a crashed render must block via the fault hook: {out}"
        );
    }

    #[test]
    fn gauntlet_alias_tables_stay_in_sync() {
        // build_items.py and default_lanes.py each carry a copy of the lane
        // alias table; a caller-forced lane must canonicalize identically on
        // the normal and degraded paths.
        fn aliases_block(src: &str) -> String {
            let start = src
                .find("ALIASES = {")
                .unwrap_or_else(|| panic!("no ALIASES table in: {src}"));
            let end = start + src[start..].find("\n}").unwrap();
            src[start..end]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        }
        let scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/review-gauntlet/scripts");
        let build_items = std::fs::read_to_string(scripts.join("build_items.py")).unwrap();
        let default_lanes = std::fs::read_to_string(scripts.join("default_lanes.py")).unwrap();
        assert_eq!(
            aliases_block(&build_items),
            aliases_block(&default_lanes),
            "build_items.ALIASES and default_lanes.ALIASES must be identical"
        );
        assert!(
            build_items.contains("Keep in sync with default_lanes.ALIASES"),
            "build_items.py must point at its twin table"
        );
    }

    #[test]
    fn gauntlet_build_items_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A truthy non-iterable forced_lanes raises inside main; the guard
        // must exit 0 (asserted in the helper) with empty lanes and a
        // PIPELINE-FAULT for the gate.
        let out = run_gauntlet_script("build_items.py", &json!({"forced_lanes": 42}));
        for key in [
            "code_review_items",
            "adversary_items",
            "security_items",
            "probe_items",
        ] {
            assert_eq!(
                out[key],
                json!([]),
                "{key} must be empty on a builder crash: {out}"
            );
        }
        assert!(
            out["signals_error"]
                .as_str()
                .unwrap()
                .contains("PIPELINE-FAULT: lane builder crashed"),
            "the crash must surface as a PIPELINE-FAULT: {out}"
        );
    }

    #[test]
    fn gauntlet_build_items_folds_degradation_note_into_summary() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "forced_lanes": ["code-review", "adversary"],
            "lanes_degraded": "lane selection degraded to deterministic defaults"
        });
        let out = run_gauntlet_script("build_items.py", &state);
        let summary = out["lanes_summary"].as_str().unwrap();
        assert!(
            summary.contains("lane selection degraded to deterministic defaults"),
            "the degradation note must survive into lanes_summary: {summary}"
        );
        assert!(
            summary.contains("forced lanes honored exactly"),
            "the forced-lanes reason must still be recorded: {summary}"
        );
    }

    #[test]
    fn gauntlet_lane_fault_normalizes_engine_failure_text() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_gauntlet_script(
            "lane_fault.py",
            &json!({"lane_ctx": {"lane": "security"}, "lane_out": "Agent node failed: kaboom"}),
        );
        assert_eq!(
            out["lane_out"],
            "PIPELINE-FAULT: security lane failed after retries — Agent node failed: kaboom"
        );

        let out = run_gauntlet_script(
            "lane_fault.py",
            &json!({"lane_out": "Agent node failed: kaboom"}),
        );
        assert!(
            out["lane_out"]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: unknown lane failed after retries"),
            "a missing lane_ctx must degrade to the unknown lane: {out}"
        );
    }

    #[test]
    fn gauntlet_parse_fault_records_fault_in_signals_error() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_gauntlet_script(
            "parse_fault.py",
            &json!({"parse_failure": "LLM node 'parse' failed: provider exploded"}),
        );
        let fault = out["signals_error"].as_str().unwrap();
        assert!(
            fault.contains("PIPELINE-FAULT: parse failed"),
            "a dead parse stage must surface as a PIPELINE-FAULT: {fault}"
        );
        assert!(
            fault.contains("provider exploded"),
            "the failure detail must be carried into the fault: {fault}"
        );
    }

    #[test]
    fn gauntlet_parse_fault_blocks_verdict_gate() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // End-to-end over the fallback route: parse_fault's emitted
        // signals_error must make the gate BLOCK even with every lane empty.
        let fault = run_gauntlet_script(
            "parse_fault.py",
            &json!({"parse_failure": "LLM node 'parse' failed: provider exploded"}),
        );
        let state = json!({
            "code_review_results": [],
            "adversary_results": [],
            "security_results": [],
            "probe_results": [],
            "signals_error": fault["signals_error"]
        });
        let out = run_gauntlet_script("verdict_gate.py", &state);
        assert_eq!(out["gauntlet_verdict"], "BLOCKED");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("## Blockers\n- pipeline: PIPELINE-FAULT: parse failed"),
            "the parse fault must appear in the Blockers section: {report}"
        );
        assert!(
            report.contains("provider exploded"),
            "the failure detail must survive into the report: {report}"
        );
        assert_eq!(
            out["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: pipeline",
            "a dead parse stage is an incomplete review under the pipeline pseudo-lane: {out}"
        );
    }

    #[test]
    fn gauntlet_default_lanes_is_deterministic() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_gauntlet_script("default_lanes.py", &json!({}));
        assert_eq!(out["forced_lanes"], json!(["code-review", "adversary"]));
        assert!(
            out["lanes_degraded"]
                .as_str()
                .unwrap()
                .contains("deterministic defaults"),
            "the degradation note must name the deterministic defaults: {out}"
        );

        let probe = run_gauntlet_script(
            "default_lanes.py",
            &json!({"consumer_surface": true, "probe_context": "make run && hurl tests/"}),
        );
        assert_eq!(
            probe["forced_lanes"],
            json!(["code-review", "adversary", "probe"]),
            "a consumer surface with a local-run recipe must widen selection to include probe: {probe}"
        );

        let again = run_gauntlet_script("default_lanes.py", &json!({}));
        assert_eq!(out, again, "same input must produce identical output");
    }

    #[test]
    fn gauntlet_default_lanes_preserves_caller_forced_lanes() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A caller who forced a lane must not lose it when selection
        // degrades — the fallback unions, never overwrites — and forced
        // names canonicalize via the same aliases build_items uses.
        let out = run_gauntlet_script(
            "default_lanes.py",
            &json!({"forced_lanes": ["security-reviewer"]}),
        );
        assert_eq!(
            out["forced_lanes"],
            json!(["code-review", "adversary", "security"]),
            "a caller-forced lane must survive the fallback: {out}"
        );

        // Unknown forced names are ignored, not crashed on.
        let bogus = run_gauntlet_script("default_lanes.py", &json!({"forced_lanes": ["bogus"]}));
        assert_eq!(
            bogus["forced_lanes"],
            json!(["code-review", "adversary"]),
            "unknown forced names must be ignored: {bogus}"
        );

        let again = run_gauntlet_script(
            "default_lanes.py",
            &json!({"forced_lanes": ["security-reviewer"]}),
        );
        assert_eq!(out, again, "same input must produce identical output");
    }

    #[test]
    fn gauntlet_default_lanes_security_signals_force_security_lane() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // The deterministic security signals are already in state when the
        // fallback runs; degradation must never drop the security lane
        // build_items' hard rules would have selected.
        for state in [
            json!({"touches_auth": true}),
            json!({"touches_deps": true}),
            json!({"touches_exec": true}),
            json!({"security_posture": "hardened"}),
        ] {
            let out = run_gauntlet_script("default_lanes.py", &state);
            assert_eq!(
                out["forced_lanes"],
                json!(["code-review", "adversary", "security"]),
                "security signal {state} must force the security lane: {out}"
            );
        }
    }

    #[test]
    fn gauntlet_default_lanes_probe_requires_probe_context() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // build_items gates probe on a local-run recipe; the fallback must
        // not force a probe that is INCONCLUSIVE by construction.
        let out = run_gauntlet_script("default_lanes.py", &json!({"consumer_surface": true}));
        assert_eq!(
            out["forced_lanes"],
            json!(["code-review", "adversary"]),
            "a consumer surface without a probe_context must not add probe: {out}"
        );

        let out = run_gauntlet_script(
            "default_lanes.py",
            &json!({"consumer_surface": true, "probe_context": "make run && hurl tests/"}),
        );
        assert_eq!(
            out["forced_lanes"],
            json!(["code-review", "adversary", "probe"]),
            "a consumer surface with a probe_context must add probe: {out}"
        );
    }

    #[test]
    fn gauntlet_default_lanes_crash_widens_and_records_fault() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A truthy non-iterable forced_lanes raises inside main; the guard
        // must emit the WIDEST safe selection (defaults ∪ security) plus a
        // PIPELINE-FAULT so verdict_gate blocks — never a narrower pass.
        let out = run_gauntlet_script(
            "default_lanes.py",
            &json!({"forced_lanes": 42, "touches_auth": true}),
        );
        assert_eq!(
            out["forced_lanes"],
            json!(["code-review", "adversary", "security"]),
            "a crashed fallback must widen to defaults ∪ security: {out}"
        );
        assert!(
            out["signals_error"]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: lane-selection fallback crashed"),
            "the crash must surface as a PIPELINE-FAULT: {out}"
        );
        assert!(
            out["lanes_degraded"]
                .as_str()
                .unwrap()
                .contains("fallback script error"),
            "the degradation note must name the crash: {out}"
        );
    }

    #[test]
    fn gauntlet_lane_prompts_carry_passthroughs() {
        use crate::graph::{GraphParser, NodeType};
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/review-gauntlet");
        let graph = GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .expect("review-gauntlet graph.yaml must parse");
        let node = graph
            .nodes
            .get("run_adversary")
            .expect("review-gauntlet graph must have a run_adversary node");
        let NodeType::Agent(adv) = &node.node_type else {
            panic!("run_adversary must be an agent node");
        };
        assert_eq!(
            adv.inputs
                .as_ref()
                .and_then(|inputs| inputs.get("verification_commands"))
                .map(String::as_str),
            Some("{{verification_commands}}"),
            "run_adversary must forward verification_commands as a lone-template input (raw \
             passthrough): {:?}",
            adv.inputs
        );
        assert!(
            !adv.prompt.contains("{{verification_commands}}"),
            "verification commands are a structured input, never lane-prompt prose: {}",
            adv.prompt
        );
        assert!(
            adv.prompt.contains("arrive as a structured input"),
            "run_adversary's prompt must say where the commands come from instead: {}",
            adv.prompt
        );
        let node = graph
            .nodes
            .get("run_probe")
            .expect("review-gauntlet graph must have a run_probe node");
        let NodeType::Agent(probe) = &node.node_type else {
            panic!("run_probe must be an agent node");
        };
        assert!(
            probe.prompt.contains("reconcile rather than duplicate"),
            "run_probe's prompt must carry the reconcile line: {}",
            probe.prompt
        );
    }

    fn now_secs() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    /// State for a single gauntlet lane whose map ran one item and collected
    /// `report`, stamped with a fresh start so the retry budget is open.
    fn gauntlet_lane_ran(lane: &str, report: &str) -> serde_json::Value {
        let key = lane.replace('-', "_");
        let mut state = json!({ "gauntlet_started_at": now_secs() });
        state[format!("{key}_items").as_str()] = json!([{ "lane": lane }]);
        state[format!("{key}_results").as_str()] = json!([report]);
        state
    }

    const GAUNTLET_MERGE_READY: &str = "# Review\n\n**Verdict: MERGE-READY**\n";
    const GAUNTLET_ADVERSARY_FAULT: &str =
        "PIPELINE-FAULT: adversary lane failed after retries — Agent node failed: boom";
    const GAUNTLET_ADVERSARY_DEGRADED: &str = "ADVERSARIAL_REVIEW: DIVERGES\nCriteria: none verified — degraded run: pipeline fault recorded (fail-closed).\nComplaints:\n1. PIPELINE-FAULT: run_checks died";
    const GAUNTLET_ADVERSARY_DEGRADED_COUNTED: &str = "ADVERSARIAL_REVIEW: DIVERGES\nCriteria: 2/3 met, 0 partial, 1 unmet/diverged — degraded run: pipeline fault recorded (fail-closed).\nComplaints:\n1. PIPELINE-FAULT: run_checks died";
    const GAUNTLET_ADVERSARY_DEGRADED_DIED: &str = "ADVERSARIAL_REVIEW: DIVERGES\nCriteria: 2/3 met, 0 partial, 1 unmet/diverged — degraded run: 1 criterion check(s) died (fail-closed).\nComplaints:\n1. c3: check died — boom";
    const GAUNTLET_ADVERSARY_CRASH_STUB: &str = "ADVERSARIAL_REVIEW: DIVERGES\nCriteria: verdict computation error: boom — treat as failed, not as conforming.";
    const GAUNTLET_ADVERSARY_DIVERGES: &str = "ADVERSARIAL_REVIEW: DIVERGES\nCriteria: 2/3 met, 0 partial, 1 unmet/diverged.\nComplaints:\n1. c3: nothing in the diff does Z";
    const GAUNTLET_ADVERSARY_QUOTED_DEGRADED: &str = "ADVERSARIAL_REVIEW: CONFORMS\nCriteria: 3/3 met, 0 partial, 0 unmet/diverged.\nComplaints:\n- none\n\nObservations:\n- an earlier run printed 'Criteria: none verified — degraded run: pipeline fault recorded' but this one is clean";

    #[test]
    fn gauntlet_retry_gate_stashes_and_restores_reports() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let first = run_gauntlet_script(
            "retry_gate.py",
            &json!({
                "code_review_items": [{"lane": "code-review"}],
                "code_review_results": [GAUNTLET_MERGE_READY],
                "adversary_items": [{"lane": "adversary"}],
                "adversary_results": [GAUNTLET_ADVERSARY_FAULT],
                "gauntlet_started_at": now_secs(),
            }),
        );
        assert_eq!(
            first["kept_results"],
            json!({"code-review": GAUNTLET_MERGE_READY, "adversary": GAUNTLET_ADVERSARY_FAULT}),
            "completed and faulted lanes must both be stashed verbatim: {first}"
        );
        assert_eq!(first["retry_lanes"], json!(["adversary"]), "{first}");
        assert_eq!(first["_next"], "build_items", "{first}");

        // The re-run pass: code-review's map ran nothing and overwrote its
        // collected results with []; adversary ran again and completed.
        let second = run_gauntlet_script(
            "retry_gate.py",
            &json!({
                "code_review_items": [],
                "code_review_results": [],
                "adversary_items": [{"lane": "adversary", "attempt": 2}],
                "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS\nCriteria: all verified"],
                "kept_results": first["kept_results"],
                "lane_attempts": first["lane_attempts"],
                "gauntlet_started_at": now_secs(),
            }),
        );
        assert_eq!(
            second["code_review_results"],
            json!([GAUNTLET_MERGE_READY]),
            "the overwritten code-review report must be restored from the stash: {second}"
        );
        assert_eq!(second["retry_lanes"], json!([]), "{second}");
        assert!(
            second.get("_next").is_none(),
            "a pass with nothing to retry must fall through statically: {second}"
        );
        assert_eq!(
            second["kept_results"]["adversary"],
            "ADVERSARIAL_REVIEW: CONFORMS\nCriteria: all verified",
            "the stash must carry the lane's latest report: {second}"
        );
        assert_eq!(second["lane_attempts"]["adversary"], 2, "{second}");

        // A pass where nothing ran: both lanes come back from the stash,
        // the faulted adversary report included — it is restored, not
        // re-classified for another retry.
        let restored = run_gauntlet_script(
            "retry_gate.py",
            &json!({
                "code_review_items": [],
                "code_review_results": [],
                "adversary_items": [],
                "adversary_results": [],
                "security_items": [],
                "security_results": [],
                "probe_items": [],
                "probe_results": [],
                "kept_results": first["kept_results"],
                "lane_attempts": first["lane_attempts"],
                "gauntlet_started_at": now_secs(),
            }),
        );
        assert_eq!(
            restored["code_review_results"],
            json!([GAUNTLET_MERGE_READY]),
            "{restored}"
        );
        assert_eq!(
            restored["adversary_results"],
            json!([GAUNTLET_ADVERSARY_FAULT]),
            "a faulted stash is restored as-is for the verdict gate: {restored}"
        );
        assert_eq!(restored["retry_lanes"], json!([]), "{restored}");
        assert!(
            restored.get("_next").is_none(),
            "restoring a stash must never trigger a retry: {restored}"
        );
        assert_eq!(
            restored["lane_attempts"], first["lane_attempts"],
            "a lane that did not run must not be charged an attempt: {restored}"
        );
    }

    #[test]
    fn gauntlet_retry_gate_classifies_lane_reports() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let completed = run_gauntlet_script(
            "retry_gate.py",
            &gauntlet_lane_ran("code-review", GAUNTLET_MERGE_READY),
        );
        assert_eq!(
            completed["retry_lanes"],
            json!([]),
            "a report carrying its sentinel is completed, never re-run: {completed}"
        );
        assert!(completed.get("_next").is_none(), "{completed}");

        let faulted = [
            ("adversary", GAUNTLET_ADVERSARY_FAULT),
            ("security", "some text"),
            (
                "code-review",
                "# Review\n\n**Verdict: NEEDS-HUMAN** — 1 pipeline fault(s) recorded; see Human attention required\n\n## Findings\n",
            ),
            (
                "adversary",
                "ADVERSARIAL_REVIEW: DIVERGES\nCriteria: none verified — degraded run: pipeline fault recorded (fail-closed).\nComplaints:\n- none",
            ),
        ];
        for (lane, report) in faulted {
            let out = run_gauntlet_script("retry_gate.py", &gauntlet_lane_ran(lane, report));
            assert_eq!(
                out["retry_lanes"],
                json!([lane]),
                "{lane} report {report:?} must classify as faulted: {out}"
            );
            assert_eq!(out["_next"], "build_items", "{lane}: {out}");
        }

        // A lane whose map ran nothing (items []) is skipped: no attempt, no
        // retry, no incompleteness — and nothing to restore without a stash.
        let mut state = gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_FAULT);
        state["probe_items"] = json!([]);
        state["probe_results"] = json!([]);
        let out = run_gauntlet_script("retry_gate.py", &state);
        assert_eq!(out["retry_lanes"], json!(["adversary"]), "{out}");
        assert_eq!(out["review_incomplete"], json!([]), "{out}");
        assert!(
            out["lane_attempts"].get("probe").is_none(),
            "a skipped lane must not count an attempt: {out}"
        );
        assert!(out.get("probe_results").is_none(), "{out}");
    }

    #[test]
    fn gauntlet_retry_gate_adversary_degraded_predicate_is_anchored() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        for degraded in [
            GAUNTLET_ADVERSARY_DEGRADED,
            GAUNTLET_ADVERSARY_DEGRADED_COUNTED,
            GAUNTLET_ADVERSARY_DEGRADED_DIED,
            GAUNTLET_ADVERSARY_CRASH_STUB,
        ] {
            let out =
                run_gauntlet_script("retry_gate.py", &gauntlet_lane_ran("adversary", degraded));
            assert_eq!(
                out["retry_lanes"],
                json!(["adversary"]),
                "the degraded-run header {degraded:?} under the sentinel is a fault: {out}"
            );
            assert_eq!(out["_next"], "build_items", "{out}");
        }

        // Anchored to the line under the sentinel: a clean report quoting
        // the wording later is a completed review.
        let out = run_gauntlet_script(
            "retry_gate.py",
            &gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_QUOTED_DEGRADED),
        );
        assert_eq!(
            out["retry_lanes"],
            json!([]),
            "a quoted degraded-run phrase must not trigger a re-run: {out}"
        );
        assert!(out.get("_next").is_none(), "{out}");
        assert_eq!(out["review_incomplete"], json!([]), "{out}");

        // A plain DIVERGES is a real finding, never re-run.
        let out = run_gauntlet_script(
            "retry_gate.py",
            &gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_DIVERGES),
        );
        assert_eq!(
            out["retry_lanes"],
            json!([]),
            "a real DIVERGES must not trigger a re-run: {out}"
        );
        assert!(out.get("_next").is_none(), "{out}");
    }

    #[test]
    fn gauntlet_retry_gate_malformed_bookkeeping_degrades() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Malformed bookkeeping must not crash the gate: attempts are still
        // counted, the faulted lane is still retried, a stashed report is
        // still restored, and the malformation is recorded as a pipeline
        // fault rather than adopted as lane names.
        let mut state = gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_FAULT);
        state["lane_attempts"] = json!(42);
        state["review_incomplete"] = json!("probe");
        state["kept_results"] = json!({"code-review": GAUNTLET_MERGE_READY});
        state["code_review_items"] = json!([]);
        state["code_review_results"] = json!([]);
        let out = run_gauntlet_script("retry_gate.py", &state);
        let fault = out["signals_error"].as_str().unwrap_or_default();
        assert!(
            !fault.starts_with("PIPELINE-FAULT: retry gate crashed"),
            "malformed bookkeeping must not reach the crash stub: {out}"
        );
        assert_eq!(out["retry_lanes"], json!(["adversary"]), "{out}");
        assert_eq!(out["lane_attempts"], json!({"adversary": 1}), "{out}");
        assert_eq!(
            out["review_incomplete"],
            json!([]),
            "a bare string is never split or adopted as a lane name: {out}"
        );
        assert_eq!(
            out["code_review_results"],
            json!([GAUNTLET_MERGE_READY]),
            "the stashed report must still be restored: {out}"
        );
        assert!(
            fault.contains("malformed review_incomplete bookkeeping"),
            "the malformation must be recorded as a fault: {out}"
        );
        assert!(
            fault.starts_with("PIPELINE-FAULT: malformed review_incomplete bookkeeping ("),
            "{out}"
        );

        // Merged over the input state as the graph would, the verdict gate
        // must block and name the pipeline.
        let mut merged = state;
        for (k, v) in out.as_object().unwrap() {
            merged[k.as_str()] = v.clone();
        }
        let verdict = run_gauntlet_script("verdict_gate.py", &merged);
        assert_eq!(verdict["gauntlet_verdict"], "BLOCKED", "{verdict}");
        assert!(
            verdict["gauntlet_incomplete_line"]
                .as_str()
                .unwrap()
                .contains("pipeline"),
            "{verdict}"
        );

        // Per-lane values that are not ints count as zero, so the lane that
        // ran lands on its first attempt.
        let mut state = gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_FAULT);
        state["lane_attempts"] = json!({"adversary": [1], "probe": "x"});
        let out = run_gauntlet_script("retry_gate.py", &state);
        assert!(
            !out["signals_error"]
                .as_str()
                .unwrap_or_default()
                .contains("retry gate crashed"),
            "{out}"
        );
        assert_eq!(out["retry_lanes"], json!(["adversary"]), "{out}");
        assert_eq!(out["lane_attempts"]["adversary"], json!(1), "{out}");

        // A list with a non-str entry keeps its str lanes and records the
        // fault; the retried lane is not declined.
        let mut state = gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_FAULT);
        state["review_incomplete"] = json!(["probe", 7]);
        let out = run_gauntlet_script("retry_gate.py", &state);
        assert_eq!(out["retry_lanes"], json!(["adversary"]), "{out}");
        assert_eq!(out["review_incomplete"], json!(["probe"]), "{out}");
        assert!(
            out["signals_error"]
                .as_str()
                .unwrap_or_default()
                .starts_with("PIPELINE-FAULT: malformed review_incomplete bookkeeping ("),
            "{out}"
        );

        // The bookkeeping fault appends to a prior signals_error.
        let mut state = gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_FAULT);
        state["review_incomplete"] = json!("probe");
        state["signals_error"] = json!("PIPELINE-FAULT: earlier");
        let out = run_gauntlet_script("retry_gate.py", &state);
        assert!(
            out["signals_error"].as_str().unwrap_or_default().starts_with(
                "PIPELINE-FAULT: earlier; PIPELINE-FAULT: malformed review_incomplete bookkeeping ("
            ),
            "{out}"
        );
    }

    #[test]
    fn gauntlet_gates_block_real_adversary_degraded_verdict() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Pins the sentinel→header adjacency both gates rely on against the
        // adversary's real verdict.py output.
        let met = json!({"id": "c1", "text": "does X", "status": "MET",
                         "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""});
        let degraded = run_adversary_script(
            "verdict.py",
            &json!({
                "pipeline_faults": ["PIPELINE-FAULT: run_checks died — boom"],
                "crit_verdicts": [met],
                "extra_complaints": [],
                "observations": "",
                "exec_results": ""
            }),
        );
        let degraded = degraded["adv_report"].as_str().unwrap();
        assert!(
            degraded.contains("degraded run: pipeline fault recorded"),
            "{degraded}"
        );
        let out = run_gauntlet_script(
            "verdict_gate.py",
            &json!({
                "code_review_results": [GAUNTLET_MERGE_READY],
                "adversary_results": [degraded],
                "security_results": [],
                "probe_results": []
            }),
        );
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        let report = out["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | BLOCKED | PIPELINE-FAULT (degraded lane) |"),
            "the real degraded header must render as a pipeline fault: {report}"
        );
        assert!(
            !report.contains("adversary: DIVERGES — the implementation does not conform"),
            "{report}"
        );
        let out = run_gauntlet_script("retry_gate.py", &gauntlet_lane_ran("adversary", degraded));
        assert_eq!(
            out["retry_lanes"],
            json!(["adversary"]),
            "the real degraded header must be re-run: {out}"
        );

        let diverges = run_adversary_script(
            "verdict.py",
            &json!({
                "pipeline_faults": [],
                "crit_verdicts": [{"id": "c1", "text": "does Y", "status": "UNMET",
                                   "evidence": "", "complaint": "nothing in the diff does Y"}],
                "extra_complaints": [],
                "observations": "",
                "exec_results": ""
            }),
        );
        let diverges = diverges["adv_report"].as_str().unwrap();
        let out = run_gauntlet_script(
            "verdict_gate.py",
            &json!({
                "code_review_results": [GAUNTLET_MERGE_READY],
                "adversary_results": [diverges],
                "security_results": [],
                "probe_results": []
            }),
        );
        assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
        assert!(
            out["gauntlet_report"]
                .as_str()
                .unwrap()
                .contains("| adversary | BLOCKED | DIVERGES |"),
            "a real DIVERGES must stay a finding: {out}"
        );
        let out = run_gauntlet_script("retry_gate.py", &gauntlet_lane_ran("adversary", diverges));
        assert_eq!(
            out["retry_lanes"],
            json!([]),
            "a real DIVERGES must never be re-run: {out}"
        );

        // The other two degraded-header forms verdict.py emits: a criterion
        // check that died, and the top-level crash stub.
        let died = run_adversary_script(
            "verdict.py",
            &json!({
                "pipeline_faults": [],
                "crit_verdicts": [
                    met,
                    {"id": "c2", "text": "does Y", "status": "UNMET",
                     "evidence": "PIPELINE-FAULT: criterion check died — boom",
                     "complaint": "boom"}
                ],
                "extra_complaints": [],
                "observations": "",
                "exec_results": ""
            }),
        );
        let died = died["adv_report"].as_str().unwrap();
        assert!(
            died.contains("degraded run: 1 criterion check(s) died"),
            "{died}"
        );
        let crashed = run_adversary_script(
            "verdict.py",
            &json!({
                "pipeline_faults": [],
                "crit_verdicts": [{"status": "UNMET", "text": 123}],
                "extra_complaints": [],
                "observations": "",
                "exec_results": ""
            }),
        );
        let crashed = crashed["adv_report"].as_str().unwrap();
        assert!(
            crashed.contains("Criteria: verdict computation error:"),
            "{crashed}"
        );
        for degraded in [died, crashed] {
            let out = run_gauntlet_script(
                "verdict_gate.py",
                &json!({
                    "code_review_results": [GAUNTLET_MERGE_READY],
                    "adversary_results": [degraded],
                    "security_results": [],
                    "probe_results": []
                }),
            );
            assert_eq!(out["gauntlet_verdict"], "BLOCKED", "{out}");
            let report = out["gauntlet_report"].as_str().unwrap();
            assert!(
                report.contains("| adversary | BLOCKED | PIPELINE-FAULT (degraded lane) |"),
                "the real degraded header must render as a pipeline fault: {report}"
            );
            assert!(
                !report.contains("adversary: DIVERGES — the implementation does not conform"),
                "{report}"
            );
            let out =
                run_gauntlet_script("retry_gate.py", &gauntlet_lane_ran("adversary", degraded));
            assert_eq!(
                out["retry_lanes"],
                json!(["adversary"]),
                "the real degraded header must be re-run: {out}"
            );
        }
    }

    #[test]
    fn gauntlet_retry_gate_counts_attempts_per_lane_that_ran() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_gauntlet_script(
            "retry_gate.py",
            &json!({
                "code_review_items": [{"lane": "code-review"}],
                "code_review_results": [GAUNTLET_MERGE_READY],
                "adversary_items": [{"lane": "adversary"}],
                "adversary_results": [GAUNTLET_ADVERSARY_FAULT],
                "security_items": [],
                "probe_items": [],
                "gauntlet_started_at": now_secs(),
            }),
        );
        assert_eq!(
            out["lane_attempts"],
            json!({"code-review": 1, "adversary": 1}),
            "only lanes that ran count an attempt: {out}"
        );

        let mut state = gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_FAULT);
        state["lane_attempts"] = json!({"adversary": 1});
        let out = run_gauntlet_script("retry_gate.py", &state);
        assert_eq!(out["lane_attempts"]["adversary"], 2, "{out}");
    }

    #[test]
    fn gauntlet_retry_gate_retries_until_attempts_are_exhausted() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        for prior in [0, 1] {
            let mut state = gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_FAULT);
            state["lane_attempts"] = json!({"adversary": prior});
            let out = run_gauntlet_script("retry_gate.py", &state);
            assert_eq!(
                out["_next"],
                "build_items",
                "attempt {} must route back into the builder: {out}",
                prior + 1
            );
            assert_eq!(out["retry_lanes"], json!(["adversary"]), "{out}");
            assert_eq!(
                out["kept_results"]["adversary"], GAUNTLET_ADVERSARY_FAULT,
                "{out}"
            );
            assert_eq!(out["review_incomplete"], json!([]), "{out}");
        }

        let mut state = gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_FAULT);
        state["lane_attempts"] = json!({"adversary": 2});
        let out = run_gauntlet_script("retry_gate.py", &state);
        assert!(
            out.get("_next").is_none(),
            "the third attempt must fall through to the verdict gate: {out}"
        );
        assert_eq!(out["lane_attempts"]["adversary"], 3, "{out}");
        assert_eq!(out["retry_lanes"], json!([]), "{out}");
        assert_eq!(out["review_incomplete"], json!(["adversary"]), "{out}");
    }

    #[test]
    fn gauntlet_retry_gate_declines_outside_wall_clock_budget() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let mut state = gauntlet_lane_ran("adversary", GAUNTLET_ADVERSARY_FAULT);
        state["gauntlet_started_at"] = json!(now_secs() - 100000.0);
        let out = run_gauntlet_script("retry_gate.py", &state);
        assert!(
            out.get("_next").is_none(),
            "a first-attempt fault past the budget must not retry: {out}"
        );
        assert_eq!(out["retry_lanes"], json!([]), "{out}");
        assert_eq!(out["review_incomplete"], json!(["adversary"]), "{out}");
    }

    #[test]
    fn gauntlet_retry_gate_missing_report_becomes_synthetic_fault() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // The map ran an item but collected nothing: without a synthetic
        // marker the lane would have no report to stash or restore and the
        // verdict gate would render it SKIPPED.
        let marker = "PIPELINE-FAULT: adversary lane produced no report";
        let out = run_gauntlet_script(
            "retry_gate.py",
            &json!({
                "adversary_items": [{"lane": "adversary"}],
                "adversary_results": [],
                "gauntlet_started_at": now_secs(),
            }),
        );
        assert_eq!(out["adversary_results"], json!([marker]), "{out}");
        assert_eq!(out["kept_results"]["adversary"], marker, "{out}");
        assert_eq!(out["retry_lanes"], json!(["adversary"]), "{out}");
        assert_eq!(out["_next"], "build_items", "{out}");
        assert_eq!(out["lane_attempts"]["adversary"], 1, "{out}");

        let verdict = run_gauntlet_script(
            "verdict_gate.py",
            &json!({
                "code_review_results": [],
                "adversary_results": out["adversary_results"],
                "security_results": [],
                "probe_results": []
            }),
        );
        let report = verdict["gauntlet_report"].as_str().unwrap();
        assert!(
            report.contains("| adversary | BLOCKED | PIPELINE-FAULT (lane failed) |"),
            "a lane that ran but produced nothing must block, never SKIPPED: {report}"
        );
    }

    #[test]
    fn gauntlet_retry_gate_crash_falls_through_without_retry() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // dict(42) raises inside main after the upstream fault was read.
        let out = run_gauntlet_script(
            "retry_gate.py",
            &json!({
                "kept_results": 42,
                "adversary_items": [{"lane": "adversary"}],
                "adversary_results": ["x"],
                "signals_error": "PIPELINE-FAULT: earlier fault",
            }),
        );
        assert!(
            out["signals_error"]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: earlier fault; PIPELINE-FAULT: retry gate crashed"),
            "the crash must append to the upstream fault, not overwrite it: {out}"
        );
        assert_eq!(out["retry_lanes"], json!([]), "{out}");
        assert!(
            out["review_incomplete"].is_array(),
            "review_incomplete must stay a list on the crash path: {out}"
        );
        assert!(
            out.get("_next").is_none(),
            "a crashed gate must never retry: {out}"
        );
    }

    #[test]
    fn gauntlet_pipeline_crash_guards_name_pipeline_incomplete() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_gauntlet_script("build_items.py", &json!({"forced_lanes": 42}));
        let gate = run_gauntlet_script(
            "verdict_gate.py",
            &json!({
                "code_review_results": [],
                "adversary_results": [],
                "security_results": [],
                "probe_results": [],
                "signals_error": out["signals_error"],
            }),
        );
        assert_eq!(gate["gauntlet_verdict"], "BLOCKED", "{gate}");
        assert_eq!(
            gate["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: pipeline",
            "an all-SKIPPED gate downstream of a builder crash must name the pipeline: {gate}"
        );

        let out = run_gauntlet_script(
            "retry_gate.py",
            &json!({
                "kept_results": 42,
                "adversary_items": [{"lane": "adversary"}],
                "adversary_results": ["x"],
                "signals_error": "git diff unavailable",
            }),
        );
        let crashed = out["signals_error"]
            .as_str()
            .expect("retry_gate crash guard must record signals_error");
        assert!(
            crashed.contains("PIPELINE-FAULT: retry gate crashed"),
            "the pipeline name below must come from the crash guard, not the seed: {out}"
        );
        let gate = run_gauntlet_script(
            "verdict_gate.py",
            &json!({
                "code_review_results": [],
                "adversary_results": [],
                "security_results": [],
                "probe_results": [],
                "signals_error": out["signals_error"],
                "review_incomplete": out["review_incomplete"],
            }),
        );
        assert_eq!(gate["gauntlet_verdict"], "BLOCKED", "{gate}");
        assert_eq!(
            gate["gauntlet_incomplete_line"], "\nGAUNTLET_REVIEW_INCOMPLETE: pipeline",
            "the verdict gate must name the pipeline after a retry-gate crash: {gate}"
        );
    }

    /// retry_gate.py decides "completed" with the same sentinel regexes
    /// verdict_gate.py parses; a drift would re-run lanes the verdict gate
    /// accepts, or accept lanes it cannot read.
    #[test]
    fn gauntlet_retry_gate_sentinels_match_verdict_gate() {
        let scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/review-gauntlet/scripts");
        let retry = read_to_string(scripts.join("retry_gate.py")).unwrap();
        let verdict = read_to_string(scripts.join("verdict_gate.py")).unwrap();
        for sentinel in [
            r"Verdict:\**\s*\**\s*(MERGE-READY|NEEDS-HUMAN)",
            r"ADVERSARIAL_REVIEW:\s*(CONFORMS|DIVERGES)",
            r"SECURITY_REVIEW:\s*(PASS|FAIL)",
            r"USAGE_PROBE:\s*(PASS|FAIL|INCONCLUSIVE)",
        ] {
            assert!(
                retry.contains(sentinel),
                "retry_gate.py must carry the sentinel regex {sentinel}"
            );
            assert!(
                verdict.contains(sentinel),
                "verdict_gate.py must carry the sentinel regex {sentinel}"
            );
        }
        for fragment in [
            r"^\*\*Verdict: NEEDS-HUMAN\*\* — .*",
            r"(pipeline fault\(s\) recorded|PIPELINE-FAULT:|report rendering error)",
        ] {
            assert!(
                retry.contains(fragment),
                "retry_gate.py must carry the nested-degraded fragment {fragment}"
            );
            assert!(
                verdict.contains(fragment),
                "verdict_gate.py must carry the nested-degraded fragment {fragment}"
            );
        }
        let adversary_verdict = read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets/agents/adversary/scripts/verdict.py"),
        )
        .unwrap();
        let degraded = "degraded run: pipeline fault recorded";
        assert!(
            retry.contains(degraded),
            "retry_gate.py must match the adversary's degraded-run wording {degraded:?}"
        );
        assert!(
            verdict.contains(degraded),
            "verdict_gate.py must match the adversary's degraded-run wording {degraded:?}"
        );
        assert!(
            adversary_verdict.contains(degraded),
            "assets/agents/adversary/scripts/verdict.py must still emit {degraded:?}"
        );
        for fragment in [
            "degraded run: pipeline fault recorded",
            r"criterion check\(s\) died",
            "verdict computation error:",
        ] {
            assert!(
                retry.contains(fragment),
                "retry_gate.py must match the adversary degraded header fragment {fragment:?}"
            );
            assert!(
                verdict.contains(fragment),
                "verdict_gate.py must match the adversary degraded header fragment {fragment:?}"
            );
        }
        for emitted in ["criterion check(s) died", "verdict computation error:"] {
            assert!(
                adversary_verdict.contains(emitted),
                "assets/agents/adversary/scripts/verdict.py must still emit {emitted:?}"
            );
        }

        fn slice_between<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
            let from = src
                .find(start)
                .unwrap_or_else(|| panic!("missing {start:?}"));
            let rest = &src[from..];
            let to = rest
                .find(end)
                .unwrap_or_else(|| panic!("missing {end:?} after {start:?}"));
            &rest[..to + end.len()]
        }
        for (start, end) in [
            ("ADVERSARY_DEGRADED_HEADER = re.compile(", "\n)\n"),
            ("def adversary_degraded(report):", "    return False\n"),
        ] {
            assert_eq!(
                slice_between(&retry, start, end),
                slice_between(&verdict, start, end),
                "the adversary degraded predicate must stay byte-identical in both gate scripts"
            );
        }
    }

    /// build_items.py's LANE_KEYS and retry_gate.py's LANES name the same
    /// four lanes and item keys; a lane present in one but not the other
    /// would be built but never stashed, or stashed but never rebuilt.
    #[test]
    fn gauntlet_build_items_lane_keys_match_retry_gate_lanes() {
        let scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/review-gauntlet/scripts");
        let build = read_to_string(scripts.join("build_items.py")).unwrap();
        let retry = read_to_string(scripts.join("retry_gate.py")).unwrap();
        assert!(
            build.contains("Keep in sync with retry_gate.LANES"),
            "build_items.py LANE_KEYS must carry its sync note"
        );
        for lane in ["code-review", "adversary", "security", "probe"] {
            let name = format!("\"{lane}\"");
            let items = format!("{}_items", lane.replace('-', "_"));
            for (file, source) in [("build_items.py", &build), ("retry_gate.py", &retry)] {
                assert!(source.contains(&name), "{file} must name the lane {name}");
                assert!(
                    source.contains(&items),
                    "{file} must carry the items key {items}"
                );
            }
        }
    }

    #[test]
    fn gauntlet_build_items_retry_pass_rebuilds_only_retried_lanes() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_gauntlet_script(
            "build_items.py",
            &json!({
                "retry_lanes": ["adversary"],
                "lane_attempts": {"adversary": 1, "code-review": 1},
                "lanes_summary": "- code-review: always on",
                "forced_lanes": ["code-review", "adversary"],
                "gauntlet_started_at": 123.0,
            }),
        );
        assert_eq!(
            out["adversary_items"],
            json!([{"lane": "adversary", "attempt": 2}]),
            "{out}"
        );
        for key in ["code_review_items", "security_items", "probe_items"] {
            assert_eq!(
                out[key],
                json!([]),
                "{key} must map over nothing on a re-run pass, even when forced: {out}"
            );
        }
        assert_eq!(out["retry_lanes"], json!([]), "{out}");
        let summary = out["lanes_summary"].as_str().unwrap();
        assert!(
            summary.contains("retried: [adversary (attempt 2)]")
                && summary.contains("code-review: always on"),
            "the retry note must append to the first pass's summary: {summary}"
        );
        assert!(
            out.get("gauntlet_started_at").is_none(),
            "an existing start stamp must be left alone: {out}"
        );
    }

    #[test]
    fn gauntlet_build_items_stamps_start_time_once() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_gauntlet_script("build_items.py", &json!({"forced_lanes": ["code-review"]}));
        assert!(
            out["gauntlet_started_at"].as_f64().is_some_and(|t| t > 0.0),
            "a first pass without a stamp must start the clock: {out}"
        );
        let out = run_gauntlet_script(
            "build_items.py",
            &json!({"forced_lanes": ["code-review"], "gauntlet_started_at": 5.0}),
        );
        assert!(
            out.get("gauntlet_started_at").is_none(),
            "an existing start stamp must be left alone: {out}"
        );
    }

    /// With the clock already stamped (the normal graph path — signals.py
    /// runs first), a first pass must serialize exactly the five-key payload
    /// it always has: same keys, same order, same values, nothing extra.
    #[test]
    fn gauntlet_build_items_first_pass_payload_is_unchanged_when_stamped() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/review-gauntlet/scripts/build_items.py");
        let state =
            json!({"forced_lanes": ["code-review", "adversary"], "gauntlet_started_at": 5.0});
        let out = std::process::Command::new("python3")
            .arg(&path)
            .env("GRAPH_STATE", state.to_string())
            .env_remove("GRAPH_STATE_FILE")
            .output()
            .expect("failed to invoke python3 build_items.py");
        assert!(
            out.status.success(),
            "build_items.py exited nonzero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.trim_end(),
            r#"{"code_review_items": [{"lane": "code-review"}], "adversary_items": [{"lane": "adversary"}], "security_items": [], "probe_items": [], "lanes_summary": "- forced lanes honored exactly: ['adversary', 'code-review']"}"#,
            "the stamped first-pass payload must be byte-identical to the pre-loop form"
        );
    }

    #[test]
    fn gauntlet_build_items_crash_appends_to_prior_fault() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_gauntlet_script(
            "build_items.py",
            &json!({
                "forced_lanes": 42,
                "signals_error": "PIPELINE-FAULT: earlier",
                "lanes_summary": "- code-review: always on",
            }),
        );
        assert!(
            out["signals_error"]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: earlier; PIPELINE-FAULT: lane builder crashed"),
            "the crash must append to the upstream fault, not overwrite it: {out}"
        );
        assert_eq!(
            out["lanes_summary"], "- code-review: always on",
            "a crash must not wipe the first pass's lanes_summary: {out}"
        );
    }

    #[test]
    fn gauntlet_signals_stamps_start_time_before_git_runs() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let dir = utils::temp_file("-gauntlet-signals-", "");
        create_dir_all(&dir).unwrap();
        let project_dir = dir.to_string_lossy().to_string();
        let out = run_gauntlet_script(
            "signals.py",
            &json!({"project_dir": project_dir, "diff_spec": "worktree"}),
        );
        let stamped = run_gauntlet_script(
            "signals.py",
            &json!({"project_dir": project_dir, "diff_spec": "worktree", "gauntlet_started_at": 77.0}),
        );
        let _ = remove_dir_all(&dir);
        assert!(
            out["signals_error"]
                .as_str()
                .is_some_and(|e| e.contains("diff signals could not be computed")),
            "a non-repo project_dir must degrade the signals: {out}"
        );
        assert!(
            out["gauntlet_started_at"].as_f64().is_some_and(|t| t > 0.0),
            "the start stamp must be set even when git fails: {out}"
        );
        assert!(
            stamped.get("gauntlet_started_at").is_none(),
            "an existing start stamp must be left alone: {stamped}"
        );
    }

    #[test]
    fn gauntlet_retry_loop_wiring() {
        use crate::graph::NodeType;
        let graph = load_bundled_graph("review-gauntlet");
        let maps = [
            "map_code_review",
            "map_adversary",
            "map_security",
            "map_probe",
        ];
        for id in maps {
            let node = graph
                .get_node(id)
                .unwrap_or_else(|| panic!("review-gauntlet must have a {id} node"));
            assert_eq!(
                node.next.as_ref().map(|n| n.as_slice()),
                Some(&["retry_gate".to_string()][..]),
                "{id} must route into retry_gate: {:?}",
                node.next
            );
        }
        let gate = graph
            .get_node("retry_gate")
            .expect("review-gauntlet must have a retry_gate node");
        let NodeType::Script(s) = &gate.node_type else {
            panic!("retry_gate must be a script node");
        };
        assert_eq!(s.script, "scripts/retry_gate.py");
        assert_eq!(
            gate.next.as_ref().map(|n| n.as_slice()),
            Some(&["verdict_gate".to_string()][..]),
            "retry_gate must fall through statically to verdict_gate: {:?}",
            gate.next
        );
        let build = graph
            .get_node("build_items")
            .expect("review-gauntlet must have a build_items node");
        let fan_out: Vec<String> = maps.iter().map(|m| m.to_string()).collect();
        assert_eq!(
            build.next.as_ref().map(|n| n.as_slice()),
            Some(fan_out.as_slice()),
            "build_items must still fan out to the four maps: {:?}",
            build.next
        );
        for (key, expected) in [
            ("lane_attempts", json!({})),
            ("kept_results", json!({})),
            ("retry_lanes", json!([])),
            ("review_incomplete", json!([])),
            ("gauntlet_started_at", json!(0)),
        ] {
            assert_eq!(
                graph.initial_state.get(key),
                Some(&expected),
                "initial_state.{key} must seed the retry loop"
            );
        }
        assert_eq!(graph.settings.timeout, Some(69600));
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/review-gauntlet/scripts/retry_gate.py");
        assert!(script.exists(), "{} must exist", script.display());
    }

    #[test]
    fn gauntlet_done_template_carries_incomplete_line() {
        use crate::graph::NodeType;
        let graph = load_bundled_graph("review-gauntlet");
        let NodeType::End(done) = &graph.get_node("done").unwrap().node_type else {
            panic!("done must be an end node")
        };
        assert!(
            done.output
                .contains("GAUNTLET: {{gauntlet_verdict}}{{gauntlet_incomplete_line}}"),
            "the incomplete line must follow the verdict immediately: {}",
            done.output
        );
        assert!(
            done.output
                .contains("{{gauntlet_incomplete_line}}\n\n{{gauntlet_report}}"),
            "the report must follow the incomplete line after one blank line: {}",
            done.output
        );
        assert_eq!(
            graph.initial_state.get("gauntlet_incomplete_line"),
            Some(&json!("")),
            "initial_state must declare the incomplete line so the template key resolves"
        );
    }

    /// MAX_RETRY_ELAPSED_SECS in retry_gate.py and settings.timeout in
    /// graph.yaml live in different files. A re-run admitted at the very end
    /// of the retry budget can still burn a full lane envelope (max_attempts
    /// × the slowest lane) plus the script stages, and a graph timeout kills
    /// from outside — no verdict would be rendered.
    #[test]
    fn gauntlet_retry_budget_fits_inside_graph_timeout() {
        use crate::graph::NodeType;
        const OTHER_STAGES_MARGIN_SECS: u64 = 1200;
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/review-gauntlet/scripts/retry_gate.py");
        let source = read_to_string(&script).unwrap();
        let line = source
            .lines()
            .find(|l| l.starts_with("MAX_RETRY_ELAPSED_SECS = "))
            .expect("retry_gate.py must define MAX_RETRY_ELAPSED_SECS");
        let max_retry: u64 = line
            .split_once(" = ")
            .and_then(|(_, v)| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("MAX_RETRY_ELAPSED_SECS no longer parses: {line}"));

        let graph = load_bundled_graph("review-gauntlet");
        let max_lane_envelope = graph
            .nodes
            .values()
            .filter_map(|node| match &node.node_type {
                NodeType::Agent(a) => a.timeout.map(|t| u64::from(a.max_attempts) * t),
                _ => None,
            })
            .max()
            .expect("review-gauntlet must have agent lanes with timeouts");
        let graph_timeout = graph
            .settings
            .timeout
            .expect("review-gauntlet must set settings.timeout");
        assert!(
            max_retry + max_lane_envelope + OTHER_STAGES_MARGIN_SECS <= graph_timeout,
            "assets/agents/review-gauntlet/scripts/retry_gate.py MAX_RETRY_ELAPSED_SECS ({max_retry}) + the largest lane envelope, max_attempts × timeout ({max_lane_envelope}) + {OTHER_STAGES_MARGIN_SECS}s margin must fit inside assets/agents/review-gauntlet/graph.yaml settings.timeout ({graph_timeout})"
        );
    }

    // The value run_checks executes with a shell must never be something an
    // LLM lifted out of the prompt — the prompt also carries pasted plan/diff
    // text from the repo under review.
    #[test]
    fn verification_commands_is_a_declared_variable_never_parsed_from_the_prompt() {
        use crate::graph::NodeType;
        for name in ["adversary", "review-gauntlet"] {
            let graph = load_bundled_graph(name);
            let var = graph
                .variables
                .iter()
                .find(|v| v.name == "verification_commands")
                .unwrap_or_else(|| {
                    panic!("{name} must declare the verification_commands variable")
                });
            assert_eq!(
                var.default.as_deref(),
                Some("[]"),
                "{name}: variables land as strings, so the default is the JSON-encoded empty list"
            );
            // The YAML block scalar wraps mid-sentence; pin the words, not the wrap.
            let description = var
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                description.contains("JSON array of shell commands the CALLER declares")
                    && description.contains("Never inferred from the prompt"),
                "{name}: the variable must tell the caller what it is and that it is never \
                 extracted from the prompt: {}",
                description
            );
            assert!(
                !graph.initial_state.contains_key("verification_commands"),
                "{name}: an initial_state key would shadow the declared variable"
            );
            let node = graph
                .nodes
                .get("parse")
                .unwrap_or_else(|| panic!("{name} graph must have a parse node"));
            let NodeType::Llm(parse) = &node.node_type else {
                panic!("{name}: parse must be an llm node");
            };
            let schema = parse
                .output_schema
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: parse must have an output_schema"));
            let properties = schema["properties"].as_object().unwrap_or_else(|| {
                panic!(
                    "{name}: parse output_schema must declare `properties`; the engine merges \
                     only declared keys into state: {schema}"
                )
            });
            assert!(
                !properties.contains_key("verification_commands"),
                "{name}: parse must not extract verification_commands: {schema}"
            );
            assert!(
                !schema["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("verification_commands")),
                "{name}: parse must not require verification_commands: {schema}"
            );
            assert!(
                !parse
                    .instructions
                    .as_deref()
                    .is_some_and(|s| s.contains("verification_commands")),
                "{name}: parse instructions must not mention verification_commands: {:?}",
                parse.instructions
            );
        }
    }

    // ---- code-reviewer suite-script regression tests ----
    //
    // Fail-closed fault paths of the code-reviewer's verdict/fault/render
    // scripts, exercised the same way as the suites above: `python3 <script>`
    // with a synthetic GRAPH_STATE env.

    fn run_code_reviewer_script(script: &str, state: &serde_json::Value) -> serde_json::Value {
        run_code_reviewer_script_raw(script, &state.to_string())
    }

    fn run_code_reviewer_script_raw(script: &str, raw_state: &str) -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/code-reviewer/scripts")
            .join(script);
        let out = std::process::Command::new("python3")
            .arg(&path)
            .env("GRAPH_STATE", raw_state)
            .env_remove("GRAPH_STATE_FILE")
            .output()
            .unwrap_or_else(|e| panic!("failed to invoke python3 {script}: {e}"));
        assert!(
            out.status.success(),
            "{script} exited nonzero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{script} stdout is not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    #[test]
    fn code_reviewer_synthesize_retries_and_fails_closed() {
        use crate::graph::{GraphParser, NodeType};
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/code-reviewer");
        let graph = GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .expect("code-reviewer graph.yaml must parse");

        // synthesize: retries once, then falls back to verdict with the
        // failure text captured in synth_failure for the fault rendering.
        let synthesize = graph.get_node("synthesize").unwrap();
        let NodeType::Llm(llm) = &synthesize.node_type else {
            panic!("synthesize must be an llm node")
        };
        assert_eq!(llm.max_attempts, 2, "synthesize must retry once");
        assert_eq!(llm.fallback.as_deref(), Some("verdict"));
        assert!(
            llm.state_updates
                .as_ref()
                .is_some_and(|u| u.contains_key("synth_failure")),
            "synthesize must capture its failure text for the verdict"
        );
        assert_eq!(synthesize.next_target(), Some("verify"));

        // verify: same retry-then-fail-closed shape.
        let NodeType::Agent(verify) = &graph.get_node("verify").unwrap().node_type else {
            panic!("verify must be an agent node")
        };
        assert_eq!(verify.max_attempts, 2, "verify must retry once");
        assert_eq!(verify.fallback.as_deref(), Some("verdict"));
    }

    #[test]
    fn code_reviewer_verdict_domain_fault_forces_needs_human() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "changed_files": ["a.rs"],
            "domain_reports": ["> ⚠️ PIPELINE-FAULT: domain review lane failed after retries — domain 'engine' (files: a.rs): Agent node failed: dead"],
            "findings": []
        });
        let out = run_code_reviewer_script("verdict.py", &state);
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "NEEDS-HUMAN",
            "a faulted domain lane must force NEEDS-HUMAN: {out}"
        );
        let first = v["attention"][0].as_str().unwrap();
        assert!(
            first.starts_with("PIPELINE-FAULT:"),
            "the fault must lead the attention list: {out}"
        );
    }

    #[test]
    fn code_reviewer_verdict_synthesis_fault_blocks() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Keyed off synth_failure, never off findings == [] — the empty
        // findings list here is exactly what a dead synthesis leaves behind.
        let state = json!({
            "changed_files": ["a.rs"],
            "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
            "findings": [],
            "synth_failure": "LLM node failed: boom"
        });
        let out = run_code_reviewer_script("verdict.py", &state);
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "NEEDS-HUMAN",
            "a dead synthesis must block: {out}"
        );
        assert!(
            v["attention"].as_array().unwrap().iter().any(|a| a
                .as_str()
                .unwrap_or("")
                .contains("PIPELINE-FAULT: synthesis failed")),
            "the synthesis fault must be in attention: {out}"
        );
    }

    #[test]
    fn code_reviewer_verdict_verifier_fault_attention() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "changed_files": ["a.rs"],
            "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
            "findings": [{
                "id": "f1", "severity": "🟡 WARNING", "marker": "",
                "file": "a.rs", "lines": "10", "title": "possible issue",
                "block": "#### possible issue"
            }],
            "verifier_output": "Agent node failed: dead"
        });
        let out = run_code_reviewer_script("verdict.py", &state);
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "NEEDS-HUMAN",
            "a dead verifier must block: {out}"
        );
        assert!(
            v["attention"].as_array().unwrap().iter().any(|a| a
                .as_str()
                .unwrap_or("")
                .contains("PIPELINE-FAULT: finding verification failed")),
            "the verifier fault must be in attention: {out}"
        );
        let block = v["findings_final"][0]["block"].as_str().unwrap();
        assert!(
            block.contains("(unverified: no verifier verdict returned"),
            "the kept finding must render as unverified: {out}"
        );
    }

    fn fv_degraded_fault(out: &serde_json::Value) -> Option<String> {
        out["verdict_out"]["attention"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a.as_str())
            .find(|a| a.starts_with("PIPELINE-FAULT: finding verification degraded"))
            .map(str::to_owned)
    }

    #[test]
    fn code_reviewer_verdict_fv_duplicate_unknown_faults_count_individually() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // finding-verifier's crash guard emits every fault entry under the
        // same "unknown" id; an id-indexed dict would collapse them to one.
        let entry = json!({
            "id": "unknown",
            "verdict": "UNVERIFIABLE",
            "note": "PIPELINE-FAULT: verdict gate error: x"
        });
        let state = json!({
            "changed_files": ["a.rs"],
            "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
            "findings": [],
            "verifier_output": json!([entry, entry]).to_string()
        });
        let out = run_code_reviewer_script("verdict.py", &state);
        assert_eq!(out["verdict_out"]["verdict"], "NEEDS-HUMAN", "{out}");
        let fault = fv_degraded_fault(&out).unwrap_or_else(|| panic!("{out}"));
        assert!(
            fault.contains("2 verifier verdict(s)"),
            "each crash-guard entry must count: {fault}"
        );
    }

    #[test]
    fn code_reviewer_verdict_fv_parse_fault_forces_needs_human() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // finding-verifier completes "successfully" with parse_fault's
        // sentinel entry inside the payload — not an "Agent node failed:"
        // banner — so the gate must read the sentinel id as a fault.
        let state = json!({
            "changed_files": ["a.rs"],
            "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
            "findings": [{
                "id": "f1", "severity": "🟡 WARNING", "marker": "",
                "file": "a.rs", "lines": "10", "title": "possible issue",
                "block": "#### possible issue"
            }],
            "verifier_output": "FINDING_VERIFIER_RESULTS\n[{\"id\":\"pipeline-fault\",\"verdict\":\"UNVERIFIABLE\",\"evidence\":\"\",\"note\":\"PIPELINE-FAULT: parse failed — findings could not be extracted…\"}]"
        });
        let out = run_code_reviewer_script("verdict.py", &state);
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "NEEDS-HUMAN",
            "a parse-faulted verifier payload must block: {out}"
        );
        assert!(
            v["reason"]
                .as_str()
                .unwrap()
                .contains("pipeline fault(s) recorded"),
            "the reason must name the fault: {out}"
        );
        let fault = fv_degraded_fault(&out).unwrap_or_else(|| {
            panic!("the degraded-verification fault must be in attention: {out}")
        });
        assert!(
            fault.contains("1 verifier verdict(s) carry a pipeline fault"),
            "the fault must count the faulted verdicts: {fault}"
        );
    }

    #[test]
    fn code_reviewer_verdict_fv_lane_fault_forces_needs_human() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // verify_fault's per-finding entry keeps the real finding id; the
        // fault is recognized by the note's PIPELINE-FAULT prefix.
        let state = json!({
            "changed_files": ["a.rs"],
            "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
            "findings": [{
                "id": "f1", "severity": "🟡 WARNING", "marker": "",
                "file": "a.rs", "lines": "10", "title": "possible issue",
                "block": "#### possible issue"
            }],
            "verifier_output": "[{\"id\":\"f1\",\"verdict\":\"UNVERIFIABLE\",\"evidence\":\"\",\"note\":\"PIPELINE-FAULT: verifier lane failed after retries — LLM node failed: x\"}]"
        });
        let out = run_code_reviewer_script("verdict.py", &state);
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "NEEDS-HUMAN",
            "a lane-faulted verifier verdict must block: {out}"
        );
        let fault = fv_degraded_fault(&out).unwrap_or_else(|| {
            panic!("the degraded-verification fault must be in attention: {out}")
        });
        assert!(
            fault.contains("verifier lane failed after retries"),
            "the fault must carry the verifier's note: {fault}"
        );
    }

    #[test]
    fn code_reviewer_verdict_quoted_fault_in_note_body_not_a_fault() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // PREFIX-anchored on the note field: a verdict whose evidence/note
        // merely QUOTES the marker mid-text is a real verdict, not a fault.
        let state = json!({
            "changed_files": ["a.rs"],
            "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
            "findings": [{
                "id": "f1", "severity": "🟢 SUGGESTION", "marker": "",
                "file": "a.rs", "lines": "10", "title": "minor",
                "block": "#### minor"
            }],
            "verifier_output": "[{\"id\":\"f1\",\"verdict\":\"VERIFIED\",\"evidence\":\"the code mentions 'PIPELINE-FAULT:' at line 3\",\"note\":\"mentions PIPELINE-FAULT: mid-text\"}]"
        });
        let out = run_code_reviewer_script("verdict.py", &state);
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "MERGE-READY",
            "a quoted marker in a verdict body must not read as a fault: {out}"
        );
        assert!(
            fv_degraded_fault(&out).is_none(),
            "no degraded-verification fault may be recorded: {out}"
        );
    }

    #[test]
    fn code_reviewer_verdict_quoted_marker_precedence() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // PREFIX-anchored fault detection: a clean report that merely QUOTES
        // the banner mid-text must not read as a faulted lane.
        let state = json!({
            "changed_files": ["a.rs"],
            "domain_reports": ["The gate prepends '> ⚠️ PIPELINE-FAULT:' when a lane dies; this slice reviewed that logic and it is correct. DOMAIN_REVIEW_COMPLETE"],
            "findings": []
        });
        let out = run_code_reviewer_script("verdict.py", &state);
        assert_eq!(
            out["verdict_out"]["verdict"], "MERGE-READY",
            "a quoted marker mid-text must not trip the fault check: {out}"
        );
    }

    #[test]
    fn code_reviewer_verdict_happy_path_regression() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "changed_files": ["a.rs"],
            "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
            "findings": [],
            "verifier_output": "",
            "parse_failure": "",
            "refine_failure": "",
            "aux_failure": "",
            "synth_failure": ""
        });
        let out = run_code_reviewer_script("verdict.py", &state);
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "MERGE-READY",
            "a clean run must stay MERGE-READY: {out}"
        );
        assert_eq!(
            v["reason"], "no blocking findings and no always-human triggers",
            "the happy-path reason must be byte-compatible: {out}"
        );
    }

    #[test]
    fn code_reviewer_domain_fault_normalizes() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "domain_group": {"domain": "engine", "files": ["a.rs"]},
            "domain_report": "Agent node failed: spawn dead\nline2"
        });
        let out = run_code_reviewer_script("domain_fault.py", &state);
        let report = out["domain_report"].as_str().unwrap();
        assert!(
            report.starts_with("> ⚠️ PIPELINE-FAULT: domain review lane failed after retries — "),
            "the banner prefix verdict.py anchors on must be exact: {report}"
        );
        assert!(
            report.contains("domain 'engine' (files: a.rs)"),
            "the fault must name the slice: {report}"
        );
        assert!(
            !report.contains('\n'),
            "the failure detail must be newline-normalized: {report}"
        );
    }

    #[test]
    fn code_reviewer_parse_fault_emits_needs_human() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_code_reviewer_script(
            "parse_fault.py",
            &json!({"parse_failure": "LLM node failed: x"}),
        );
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "NEEDS-HUMAN",
            "a dead parse must block: {out}"
        );
        assert!(
            v["reason"]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: parse failed"),
            "the reason must carry the fault marker: {out}"
        );
    }

    #[test]
    fn code_reviewer_parse_fault_renders_well_formed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A parse fault dies before facts, so changed_files is empty — the
        // fault verdict must still render as a full report, not the
        // "No changes to review." stub.
        let fault = run_code_reviewer_script(
            "parse_fault.py",
            &json!({"parse_failure": "LLM node failed: x"}),
        );
        let state = json!({
            "changed_files": [],
            "verdict_out": fault["verdict_out"].clone()
        });
        let out = run_code_reviewer_script("render.py", &state);
        let report = out["final_report"].as_str().unwrap();
        assert!(
            report.contains("**Verdict: NEEDS-HUMAN**"),
            "the fault verdict must render: {report}"
        );
        assert!(
            report.contains("PIPELINE-FAULT: parse failed"),
            "the fault reason must render: {report}"
        );
    }

    #[test]
    fn code_reviewer_render_no_changes_regression() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "changed_files": [],
            "verdict_out": {
                "verdict": "MERGE-READY",
                "reason": "no blocking findings and no always-human triggers"
            }
        });
        let out = run_code_reviewer_script("render.py", &state);
        assert!(
            out["final_report"]
                .as_str()
                .unwrap()
                .contains("No changes to review."),
            "an empty non-fault diff must keep the stub report: {out}"
        );
    }

    #[test]
    fn code_reviewer_aux_fault_normalizes() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_code_reviewer_script(
            "aux_fault.py",
            &json!({"aux_failure": "LLM node failed: y"}),
        );
        assert!(
            out["aux_note"]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: aux context lanes failed after retries —"),
            "the aux fault must land in aux_note: {out}"
        );
    }

    #[test]
    fn code_reviewer_cover_gate_notes_refine_failure() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // refine_groups' fallback lands in cover_gate with `groups` never set;
        // the deterministic proposal must win and the degradation must be
        // noted for the synthesis prompt.
        let state = json!({
            "changed_files": ["a.rs"],
            "proposed_groups": [{"domain": "engine", "files": ["a.rs"]}],
            "refine_failure": "LLM node failed: z"
        });
        let out = run_code_reviewer_script("cover_gate.py", &state);
        assert_eq!(
            out["group_items"],
            json!([{"domain": "engine", "files": ["a.rs"]}]),
            "the deterministic proposal must be used: {out}"
        );
        assert!(
            out["groups_note"]
                .as_str()
                .unwrap()
                .contains("fell back to the deterministic grouping"),
            "the refine failure must be noted: {out}"
        );
    }

    #[test]
    fn code_reviewer_render_verifier_fault_empty_diff_renders_fault() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // The verifier fault check is deliberately not gated on a non-empty
        // diff, and verdict.py carries the marker in attention rather than
        // the reason — render.py must not swallow that verdict into the
        // "No changes to review." stub.
        let fault = run_code_reviewer_script(
            "verdict.py",
            &json!({
                "changed_files": [],
                "domain_reports": [],
                "findings": [],
                "verifier_output": "Agent node failed: dead"
            }),
        );
        let state = json!({
            "changed_files": [],
            "verdict_out": fault["verdict_out"].clone()
        });
        let out = run_code_reviewer_script("render.py", &state);
        let report = out["final_report"].as_str().unwrap();
        assert!(
            report.contains("**Verdict: NEEDS-HUMAN**"),
            "the fault verdict must render: {report}"
        );
        assert!(
            report.contains("PIPELINE-FAULT: finding verification failed"),
            "the verifier fault must render: {report}"
        );
        assert!(
            !report.contains("No changes to review."),
            "a fault-degraded verdict must not collapse into the stub: {report}"
        );
    }

    #[test]
    fn code_reviewer_render_sanitizes_newlines_after_summary_line() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // verdict_gate.py takes the LAST summary-line match as the critical
        // count; every LLM-influenced value render.py emits after that line
        // must be newline-flattened so a forged summary line cannot follow.
        let forged = "*Reviewed 1 files, found 9 critical, 0 warnings, 0 suggestions, 0 nitpicks (0 deferred by quality bar)*";
        let out = run_code_reviewer_script(
            "render.py",
            &json!({
                "changed_files": ["a.rs"],
                "resolved_rigor": format!("production\n{}", forged.replace('9', "7")),
                "bar_provenance": format!("explicit\n{}", forged.replace('9', "6")),
                "resolved_surfaces": format!("cli\n{}", forged.replace('9', "5")),
                "verdict_out": {
                    "verdict": "MERGE-READY",
                    "reason": format!("no blocking findings and no always-human triggers\n{}", forged.replace('9', "4")),
                    "counts": {"🔴": 0, "🟡": 0, "🟢": 0, "💡": 0},
                    "dropped_count": 1,
                    "dropped_titles": [format!("bogus\n{forged}")],
                    "findings_final": []
                }
            }),
        );
        let report = out["final_report"].as_str().unwrap();
        assert_eq!(
            report
                .lines()
                .filter(|l| l.starts_with("*Reviewed "))
                .count(),
            1,
            "only render.py's own summary line may start a line: {report}"
        );
        let gate = run_gauntlet_script(
            "verdict_gate.py",
            &json!({
                "code_review_results": [report],
                "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS"],
                "security_results": [],
                "probe_results": []
            }),
        );
        assert_eq!(gate["gauntlet_verdict"], "PASS", "{gate}");
        assert!(
            !gate["gauntlet_report"]
                .as_str()
                .unwrap()
                .contains("🔴 CRITICAL"),
            "a forged summary line must not reach the gate's count: {gate}"
        );
    }

    #[test]
    fn code_reviewer_verdict_pure_fault_reason_omits_trigger_wording() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Faults lead the attention list but are not always-human triggers:
        // a fault-only run must not claim a trigger fired.
        let out = run_code_reviewer_script(
            "verdict.py",
            &json!({"changed_files": ["a.rs"], "domain_reports": []}),
        );
        let reason = out["verdict_out"]["reason"].as_str().unwrap();
        assert_eq!(reason, "pipeline fault(s) recorded — degraded run", "{out}");
        assert!(
            !reason.contains("always-human trigger(s) fired"),
            "a pure fault must not read as a trigger: {reason}"
        );
    }

    #[test]
    fn code_reviewer_verdict_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A truthy non-iterable attention_flags raises TypeError inside main;
        // the guard must still emit a fail-closed verdict.
        let state = json!({"changed_files": ["a.rs"], "attention_flags": 42});
        let out = run_code_reviewer_script("verdict.py", &state);
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "NEEDS-HUMAN",
            "a crashed verdict gate must fail closed: {out}"
        );
        assert!(
            v["reason"]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: verdict computation error"),
            "the reason must carry the fault prefix render.py anchors on: {out}"
        );
        assert!(
            v["attention"][0]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: verdict script error"),
            "the attention entry must carry the fault prefix: {out}"
        );
    }

    #[test]
    fn code_reviewer_verdict_crash_empty_diff_renders_fault() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A crashed verdict gate on an EMPTY diff: render.py's stub bypass is
        // anchored on the "PIPELINE-FAULT:" prefix, so the crash-guard verdict
        // must carry it — otherwise the fail-closed NEEDS-HUMAN collapses into
        // the "No changes to review." stub and never reaches the final report.
        let fault = run_code_reviewer_script(
            "verdict.py",
            &json!({"changed_files": [], "attention_flags": 42}),
        );
        let state = json!({
            "changed_files": [],
            "verdict_out": fault["verdict_out"].clone()
        });
        let out = run_code_reviewer_script("render.py", &state);
        let report = out["final_report"].as_str().unwrap();
        assert!(
            report.contains("**Verdict: NEEDS-HUMAN**"),
            "the crash verdict must render: {report}"
        );
        assert!(
            report.contains("PIPELINE-FAULT: verdict computation error"),
            "the crash reason must render: {report}"
        );
        assert!(
            !report.contains("No changes to review."),
            "a crashed gate on an empty diff must not collapse into the stub: {report}"
        );
    }

    #[test]
    fn code_reviewer_verdict_fault_survives_attention_cap() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Faults lead attention and the [:5] cap applies only to the
        // non-fault entries — pack 7 user flags alongside a verifier fault
        // and assert 1 fault + 5 capped flags, fault first.
        let state = json!({
            "changed_files": ["a.rs"],
            "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
            "findings": [],
            "verifier_output": "Agent node failed: dead",
            "attention_flags": ["f1", "f2", "f3", "f4", "f5", "f6", "f7"]
        });
        let out = run_code_reviewer_script("verdict.py", &state);
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "NEEDS-HUMAN",
            "a dead verifier must block: {out}"
        );
        let attention = v["attention"].as_array().unwrap();
        assert_eq!(
            attention.len(),
            6,
            "attention must be 1 uncapped fault + 5 capped flags: {out}"
        );
        assert!(
            attention[0]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: finding verification failed"),
            "the fault must survive the attention cap by leading the list: {out}"
        );
        assert_eq!(
            attention[5], "f5",
            "the cap must apply to the non-fault entries only: {out}"
        );
    }

    #[test]
    fn code_reviewer_parse_fault_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Unparseable state makes load_state raise before main runs; the
        // fault marker itself must still emit a prefix-anchored fault verdict.
        let out = run_code_reviewer_script_raw("parse_fault.py", "not json");
        let v = &out["verdict_out"];
        assert_eq!(
            v["verdict"], "NEEDS-HUMAN",
            "a crashed parse-fault marker must fail closed: {out}"
        );
        assert!(
            v["reason"]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: parse failed"),
            "the fault prefix must survive a crash: {out}"
        );
        assert!(
            v["reason"]
                .as_str()
                .unwrap()
                .contains("fault-marker script error"),
            "the crash must be named: {out}"
        );
    }

    #[test]
    fn code_reviewer_aux_fault_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Unparseable state makes load_state raise before main runs; the
        // fault marker itself must still emit the prefixed aux_note.
        let out = run_code_reviewer_script_raw("aux_fault.py", "not json");
        let note = out["aux_note"].as_str().unwrap();
        assert!(
            note.starts_with("PIPELINE-FAULT: aux context lanes failed after retries —"),
            "the aux fault prefix must survive a crash: {note}"
        );
        assert!(
            note.contains("fault-marker script error"),
            "the crash must be named: {note}"
        );
    }

    #[test]
    fn code_reviewer_cover_gate_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A domain-less proposed group raises KeyError while building the
        // fallback grouping; the guard must emit one catch-all group, never
        // an empty review.
        let state = json!({"changed_files": ["a.rs"], "proposed_groups": [{}]});
        let out = run_code_reviewer_script("cover_gate.py", &state);
        assert_eq!(
            out["group_items"],
            json!([{"domain": "all-changes", "files": ["a.rs"]}]),
            "a crashed gate must fall back to a catch-all group: {out}"
        );
        assert!(
            out["groups_note"]
                .as_str()
                .unwrap()
                .contains("cover gate error"),
            "the crash must be noted: {out}"
        );
    }

    #[test]
    fn code_reviewer_domain_fault_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Unparseable state makes load_state raise before main runs; the
        // fault marker itself must still emit the banner-prefixed report.
        let out = run_code_reviewer_script_raw("domain_fault.py", "not json");
        let report = out["domain_report"].as_str().unwrap();
        assert!(
            report.starts_with("> ⚠️ PIPELINE-FAULT: domain review lane failed after retries — "),
            "the banner prefix verdict.py anchors on must survive a crash: {report}"
        );
        assert!(
            report.contains("fault-marker script error"),
            "the crash must be named: {report}"
        );
    }

    // ---- finding-verifier suite-script regression tests ----
    //
    // Fail-closed fault paths of the finding-verifier's marker scripts,
    // exercised the same way as the suites above: `python3 <script>` with a
    // synthetic GRAPH_STATE env.

    fn run_fv_script(script: &str, state: &serde_json::Value) -> serde_json::Value {
        run_fv_script_raw(script, &state.to_string())
    }

    fn run_fv_script_raw(script: &str, raw_state: &str) -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/finding-verifier/scripts")
            .join(script);
        let out = std::process::Command::new("python3")
            .arg(&path)
            .env("GRAPH_STATE", raw_state)
            .env_remove("GRAPH_STATE_FILE")
            .output()
            .unwrap_or_else(|e| panic!("failed to invoke python3 {script}: {e}"));
        assert!(
            out.status.success(),
            "{script} exited nonzero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{script} stdout is not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    #[test]
    fn finding_verifier_parse_fault_emits_unverifiable_entry() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_fv_script(
            "parse_fault.py",
            &json!({"parse_failure": "LLM node failed: model exploded\nline2"}),
        );
        let verdicts = out["verdicts"].as_array().unwrap();
        assert_eq!(verdicts.len(), 1, "exactly one fault entry: {out}");
        let v = &verdicts[0];
        assert_eq!(v["id"], "pipeline-fault", "{out}");
        assert_eq!(v["verdict"], "UNVERIFIABLE", "{out}");
        assert_eq!(v["evidence"], "", "{out}");
        let note = v["note"].as_str().unwrap();
        assert!(
            note.starts_with("PIPELINE-FAULT: parse failed"),
            "the note must carry the fault prefix: {note}"
        );
        assert!(
            note.contains("model exploded") && !note.contains('\n'),
            "the failure detail must be carried, newline-normalized: {note}"
        );
    }

    #[test]
    fn finding_verifier_parse_fault_without_detail_degrades() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // parse_failure not holding an engine failure string (unset here)
        // must degrade to a generic detail, never echo unrelated state.
        let out = run_fv_script("parse_fault.py", &json!({}));
        let note = out["verdicts"][0]["note"].as_str().unwrap();
        assert!(
            note.starts_with("PIPELINE-FAULT: parse failed"),
            "the fault prefix must not depend on the detail: {note}"
        );
        assert!(
            note.contains("died without recording a failure detail"),
            "a missing detail must be named as such: {note}"
        );
    }

    #[test]
    fn finding_verifier_parse_fault_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Unparseable state makes load_state raise before main runs; the
        // fault marker itself must still emit a schema-conformant verdict.
        let out = run_fv_script_raw("parse_fault.py", "not json");
        let v = &out["verdicts"][0];
        assert_eq!(
            v["verdict"], "UNVERIFIABLE",
            "a crashed parse-fault marker must fail closed: {out}"
        );
        let note = v["note"].as_str().unwrap();
        assert!(
            note.starts_with("PIPELINE-FAULT: parse failed"),
            "the fault prefix must survive a crash: {note}"
        );
        assert!(
            note.contains("fault-marker script error"),
            "the crash must be named: {note}"
        );
    }

    #[test]
    fn finding_verifier_verify_fault_normalizes_failure_text() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // The engine wrote the failure text into `verdict` via verify_one's
        // state_updates before routing here; the marker normalizes it into a
        // schema-conformant UNVERIFIABLE verdict with the authoritative id.
        let state = json!({
            "finding": {"id": "f3", "severity": "🔴 CRITICAL", "path": "a.rs", "lines": "10", "claim": "x"},
            "verdict": "LLM node failed: boom\nline2"
        });
        let out = run_fv_script("verify_fault.py", &state);
        let v = &out["verdict"];
        assert_eq!(v["id"], "f3", "the finding's id must be stamped: {out}");
        assert_eq!(v["verdict"], "UNVERIFIABLE", "{out}");
        assert_eq!(v["evidence"], "", "{out}");
        let note = v["note"].as_str().unwrap();
        assert!(
            note.starts_with("PIPELINE-FAULT: verifier lane failed after retries — "),
            "the note must carry the fault prefix: {note}"
        );
        assert!(
            note.contains("boom") && !note.contains('\n'),
            "the failure detail must be carried, newline-normalized: {note}"
        );
    }

    #[test]
    fn finding_verifier_verify_fault_without_detail_degrades() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A `verdict` value that is not the engine's failure string (stale
        // model output here) must degrade to a generic detail — never leak
        // non-fault text into the fault note.
        let state = json!({
            "finding": {"id": "f3"},
            "verdict": "some stale model output"
        });
        let out = run_fv_script("verify_fault.py", &state);
        let v = &out["verdict"];
        assert_eq!(v["id"], "f3", "{out}");
        assert_eq!(v["verdict"], "UNVERIFIABLE", "{out}");
        let note = v["note"].as_str().unwrap();
        assert!(
            note.contains("died without recording a failure detail"),
            "a non-engine detail must be named as missing: {note}"
        );
    }

    #[test]
    fn finding_verifier_verify_fault_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Unparseable state makes load_state raise before main runs; the
        // fault marker itself must still emit a schema-conformant verdict.
        let out = run_fv_script_raw("verify_fault.py", "not json");
        let v = &out["verdict"];
        assert_eq!(v["id"], "unknown", "{out}");
        assert_eq!(
            v["verdict"], "UNVERIFIABLE",
            "a crashed verify-fault marker must fail closed: {out}"
        );
        let note = v["note"].as_str().unwrap();
        assert!(
            note.starts_with("PIPELINE-FAULT: verifier lane failed after retries — "),
            "the fault prefix must survive a crash: {note}"
        );
        assert!(
            note.contains("fault-marker script error"),
            "the crash must be named: {note}"
        );
    }

    #[test]
    fn finding_verifier_verdict_gate_crash_fails_closed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Unparseable state makes load_state raise before main runs; the
        // gate's crash note must carry the PIPELINE-FAULT prefix because
        // code-reviewer's verdict.py anchors fault detection on it.
        let out = run_fv_script_raw("verdict_gate.py", "not json");
        let v = &out["verdict"];
        assert_eq!(v["id"], "unknown", "{out}");
        assert_eq!(
            v["verdict"], "UNVERIFIABLE",
            "a crashed verdict gate must fail closed: {out}"
        );
        let note = v["note"].as_str().unwrap();
        assert!(
            note.starts_with("PIPELINE-FAULT: verdict gate error: "),
            "the crash note must be prefix-anchored as a pipeline fault: {note}"
        );
    }

    #[test]
    fn finding_verifier_verdict_gate_after_retry_unverifiable_is_not_prefixed() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A second malformed response is model misbehavior, not
        // infrastructure: the after-retry verdict must not carry the
        // PIPELINE-FAULT prefix code-reviewer's verdict.py blocks on.
        let out = run_fv_script(
            "verdict_gate.py",
            &json!({"finding": {"id": "f1"}, "gate_attempts": 1, "verdict": "still not json"}),
        );
        let v = &out["verdict"];
        assert_eq!(v["id"], "f1", "{out}");
        assert_eq!(v["verdict"], "UNVERIFIABLE", "{out}");
        let note = v["note"].as_str().unwrap();
        assert!(
            note.contains("failed machine validation after retry"),
            "the after-retry failure must be named: {note}"
        );
        assert!(
            !note.starts_with("PIPELINE-FAULT:"),
            "model misbehavior must not read as a pipeline fault: {note}"
        );
    }

    #[test]
    fn finding_verifier_readme_names_all_fault_emitters() {
        let readme = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets/agents/finding-verifier/README.md"),
        )
        .unwrap();
        assert!(
            !readme.contains("Both fault markers"),
            "finding-verifier README must not undercount the fault emitters"
        );
        assert!(
            readme.contains("All three fault emitters (`parse_fault`, `verify_fault`,")
                && readme.contains(
                    "and `verdict_gate`'s own crash guard) surface as `PIPELINE-FAULT:` text"
                ),
            "finding-verifier README must name all three PIPELINE-FAULT emitters"
        );
    }

    #[test]
    fn finding_verifier_fault_wiring_preserves_sentinel() {
        use crate::graph::{GraphParser, NodeType};
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/finding-verifier");
        let graph = GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .unwrap();

        let NodeType::Llm(parse) = &graph.get_node("parse").unwrap().node_type else {
            panic!("parse must be an llm node")
        };
        assert_eq!(parse.max_attempts, 2, "parse is retried once");
        assert_eq!(parse.fallback.as_deref(), Some("parse_fault"));
        assert!(
            parse
                .state_updates
                .as_ref()
                .is_some_and(|u| u.contains_key("parse_failure")),
            "parse must capture its failure text for the fault marker"
        );
        assert_eq!(
            graph.get_node("parse_fault").unwrap().next_target(),
            Some("done"),
            "the parse fault marker must continue to the sentinel-emitting end"
        );

        let NodeType::Llm(verify) = &graph.get_node("verify_one").unwrap().node_type else {
            panic!("verify_one must be an llm node")
        };
        assert_eq!(verify.fallback.as_deref(), Some("verify_fault"));
        assert!(
            graph.get_node("verify_fault").unwrap().next.is_none(),
            "verify_fault must end the branch chain so the map collects the verdict"
        );

        let NodeType::End(done) = &graph.get_node("done").unwrap().node_type else {
            panic!("done must be an end node")
        };
        assert!(
            done.output.starts_with("FINDING_VERIFIER_RESULTS"),
            "the sentinel must lead the output: {}",
            done.output
        );
        assert!(
            done.output.contains("{{verdicts}}"),
            "the output must interpolate the collected verdicts: {}",
            done.output
        );
    }

    // ---- step-runner fault-wiring regression tests ----
    //
    // route_review.sh is a bash script node; the graph's script executor runs
    // `.sh` scripts through bash with the same GRAPH_STATE env contract, so
    // it is exercised the same way as the python suites above (guarded on
    // bash + jq, which the script requires).

    fn run_step_runner_script(script: &str, state: &serde_json::Value) -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/step-runner/scripts")
            .join(script);
        let out = std::process::Command::new("bash")
            .arg(&path)
            .env("GRAPH_STATE", state.to_string())
            .env_remove("GRAPH_STATE_FILE")
            .output()
            .unwrap_or_else(|e| panic!("failed to invoke bash {script}: {e}"));
        assert!(
            out.status.success(),
            "{script} exited nonzero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{script} stdout is not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    /// Gate for the bash-harness tests below. The step-runner `.sh` scripts
    /// are POSIX-oriented: git-bash on Windows satisfies `cmd_available` but
    /// the POSIX invocation (path/quoting/env semantics) fails there, so
    /// Windows always skips; the scripts are exercised on unix runners.
    fn skip_step_runner_bash_harness() -> bool {
        if cfg!(windows) {
            eprintln!("skipping: POSIX bash script harness");
            return true;
        }
        if !cmd_available("bash") || !cmd_available("jq") {
            eprintln!("skipping: bash/jq not available");
            return true;
        }
        false
    }

    #[test]
    fn step_runner_route_review_fault_text_skips_fix_loop() {
        if skip_step_runner_bash_harness() {
            return;
        }
        // The engine's failure text must never be mistaken for review
        // findings — even a 🔴 embedded in the error chain must not spend a
        // fix-loop attempt (the guard sits BEFORE the 🔴 grep).
        let state = json!({
            "review_report": "Agent node failed: reviewer died mid-report: 🔴 CRITICAL",
            "review_attempts": 0,
            "max_review_attempts": 1
        });
        let out = run_step_runner_script("route_review.sh", &state);
        assert_eq!(
            out,
            json!({"_next": "write_handoff"}),
            "a fault report must route straight to the handoff"
        );
    }

    #[test]
    fn step_runner_route_review_critical_finding_still_loops() {
        if skip_step_runner_bash_harness() {
            return;
        }
        // Non-fault routing regression: a real 🔴 report still enters the
        // bounded fix loop exactly as before.
        let state = json!({
            "review_report": "🔴 CRITICAL: bug in a.rs",
            "review_attempts": 0,
            "max_review_attempts": 1
        });
        let out = run_step_runner_script("route_review.sh", &state);
        assert_eq!(out["_next"], "implement", "{out}");
        assert_eq!(out["review_attempts"], 1, "{out}");
        assert_eq!(out["needs_independent_review"], false, "{out}");
        assert!(
            out["fix_instructions"]
                .as_str()
                .unwrap()
                .contains("🔴 CRITICAL: bug in a.rs"),
            "the findings must reach the implementer verbatim: {out}"
        );
    }

    #[test]
    fn step_runner_route_review_clean_report_proceeds() {
        if skip_step_runner_bash_harness() {
            return;
        }
        let out =
            run_step_runner_script("route_review.sh", &json!({"review_report": "all clean 🟢"}));
        assert_eq!(
            out,
            json!({"_next": "write_handoff"}),
            "a clean report must proceed to the handoff unchanged"
        );
    }

    #[test]
    fn step_runner_note_llm_fault_orient_failure() {
        if skip_step_runner_bash_harness() {
            return;
        }
        let state = json!({
            "orient_failure": "LLM node failed: boom",
            "handoff_failure": ""
        });
        let out = run_step_runner_script("note_llm_fault.sh", &state);
        assert_eq!(
            out,
            json!({"fault_note": "PIPELINE-FAULT: orient stage failed — LLM node failed: boom"})
        );
    }

    #[test]
    fn step_runner_note_llm_fault_handoff_failure() {
        if skip_step_runner_bash_harness() {
            return;
        }
        // On success orient_failure holds the node's structured JSON output,
        // which never starts with the engine's "LLM node" prefix — only the
        // handoff fault must be reported.
        let state = json!({
            "orient_failure": "{\"plan_summary\":\"ok\"}",
            "handoff_failure": "LLM node structured-extraction failed: bad schema"
        });
        let out = run_step_runner_script("note_llm_fault.sh", &state);
        assert!(
            out["fault_note"].as_str().unwrap().starts_with(
                "PIPELINE-FAULT: handoff stage failed — LLM node structured-extraction failed:"
            ),
            "{out}"
        );
    }

    #[test]
    fn step_runner_note_llm_fault_clean_path() {
        if skip_step_runner_bash_harness() {
            return;
        }
        let state = json!({
            "orient_failure": "{\"plan_summary\":\"ok\"}",
            "handoff_failure": ""
        });
        let out = run_step_runner_script("note_llm_fault.sh", &state);
        assert_eq!(out, json!({"fault_note": ""}));
    }

    #[test]
    fn step_runner_note_llm_fault_survives_empty_state() {
        if skip_step_runner_bash_harness() {
            return;
        }
        // A fault-noting script must never itself kill the pipeline —
        // the harness asserts exit 0 + JSON stdout even for a bare state.
        let out = run_step_runner_script("note_llm_fault.sh", &json!({}));
        assert_eq!(out, json!({"fault_note": ""}));
    }

    #[test]
    fn step_runner_note_llm_fault_truncates_long_failure() {
        if skip_step_runner_bash_harness() {
            return;
        }
        let failure = format!("LLM node failed: {}", "x".repeat(400));
        let out = run_step_runner_script("note_llm_fault.sh", &json!({"orient_failure": failure}));
        let note = out["fault_note"].as_str().unwrap();
        assert!(
            note.starts_with("PIPELINE-FAULT: orient stage failed — LLM node failed:"),
            "{out}"
        );
        assert!(
            note.chars().count() <= 340,
            "fault_note must be bounded to the prefix + 300 chars, got {}: {note}",
            note.chars().count()
        );
    }

    #[test]
    fn step_runner_fault_wiring_preserves_sentinels() {
        use crate::graph::{GraphParser, NodeType};
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/step-runner");
        let graph = GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .unwrap();

        let NodeType::Agent(implement) = &graph.get_node("implement").unwrap().node_type else {
            panic!("implement must be an agent node")
        };
        assert_eq!(implement.fallback.as_deref(), Some("end_failure"));
        assert_eq!(
            implement.max_attempts, 1,
            "rerunning a coder that died mid-edit against a mutated tree is unsafe — never retried"
        );

        let NodeType::Agent(review) = &graph.get_node("independent_review").unwrap().node_type
        else {
            panic!("independent_review must be an agent node")
        };
        assert_eq!(review.fallback.as_deref(), Some("write_handoff"));

        // The llm nodes with fallbacks capture their failure text so the
        // fallback path can see why it was reached (the validator's
        // fallback-capture warning — this is what shrank the step-runner
        // warning baseline to zero).
        // orient and write_handoff route their fallbacks through
        // note_llm_fault, which distills the capture into fault_note;
        // edge_case_sweep still falls back to write_handoff, which renders
        // sweep_failure in its prompt.
        for (node_id, key, fallback) in [
            ("orient", "orient_failure", "note_llm_fault"),
            ("edge_case_sweep", "sweep_failure", "write_handoff"),
            ("write_handoff", "handoff_failure", "note_llm_fault"),
        ] {
            let NodeType::Llm(llm) = &graph.get_node(node_id).unwrap().node_type else {
                panic!("{node_id} must be an llm node")
            };
            assert!(
                llm.state_updates
                    .as_ref()
                    .is_some_and(|u| u.contains_key(key)),
                "{node_id} must capture its failure text into {key}"
            );
            assert_eq!(
                llm.fallback.as_deref(),
                Some(fallback),
                "{node_id} must fall back to {fallback}"
            );
        }

        // note_llm_fault sits on the failure path; its own fallback also
        // lands on end_failure so a broken script can never strand the step.
        let note_node = graph.get_node("note_llm_fault").unwrap();
        let NodeType::Script(note) = &note_node.node_type else {
            panic!("note_llm_fault must be a script node")
        };
        assert_eq!(note_node.next_target(), Some("end_failure"));
        assert_eq!(note.fallback.as_deref(), Some("end_failure"));

        // write_handoff must teach the review-fault flag: a review_report
        // holding the engine's failure text is "review DID NOT RUN", flagged
        // prominently — never presented as findings.
        let NodeType::Llm(handoff) = &graph.get_node("write_handoff").unwrap().node_type else {
            panic!("write_handoff must be an llm node")
        };
        assert!(
            handoff
                .instructions
                .as_ref()
                .is_some_and(|i| i.contains("Agent node failed:")),
            "write_handoff instructions must handle the review-fault text"
        );
        // Same treatment for the sweep fault: sweep_failure is interpolated
        // in the prompt and the instructions teach the "LLM node" anchor.
        assert!(
            handoff.prompt.contains("{{sweep_failure}}"),
            "write_handoff prompt must surface the sweep capture"
        );
        assert!(
            handoff
                .instructions
                .as_ref()
                .is_some_and(|i| i.contains("LLM node")),
            "write_handoff instructions must handle the sweep-fault text"
        );

        // end_failure renders sensibly when reached via implement's fallback:
        // STEP_FAILED leads, and every interpolated key has an initial_state
        // default (coder_result carries the failure text via state_updates).
        let NodeType::End(end) = &graph.get_node("end_failure").unwrap().node_type else {
            panic!("end_failure must be an end node")
        };
        assert!(
            end.output.starts_with("STEP_FAILED"),
            "the sentinel must lead the output: {}",
            end.output
        );
        assert!(
            end.output.contains("{{fault_note}}"),
            "end_failure must render the distilled fault note: {}",
            end.output
        );
        for chunk in end.output.split("{{").skip(1) {
            let key = chunk.split("}}").next().unwrap().trim();
            assert!(
                graph.initial_state.contains_key(key),
                "end_failure interpolates '{key}' which has no initial_state default"
            );
        }
    }

    // ---- deep-research suite-script regression tests ----
    //
    // The deep-research graph's crash guards and fault markers, exercised by
    // invoking `python3 <script>` with a synthetic GRAPH_STATE env. The raw
    // runner feeds deliberately malformed JSON to prove every guarded script
    // emits a sane degraded output instead of crashing the node.

    fn run_deep_research_script(script: &str, state: &serde_json::Value) -> serde_json::Value {
        run_deep_research_script_raw(script, &state.to_string())
    }

    fn run_deep_research_script_raw(script: &str, raw_state: &str) -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/agents/deep-research/scripts")
            .join(script);
        let out = std::process::Command::new("python3")
            .arg(&path)
            .env("GRAPH_STATE", raw_state)
            .env_remove("GRAPH_STATE_FILE")
            .output()
            .unwrap_or_else(|e| panic!("failed to invoke python3 {script}: {e}"));
        assert!(
            out.status.success(),
            "{script} exited nonzero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{script} stdout is not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    #[test]
    fn deep_research_scripts_survive_malformed_state() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Fail-safe: every guarded script and fault marker must exit 0 with JSON
        // stdout even when GRAPH_STATE is not JSON at all (the runner
        // asserts both); per-script degraded shapes are pinned below.
        for script in [
            "parse_request.py",
            "bootstrap_research.py",
            "combine_findings.py",
            "reflexion_gate.py",
            "incorporate_feedback.py",
            "verify_sources.py",
            "plan_fault.py",
            "question_fault.py",
            "vet_fault.py",
            "critique_fault.py",
            "synth_fault.py",
        ] {
            let _ = run_deep_research_script_raw(script, "not json {");
        }
    }

    #[test]
    fn deep_research_parse_request_crash_asks_user() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // An unreadable state means the caller's prompt is lost — the sane
        // degraded route is asking the user for the topic directly.
        let out = run_deep_research_script_raw("parse_request.py", "not json {");
        assert_eq!(out["_next"], "ask_topic", "{out}");
        assert!(
            out["pipeline_faults"][0]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: request parsing crashed"),
            "{out}"
        );
    }

    #[test]
    fn deep_research_parse_request_happy_path_unchanged() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out =
            run_deep_research_script("parse_request.py", &json!({"initial_prompt": " quantum "}));
        assert_eq!(out, json!({"topic": "quantum"}));
        let out = run_deep_research_script("parse_request.py", &json!({"initial_prompt": ""}));
        assert_eq!(out, json!({"_next": "ask_topic"}));
    }

    #[test]
    fn deep_research_bootstrap_survives_malformed_state() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // The fan-out source's happy-path output IS its degraded output.
        assert_eq!(
            run_deep_research_script_raw("bootstrap_research.py", "not json {"),
            json!({})
        );
    }

    #[test]
    fn deep_research_combine_findings_crash_degrades() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_deep_research_script_raw("combine_findings.py", "not json {");
        let findings = out["findings"].as_str().unwrap();
        assert!(
            findings.starts_with("PIPELINE-FAULT: combining findings crashed"),
            "{out}"
        );
        assert_eq!(out["pipeline_faults"], json!([findings]), "{out}");
    }

    #[test]
    fn deep_research_combine_findings_happy_path_unchanged() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let state = json!({
            "questions": ["Q1", "Q2"],
            "question_findings": ["finding one", "finding two"],
            "pipeline_faults": []
        });
        let out = run_deep_research_script("combine_findings.py", &state);
        assert_eq!(
            out,
            json!({
                "findings": "## Q1\n\nfinding one\n\n## Q2\n\nfinding two",
                "pipeline_faults": []
            })
        );
    }

    #[test]
    fn deep_research_combine_findings_lifts_question_faults() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // A dead research lane's PIPELINE-FAULT finding (written branch-local
        // by question_fault, where pipeline_faults is out of reach) must be
        // lifted into pipeline_faults so it reaches the Pipeline notes.
        let fault = "PIPELINE-FAULT: question research failed — finding unavailable — LLM node failed: boom (question: \"Q2\")";
        let out = run_deep_research_script(
            "combine_findings.py",
            &json!({
                "questions": ["Q1", "Q2"],
                "question_findings": ["finding one", fault],
                "pipeline_faults": []
            }),
        );
        assert_eq!(out["pipeline_faults"], json!([fault]), "{out}");
        assert!(out["findings"].as_str().unwrap().contains(fault), "{out}");
        // Reflexion/feedback loops re-run the map: the lift must deduplicate.
        let out = run_deep_research_script(
            "combine_findings.py",
            &json!({
                "questions": ["Q1"],
                "question_findings": [fault],
                "pipeline_faults": [fault]
            }),
        );
        assert_eq!(out["pipeline_faults"], json!([fault]), "{out}");
    }

    #[test]
    fn deep_research_reflexion_gate_crash_fails_forward() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Same direction as the documented malformed-critique PASS default:
        // a broken gate costs the automated critique routing, never the run.
        let out = run_deep_research_script_raw("reflexion_gate.py", "not json {");
        assert_eq!(out["_next"], "synthesize", "{out}");
        assert!(
            out["pipeline_faults"][0]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: reflexion gate crashed"),
            "{out}"
        );
    }

    #[test]
    fn deep_research_reflexion_gate_happy_paths_unchanged() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_deep_research_script(
            "reflexion_gate.py",
            &json!({"critique": "VERDICT: REVISE\nFEEDBACK: missing X", "research_attempts": 0}),
        );
        assert_eq!(out["_next"], "research_each_question", "{out}");
        assert_eq!(out["research_attempts"], 1, "{out}");
        let out = run_deep_research_script(
            "reflexion_gate.py",
            &json!({"critique": "VERDICT: PASS\nFEEDBACK: none", "research_attempts": 0}),
        );
        assert_eq!(out, json!({"_next": "synthesize"}));
    }

    #[test]
    fn deep_research_incorporate_feedback_crash_still_loops() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // The user's intent (another pass) is unambiguous even when their
        // feedback text is lost — the degraded route still loops.
        let out = run_deep_research_script_raw("incorporate_feedback.py", "not json {");
        assert_eq!(out["_next"], "research_each_question", "{out}");
        assert_eq!(out["research_attempts"], 0, "{out}");
        assert!(
            out["research_feedback"]
                .as_str()
                .unwrap()
                .contains("could not be recovered"),
            "{out}"
        );
        assert!(
            out["pipeline_faults"][0]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: feedback incorporation crashed"),
            "{out}"
        );
    }

    #[test]
    fn deep_research_incorporate_feedback_happy_path_unchanged() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out =
            run_deep_research_script("incorporate_feedback.py", &json!({"decision": "add X"}));
        assert_eq!(out["_next"], "research_each_question", "{out}");
        assert_eq!(out["research_attempts"], 0, "{out}");
        assert!(
            out["research_feedback"].as_str().unwrap().contains("add X"),
            "{out}"
        );
        assert!(out.get("pipeline_faults").is_none(), "{out}");
    }

    #[test]
    fn deep_research_verify_sources_crash_degrades() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_deep_research_script_raw("verify_sources.py", "not json {");
        assert!(
            out["source_check"]
                .as_str()
                .unwrap()
                .contains("NOT checked"),
            "{out}"
        );
        assert!(
            out["pipeline_notes"]
                .as_str()
                .unwrap()
                .contains("## Pipeline notes"),
            "the crash fault must still render in the notes: {out}"
        );
        assert!(
            out["pipeline_faults"][0]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: source verification crashed"),
            "{out}"
        );
    }

    #[test]
    fn deep_research_verify_sources_folds_pipeline_notes() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // No URLs in the report → no network probes; the notes fold is what
        // this pins. Non-empty faults render the "## Pipeline notes" section…
        let out = run_deep_research_script(
            "verify_sources.py",
            &json!({
                "report": "plain text, no links",
                "pipeline_faults": ["PIPELINE-FAULT: critique failed — LLM node failed: x"]
            }),
        );
        assert_eq!(
            out["source_check"],
            "No web sources were cited in the report."
        );
        assert_eq!(
            out["pipeline_notes"],
            "\n\n## Pipeline notes\n\n- PIPELINE-FAULT: critique failed — LLM node failed: x"
        );
        // …and a fault-free run folds to "" so the accepted-path output
        // ({{report}}{{pipeline_notes}}) stays byte-identical.
        let out = run_deep_research_script(
            "verify_sources.py",
            &json!({"report": "plain text, no links", "pipeline_faults": []}),
        );
        assert_eq!(out["pipeline_notes"], "");
    }

    #[test]
    fn deep_research_plan_fault_normalizes_capture() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_deep_research_script(
            "plan_fault.py",
            &json!({"plan_failure": "LLM node failed: boom", "pipeline_faults": []}),
        );
        assert_eq!(
            out["fault_text"],
            "planning failed — nothing to research: LLM node failed: boom"
        );
        assert_eq!(
            out["pipeline_faults"],
            json!(["PIPELINE-FAULT: planning failed — nothing to research: LLM node failed: boom"])
        );
        // On success plan_failure holds the node's structured JSON output —
        // the prefix anchor must never mistake it for a failure detail.
        let out = run_deep_research_script(
            "plan_fault.py",
            &json!({"plan_failure": "{\"research_plan\":\"ok\"}"}),
        );
        assert!(
            out["fault_text"]
                .as_str()
                .unwrap()
                .contains("died without recording"),
            "{out}"
        );
    }

    #[test]
    fn deep_research_question_fault_normalizes_finding() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_deep_research_script(
            "question_fault.py",
            &json!({"finding": "LLM node failed: boom", "question": "What is X?"}),
        );
        assert_eq!(
            out,
            json!({
                "finding":
                    "PIPELINE-FAULT: question research failed — finding unavailable — LLM node failed: boom (question: \"What is X?\")"
            })
        );
        // state_updates run on success too — a real finding (no "LLM node"
        // prefix) must not leak into the fault detail.
        let out = run_deep_research_script(
            "question_fault.py",
            &json!({"finding": "real research findings", "question": "What is X?"}),
        );
        let finding = out["finding"].as_str().unwrap();
        assert!(
            finding.starts_with("PIPELINE-FAULT: question research failed — finding unavailable —"),
            "{out}"
        );
        assert!(finding.contains("died without recording"), "{out}");
        assert!(finding.contains("(question: \"What is X?\")"), "{out}");

        // Distinct dead lanes with identical engine text must not collapse
        // into one entry when combine_findings dedupes — the question is
        // part of the fault; a missing question degrades to a placeholder.
        let other = run_deep_research_script(
            "question_fault.py",
            &json!({"finding": "LLM node failed: boom", "question": "What is Y?"}),
        );
        assert_ne!(out["finding"], other["finding"], "{out} vs {other}");
        assert!(
            other["finding"]
                .as_str()
                .unwrap()
                .contains("(question: \"What is Y?\")"),
            "{other}"
        );
        let unnamed = run_deep_research_script(
            "question_fault.py",
            &json!({"finding": "LLM node failed: boom"}),
        );
        assert!(
            unnamed["finding"]
                .as_str()
                .unwrap()
                .contains("(question: \"(unknown question)\")"),
            "{unnamed}"
        );
    }

    #[test]
    fn deep_research_question_fault_bounds_label_and_survives_crash() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        fn question_label(finding: &str) -> &str {
            finding
                .rsplit_once("(question: \"")
                .and_then(|(_, tail)| tail.strip_suffix("\")"))
                .unwrap_or_else(|| panic!("no question label in: {finding}"))
        }
        let long = "q".repeat(250);
        let out = run_deep_research_script(
            "question_fault.py",
            &json!({"finding": "LLM node failed: boom", "question": long}),
        );
        let label = question_label(out["finding"].as_str().unwrap());
        assert_eq!(label.chars().count(), 200, "{out}");
        assert!(label.ends_with('…'), "{out}");

        let out = run_deep_research_script(
            "question_fault.py",
            &json!({"finding": "LLM node failed: boom", "question": "line one\nline two"}),
        );
        let finding = out["finding"].as_str().unwrap();
        assert!(!finding.contains('\n'), "{out}");
        assert_eq!(question_label(finding), "line one line two", "{out}");

        // Unparseable state makes load_state raise before main runs; the
        // crash guard must still emit a well-formed fault finding.
        let out = run_deep_research_script_raw("question_fault.py", "not json");
        let finding = out["finding"].as_str().unwrap();
        assert!(
            finding.starts_with(
                "PIPELINE-FAULT: question research failed — finding unavailable — fault-marker script error"
            ),
            "{out}"
        );
        assert!(
            finding.contains("(question: \"(unknown question)\")"),
            "{out}"
        );
    }

    #[test]
    fn deep_research_vet_fault_degrades_and_preserves() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_deep_research_script(
            "vet_fault.py",
            &json!({
                "source_assessment": "LLM node failed: kaboom",
                "pipeline_faults": ["PIPELINE-FAULT: earlier"]
            }),
        );
        assert_eq!(
            out["pipeline_faults"],
            json!([
                "PIPELINE-FAULT: earlier",
                "PIPELINE-FAULT: source vetting failed — LLM node failed: kaboom"
            ]),
            "existing faults must be preserved"
        );
        let assessment = out["source_assessment"].as_str().unwrap();
        assert!(
            !assessment.starts_with("LLM node"),
            "raw engine error text must not flow into downstream prompts: {out}"
        );
        assert!(assessment.contains("unvetted"), "{out}");
        assert!(
            assessment.contains("Do not request revision"),
            "the note must steer critique away from a REVISE loop the fault cannot fix: {out}"
        );
    }

    #[test]
    fn deep_research_vet_fault_dedupes_repeated_fault() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let fault = "PIPELINE-FAULT: source vetting failed — LLM node failed: kaboom";
        let out = run_deep_research_script(
            "vet_fault.py",
            &json!({
                "source_assessment": "LLM node failed: kaboom",
                "pipeline_faults": [fault]
            }),
        );
        assert_eq!(
            out["pipeline_faults"],
            json!([fault]),
            "an identical fault already recorded must not be appended again: {out}"
        );
    }

    #[test]
    fn deep_research_vet_fault_crash_fails_visibly() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Unparseable state makes load_state raise before main runs; the
        // crash guard must still record its own fault and neutralize the
        // assessment so downstream prompts never see raw error text.
        let out = run_deep_research_script_raw("vet_fault.py", "not json");
        let faults = out["pipeline_faults"].as_array().unwrap();
        assert_eq!(faults.len(), 1, "{out}");
        assert!(
            faults[0]
                .as_str()
                .unwrap()
                .starts_with("PIPELINE-FAULT: source vetting failed — fault-marker script error"),
            "the crash must be recorded as a fault: {out}"
        );
        assert!(
            out["source_assessment"]
                .as_str()
                .unwrap()
                .starts_with("Source credibility assessment unavailable"),
            "{out}"
        );
    }

    #[test]
    fn deep_research_critique_fault_rides_the_pass_default() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_deep_research_script(
            "critique_fault.py",
            &json!({"critique": "LLM node failed: dead", "pipeline_faults": []}),
        );
        assert_eq!(
            out["pipeline_faults"],
            json!(["PIPELINE-FAULT: critique failed — LLM node failed: dead"])
        );
        let note = out["critique"].as_str().unwrap();
        assert!(
            !note.contains("VERDICT:"),
            "the note must not synthesize a verdict line: {note}"
        );
        // Integration: the rewritten critique rides reflexion_gate's
        // malformed-critique PASS default straight to synthesis.
        let gate = run_deep_research_script(
            "reflexion_gate.py",
            &json!({"critique": note, "research_attempts": 0}),
        );
        assert_eq!(gate, json!({"_next": "synthesize"}));
    }

    #[test]
    fn deep_research_critique_fault_dedupes_repeated_fault() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let fault = "PIPELINE-FAULT: critique failed — LLM node failed: dead";
        let out = run_deep_research_script(
            "critique_fault.py",
            &json!({"critique": "LLM node failed: dead", "pipeline_faults": [fault]}),
        );
        assert_eq!(
            out["pipeline_faults"],
            json!([fault]),
            "an identical fault already recorded must not be appended again: {out}"
        );
    }

    #[test]
    fn deep_research_synth_fault_normalizes_report() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let out = run_deep_research_script(
            "synth_fault.py",
            &json!({"report": "Agent node failed: writer died", "pipeline_faults": []}),
        );
        assert_eq!(
            out["fault_text"],
            "report synthesis failed — no report was produced: Agent node failed: writer died"
        );
        assert_eq!(
            out["pipeline_faults"],
            json!([
                "PIPELINE-FAULT: report synthesis failed — no report was produced: Agent node failed: writer died"
            ])
        );
        // A real report never starts with the engine's failure prefix.
        let out = run_deep_research_script("synth_fault.py", &json!({"report": "# A real report"}));
        assert!(
            out["fault_text"]
                .as_str()
                .unwrap()
                .contains("died without recording"),
            "{out}"
        );
    }

    #[test]
    fn deep_research_fault_wiring_preserves_sentinel() {
        use crate::graph::{GraphParser, NodeType};
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents/deep-research");
        let graph = GraphParser::new(&dir)
            .load_from_file(dir.join("graph.yaml"))
            .unwrap();

        // plan: fail-closed. Its output_schema + fallback combination requires
        // a state_updates capture (the validator's fallback-capture warning) so
        // plan_fault can see why it was reached.
        let NodeType::Llm(plan) = &graph.get_node("plan").unwrap().node_type else {
            panic!("plan must be an llm node")
        };
        assert!(plan.output_schema.is_some());
        assert_eq!(plan.fallback.as_deref(), Some("plan_fault"));
        assert!(
            plan.state_updates
                .as_ref()
                .is_some_and(|u| u.contains_key("plan_failure")),
            "plan must capture its failure text for the fault marker"
        );
        let plan_fault = graph.get_node("plan_fault").unwrap();
        assert_eq!(plan_fault.next_target(), Some("end_fault"));
        let NodeType::Script(pf) = &plan_fault.node_type else {
            panic!("plan_fault must be a script node")
        };
        assert_eq!(pf.fallback.as_deref(), Some("end_fault"));

        // research_one_question: branch-local marker; no `next` so the map
        // collects the fault as the lane's finding.
        let NodeType::Llm(question) = &graph.get_node("research_one_question").unwrap().node_type
        else {
            panic!("research_one_question must be an llm node")
        };
        assert_eq!(question.fallback.as_deref(), Some("question_fault"));
        assert!(
            question
                .state_updates
                .as_ref()
                .is_some_and(|u| u.contains_key("finding")),
            "the failure text must land in the key question_fault normalizes"
        );
        assert!(
            graph.get_node("question_fault").unwrap().next.is_none(),
            "question_fault must end the branch chain so the map collects the finding"
        );
        let NodeType::Map(map) = &graph.get_node("research_each_question").unwrap().node_type
        else {
            panic!("research_each_question must be a map node")
        };
        assert_eq!(map.over, "{{questions}}");
        assert_eq!(map.output_key, "finding");

        // vet_sources / critique: degrade-visibly markers that continue to the
        // stage each node's own `next` pointed to.
        let NodeType::Llm(vet) = &graph.get_node("vet_sources").unwrap().node_type else {
            panic!("vet_sources must be an llm node")
        };
        assert_eq!(vet.fallback.as_deref(), Some("vet_fault"));
        assert_eq!(
            graph.get_node("vet_fault").unwrap().next_target(),
            Some("critique")
        );
        let NodeType::Llm(critique) = &graph.get_node("critique").unwrap().node_type else {
            panic!("critique must be an llm node")
        };
        assert_eq!(critique.fallback.as_deref(), Some("critique_fault"));
        assert_eq!(
            graph.get_node("critique_fault").unwrap().next_target(),
            Some("reflexion_gate")
        );

        // synthesize: fail-closed via synth_fault → end_fault; the failure
        // text lands in `report` via state_updates.
        let NodeType::Agent(synthesize) = &graph.get_node("synthesize").unwrap().node_type else {
            panic!("synthesize must be an agent node")
        };
        assert_eq!(synthesize.fallback.as_deref(), Some("synth_fault"));
        assert_eq!(synthesize.max_attempts, 2);
        assert!(
            synthesize
                .state_updates
                .as_ref()
                .is_some_and(|u| u.contains_key("report"))
        );
        assert_eq!(
            graph.get_node("synth_fault").unwrap().next_target(),
            Some("end_fault")
        );

        // end_fault renders the naming-pinned sentinel.
        let NodeType::End(end) = &graph.get_node("end_fault").unwrap().node_type else {
            panic!("end_fault must be an end node")
        };
        assert_eq!(
            end.output,
            "DEEP_RESEARCH FAILED — PIPELINE-FAULT: {{fault_text}}"
        );

        // Accepted-path visibility: pipeline_notes ("" on a fault-free run,
        // keeping the happy-path output byte-identical) is appended to the
        // report and shown at the approval gate.
        let NodeType::End(accepted) = &graph.get_node("end_accepted").unwrap().node_type else {
            panic!("end_accepted must be an end node")
        };
        assert_eq!(accepted.output, "{{report}}{{pipeline_notes}}");
        let NodeType::Approval(approve) = &graph.get_node("approve").unwrap().node_type else {
            panic!("approve must be an approval node")
        };
        assert!(approve.question.contains("{{pipeline_notes}}"));

        // Every fault-path state key has an initial_state default; questions'
        // [] also keeps the sibling knowledge_lookup lane alive for the one
        // super-step where a dead plan races end_fault (the map resolves
        // {{questions}} to zero items instead of erroring).
        assert_eq!(graph.initial_state.get("pipeline_faults"), Some(&json!([])));
        assert_eq!(graph.initial_state.get("pipeline_notes"), Some(&json!("")));
        assert_eq!(graph.initial_state.get("fault_text"), Some(&json!("")));
        assert_eq!(graph.initial_state.get("questions"), Some(&json!([])));
    }

    #[test]
    #[serial]
    fn install_functions_force_preserves_user_mcp_json() {
        let _guard = TestConfigDirGuard::new();

        Functions::install_builtin_global_tools(false).unwrap();
        let mcp = paths::mcp_config_file();
        assert!(mcp.exists(), "mcp.json should be installed on first run");

        write(&mcp, "USER_MCP_CONFIG").unwrap();
        Functions::install_builtin_global_tools(true).unwrap();
        assert_eq!(
            read_to_string(&mcp).unwrap(),
            "USER_MCP_CONFIG",
            "force install must NOT overwrite the user's mcp.json"
        );
    }

    #[test]
    #[serial]
    fn install_mcp_config_merges_existing() {
        let _guard = TestConfigDirGuard::new();

        Functions::install_mcp_config().unwrap();
        let mcp = paths::mcp_config_file();
        assert!(mcp.exists(), "install_mcp_config should create mcp.json");

        let custom_json =
            r#"{"mcpServers":{"my-custom-server":{"type":"stdio","command":"custom-cmd"}}}"#;
        write(&mcp, custom_json).unwrap();
        Functions::install_mcp_config().unwrap();

        let result = read_to_string(&mcp).unwrap();
        assert!(
            result.contains("my-custom-server"),
            "install_mcp_config must preserve user-added MCP servers"
        );
        assert!(
            result.contains("github"),
            "install_mcp_config must add new bundled servers"
        );
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn toggled_enabled_macros_covers_all_transitions() {
        let all_active = strings(&["a", "b", "c"]);
        type ToggleCase = (Option<Vec<String>>, &'static str, bool, Option<Vec<String>>);
        let cases: Vec<ToggleCase> = vec![
            (None, "a", true, None),
            (Some(strings(&["a"])), "a", true, None),
            (Some(strings(&["a"])), "b", true, Some(strings(&["a", "b"]))),
            (None, "b", false, Some(strings(&["a", "c"]))),
            (
                Some(strings(&["a", "b"])),
                "b",
                false,
                Some(strings(&["a"])),
            ),
            (Some(strings(&["a"])), "b", false, None),
        ];
        for (current, name, enable, expected) in cases {
            let result = toggled_enabled_macros(current.as_deref(), &all_active, name, enable);
            assert_eq!(
                result, expected,
                "current={current:?} name={name} enable={enable}"
            );
        }
    }

    fn resolved(state: MacroState) -> ResolvedMacro {
        ResolvedMacro {
            name: "m".to_string(),
            source: Some(MacroSource::Global),
            description: None,
            isolated: None,
            shadowed_by_workspace: false,
            state,
        }
    }

    #[test]
    fn macro_state_display_covers_all_states() {
        let owner = |level: MacroAllowlistLevel| format!("{level}:test");
        let cases = vec![
            (MacroState::Enabled, "enabled"),
            (MacroState::DisabledRuntime, "disabled (runtime)"),
            (
                MacroState::Locked {
                    level: MacroAllowlistLevel::Agent,
                },
                "locked (agent:test enabled_macros)",
            ),
            (MacroState::Missing, "missing"),
            (MacroState::ShadowedBuiltin, "shadowed (built-in)"),
            (
                MacroState::Invalid {
                    reason: "boom".to_string(),
                },
                "invalid (boom)",
            ),
        ];
        for (state, expected) in cases {
            assert_eq!(macro_state_display(&resolved(state), owner), expected);
        }
    }

    #[test]
    fn macro_source_display_names_source_or_dash() {
        assert_eq!(
            macro_source_display(Some(MacroSource::Workspace)),
            "workspace"
        );
        assert_eq!(macro_source_display(Some(MacroSource::Global)), "global");
        assert_eq!(macro_source_display(None), "-");
    }

    #[test]
    fn set_completion_keys_include_enabled_skills_and_macros() {
        assert!(SET_COMPLETION_KEYS.contains(&"enabled_skills"));
        assert!(SET_COMPLETION_KEYS.contains(&"enabled_macros"));
    }

    #[test]
    fn new_macro_rejects_reserved_names() {
        let ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        for name in RESERVED_MACRO_NAMES {
            let err = ctx.new_macro(&app, name).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("'{name}' is a reserved macro name")
            );
        }
    }

    #[test]
    fn macro_lock_owner_names_the_owning_config() {
        let mut ctx = create_test_ctx();
        assert_eq!(ctx.macro_lock_owner(MacroAllowlistLevel::Role), "role");
        ctx.role = Some(Role::new("coder", "prompt"));
        assert_eq!(
            ctx.macro_lock_owner(MacroAllowlistLevel::Role),
            "role:coder"
        );
        assert_eq!(
            ctx.macro_lock_owner(MacroAllowlistLevel::Global),
            "global config"
        );
    }

    #[test]
    fn select_functions_hides_job_tools_under_empty_role_filter() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_job_functions();

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec![]));

        assert!(
            ctx.select_functions(&role).is_none(),
            "an empty tool filter declares nothing backgroundable, so job__ tools must be hidden"
        );
    }

    #[test]
    fn select_functions_keeps_lifecycle_job_tools_when_context_owns_jobs() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_job_functions();
        let sup = Arc::new(RwLock::new(
            Supervisor::new(4, 3).with_max_concurrent_jobs(1),
        ));
        sup.write()
            .register(make_running_job(utils::create_abort_signal()))
            .unwrap();
        ctx.supervisor = Some(sup);

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec![]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["job__check", "job__collect", "job__cancel", "job__list"],
            "lifecycle verbs must stay reachable while the context owns a job; job__start must not"
        );
    }

    #[test]
    fn owns_active_jobs_respects_node_scope() {
        let mut ctx = create_test_ctx();
        let sup = Arc::new(RwLock::new(
            Supervisor::new(4, 3).with_max_concurrent_jobs(1),
        ));
        sup.write()
            .register(make_running_job(utils::create_abort_signal()))
            .unwrap();
        ctx.supervisor = Some(sup);

        assert!(
            ctx.owns_active_jobs(),
            "outside a node, the context owns every registry job"
        );

        ctx.node_job_scope = Some(vec![]);
        assert!(
            !ctx.owns_active_jobs(),
            "a node owns only jobs it started, not other registry entries"
        );

        ctx.node_job_scope = Some(vec!["j1".to_string()]);
        assert!(ctx.owns_active_jobs());
    }

    #[test]
    #[serial]
    fn select_functions_hides_job_tools_under_empty_agent_filter() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_job_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        let abort = utils::create_abort_signal();
        run_async(ctx.use_agent(&app, &agent_name, None, abort)).unwrap();

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec![]));

        let names: Vec<String> = ctx
            .select_functions(&role)
            .unwrap_or_default()
            .iter()
            .map(|f| f.name.clone())
            .collect();
        assert!(
            !names.iter().any(|n| n.starts_with("job__")),
            "job__ tools must be hidden under an empty agent filter, got: {names:?}"
        );
    }

    #[test]
    #[serial]
    fn select_functions_when_jobs_disabled_is_byte_identical_to_no_jobs_baseline() {
        let _guard = TestConfigDirGuard::new();
        let app_state = app_state_with_mcp_config(false, &[]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Repl);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["all".to_string()]));

        let jobs_off = AppConfig {
            max_concurrent_jobs: Some(0),
            ..(*app).clone()
        };
        run_async(ctx.rebuild_tool_scope(&jobs_off, None, abort.clone())).unwrap();
        ctx.tool_scope
            .functions
            .append_declaration(test_decl("echo"));
        let without_jobs = serde_json::to_string(&ctx.select_functions(&role)).unwrap();
        assert!(
            !without_jobs.contains("job__"),
            "no job__ declarations may leak when jobs are disabled, got: {without_jobs}"
        );

        run_async(ctx.rebuild_tool_scope(&app, None, abort)).unwrap();
        ctx.tool_scope
            .functions
            .append_declaration(test_decl("echo"));
        let with_jobs = ctx.select_functions(&role).unwrap();
        assert!(with_jobs.iter().any(|f| f.name.starts_with("job__")));
        let stripped: Vec<FunctionDeclaration> = with_jobs
            .into_iter()
            .filter(|f| !f.name.starts_with("job__"))
            .collect();

        assert_eq!(
            without_jobs,
            serde_json::to_string(&Some(stripped)).unwrap(),
            "jobs-disabled tool list must be byte-identical to the jobs-enabled list minus job__ declarations"
        );
    }

    #[test]
    fn select_functions_returns_none_when_no_tools_enabled_and_jobs_disabled() {
        let app_state = {
            let config = AppConfig {
                max_concurrent_jobs: Some(0),
                ..AppConfig::default()
            };
            Arc::new(AppState {
                config: Arc::new(config),
                vault: Arc::new(Vault::default()),
                mcp_factory: Arc::new(McpFactory::default()),
                rag_cache: Arc::new(RagCache::default()),
                mcp_config: None,
                mcp_log_path: None,
                mcp_registry: None,
                functions: Functions::default(),
            })
        };
        let ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        assert!(ctx.select_functions(&Role::default()).is_none());
    }

    #[test]
    fn tools_info_lists_job_tools_when_enabled() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_job_functions();
        ctx.tool_scope
            .functions
            .append_declaration(test_decl("echo"));
        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["echo".to_string()]));
        ctx.role = Some(role);

        let info = ctx.tools_info().unwrap();

        for name in [
            "job__start",
            "job__check",
            "job__collect",
            "job__cancel",
            "job__list",
        ] {
            assert!(
                info.contains(name),
                "expected {name} in output, got: {info}"
            );
        }
    }

    fn make_running_job(abort_signal: utils::AbortSignal) -> JobHandle {
        // Leak the runtime so the spawned task is never polled and the job
        // stays running for the duration of the test.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let join_handle = rt.spawn(async {
            Ok(JobResult {
                output: serde_json::Value::Null,
                exit_code: Some(0),
                output_bytes_captured: 0,
            })
        });
        mem::forget(rt);
        JobHandle {
            id: "j1".to_string(),
            tool: "execute_command".to_string(),
            started_at: Instant::now(),
            join_handle,
            abort_signal,
            state: Arc::new(parking_lot::Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            })),
            output_buf: Arc::new(parking_lot::Mutex::new(RingBuf::default())),
            no_change_checks: 0,
            last_check_state: None,
        }
    }

    #[test]
    #[serial]
    fn use_agent_cancels_running_jobs_of_previous_supervisor() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n"),
        )
        .unwrap();

        let job_sig = utils::create_abort_signal();
        let old_sup = Arc::new(RwLock::new(
            Supervisor::new(4, 3).with_max_concurrent_jobs(1),
        ));
        old_sup
            .write()
            .register(make_running_job(job_sig.clone()))
            .unwrap();
        ctx.supervisor = Some(old_sup);

        run_async(ctx.use_agent(&app, &agent_name, None, utils::create_abort_signal())).unwrap();

        assert!(
            job_sig.aborted(),
            "running jobs of the previous supervisor must be cancelled"
        );
        assert!(ctx.supervisor.is_some());
    }

    #[test]
    #[serial]
    fn exit_agent_cancels_running_jobs() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();

        let job_sig = utils::create_abort_signal();
        let sup = Arc::new(RwLock::new(
            Supervisor::new(4, 3).with_max_concurrent_jobs(1),
        ));
        sup.write()
            .register(make_running_job(job_sig.clone()))
            .unwrap();
        ctx.agent = Some(Agent::test_new(AgentConfig::default()));
        ctx.supervisor = Some(sup);

        ctx.exit_agent(&app).unwrap();

        assert!(job_sig.aborted(), "exit_agent must cancel running jobs");
        assert!(ctx.supervisor.is_none());
    }

    #[test]
    fn toggle_tool_rejects_job_tools_as_unknown() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_job_functions();

        for action in ["enable", "disable"] {
            let err = ctx.toggle_tool(action, "job__start").unwrap_err();
            assert!(
                err.to_string().contains("Unknown tool 'job__start'"),
                "expected job__start to be rejected on {action}, got: {err}"
            );
        }
    }

    fn mcp_app_state(servers: &[&str]) -> Arc<AppState> {
        let mcp_servers = servers
            .iter()
            .map(|name| {
                (
                    name.to_string(),
                    McpServer {
                        transport_type: McpTransportType::Stdio,
                        command: Some("echo".to_string()),
                        args: None,
                        env: None,
                        cwd: None,
                        url: None,
                        headers: None,
                        oauth: None,
                        allowed_tools: None,
                    },
                )
            })
            .collect();
        Arc::new(AppState {
            config: Arc::new(AppConfig::default()),
            vault: Arc::new(Vault::default()),
            mcp_factory: Arc::new(McpFactory::default()),
            rag_cache: Arc::new(RagCache::default()),
            mcp_config: Some(McpServersConfig { mcp_servers }),
            mcp_log_path: None,
            mcp_registry: None,
            functions: Functions::default(),
        })
    }

    fn gh_get_only_map() -> IndexMap<String, Vec<String>> {
        IndexMap::from([("gh".to_string(), vec!["get_*".to_string()])])
    }

    #[test]
    #[serial]
    fn rebuild_tool_scope_populates_filters_from_a_filtered_role() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        let mut role = Role::new("dev", "prompt");
        role.set_mcp_tools(Some(gh_get_only_map()));
        ctx.role = Some(role);

        run_async(ctx.refresh_tool_scope(utils::create_abort_signal())).unwrap();

        let filter = ctx
            .tool_scope
            .mcp_runtime
            .tool_filters
            .get("gh")
            .expect("a rebuild must never leave a filtered role unfiltered");
        assert!(filter.allows("get_issue"));
        assert!(!filter.allows("delete_repo"));
    }

    #[test]
    #[serial]
    fn mid_node_skill_load_keeps_node_filter_layer() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        ctx.update_app_config(|app| app.mcp_server_support = false);
        ctx.active_node_mcp_tools = Some(("n1".to_string(), gh_get_only_map()));
        ctx.refresh_mcp_tool_filters();
        assert!(!ctx.tool_scope.mcp_runtime.tool_filters["gh"].allows("list_prs"));

        ctx.skill_registry
            .insert(Skill::new(
                "sec",
                "---\nenabled_mcp_servers: gh\nmcp_tools:\n  gh: [get_*, list_*]\n---\nBody",
            ))
            .unwrap();
        run_async(ctx.refresh_tool_scope(utils::create_abort_signal())).unwrap();

        let filter = &ctx.tool_scope.mcp_runtime.tool_filters["gh"];
        assert!(filter.allows("get_issue"));
        assert!(
            !filter.allows("list_prs"),
            "the node layer must survive a mid-node skill load"
        );

        ctx.active_node_mcp_tools = None;
        ctx.refresh_mcp_tool_filters();
        let filter = &ctx.tool_scope.mcp_runtime.tool_filters["gh"];
        assert!(
            filter.allows("list_prs"),
            "the node layer must not outlive the node"
        );
        assert!(!filter.allows("delete_repo"));
    }

    #[test]
    #[serial]
    fn set_skills_enabled_does_not_drop_filters() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        let mut role = Role::new("dev", "prompt");
        role.set_mcp_tools(Some(gh_get_only_map()));
        ctx.role = Some(role);
        ctx.refresh_mcp_tool_filters();
        assert!(ctx.tool_scope.mcp_runtime.tool_filters.contains_key("gh"));

        run_async(ctx.update("skills_enabled false", utils::create_abort_signal())).unwrap();

        assert!(ctx.tool_scope.mcp_runtime.tool_filters.contains_key("gh"));
    }

    #[test]
    #[serial]
    fn use_session_applies_persisted_mcp_tools_immediately() {
        // use_session rebuilds the tool scope BEFORE `self.session` is
        // assigned, so a re-attached session's persisted allowlist is
        // enforced only by the filter refresh that runs after the
        // assignment. Drive the real use_session against a session file on
        // disk to prove that refresh happens.
        let _guard = TestConfigDirGuard::new();
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        let session_path = ctx.session_file("persisted");
        ensure_parent_exists(&session_path).unwrap();
        write(
            &session_path,
            "model: test-seeded:test-chat\nmessages: []\nmcp_tools:\n  gh:\n    - get_*\n",
        )
        .unwrap();
        let app = ctx.app.config.clone();

        run_async(ctx.use_session(&app, Some("persisted"), utils::create_abort_signal())).unwrap();

        let filter = ctx
            .tool_scope
            .mcp_runtime
            .tool_filters
            .get("gh")
            .expect("a re-attached session's persisted map must apply immediately");
        assert!(filter.allows("get_issue"));
        assert!(!filter.allows("delete_repo"));
    }

    #[test]
    #[serial]
    fn use_rag_does_not_drop_role_filters() {
        // Attaching a RAG rebuilds the tool scope from scratch, and the
        // rebuilt McpRuntime starts with no filters — so the rebuild must
        // recompute the declarative filter layers or the active role's
        // allowlist silently disappears. Uses the yaml driver so the load
        // stays on the local filesystem; the externally-backed attach path
        // needs a live vector store and cannot run here, but it funnels
        // through the same tool-scope refresh.
        let _guard = TestConfigDirGuard::new();
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        let mut role = Role::new("dev", "prompt");
        role.set_mcp_tools(Some(gh_get_only_map()));
        ctx.role = Some(role);
        ctx.refresh_mcp_tool_filters();
        assert!(ctx.tool_scope.mcp_runtime.tool_filters.contains_key("gh"));

        let rag_path = ctx.rag_file("kb");
        ensure_parent_exists(&rag_path).unwrap();
        write(
            &rag_path,
            "driver: yaml\nembedding_model: test-seeded:test-embedder\nchunk_size: 512\nchunk_overlap: 64\ntop_k: 5\n",
        )
        .unwrap();

        run_async(ctx.use_rag(Some("kb"), utils::create_abort_signal())).unwrap();

        assert!(ctx.rag.is_some(), "the RAG must actually load");
        let filter = ctx
            .tool_scope
            .mcp_runtime
            .tool_filters
            .get("gh")
            .expect("attaching a RAG must not drop the role's filter layer");
        assert!(filter.allows("get_issue"));
        assert!(!filter.allows("delete_repo"));
    }

    #[test]
    #[serial]
    fn use_agent_applies_agent_filters_without_a_session() {
        let _guard = TestConfigDirGuard::new();
        let config_path = paths::agent_config_file("filterer");
        ensure_parent_exists(&config_path).unwrap();
        write(
            &config_path,
            "name: filterer\ninstructions: hi\nmcp_tools:\n  gh:\n    - get_*\n",
        )
        .unwrap();
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        let app = ctx.app.config.clone();

        run_async(ctx.use_agent(&app, "filterer", None, utils::create_abort_signal())).unwrap();

        let filter = ctx
            .tool_scope
            .mcp_runtime
            .tool_filters
            .get("gh")
            .expect("the agent layer must apply on the no-session use_agent path");
        assert!(filter.allows("get_issue"));
        assert!(!filter.allows("delete_repo"));
    }

    #[test]
    #[serial]
    fn use_temp_role_recomputes_filters() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        let mut role = Role::new("dev", "prompt");
        role.set_mcp_tools(Some(gh_get_only_map()));
        ctx.role = Some(role);
        ctx.refresh_mcp_tool_filters();
        assert!(ctx.tool_scope.mcp_runtime.tool_filters.contains_key("gh"));

        let app = ctx.app.config.clone();
        ctx.use_temp_role(&app, "temp prompt").unwrap();

        assert!(
            ctx.tool_scope.mcp_runtime.tool_filters.is_empty(),
            "a temp role must clear the replaced role's filter layer"
        );
    }

    #[test]
    fn info_mcp_server_renders_layers_chains_and_dead_patterns() {
        let mut ctx = RequestContext::new(mcp_app_state(&["fixture"]), WorkingMode::Cmd);

        let info = run_async(async {
            let (runtime, _server) = fixture_runtime(FixtureServer {
                tool_names: vec!["get_issue", "list_prs", "delete_repo"],
                ..Default::default()
            })
            .await;
            ctx.tool_scope.mcp_runtime = runtime;
            let mut filter = ToolFilter::default();
            filter.push_layer(
                LayerSource::Global,
                &["get_*".to_string(), "list_*".to_string()],
            );
            filter.push_layer(
                LayerSource::Role("reviewer".to_string()),
                &["get_*".to_string(), "bogus_zzz*".to_string()],
            );
            ctx.tool_scope
                .mcp_runtime
                .tool_filters
                .insert("fixture".to_string(), filter);
            ctx.mcp_server_info("fixture").await.unwrap()
        });

        assert!(
            info.contains("server         fixture (stdio, connected)"),
            "got:\n{info}"
        );
        let cap_line = info
            .lines()
            .find(|line| line.starts_with("capabilities"))
            .unwrap();
        assert_eq!(cap_line, "capabilities   tools", "got:\n{info}");
        assert!(info.contains("global (mcp.json):"), "got:\n{info}");
        assert!(info.contains("get_* | list_*"), "got:\n{info}");
        assert!(info.contains("role (reviewer):"), "got:\n{info}");
        assert!(info.contains("get_* | bogus_zzz*"), "got:\n{info}");
        assert!(info.contains("tools (1 allowed / 3 total)"), "got:\n{info}");
        assert!(
            info.lines().any(|line| line.contains('✓')
                && line.contains("get_issue")
                && line.contains("get_* (global) ∧ get_* (role)")),
            "got:\n{info}"
        );
        assert!(
            info.lines().any(|line| line.contains('✗')
                && line.contains("list_prs")
                && line.contains("hidden by role layer")),
            "got:\n{info}"
        );
        assert!(
            info.lines().any(|line| line.contains('✗')
                && line.contains("delete_repo")
                && line.contains("hidden by global layer")),
            "got:\n{info}"
        );
        assert!(
            info.contains("⚠ role pattern 'bogus_zzz*' matches no allowed tools"),
            "got:\n{info}"
        );
    }

    #[test]
    fn info_mcp_server_counts_served_prompts_and_resources() {
        let mut ctx = RequestContext::new(mcp_app_state(&["fixture"]), WorkingMode::Cmd);

        let info = run_async(async {
            let (runtime, _server) = fixture_runtime(FixtureServer {
                resources_capability: true,
                prompts_capability: true,
                ..Default::default()
            })
            .await;
            ctx.tool_scope.mcp_runtime = runtime;
            ctx.mcp_server_info("fixture").await.unwrap()
        });

        assert!(
            info.contains("capabilities   tools, resources (3), prompts (1)"),
            "got:\n{info}"
        );
    }

    #[test]
    fn info_mcp_server_omits_declared_but_empty_capabilities() {
        let mut ctx = RequestContext::new(mcp_app_state(&["fixture"]), WorkingMode::Cmd);

        let info = run_async(async {
            let (runtime, _server) = fixture_runtime(FixtureServer {
                resources_capability: true,
                prompts_capability: true,
                empty_resource_listings: true,
                empty_prompt_listings: true,
                ..Default::default()
            })
            .await;
            ctx.tool_scope.mcp_runtime = runtime;
            ctx.mcp_server_info("fixture").await.unwrap()
        });

        let cap_line = info
            .lines()
            .find(|line| line.starts_with("capabilities"))
            .unwrap();
        assert_eq!(cap_line, "capabilities   tools", "got:\n{info}");
    }

    #[test]
    fn info_mcp_server_annotates_failed_capability_listings() {
        let mut ctx = RequestContext::new(mcp_app_state(&["fixture"]), WorkingMode::Cmd);

        let info = run_async(async {
            let (runtime, _server) = fixture_runtime(FixtureServer {
                resources_capability: true,
                prompts_capability: true,
                fail_resource_listings: true,
                fail_prompt_listings: true,
                ..Default::default()
            })
            .await;
            ctx.tool_scope.mcp_runtime = runtime;
            ctx.mcp_server_info("fixture").await.unwrap()
        });

        assert!(
            info.contains(
                "capabilities   tools, resources (declared, list failed), prompts (declared, list failed)"
            ),
            "got:\n{info}"
        );
    }

    #[test]
    fn info_mcp_server_annotates_partial_resource_listing_failure() {
        let mut ctx = RequestContext::new(mcp_app_state(&["fixture"]), WorkingMode::Cmd);

        let info = run_async(async {
            let (runtime, _server) = fixture_runtime(FixtureServer {
                resources_capability: true,
                empty_resource_listings: true,
                fail_template_listings: true,
                ..Default::default()
            })
            .await;
            ctx.tool_scope.mcp_runtime = runtime;
            ctx.mcp_server_info("fixture").await.unwrap()
        });

        let cap_line = info
            .lines()
            .find(|line| line.starts_with("capabilities"))
            .unwrap();
        assert_eq!(
            cap_line, "capabilities   tools, resources (declared, list failed)",
            "got:\n{info}"
        );
    }

    #[test]
    fn info_mcp_server_errors_when_unconfigured_or_not_running() {
        let ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);

        let err = run_async(ctx.mcp_server_info("nope")).unwrap_err();
        assert!(err.to_string().contains("not configured"), "got: {err}");

        let err = run_async(ctx.mcp_server_info("gh")).unwrap_err();
        assert!(err.to_string().contains("not running"), "got: {err}");
    }

    #[test]
    fn info_mcp_server_unfiltered_renders_all_allowed() {
        let mut ctx = RequestContext::new(mcp_app_state(&["fixture"]), WorkingMode::Cmd);

        let info = run_async(async {
            let (runtime, _server) = fixture_runtime(FixtureServer::default()).await;
            ctx.tool_scope.mcp_runtime = runtime;
            ctx.mcp_server_info("fixture").await.unwrap()
        });

        assert!(info.contains("(none — all tools allowed)"), "got:\n{info}");
        assert!(info.contains("tools (1 allowed / 1 total)"), "got:\n{info}");
        assert!(
            info.lines()
                .any(|line| line.contains('✓') && line.contains("dup")),
            "got:\n{info}"
        );
        assert!(!info.contains('∧'), "got:\n{info}");
    }

    #[test]
    fn repl_complete_info_mcp_server_offers_only_running_servers() {
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        let (runtime, _server) = run_async(fixture_runtime(FixtureServer::default()));
        ctx.tool_scope.mcp_runtime = runtime;

        let values = ctx.repl_complete(".info", &["mcp-server", ""], "");

        assert!(
            values.iter().any(|(name, _)| name == "fixture"),
            "running server must be offered, got: {values:?}"
        );
        assert!(
            !values.iter().any(|(name, _)| name == "gh"),
            "configured-but-not-running server must not be offered, got: {values:?}"
        );
    }

    #[test]
    fn mcp_servers_listing_tags_filtered_servers() {
        let mut ctx = RequestContext::new(mcp_app_state(&["gh", "jira"]), WorkingMode::Cmd);
        let mut filter = ToolFilter::default();
        filter.push_layer(LayerSource::Global, &["get_*".to_string()]);
        ctx.tool_scope
            .mcp_runtime
            .tool_filters
            .insert("gh".to_string(), filter);
        ctx.tool_scope
            .mcp_runtime
            .tool_filters
            .insert("jira".to_string(), ToolFilter::default());

        let listing = ctx.mcp_servers_listing().unwrap();

        let gh_line = listing.lines().find(|line| line.contains(" gh")).unwrap();
        assert!(gh_line.contains("[filtered]"), "got:\n{listing}");
        let jira_line = listing.lines().find(|line| line.contains("jira")).unwrap();
        assert!(
            !jira_line.contains("[filtered]"),
            "a zero-layer filter entry must not count as filtered, got:\n{listing}"
        );
    }

    #[test]
    fn mcp_servers_listing_tags_aliases_of_filtered_servers() {
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        ctx.update_app_config(|app| {
            app.mapping_mcp_servers
                .insert("hub".to_string(), "gh".to_string());
        });
        let mut filter = ToolFilter::default();
        filter.push_layer(LayerSource::Global, &["get_*".to_string()]);
        ctx.tool_scope
            .mcp_runtime
            .tool_filters
            .insert("gh".to_string(), filter);

        let listing = ctx.mcp_servers_listing().unwrap();

        let hub_line = listing.lines().find(|line| line.contains("hub")).unwrap();
        assert!(hub_line.contains("[filtered]"), "got:\n{listing}");
    }

    #[test]
    fn set_mcp_tools_writes_app_layer_and_refreshes_filters() {
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        ctx.update_app_config(|app| app.enabled_mcp_servers = Some(vec!["all".to_string()]));

        run_async(ctx.update(
            "mcp_tools.gh get_*,list_issues",
            utils::create_abort_signal(),
        ))
        .unwrap();

        let map = ctx.app.config.mcp_tools.as_ref().unwrap();
        assert_eq!(
            map.get("gh").unwrap(),
            &vec!["get_*".to_string(), "list_issues".to_string()]
        );
        let filter = ctx.tool_scope.mcp_runtime.tool_filters.get("gh").unwrap();
        assert!(filter.allows("get_issue"));
        assert!(!filter.allows("delete_repo"));
    }

    #[test]
    fn set_mcp_tools_writes_session_layer_when_attached() {
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        let mut session = Session::default();
        session.set_enabled_mcp_servers(Some(vec!["all".to_string()]));
        ctx.session = Some(session);

        run_async(ctx.update("mcp_tools.gh get_*", utils::create_abort_signal())).unwrap();

        let map = ctx.session.as_ref().unwrap().mcp_tools().unwrap();
        assert_eq!(map.get("gh").unwrap(), &vec!["get_*".to_string()]);
        assert!(ctx.app.config.mcp_tools.is_none());
    }

    #[test]
    fn set_mcp_tools_keeps_dotted_server_names_verbatim() {
        let mut ctx = RequestContext::new(mcp_app_state(&["my.server"]), WorkingMode::Cmd);
        ctx.update_app_config(|app| app.enabled_mcp_servers = Some(vec!["all".to_string()]));

        run_async(ctx.update("mcp_tools.my.server get_*", utils::create_abort_signal())).unwrap();

        let map = ctx.app.config.mcp_tools.as_ref().unwrap();
        assert!(map.contains_key("my.server"));
        assert!(ctx.tool_scope.mcp_runtime.tool_filters["my.server"].allows("get_issue"));
    }

    #[test]
    #[serial]
    fn set_mcp_tools_rejects_graph_agents_on_both_arms() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        let agent_name = format!(
            "test_graph_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\nversion: \"1.0\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: ok\n"
            ),
        )
        .unwrap();
        run_async(ctx.use_agent(&app, &agent_name, None, utils::create_abort_signal())).unwrap();

        for data in ["mcp_tools.gh get_*", "mcp_tools null"] {
            let err = run_async(ctx.update(data, utils::create_abort_signal())).unwrap_err();
            assert!(
                err.to_string()
                    .contains("Graph agents define MCP tool filters per-node"),
                "got: {err}"
            );
        }
    }

    #[test]
    fn set_mcp_tools_rejects_unknown_and_disabled_servers() {
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        ctx.update_app_config(|app| app.enabled_mcp_servers = Some(vec!["all".to_string()]));

        let err =
            run_async(ctx.update("mcp_tools.nope x", utils::create_abort_signal())).unwrap_err();
        assert!(err.to_string().contains("not configured"), "got: {err}");

        ctx.update_app_config(|app| app.enabled_mcp_servers = None);
        let err =
            run_async(ctx.update("mcp_tools.gh get_*", utils::create_abort_signal())).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not enabled"), "got: {message}");
        assert!(message.contains(".list mcp-servers"), "got: {message}");
    }

    #[test]
    fn set_mcp_tools_null_removes_entries_and_clears_layer() {
        let mut ctx = RequestContext::new(mcp_app_state(&["gh", "jira"]), WorkingMode::Cmd);
        ctx.update_app_config(|app| app.enabled_mcp_servers = Some(vec!["all".to_string()]));
        let abort = utils::create_abort_signal;

        run_async(ctx.update("mcp_tools.gh get_*", abort())).unwrap();
        run_async(ctx.update("mcp_tools.gh null", abort())).unwrap();
        assert!(
            ctx.app.config.mcp_tools.is_none(),
            "removing the only entry must store None, not an empty map"
        );
        assert!(!ctx.tool_scope.mcp_runtime.tool_filters.contains_key("gh"));

        run_async(ctx.update("mcp_tools.gh get_*", abort())).unwrap();
        run_async(ctx.update("mcp_tools.jira list_*", abort())).unwrap();
        run_async(ctx.update("mcp_tools.gh null", abort())).unwrap();
        let map = ctx.app.config.mcp_tools.as_ref().unwrap();
        assert!(!map.contains_key("gh"));
        assert!(map.contains_key("jira"));

        run_async(ctx.update("mcp_tools null", abort())).unwrap();
        assert!(ctx.app.config.mcp_tools.is_none());
        assert!(ctx.tool_scope.mcp_runtime.tool_filters.is_empty());

        let err = run_async(ctx.update("mcp_tools get_*", abort())).unwrap_err();
        assert!(
            err.to_string().contains("Usage: .set mcp_tools.<server>"),
            "got: {err}"
        );
    }

    #[test]
    fn set_mcp_tools_session_layer_cannot_widen_role_layer() {
        let mut ctx = RequestContext::new(mcp_app_state(&["gh"]), WorkingMode::Cmd);
        let mut role = Role::new("dev", "prompt");
        role.set_mcp_tools(Some(gh_get_only_map()));
        ctx.role = Some(role);
        let mut session = Session::default();
        session.set_enabled_mcp_servers(Some(vec!["all".to_string()]));
        ctx.session = Some(session);

        run_async(ctx.update("mcp_tools.gh *", utils::create_abort_signal())).unwrap();

        let filter = &ctx.tool_scope.mcp_runtime.tool_filters["gh"];
        assert!(filter.allows("get_issue"));
        assert!(
            !filter.allows("delete_repo"),
            "a session layer can only narrow, never widen"
        );
    }
}
