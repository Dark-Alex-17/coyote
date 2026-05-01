//! Per-request mutable state for a single Loki interaction.
//!
//! `RequestContext` owns the runtime state that was previously stored
//! on `Config` as `#[serde(skip)]` fields: the active role, session,
//! agent, RAG, supervisor state, inbox/escalation queues, the
//! conversation's "last message" cursor, and the per-scope
//! [`ToolScope`](super::tool_scope::ToolScope) carrying functions and
//! live MCP handles.
//!
//! Each frontend constructs and owns a `RequestContext`:
//!
//! * **CLI** — one `RequestContext` per invocation, dropped at exit.
//! * **REPL** — one long-lived `RequestContext` mutated across turns.
//! * **API** — one `RequestContext` per HTTP request, hydrated from a
//!   persisted session and written back at the end.
//!
//! `RequestContext` is built via [`RequestContext::bootstrap`] (CLI/REPL
//! entry point) or [`RequestContext::new`] (test/child-agent helper).
//! It holds an `Arc<AppState>` for shared, immutable services
//! (config, vault, MCP factory, RAG cache, MCP registry, base
//! functions).

use super::MessageContentToolCalls;
use super::rag_cache::{RagCache, RagKey};
use super::session::Session;
use super::todo::TodoList;
use super::tool_scope::{McpRuntime, ToolScope};
use super::{
    AGENTS_DIR_NAME, Agent, AgentVariables, AppConfig, AppState, CREATE_TITLE_ROLE, Input,
    LEFT_PROMPT, LastMessage, MESSAGES_FILE_NAME, RIGHT_PROMPT, Role, RoleLike, SESSIONS_DIR_NAME,
    SUMMARIZATION_PROMPT, SUMMARY_CONTEXT_PROMPT, StateFlags, TEMP_ROLE_NAME, TEMP_SESSION_NAME,
    WorkingMode, ensure_parent_exists, list_agents, paths,
};
use crate::client::{Model, ModelType, list_models};
use crate::function::{
    FunctionDeclaration, Functions, ToolCallTracker, ToolResult,
    user_interaction::USER_FUNCTION_PREFIX,
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

use anyhow::{Context, Error, Result, bail};
use indoc::formatdoc;
use inquire::{Confirm, MultiSelect, Text, list_option::ListOption, validator::Validation};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{File, OpenOptions, read_dir, read_to_string, remove_dir_all, remove_file};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    pub todo_list: TodoList,
    pub last_continuation_response: Option<String>,
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
            todo_list: TodoList::default(),
            last_continuation_response: None,
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
            todo_list: TodoList::default(),
            last_continuation_response: None,
        })
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
            todo_list: TodoList::default(),
            last_continuation_response: None,
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
        flags
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

    pub fn extract_role(&self, app: &AppConfig) -> Role {
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
                app.temperature,
                app.top_p,
                app.enabled_tools.clone(),
                app.enabled_mcp_servers.clone(),
            );
            role
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

    pub fn set_enabled_tools_on_role_like(&mut self, value: Option<String>) -> bool {
        match self.role_like_mut() {
            Some(role_like) => {
                role_like.set_enabled_tools(value);
                true
            }
            None => false,
        }
    }

    pub fn set_enabled_mcp_servers_on_role_like(&mut self, value: Option<String>) -> bool {
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
        let role = self.extract_role(app);
        let mut items = vec![
            ("model", role.model().id()),
            (
                "temperature",
                super::format_option_value(&role.temperature()),
            ),
            ("top_p", super::format_option_value(&role.top_p())),
            (
                "enabled_tools",
                super::format_option_value(&role.enabled_tools()),
            ),
            (
                "enabled_mcp_servers",
                super::format_option_value(&role.enabled_mcp_servers()),
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
            ("sessions_dir", display_path(&self.sessions_dir())),
            ("rags_dir", display_path(&paths::rags_dir())),
            ("macros_dir", display_path(&paths::macros_dir())),
            ("functions_dir", display_path(&paths::functions_dir())),
            ("messages_file", display_path(&self.messages_file())),
            (
                "vault_password_file",
                display_path(&app.vault_password_file()),
            ),
        ];
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
        let role = self.extract_role(app);
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
            if let Some(enabled_tools) = role.enabled_tools() {
                let mut tool_names: HashSet<String> = Default::default();
                let declaration_names: HashSet<String> = self
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
                if enabled_tools == "all" {
                    tool_names.extend(declaration_names);
                } else {
                    for item in enabled_tools.split(',') {
                        let item = item.trim();
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
                        v.name.starts_with(USER_FUNCTION_PREFIX) && !existing.contains(&v.name)
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
            if let Some(enabled_mcp_servers) = role.enabled_mcp_servers() {
                let mut server_names: HashSet<String> = Default::default();
                let mcp_declaration_names: HashSet<String> = self
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
        let agent_config_path = paths::agent_config_file(agent_name);
        ensure_parent_exists(&agent_config_path)?;
        if !agent_config_path.exists() {
            std::fs::write(
                &agent_config_path,
                "# see https://github.com/Dark-Alex-17/loki/blob/main/config.agent.example.yaml\n",
            )
            .with_context(|| format!("Failed to write to '{}'", agent_config_path.display()))?;
        }
        let editor = app.editor()?;
        edit_file(&editor, &agent_config_path)?;
        println!(
            "NOTE: Remember to reload the agent if there are changes made to '{}'",
            agent_config_path.display()
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
        let parts: Vec<&str> = data.split_whitespace().collect();
        if parts.len() != 2 {
            bail!("Usage: .set <key> <value>. If value is null, unset key.");
        }
        let key = parts[0];
        let value = parts[1];
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
                let value = super::parse_value(value)?;
                if !self.set_enabled_tools_on_role_like(value.clone()) {
                    self.update_app_config(|app| app.enabled_tools = value);
                }
            }
            "enabled_mcp_servers" => {
                let value: Option<String> = super::parse_value(value)?;
                if let Some(servers) = value.as_ref() {
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

                    if !servers.split(',').all(|s| {
                        let server = s.trim();
                        server == "all" || mcp_config.mcp_servers.contains_key(server)
                    }) {
                        bail!(
                            "Some of the specified MCP servers in 'enabled_mcp_servers' are not fully configured. Please check your MCP server configuration."
                        );
                    }
                }
                if !self.set_enabled_mcp_servers_on_role_like(value.clone()) {
                    self.update_app_config(|app| app.enabled_mcp_servers = value.clone());
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
                ".delete" => super::map_completion_values(vec![
                    "role",
                    "session",
                    "rag",
                    "macro",
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
                _ => vec![],
            };
            values = candidates.into_iter().map(|v| (v, None)).collect();
        } else if cmd == ".vault" && args.len() == 2 {
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
                let dir = paths::agent_data_dir(args[0]).join(super::SESSIONS_DIR_NAME);
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
        enabled_mcp_servers: Option<String>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let mut mcp_runtime = McpRuntime::new();

        if app.mcp_server_support
            && let Some(mcp_config) = &self.app.mcp_config
        {
            let server_ids: Vec<String> = match &enabled_mcp_servers {
                Some(servers) if servers == "all" => {
                    mcp_config.mcp_servers.keys().cloned().collect()
                }
                Some(servers) => {
                    let mut ids = Vec::new();
                    for item in servers.split(',').map(|s| s.trim()) {
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
        if !mcp_runtime.is_empty() {
            functions.append_mcp_meta_functions(mcp_runtime.server_names());
        }

        let tool_tracker = self.tool_scope.tool_tracker.clone();
        self.tool_scope = ToolScope {
            functions,
            mcp_runtime,
            tool_tracker,
        };
        Ok(())
    }

    pub async fn use_role(
        &mut self,
        app: &AppConfig,
        name: &str,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let role = self.retrieve_role(app, name)?;
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

        self.rebuild_tool_scope(app, mcp_servers, abort_signal)
            .await?;
        self.use_role_obj(role)
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
                session = Some(Session::new_from_ctx(self, app, TEMP_SESSION_NAME));
            }
            Some(name) => {
                let session_path = self.session_file(name);
                if !session_path.exists() {
                    session = Some(Session::new_from_ctx(self, app, name));
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

        let mcp_servers = if app.mcp_server_support {
            (!agent.mcp_server_names().is_empty()).then(|| agent.mcp_server_names().join(","))
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

        let session_name = session_name.map(|v| v.to_string()).or_else(|| {
            if self.macro_flag {
                None
            } else {
                agent.agent_session().map(|v| v.to_string())
            }
        });

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
                supervisor.read().cancel_all();
            }
            self.supervisor = None;
            self.parent_supervisor = None;
            self.self_agent_id = None;
            self.inbox = None;
            self.escalation_queue = None;
            self.current_depth = 0;
            self.auto_continue_count = 0;
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
    ) -> Option<String> {
        if !start_mcp_servers || !app.mcp_server_support {
            return None;
        }
        if let Some(agent) = self.agent.as_ref() {
            return (!agent.mcp_server_names().is_empty())
                .then(|| agent.mcp_server_names().join(","));
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
        let input = Input::from_str(self, &prompt, None);
        let summary = input.fetch_chat_text().await?;
        let summary_context_prompt = self
            .app
            .config
            .summary_context_prompt
            .clone()
            .unwrap_or_else(|| SUMMARY_CONTEXT_PROMPT.into());

        let todo_prefix = if self.agent.is_some() && !self.todo_list.is_empty() {
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
        let input = Input::from_str(self, &text, Some(role));
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
    use super::*;
    use crate::config::AppState;
    use crate::utils::get_env_name;
    use std::env;
    use std::fs::{create_dir_all, remove_dir_all, write};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use crate::function::ToolCall;
    use crate::mcp::{McpServer, McpServersConfig, McpTransportType};
    use crate::utils;
    use crate::vault::Vault;
    use super::super::mcp_factory::McpFactory;

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
            let path = env::temp_dir().join(format!("loki-request-context-tests-{unique}"));
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
        Arc::new(AppState {
            config: Arc::new(AppConfig::default()),
            vault: Arc::new(Vault::default()),
            mcp_factory: Arc::new(McpFactory::default()),
            rag_cache: Arc::new(RagCache::default()),
            mcp_config: None,
            mcp_log_path: None,
            mcp_registry: None,
            functions: Functions::default(),
        })
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
        let extracted = ctx.extract_role(&app);
        assert_eq!(extracted.name(), "myrole");
    }

    #[test]
    fn extract_role_returns_default_when_nothing_active() {
        let ctx = create_test_ctx();
        let app = ctx.app.config.clone();
        let extracted = ctx.extract_role(&app);
        assert_eq!(extracted.name(), "");
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
        let mut app_config = AppConfig::default();
        app_config.mcp_server_support = mcp_server_support;

        let mcp_config = if server_names.is_empty() {
            None
        } else {
            let mut servers = HashMap::new();
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
    fn rebuild_tool_scope_mcp_disabled_skips_servers() {
        let app_state = app_state_with_mcp_config(false, &["github", "slack"]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        run_async(ctx.rebuild_tool_scope(&app, Some("all".to_string()), abort)).unwrap();

        assert!(ctx.tool_scope.mcp_runtime.is_empty());
    }

    #[test]
    fn rebuild_tool_scope_no_enabled_servers_yields_empty_runtime() {
        let app_state = app_state_with_mcp_config(true, &["github"]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        run_async(ctx.rebuild_tool_scope(&app, None, abort)).unwrap();

        assert!(ctx.tool_scope.mcp_runtime.is_empty());
    }

    #[test]
    fn rebuild_tool_scope_no_mcp_config_yields_empty_runtime() {
        let app_state = app_state_with_mcp_config(true, &[]);
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        let abort = utils::create_abort_signal();

        run_async(ctx.rebuild_tool_scope(&app, Some("all".to_string()), abort)).unwrap();

        assert!(ctx.tool_scope.mcp_runtime.is_empty());
    }

    #[test]
    fn rebuild_tool_scope_preserves_tool_tracker() {
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
    fn rebuild_tool_scope_repl_mode_appends_user_interaction_functions() {
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
    fn rebuild_tool_scope_cmd_mode_no_user_interaction_functions() {
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
}
