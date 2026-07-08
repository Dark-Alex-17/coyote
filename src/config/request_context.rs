use super::rag_cache::{RagCache, RagKey};
use super::session::Session;
use super::skill::{SKILL_SCAFFOLD, Skill};
use super::skill_policy::SkillPolicy;
use super::skill_registry::SkillRegistry;
use super::todo::TodoList;
use super::tool_scope::{McpRuntime, ToolScope};
use super::{
    AGENTS_DIR_NAME, Agent, AgentVariables, AppConfig, AppState, AssetCategory, CREATE_TITLE_ROLE,
    Input, InstallFilter, LEFT_PROMPT, LastMessage, MESSAGES_FILE_NAME, RIGHT_PROMPT, Role,
    RoleLike, SESSIONS_DIR_NAME, SUMMARIZATION_PROMPT, SUMMARY_CONTEXT_PROMPT, StateFlags,
    TEMP_ROLE_NAME, TEMP_SESSION_NAME, WorkingMode, ensure_parent_exists, list_agents, memory,
    paths,
};
use super::{MessageContentToolCalls, prompts};
use crate::client::{Model, ModelType, list_models};
use crate::function::{
    FunctionDeclaration, Functions, ToolCallTracker, ToolResult, skill::SKILL_FUNCTION_PREFIX,
    todo::TODO_FUNCTION_PREFIX, user_interaction::USER_FUNCTION_PREFIX,
};
use crate::mcp::{
    MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX, MCP_INVOKE_META_FUNCTION_NAME_PREFIX,
    MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
};
use crate::rag::Rag;
use crate::supervisor::Supervisor;
use crate::supervisor::escalation::EscalationQueue;
use crate::supervisor::mailbox::Inbox;
use crate::utils::{
    AbortSignal, abortable_run_with_spinner, edit_file, fuzzy_filter, get_env_name,
    list_file_names, now, render_prompt, temp_file,
};

use super::memory::{
    DEFAULT_MEMORY_CAP_WITH_TOOLS, DEFAULT_MEMORY_CAP_WITHOUT_TOOLS, MemoryStore, WorkspaceMemory,
};
use crate::graph;
use anyhow::{Context, Error, Result, bail};
use gman::providers::SupportedProvider;
#[cfg(test)]
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
use std::{env, fs};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderMode {
    #[default]
    Streaming,
    Silent,
}

pub struct RequestContext {
    pub app: Arc<AppState>,

    pub macro_flag: bool,
    pub info_flag: bool,
    pub working_mode: WorkingMode,

    pub model: Model,
    pub agent_variables: Option<AgentVariables>,

    pub role: Option<Role>,
    pub session: Option<Session>,
    pub rag: Option<Arc<Rag>>,
    pub agent: Option<Agent>,

    pub last_message: Option<LastMessage>,

    pub tool_scope: ToolScope,

    pub supervisor: Option<Arc<RwLock<Supervisor>>>,
    pub parent_supervisor: Option<Arc<RwLock<Supervisor>>>,
    pub self_agent_id: Option<String>,
    pub inbox: Option<Arc<Inbox>>,
    pub escalation_queue: Option<Arc<EscalationQueue>>,
    pub current_depth: usize,
    pub auto_continue_count: usize,
    pub pending_agents_guardrail_count: u32,
    pub todo_list: TodoList,
    pub skill_registry: SkillRegistry,
    pub last_continuation_response: Option<String>,

    pub render_mode: RenderMode,
}

impl RequestContext {
    pub fn new(app: Arc<AppState>, working_mode: WorkingMode) -> Self {
        Self {
            app,
            macro_flag: false,
            info_flag: false,
            working_mode,
            model: Default::default(),
            agent_variables: None,
            role: None,
            session: None,
            rag: None,
            agent: None,
            last_message: None,
            tool_scope: ToolScope::default(),
            supervisor: None,
            parent_supervisor: None,
            self_agent_id: None,
            inbox: None,
            escalation_queue: None,
            current_depth: 0,
            auto_continue_count: 0,
            pending_agents_guardrail_count: 0,
            todo_list: TodoList::default(),
            skill_registry: SkillRegistry::default(),
            last_continuation_response: None,
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

        Ok(Self {
            app,
            macro_flag: false,
            info_flag,
            working_mode,
            model,
            agent_variables: None,
            role: None,
            session: None,
            rag: None,
            agent: None,
            last_message: None,
            tool_scope: ToolScope {
                functions,
                mcp_runtime,
                tool_tracker: ToolCallTracker::default(),
            },
            supervisor: None,
            parent_supervisor: None,
            self_agent_id: None,
            inbox: None,
            escalation_queue: None,
            current_depth: 0,
            auto_continue_count: 0,
            pending_agents_guardrail_count: 0,
            todo_list: TodoList::default(),
            skill_registry: SkillRegistry::default(),
            last_continuation_response: None,
            render_mode: RenderMode::default(),
        })
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
            info_flag: self.info_flag,
            working_mode: self.working_mode,
            model: self.model.clone(),
            agent_variables: self.agent_variables.clone(),
            role: self.role.clone(),
            session: self.session.clone(),
            rag: self.rag.clone(),
            agent: self.agent.clone(),
            last_message: self.last_message.clone(),
            tool_scope: self.tool_scope.clone(),
            supervisor: self.supervisor.clone(),
            parent_supervisor: self.parent_supervisor.clone(),
            self_agent_id: self.self_agent_id.clone(),
            inbox: self.inbox.clone(),
            escalation_queue: self.escalation_queue.clone(),
            current_depth: self.current_depth,
            auto_continue_count: 0,
            pending_agents_guardrail_count: 0,
            todo_list: self.todo_list.clone(),
            skill_registry: self.skill_registry.clone(),
            last_continuation_response: None,
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
            info_flag: parent.info_flag,
            working_mode: WorkingMode::Cmd,
            model: parent.model.clone(),
            agent_variables: parent.agent_variables.clone(),
            role: None,
            session: None,
            rag: None,
            agent: None,
            last_message: None,
            tool_scope: ToolScope {
                functions: Functions::default(),
                mcp_runtime: McpRuntime::default(),
                tool_tracker: tool_call_tracker,
            },
            supervisor: None,
            parent_supervisor: parent.supervisor.clone(),
            self_agent_id: Some(self_agent_id),
            inbox: Some(inbox),
            escalation_queue: parent.escalation_queue.clone(),
            current_depth,
            auto_continue_count: 0,
            pending_agents_guardrail_count: 0,
            todo_list: TodoList::default(),
            skill_registry: SkillRegistry::default(),
            last_continuation_response: None,
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

    pub fn rag_cache(&self) -> &Arc<RagCache> {
        &self.app.rag_cache
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

    pub fn clear_todo_list(&mut self) {
        self.todo_list.clear();
        self.auto_continue_count = 0;
    }

    pub fn increment_auto_continue_count(&mut self) {
        self.auto_continue_count += 1;
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
                Err(_) => paths::cache_path().join(MESSAGES_FILE_NAME),
            },
            Some(agent) => paths::cache_path()
                .join(AGENTS_DIR_NAME)
                .join(agent.name())
                .join(MESSAGES_FILE_NAME),
        }
    }

    pub fn sessions_dir(&self) -> PathBuf {
        match &self.agent {
            None => match env::var(get_env_name("sessions_dir")) {
                Ok(value) => PathBuf::from(value),
                Err(_) => paths::local_path(SESSIONS_DIR_NAME),
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

    pub fn exit_rag(&mut self) -> Result<()> {
        self.rag.take();
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
        self.last_message = Some(LastMessage::new(input.clone(), String::new()));
        Ok(())
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
            )?;
            agent.set_shared_variables(new_variables);
        }
        if !self.info_flag {
            agent.update_shared_dynamic_instructions(false)?;
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
            agent.to_role()
        } else if let Some(role) = self.role.as_ref() {
            role.clone()
        } else {
            let mut role = Role::default();
            role.batch_set(
                &self.model,
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

        let global_exists = paths::global_memory_index_path().exists();
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
            ("theme", super::format_option_value(&app.theme)),
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
                    .filter(|v| {
                        !v.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                            && !v.name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX)
                            && !v.name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX)
                    })
                    .map(|v| v.name.to_string())
                    .collect();

                if let Some(agent) = &self.agent {
                    declaration_names.extend(
                        agent
                            .functions()
                            .declarations()
                            .iter()
                            .filter(|v| {
                                !v.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                                    && !v.name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX)
                                    && !v.name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX)
                            })
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
                                && v.name.starts_with(TODO_FUNCTION_PREFIX)))
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
                    .filter(|v| {
                        !v.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                            && !v.name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX)
                            && !v.name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX)
                    })
                    .collect();

                if let Some(ref tool_names) = role_filter {
                    agent_functions.retain(|v| {
                        tool_names.contains(&v.name)
                            || (!matches!(agent.skills_enabled(), Some(false))
                                && v.name.starts_with(SKILL_FUNCTION_PREFIX))
                            || v.name.starts_with(USER_FUNCTION_PREFIX)
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
                        .filter(|v| {
                            v.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                                || v.name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX)
                                || v.name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX)
                        })
                        .map(|v| v.name.to_string())
                        .collect();
                    if let Some(agent) = &self.agent {
                        mcp_declaration_names.extend(
                            agent
                                .functions()
                                .declarations()
                                .iter()
                                .filter(|v| {
                                    v.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                                        || v.name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX)
                                        || v.name
                                            .starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX)
                                })
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

                            let item_invoke_name =
                                format!("{}_{item}", MCP_INVOKE_META_FUNCTION_NAME_PREFIX);
                            let item_search_name =
                                format!("{}_{item}", MCP_SEARCH_META_FUNCTION_NAME_PREFIX);
                            let item_describe_name =
                                format!("{}_{item}", MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX);
                            if let Some(values) = app.mapping_mcp_servers.get(item) {
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
                    .filter(|v| {
                        v.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                            || v.name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX)
                            || v.name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX)
                    })
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

        if functions.is_empty() {
            None
        } else {
            Some(functions)
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

    pub fn use_prompt(&mut self, _app: &AppConfig, prompt: &str) -> Result<()> {
        let mut role = Role::new(TEMP_ROLE_NAME, prompt);
        role.set_model(self.current_model().clone());
        self.use_role_obj(role)
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
            "enabled_tools" => {
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
                let value = value.parse().with_context(|| "Invalid value")?;
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
                ".agent" => super::map_completion_values(list_agents()),
                ".install" => {
                    let mut values: Vec<String> =
                        AssetCategory::NAMES.iter().map(|s| s.to_string()).collect();
                    values.push("remote".to_string());
                    super::map_completion_values(values)
                }
                ".macro" => super::map_completion_values(paths::list_macros()),
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
                        "auto_continue",
                        "continuation_prompt",
                        "temperature",
                        "top_p",
                        "enabled_tools",
                        "enabled_mcp_servers",
                        "inject_todo_instructions",
                        "inject_skill_instructions",
                        "skill_instructions",
                        "max_auto_continues",
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
                    ];
                    values.sort_unstable();
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
        } else if (cmd == ".edit" && args.first() == Some(&"skill") && args.len() == 2)
            || (cmd == ".skill" && args.first() == Some(&"load") && args.len() == 2)
        {
            values = super::map_completion_values(paths::list_skills());
        } else if cmd == ".skill" && args.first() == Some(&"unload") && args.len() == 2 {
            values = super::map_completion_values(self.skill_registry.loaded_names());
        } else if cmd == ".install" && args.first() == Some(&"remote") && args.len() >= 2 {
            let prev = args.get(args.len() - 2).copied().unwrap_or("");
            if prev == "--filter" {
                values = super::map_completion_values(
                    InstallFilter::NAMES.iter().map(|s| s.to_string()).collect(),
                );
            } else {
                let has_filter = args.iter().enumerate().any(|(i, a)| {
                    a.starts_with("--filter=") || (*a == "--filter" && i < args.len() - 1)
                });
                let has_force = args.contains(&"--force");
                let mut available: Vec<&str> = vec![];

                if !has_filter {
                    available.push("--filter");
                }
                if !has_force {
                    available.push("--force");
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
        };
        fuzzy_filter(values, |v| v.0.as_str(), filter)
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
                Some(servers) if servers.iter().any(|s| s.trim() == "all") => {
                    mcp_config.mcp_servers.keys().cloned().collect()
                }
                Some(servers) => {
                    let mut ids = Vec::new();
                    for item in servers.iter().map(|s| s.trim()) {
                        if mcp_config.mcp_servers.contains_key(item) {
                            ids.push(item.to_string());
                        } else if let Some(mapped) = app.mapping_mcp_servers.get(item) {
                            for mapped_id in mapped.split(',').map(|s| s.trim()) {
                                if mcp_config.mcp_servers.contains_key(mapped_id) {
                                    ids.push(mapped_id.to_string());
                                }
                            }
                        }
                    }
                    ids
                }
                None => vec![],
            };

            if !server_ids.is_empty() {
                let app_ref = &self.app;
                let acquire_all = async {
                    let mut handles = Vec::new();
                    for id in &server_ids {
                        if let Some(spec) = mcp_config.mcp_servers.get(id) {
                            let handle = app_ref
                                .mcp_factory
                                .acquire(id, spec, app_ref.mcp_log_path.as_deref())
                                .await?;
                            handles.push((id.clone(), handle));
                        }
                    }
                    Ok::<_, Error>(handles)
                };
                let handles = abortable_run_with_spinner(
                    acquire_all,
                    "Loading MCP servers",
                    abort_signal.clone(),
                )
                .await?;
                for (id, handle) in handles {
                    mcp_runtime.insert(id, handle);
                }
            }
        }

        let mut functions = Functions::init(app.visible_tools.as_ref().unwrap_or(&Vec::new()))?;
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
            functions.append_mcp_meta_functions(mcp_runtime.server_names());
        }
        if app.function_calling_support && policy.skills_enabled {
            functions.append_skill_functions();
        }
        if self.should_register_memory_tools() {
            functions.append_memory_functions();
        }

        let tool_tracker = self.tool_scope.tool_tracker.clone();
        self.tool_scope = ToolScope {
            functions,
            mcp_runtime,
            tool_tracker,
        };
        Ok(())
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

        self.use_role_obj(role)?;
        self.rebuild_tool_scope(app, mcp_servers, abort_signal)
            .await
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
        let session_name = session_name.map(|v| v.to_string()).or_else(|| {
            if self.macro_flag || is_graph_agent {
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

        let should_init_supervisor = agent.can_spawn_agents();
        let max_concurrent = agent.max_concurrent_agents();
        let max_depth = agent.max_agent_depth();
        let supervisor = should_init_supervisor
            .then(|| Arc::new(RwLock::new(Supervisor::new(max_concurrent, max_depth))));

        self.rag = agent.rag();
        self.agent = Some(agent);
        self.supervisor = supervisor;
        self.inbox = None;
        self.escalation_queue = None;
        self.self_agent_id = None;
        self.parent_supervisor = None;
        self.current_depth = 0;
        self.auto_continue_count = 0;
        self.todo_list = TodoList::default();

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
        let mut functions = Functions::init(app.visible_tools.as_ref().unwrap_or(&Vec::new()))?;
        if self.working_mode.is_repl() {
            functions.append_user_interaction_functions();
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
            self.escalation_queue = None;
            self.current_depth = 0;
            self.auto_continue_count = 0;
            self.pending_agents_guardrail_count = 0;
            self.todo_list = TodoList::default();
            self.rag.take();
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
        let summary = input.fetch_chat_text().await?;
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

        if let Some(session) = self.session.as_mut() {
            session.compress(format!("{todo_prefix}{summary_context_prompt}{summary}"));
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
        let rag_cache = self.rag_cache();
        let working_mode = self.working_mode;

        let rag: Arc<Rag> = match rag {
            None => {
                let rag_path = self.rag_file(super::TEMP_RAG_NAME);
                if rag_path.exists() {
                    remove_file(&rag_path).with_context(|| {
                        format!("Failed to cleanup previous '{}' rag", super::TEMP_RAG_NAME)
                    })?;
                }
                Arc::new(Rag::init(&app, super::TEMP_RAG_NAME, &rag_path, &[], abort_signal).await?)
            }
            Some(name) => {
                let rag_path = self.rag_file(name);
                let key = RagKey::Named(name.to_string());

                rag_cache
                    .load_with(key, || {
                        let app = app.clone();
                        let rag_path = rag_path.clone();
                        let abort_signal = abort_signal.clone();
                        async move {
                            if !rag_path.exists() {
                                if working_mode.is_cmd() {
                                    bail!("Unknown RAG '{name}'");
                                }
                                Rag::init(&app, name, &rag_path, &[], abort_signal.clone()).await
                            } else {
                                Rag::load(&app, name, &rag_path)
                            }
                        }
                    })
                    .await?
            }
        };
        self.rag = Some(rag);
        Ok(())
    }

    pub async fn edit_rag_docs(&mut self, abort_signal: AbortSignal) -> Result<()> {
        let mut rag = match self.rag.clone() {
            Some(v) => v.as_ref().clone(),
            None => bail!("No RAG"),
        };

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

        let key = if self.agent.is_some() {
            RagKey::Agent(rag.name().to_string())
        } else {
            RagKey::Named(rag.name().to_string())
        };
        self.rag_cache().invalidate(&key);

        rag.refresh_document_paths(&new_document_paths, false, &self.app.config, abort_signal)
            .await?;
        self.rag = Some(Arc::new(rag));
        Ok(())
    }

    pub async fn rebuild_rag(&mut self, abort_signal: AbortSignal) -> Result<()> {
        let mut rag = match self.rag.clone() {
            Some(v) => v.as_ref().clone(),
            None => bail!("No RAG"),
        };

        let key = if self.agent.is_some() {
            RagKey::Agent(rag.name().to_string())
        } else {
            RagKey::Named(rag.name().to_string())
        };
        self.rag_cache().invalidate(&key);

        let document_paths = rag.document_paths().to_vec();
        rag.refresh_document_paths(&document_paths, true, &self.app.config, abort_signal)
            .await?;
        self.rag = Some(Arc::new(rag));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::mcp_factory::McpFactory;
    use super::*;
    use crate::config::AppState;
    use crate::function::{ToolCall, skill};
    use crate::mcp::{McpServer, McpServersConfig, McpTransportType};
    use crate::utils;
    use crate::utils::get_env_name;
    use crate::vault::Vault;
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use std::fs::{create_dir_all, remove_dir_all, write};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

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
    fn use_prompt_creates_temp_role() {
        let mut ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        ctx.use_prompt(&app, "you are a pirate").unwrap();
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
    fn escalation_queue_defaults_to_none() {
        let ctx = create_test_ctx();
        assert!(ctx.root_escalation_queue().is_none());
    }

    fn app_state_with_mcp_config(mcp_server_support: bool, server_names: &[&str]) -> Arc<AppState> {
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
                        command: Some("echo".to_string()),
                        args: None,
                        env: None,
                        cwd: None,
                        url: None,
                        headers: None,
                        oauth: None,
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
    fn select_functions_all_enabled_tools_returns_all_non_mcp() {
        let mut ctx = create_test_ctx();
        ctx.tool_scope.functions.append_todo_functions();
        ctx.tool_scope.functions.append_user_interaction_functions();

        let mut role = Role::new("r", "p");
        role.set_enabled_tools(Some(vec!["all".to_string()]));

        let fns = ctx.select_functions(&role).unwrap();
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"todo__init"));
        assert!(names.contains(&"user__ask"));
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
        assert!(names.contains(&"user__ask"));
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
        ctx.tool_scope
            .functions
            .append_mcp_meta_functions(vec!["github".into(), "slack".into()]);

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
        ctx.tool_scope
            .functions
            .append_mcp_meta_functions(vec!["github".into(), "slack".into()]);

        let mut role = Role::new("r", "p");
        role.set_enabled_mcp_servers(Some(vec!["github".to_string()]));

        let fns = ctx.select_enabled_mcp_servers(&role);
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"mcp_invoke_github"));
        assert!(!names.contains(&"mcp_invoke_slack"));
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
        let sessions_dir = paths::local_path("sessions");
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
        let sessions_dir = paths::local_path("sessions");
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
        let sessions_dir = paths::local_path("sessions");
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
    fn bundled_graph_agents_parse_and_validate() {
        use crate::graph::GraphParser;
        use crate::graph::validator::GraphValidator;

        let _guard = TestConfigDirGuard::new();

        Agent::install_builtin_agents(false).unwrap();
        Skill::install_builtin_skills(false).unwrap();

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
            let result = GraphValidator::new(&dir).validate(&graph);
            assert!(
                result.errors.is_empty(),
                "graph.yaml for '{name}' failed validation: {:#?}",
                result.errors
            );
            checked.push(name);
        }
        checked.sort();
        for expected in ["coder", "librarian", "step-runner"] {
            assert!(
                checked.iter().any(|n| n == expected),
                "expected bundled graph agent '{expected}' to be checked; found {checked:?}"
            );
        }
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
}
