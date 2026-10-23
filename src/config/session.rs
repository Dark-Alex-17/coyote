use super::input::*;
use super::todo::TodoList;
use super::*;

use crate::client::{Message, MessageContent, MessageRole, TokenUsage};
use crate::graph::LastRun;
use crate::hooks::HooksMap;
use crate::render::MarkdownRender;

use anyhow::{Context, Result, bail};
use fancy_regex::Regex;
use inquire::{Confirm, Text, validator::Validation};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::fs::{OpenOptions, read_to_string, remove_file, rename};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

static RE_AUTONAME_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{8}T\d{6}-").unwrap());

/// Distinguishes the temp files of concurrent `save` calls in one process,
/// so two in-process writers sharing one session path (a caller that
/// bypasses the ownership gate, or a future one) cannot have the second
/// writer's stale-temp retry unlink the first writer's in-flight temp file.
static SAVE_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub const INTERRUPTED_RESPONSE_TEXT: &str = "[Response interrupted due to error]";

/// Closes a turn the user ended mid-tool-loop with Ctrl-C. Distinct from
/// `INTERRUPTED_RESPONSE_TEXT` so a deliberate interrupt never reads as
/// an error checkpoint.
pub const TURN_INTERRUPTED_TEXT: &str = "[Turn interrupted by the user before the tool loop finished. Background tasks started above were left uncollected; reclaim each with agent__collect / job__collect, or cancel it, before relying on its result.]";

/// Auto-compression only fires when at least `threshold / 5` (20%) of the
/// threshold is reclaimable, so each compression folds a meaningful batch
/// instead of thrashing every turn.
const COMPRESSION_MIN_RECLAIM_DIVISOR: usize = 5;

/// Reclaimable tokens the threshold trigger needs before it fires. `None`
/// when the threshold is off.
pub fn min_reclaim_tokens(threshold: usize) -> Option<usize> {
    (threshold >= 1).then(|| (threshold / COMPRESSION_MIN_RECLAIM_DIVISOR).max(1))
}

/// What compression keeps verbatim: at most `last` trailing messages, and
/// only as many of those as fit in `tokens`. A count alone cannot bound the
/// kept tail, because one tool-loop message can hold hundreds of results.
#[derive(Clone, Copy, Debug)]
pub struct KeepPolicy {
    pub last: usize,
    pub tokens: usize,
}

/// Prompt size at which the safety valve fires: `valve` of the model
/// window. `None` when the model declares no window or the valve is off.
pub fn valve_limit(valve: f32, window: Option<usize>) -> Option<usize> {
    window
        .filter(|_| valve.is_finite() && valve > 0.0)
        .map(|window| (valve * window as f32) as usize)
}

/// Token budget for the kept tail: half the compression threshold or a
/// quarter of the valve's share of the model window, whichever applies and
/// is smaller. A tail larger than this would leave compression unable to
/// bring the session back under the limit that triggered it.
pub fn keep_token_budget(threshold: usize, valve: f32, window: Option<usize>) -> usize {
    let threshold_term = (threshold >= 1).then_some(threshold / 2);
    let valve_term = valve_limit(valve, window).map(|limit| limit / 4);
    threshold_term
        .into_iter()
        .chain(valve_term)
        .min()
        .unwrap_or(usize::MAX)
}

/// Effective compression settings, the limits derived from them and, when
/// built inside a session, what a compression right now would do.
#[derive(Clone, Debug, PartialEq)]
pub struct CompressionInfo {
    pub threshold: usize,
    pub keep_last: usize,
    pub safety_valve: f32,
    pub model: Option<String>,
    pub valve_limit: Option<usize>,
    pub keep_tokens: usize,
    pub checkpoint_tokens: usize,
    pub min_reclaim: Option<usize>,
    pub last_prompt_tokens: Option<usize>,
    pub reclaimable_tokens: Option<usize>,
    pub kept_tail_messages: Option<usize>,
    pub kept_tail_tokens: Option<usize>,
    pub archived_messages: Option<usize>,
}

impl CompressionInfo {
    pub fn rows(&self) -> Vec<(&'static str, String)> {
        let mut rows = vec![
            ("compression_threshold", self.threshold.to_string()),
            ("compression_keep_last", self.keep_last.to_string()),
            ("compression_safety_valve", self.safety_valve.to_string()),
            ("compression_model", format_option_value(&self.model)),
            (
                "compression_valve_limit",
                format_option_value(&self.valve_limit),
            ),
            (
                "compression_keep_tokens",
                match self.keep_tokens {
                    usize::MAX => "unlimited".to_string(),
                    tokens => tokens.to_string(),
                },
            ),
            (
                "compression_checkpoint_tokens",
                self.checkpoint_tokens.to_string(),
            ),
            (
                "compression_min_reclaim",
                format_option_value(&self.min_reclaim),
            ),
            (
                "compression_last_prompt_tokens",
                format_option_value(&self.last_prompt_tokens),
            ),
        ];
        let live = [
            ("compression_reclaimable", self.reclaimable_tokens),
            ("compression_kept_tail_messages", self.kept_tail_messages),
            ("compression_kept_tail_tokens", self.kept_tail_tokens),
            ("compression_archived_messages", self.archived_messages),
        ];
        rows.extend(
            live.into_iter()
                .filter_map(|(key, value)| Some((key, value?.to_string()))),
        );
        rows
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut data = json!({
            "threshold": self.threshold,
            "keep_last": self.keep_last,
            "safety_valve": (f64::from(self.safety_valve) * 1e6).round() / 1e6,
            "model": self.model,
            "valve_limit": self.valve_limit,
            "keep_tokens": match self.keep_tokens {
                usize::MAX => json!("unlimited"),
                tokens => tokens.into(),
            },
            "checkpoint_tokens": self.checkpoint_tokens,
            "min_reclaim": self.min_reclaim,
            "last_prompt_tokens": self.last_prompt_tokens,
        });
        let live = [
            ("reclaimable", self.reclaimable_tokens),
            ("kept_tail_messages", self.kept_tail_messages),
            ("kept_tail_tokens", self.kept_tail_tokens),
            ("archived_messages", self.archived_messages),
        ];
        for (key, value) in live {
            if let Some(value) = value {
                data[key] = value.into();
            }
        }
        data
    }
}

fn cost_is_zero(v: &f64) -> bool {
    *v == 0.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionScope {
    #[default]
    Global,
    Workspace,
}

impl fmt::Display for SessionScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionScope::Global => write!(f, "global"),
            SessionScope::Workspace => write!(f, "workspace"),
        }
    }
}

pub fn session_scope_dirs(agent: Option<&str>) -> (PathBuf, PathBuf) {
    match agent {
        Some(agent) => (
            paths::workspace_agent_sessions_dir(agent),
            paths::agent_sessions_dir(agent),
        ),
        None => (paths::workspace_sessions_dir(), default_sessions_dir()),
    }
}

/// Session names across both scope dirs, workspace first, deduped by name so
/// a workspace session shadows a global one with the same name.
pub fn labeled_session_names(
    workspace_dir: &Path,
    global_dir: &Path,
) -> Vec<(String, SessionScope)> {
    let mut values = Vec::new();
    let mut seen = HashSet::new();
    for (dir, scope) in [
        (workspace_dir, SessionScope::Workspace),
        (global_dir, SessionScope::Global),
    ] {
        for name in list_file_names(dir, ".yaml") {
            if seen.insert(name.clone()) {
                values.push((name, scope));
            }
        }
    }
    values
}

pub fn merged_session_names(workspace_dir: &Path, global_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = labeled_session_names(workspace_dir, global_dir)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    names.sort_unstable();
    names
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Session {
    #[serde(rename(serialize = "model", deserialize = "model"))]
    model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::deserialize_csv_or_vec"
    )]
    enabled_tools: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::deserialize_csv_or_vec"
    )]
    enabled_mcp_servers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mcp_tools: Option<IndexMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::deserialize_csv_or_vec"
    )]
    enabled_skills: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::deserialize_csv_or_vec"
    )]
    enabled_macros: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    save_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compression_threshold: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compression_keep_last: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_continue: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_auto_continues: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inject_todo_instructions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    continuation_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inject_skill_instructions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skill_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    role_name: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    agent_variables: AgentVariables,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    agent_instructions: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    compressed_messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compressed_system_prompt: Option<String>,
    messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    data_urls: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "TodoList::is_default")]
    todo_list: TodoList,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auto_continue_paused: Option<String>,
    #[serde(default, skip_serializing_if = "TokenUsage::is_empty")]
    token_usage: TokenUsage,
    #[serde(default, skip_serializing_if = "cost_is_zero")]
    cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_run: Option<LastRun>,
    /// Stable identity of the directory this session's child sessions live
    /// under; minted the first time a graph runs under a persisted session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uid: Option<Uuid>,
    /// The config agent that answers this session's chat turns in place of
    /// the graph agent, with the variables and rendered prompt it was seated
    /// with; reused as is when the session is resumed under the same agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat_agent: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    chat_agent_variables: AgentVariables,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    chat_agent_instructions: String,

    #[serde(skip)]
    model: Model,
    #[serde(skip)]
    role_prompt: String,
    #[serde(skip)]
    role_hooks: Option<HooksMap>,
    #[serde(skip)]
    name: String,
    #[serde(skip)]
    path: Option<String>,
    #[serde(skip)]
    owner_only: bool,
    #[serde(skip)]
    scope: SessionScope,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    save_session_this_time: bool,
    #[serde(skip)]
    compressing: bool,
    #[serde(skip)]
    autoname: Option<AutoName>,
    #[serde(skip)]
    tokens: usize,
}

impl Session {
    pub fn skills_enabled(&self) -> Option<bool> {
        self.skills_enabled
    }

    pub fn enabled_skills(&self) -> Option<&[String]> {
        self.enabled_skills.as_deref()
    }

    pub fn enabled_macros(&self) -> Option<&[String]> {
        self.enabled_macros.as_deref()
    }

    pub fn set_skills_enabled(&mut self, value: Option<bool>) {
        if self.skills_enabled != value {
            self.skills_enabled = value;
            self.dirty = true;
        }
    }

    pub fn new_from_ctx(ctx: &RequestContext, app: &AppConfig, name: &str) -> Result<Self> {
        let role = ctx.extract_role(app)?;
        let mut session = Self {
            name: name.to_string(),
            save_session: app.save_session,
            todo_list: ctx.todo_list.clone(),
            auto_continue_paused: ctx.auto_continue_paused.clone(),
            ..Default::default()
        };
        session.set_role(role);
        // Undo set_role's dirtying, but stay dirty when carrying in-flight
        // todo/pause state: `Session::exit` only saves dirty sessions, and a
        // create-then-exit must not destroy the sole copy of that state.
        session.dirty = !ctx.todo_list.is_default() || ctx.auto_continue_paused.is_some();
        Ok(session)
    }

    pub fn load_from_ctx(
        ctx: &RequestContext,
        app: &AppConfig,
        name: &str,
        path: &Path,
        scope: SessionScope,
    ) -> Result<Self> {
        let content = read_to_string(path)
            .with_context(|| format!("Failed to load session {} at {}", name, path.display()))?;
        let mut session: Self =
            serde_yaml::from_str(&content).with_context(|| format!("Invalid session {name}"))?;

        session.model = Model::retrieve_model(app, &session.model_id, ModelType::Chat)?;
        session.scope = scope;

        if let Some(autoname) = name.strip_prefix("_/") {
            session.name = TEMP_SESSION_NAME.to_string();
            session.path = None;
            if let Ok(true) = RE_AUTONAME_PREFIX.is_match(autoname) {
                session.autoname = Some(AutoName::new(autoname[16..].to_string()));
            }
        } else {
            session.name = name.to_string();
            session.path = Some(path.display().to_string());
        }

        if let Some(role_name) = &session.role_name
            && let Ok(role) = ctx.retrieve_role(app, role_name)
        {
            session.role_prompt = role.prompt().to_string();
            session.role_hooks = role.hooks().cloned();
        }

        session.update_tokens();

        Ok(session)
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.compressed_messages.is_empty()
    }

    pub fn has_interrupted_error_checkpoint(&self) -> bool {
        self.messages.last().is_some_and(|message| {
            message.role.is_assistant()
                && matches!(&message.content, MessageContent::Text(text) if text.ends_with(INTERRUPTED_RESPONSE_TEXT))
        })
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn compressed_messages(&self) -> &[Message] {
        &self.compressed_messages
    }

    pub fn todo_list(&self) -> &TodoList {
        &self.todo_list
    }

    pub fn auto_continue_paused(&self) -> Option<&str> {
        self.auto_continue_paused.as_deref()
    }

    pub fn sync_todo_state(&mut self, todo_list: &TodoList, auto_continue_paused: Option<&str>) {
        if self.todo_list == *todo_list
            && self.auto_continue_paused.as_deref() == auto_continue_paused
        {
            return;
        }
        self.todo_list = todo_list.clone();
        self.auto_continue_paused = auto_continue_paused.map(str::to_string);
        self.dirty = true;
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn scope(&self) -> SessionScope {
        self.scope
    }

    pub fn set_scope(&mut self, scope: SessionScope) {
        self.scope = scope;
    }

    pub fn clear_autoname(&mut self) {
        self.autoname = None;
    }

    pub fn role_name(&self) -> Option<&str> {
        self.role_name.as_deref()
    }

    /// Hooks of the role this session holds. `set_role` snapshots them
    /// because only the frontmatter-stripped role prompt is persisted;
    /// `load_from_ctx` restores them by re-reading the role file on resume.
    pub fn role_hooks(&self) -> Option<&HooksMap> {
        self.role_hooks.as_ref()
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn save_session(&self) -> Option<bool> {
        self.save_session
    }

    pub fn effective_save_session(&self) -> Option<bool> {
        if self.save_session_this_time {
            Some(true)
        } else {
            self.save_session()
        }
    }

    pub fn tokens(&self) -> usize {
        self.tokens
    }

    pub fn update_tokens(&mut self) {
        self.tokens = self.model().total_tokens(&self.messages);
    }

    pub fn token_usage(&self) -> &TokenUsage {
        &self.token_usage
    }

    /// Accumulates usage reported by one API call. Each tool-loop iteration
    /// is a separately billed request, so totals grow per call; compression
    /// rewrites messages but never resets these counters.
    pub fn accumulate_token_usage(&mut self, usage: &TokenUsage) {
        self.token_usage.accumulate(usage);
        self.dirty = true;
    }

    pub fn cost(&self) -> f64 {
        self.cost
    }

    pub fn accumulate_cost(&mut self, cost: f64) {
        self.cost += cost;
        self.dirty = true;
    }

    pub fn last_run(&self) -> Option<&LastRun> {
        self.last_run.as_ref()
    }

    /// Call after the run's exchange has been saved: the recorded count must
    /// include it for `pop_last_exchange` to recognise the undo.
    pub fn set_last_run(&mut self, run: Option<LastRun>) {
        let recorded_at_message_count = self.messages.len();
        self.last_run = run.map(|mut run| {
            run.recorded_at_message_count = recorded_at_message_count;
            run
        });
        self.dirty = true;
    }

    pub fn has_user_messages(&self) -> bool {
        self.messages.iter().any(|v| v.role.is_user())
    }

    pub fn user_messages_len(&self) -> usize {
        self.messages.iter().filter(|v| v.role.is_user()).count()
    }

    pub fn export(&self, compression: Option<&CompressionInfo>) -> Result<String> {
        let mut data = json!({
            "path": self.path,
            "scope": self.scope.to_string(),
            "model": self.model().id(),
        });
        if let Some(temperature) = self.temperature() {
            data["temperature"] = temperature.into();
        }
        if let Some(top_p) = self.top_p() {
            data["top_p"] = top_p.into();
        }
        if let Some(enabled_tools) = self.enabled_tools() {
            data["enabled_tools"] = json!(enabled_tools);
        }
        if let Some(enabled_mcp_servers) = self.enabled_mcp_servers() {
            data["enabled_mcp_servers"] = json!(enabled_mcp_servers);
        }
        if let Some(mcp_tools) = self.mcp_tools() {
            data["mcp_tools"] = json!(mcp_tools);
        }
        if let Some(skills_enabled) = self.skills_enabled() {
            data["skills_enabled"] = skills_enabled.into();
        }
        if let Some(enabled_skills) = self.enabled_skills() {
            data["enabled_skills"] = json!(enabled_skills);
        }
        if let Some(enabled_macros) = self.enabled_macros() {
            data["enabled_macros"] = json!(enabled_macros);
        }
        if let Some(save_session) = self.save_session() {
            data["save_session"] = save_session.into();
        }
        if let Some(auto_continue) = self.auto_continue() {
            data["auto_continue"] = auto_continue.into();
        }
        if let Some(max_auto_continues) = self.max_auto_continues() {
            data["max_auto_continues"] = max_auto_continues.into();
        }
        if let Some(inject_todo_instructions) = self.inject_todo_instructions() {
            data["inject_todo_instructions"] = inject_todo_instructions.into();
        }
        if let Some(continuation_prompt) = self.continuation_prompt() {
            data["continuation_prompt"] = continuation_prompt.into();
        }
        if let Some(inject_skill_instructions) = self.inject_skill_instructions() {
            data["inject_skill_instructions"] = inject_skill_instructions.into();
        }
        if let Some(skill_instructions) = self.skill_instructions() {
            data["skill_instructions"] = skill_instructions.into();
        }
        if let Some(memory) = self.memory() {
            data["memory"] = memory.into();
        }
        let (tokens, percent) = self.tokens_usage();
        data["total_tokens"] = tokens.into();
        if let Some(max_input_tokens) = self.model().max_input_tokens() {
            data["max_input_tokens"] = max_input_tokens.into();
        }
        if percent != 0.0 {
            data["total/max"] = format!("{percent}%").into();
        }
        if let Some(compression) = compression {
            data["compression"] = compression.to_json();
        }
        if let Some(value) = self.token_usage.input_tokens {
            data["real_input_tokens"] = value.into();
        }
        if let Some(value) = self.token_usage.output_tokens {
            data["real_output_tokens"] = value.into();
        }
        if let Some(value) = self.token_usage.cache_read_input_tokens {
            data["cache_read_tokens"] = value.into();
        }
        if let Some(value) = self.token_usage.cache_creation_input_tokens {
            data["cache_creation_tokens"] = value.into();
        }
        if self.cost != 0.0 {
            data["total_cost"] = ((self.cost * 1e6).round() / 1e6).into();
        }
        data["messages"] = json!(self.messages);

        let output = serde_yaml::to_string(&data)
            .with_context(|| format!("Unable to show info about session '{}'", self.name))?;
        Ok(output)
    }

    pub fn render(
        &self,
        render: &mut MarkdownRender,
        agent_info: &Option<(String, Vec<String>)>,
        compression: Option<&CompressionInfo>,
    ) -> Result<String> {
        let mut items = vec![];

        if let Some(path) = &self.path {
            items.push(("path", path.to_string()));
        }
        items.push(("scope", self.scope.to_string()));

        if let Some(autoname) = self.autoname() {
            items.push(("autoname", autoname.to_string()));
        }

        items.push(("model", self.model().id()));

        if let Some(temperature) = self.temperature() {
            items.push(("temperature", temperature.to_string()));
        }
        if let Some(top_p) = self.top_p() {
            items.push(("top_p", top_p.to_string()));
        }

        if let Some(enabled_tools) = self.enabled_tools() {
            items.push(("enabled_tools", enabled_tools.join(",")));
        }

        if let Some(enabled_mcp_servers) = self.enabled_mcp_servers() {
            items.push(("enabled_mcp_servers", enabled_mcp_servers.join(",")));
        }

        if let Some(mcp_tools) = self.mcp_tools() {
            items.push((
                "mcp_tools",
                serde_json::to_string(&mcp_tools).unwrap_or_default(),
            ));
        }

        if let Some(skills_enabled) = self.skills_enabled() {
            items.push(("skills_enabled", skills_enabled.to_string()));
        }

        if let Some(enabled_skills) = self.enabled_skills() {
            items.push(("enabled_skills", enabled_skills.join(",")));
        }

        if let Some(enabled_macros) = self.enabled_macros() {
            items.push(("enabled_macros", enabled_macros.join(",")));
        }

        if let Some(save_session) = self.save_session() {
            items.push(("save_session", save_session.to_string()));
        }

        if let Some(compression) = compression {
            items.extend(compression.rows());
        } else {
            if let Some(compression_threshold) = self.compression_threshold {
                items.push(("compression_threshold", compression_threshold.to_string()));
            }
            if let Some(compression_keep_last) = self.compression_keep_last {
                items.push(("compression_keep_last", compression_keep_last.to_string()));
            }
        }

        if let Some(auto_continue) = self.auto_continue() {
            items.push(("auto_continue", auto_continue.to_string()));
        }
        if let Some(max_auto_continues) = self.max_auto_continues() {
            items.push(("max_auto_continues", max_auto_continues.to_string()));
        }
        if let Some(inject_todo_instructions) = self.inject_todo_instructions() {
            items.push((
                "inject_todo_instructions",
                inject_todo_instructions.to_string(),
            ));
        }
        if let Some(continuation_prompt) = self.continuation_prompt() {
            items.push(("continuation_prompt", continuation_prompt.to_string()));
        }
        if let Some(inject_skill_instructions) = self.inject_skill_instructions() {
            items.push((
                "inject_skill_instructions",
                inject_skill_instructions.to_string(),
            ));
        }
        if let Some(skill_instructions) = self.skill_instructions() {
            items.push(("skill_instructions", skill_instructions.to_string()));
        }
        if let Some(memory) = self.memory() {
            items.push(("memory", memory.to_string()));
        }

        let (tokens, percent) = self.tokens_usage();
        items.push(("total_tokens", tokens.to_string()));
        if let Some(max_input_tokens) = self.model().max_input_tokens() {
            items.push(("max_input_tokens", max_input_tokens.to_string()));
        }
        if percent != 0.0 {
            items.push(("total/max", format!("{percent}%")));
        }

        if let Some(value) = self.token_usage.input_tokens {
            items.push(("real_input_tokens", value.to_string()));
        }
        if let Some(value) = self.token_usage.output_tokens {
            items.push(("real_output_tokens", value.to_string()));
        }
        if let Some(value) = self.token_usage.cache_read_input_tokens {
            items.push(("cache_read_tokens", value.to_string()));
        }
        if let Some(value) = self.token_usage.cache_creation_input_tokens {
            items.push(("cache_creation_tokens", value.to_string()));
        }
        if self.cost != 0.0 {
            items.push(("total_cost", format!("${:.4}", self.cost)));
        }

        let mut lines: Vec<String> = items
            .iter()
            .map(|(name, value)| format!("{name:<20}{value}"))
            .collect();

        lines.push(String::new());

        if !self.is_empty() {
            let resolve_url_fn = |url: &str| resolve_data_url(&self.data_urls, url.to_string());

            for message in &self.messages {
                match message.role {
                    MessageRole::System => {
                        let body = render
                            .render(&message.content.render_input(resolve_url_fn, agent_info));
                        let tail = render.finalize();
                        if tail.is_empty() {
                            lines.push(body);
                        } else {
                            lines.push(format!("{body}\n{tail}"));
                        }
                    }
                    MessageRole::Assistant => {
                        if let MessageContent::Text(text) = &message.content {
                            let body = render.render(text);
                            let tail = render.finalize();
                            if tail.is_empty() {
                                lines.push(body);
                            } else {
                                lines.push(format!("{body}\n{tail}"));
                            }
                        }
                        lines.push("".into());
                    }
                    MessageRole::User => {
                        lines.push(format!(
                            ">> {}",
                            message.content.render_input(resolve_url_fn, agent_info)
                        ));
                    }
                    MessageRole::Tool => {
                        lines.push(message.content.render_input(resolve_url_fn, agent_info));
                    }
                }
            }
        }

        Ok(lines.join("\n"))
    }

    pub fn tokens_usage(&self) -> (usize, f32) {
        let tokens = self.tokens();
        let max_input_tokens = self.model().max_input_tokens().unwrap_or_default();
        let percent = if max_input_tokens == 0 {
            0.0
        } else {
            let percent = tokens as f32 / max_input_tokens as f32 * 100.0;
            (percent * 100.0).round() / 100.0
        };
        (tokens, percent)
    }

    pub fn set_role(&mut self, role: Role) {
        self.model_id = role.model().id();
        self.temperature = role.temperature();
        self.top_p = role.top_p();
        self.reasoning_effort = role.reasoning_effort();
        self.enabled_tools = role.enabled_tools();
        self.enabled_mcp_servers = role.enabled_mcp_servers();
        self.model = role.model().clone();
        self.role_name = convert_option_string(role.name());
        self.role_prompt = role.prompt().to_string();
        self.role_hooks = role.hooks().cloned();
        self.dirty = true;
        self.update_tokens();
    }

    pub fn clear_role(&mut self) {
        self.role_name = None;
        self.role_prompt.clear();
        self.role_hooks = None;
    }

    pub fn sync_agent(&mut self, agent: &Agent) {
        self.adopt_agent_prompt(agent);
        self.agent_variables = agent.variables().clone();
        self.agent_instructions = self.role_prompt.clone();
        self.chat_agent = None;
        self.chat_agent_variables.clear();
        self.chat_agent_instructions.clear();
    }

    pub fn sync_chat_agent(&mut self, agent: &Agent) {
        self.adopt_agent_prompt(agent);
        self.chat_agent = Some(agent.name().to_string());
        self.chat_agent_variables = agent.variables().clone();
        self.chat_agent_instructions = self.role_prompt.clone();
    }

    fn adopt_agent_prompt(&mut self, agent: &Agent) {
        self.role_name = None;
        self.role_prompt = agent.interpolated_instructions();
        self.role_hooks = None;
        if let Some(threshold) = agent.compression_threshold() {
            self.set_compression_threshold(Some(threshold));
        }
        if let Some(keep_last) = agent.compression_keep_last() {
            self.set_compression_keep_last(Some(keep_last));
        }
    }

    pub fn agent_variables(&self) -> &AgentVariables {
        &self.agent_variables
    }

    pub fn agent_instructions(&self) -> &str {
        &self.agent_instructions
    }

    pub fn chat_agent(&self) -> Option<&str> {
        self.chat_agent.as_deref()
    }

    pub fn chat_agent_variables(&self) -> &AgentVariables {
        &self.chat_agent_variables
    }

    pub fn chat_agent_instructions(&self) -> &str {
        &self.chat_agent_instructions
    }

    pub fn set_save_session(&mut self, value: Option<bool>) {
        if self.save_session != value {
            self.save_session = value;
            self.dirty = true;
        }
    }

    pub fn uid(&self) -> Option<Uuid> {
        self.uid
    }

    pub fn set_uid(&mut self, value: Option<Uuid>) {
        if self.uid != value {
            self.uid = value;
            self.dirty = true;
        }
    }

    /// Writes the session file readable by its owner alone; set for sessions
    /// that hold a spawned agent's transcript.
    #[allow(dead_code)] // TASK-237
    pub fn set_owner_only(&mut self, value: bool) {
        self.owner_only = value;
    }

    /// Points the session at the file `flush` writes, without saving.
    #[allow(dead_code)] // TASK-237
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path.display().to_string());
    }

    pub fn set_save_session_this_time(&mut self) {
        self.save_session_this_time = true;
    }

    pub fn compression_threshold(&self) -> Option<usize> {
        self.compression_threshold
    }

    pub fn set_compression_threshold(&mut self, value: Option<usize>) {
        if self.compression_threshold != value {
            self.compression_threshold = value;
            self.dirty = true;
        }
    }

    pub fn compression_keep_last(&self) -> Option<usize> {
        self.compression_keep_last
    }

    pub fn set_compression_keep_last(&mut self, value: Option<usize>) {
        if self.compression_keep_last != value {
            self.compression_keep_last = value;
            self.dirty = true;
        }
    }

    pub fn auto_continue(&self) -> Option<bool> {
        self.auto_continue
    }

    pub fn max_auto_continues(&self) -> Option<usize> {
        self.max_auto_continues
    }

    pub fn set_auto_continue(&mut self, value: Option<bool>) {
        if self.auto_continue != value {
            self.auto_continue = value;
            self.dirty = true;
        }
    }

    pub fn set_max_auto_continues(&mut self, value: Option<usize>) {
        if self.max_auto_continues != value {
            self.max_auto_continues = value;
            self.dirty = true;
        }
    }

    pub fn inject_todo_instructions(&self) -> Option<bool> {
        self.inject_todo_instructions
    }

    pub fn continuation_prompt(&self) -> Option<&str> {
        self.continuation_prompt.as_deref()
    }

    pub fn inject_skill_instructions(&self) -> Option<bool> {
        self.inject_skill_instructions
    }

    pub fn skill_instructions(&self) -> Option<&str> {
        self.skill_instructions.as_deref()
    }

    pub fn memory(&self) -> Option<bool> {
        self.memory
    }

    pub fn set_inject_todo_instructions(&mut self, value: Option<bool>) {
        if self.inject_todo_instructions != value {
            self.inject_todo_instructions = value;
            self.dirty = true;
        }
    }

    pub fn set_continuation_prompt(&mut self, value: Option<String>) {
        if self.continuation_prompt != value {
            self.continuation_prompt = value;
            self.dirty = true;
        }
    }

    pub fn set_inject_skill_instructions(&mut self, value: Option<bool>) {
        if self.inject_skill_instructions != value {
            self.inject_skill_instructions = value;
            self.dirty = true;
        }
    }

    pub fn set_memory(&mut self, value: Option<bool>) {
        if self.memory != value {
            self.memory = value;
            self.dirty = true;
        }
    }

    pub fn set_skill_instructions(&mut self, value: Option<String>) {
        if self.skill_instructions != value {
            self.skill_instructions = value;
            self.dirty = true;
        }
    }

    pub fn needs_compression(&self, global_compression_threshold: usize, keep: KeepPolicy) -> bool {
        if self.compressing {
            return false;
        }
        let threshold = self
            .compression_threshold
            .unwrap_or(global_compression_threshold);
        let Some(min_reclaim) = min_reclaim_tokens(threshold) else {
            return false;
        };
        if self.tokens() <= threshold {
            return false;
        }
        // Compression re-embeds the leading system message's content in the
        // summary message and keeps the `keep` tail verbatim, so
        // only the messages between them are actually reclaimable. Without this
        // guard, a session whose sticky floor (system prompt + kept tail) sits
        // near the threshold re-compresses after every single turn, burning a
        // summarization LLM call each time for almost no reduction.
        self.reclaimable_tokens(keep) >= min_reclaim
    }

    /// Whether the prompt is close enough to the model's hard input limit
    /// that compression must run regardless of how `compression_threshold`
    /// is configured. Sizes the prompt as the larger of the API-reported
    /// measurement from the last request that reported usage and the local
    /// estimate, because the estimate can run well below the provider's
    /// real count.
    pub fn safety_valve_triggered(
        &self,
        valve: f32,
        keep: KeepPolicy,
        last_prompt_tokens: Option<usize>,
    ) -> bool {
        if self.compressing {
            return false;
        }
        let Some(limit) = valve_limit(valve, self.model().max_input_tokens()) else {
            return false;
        };
        // With nothing to fold away, compressing is a no-op; firing anyway
        // would spin compress attempts while a single in-flight turn holds
        // all the tokens.
        if self.reclaimable_tokens(keep) == 0 {
            return false;
        }
        last_prompt_tokens.unwrap_or(0).max(self.tokens()) >= limit
    }

    /// Index where the tail kept verbatim by compression begins. The tail
    /// starts at a user message so a kept exchange is never cut mid-turn,
    /// and it shrinks by whole exchanges until it fits `keep.tokens`.
    pub fn keep_tail_start(&self, keep: KeepPolicy) -> usize {
        let len = self.messages.len();
        let start = usize::from(
            self.messages
                .first()
                .is_some_and(|v| v.role == MessageRole::System),
        );
        let mut t = self.next_user_at(len.saturating_sub(keep.last).max(start));
        while t < len && self.model().total_tokens(&self.messages[t..]) > keep.tokens {
            t = self.next_user_at(t + 1);
        }
        t
    }

    fn next_user_at(&self, from: usize) -> usize {
        self.messages
            .iter()
            .skip(from)
            .position(|v| v.role == MessageRole::User)
            .map_or(self.messages.len(), |offset| from + offset)
    }

    pub fn foldable_messages(&self, keep: KeepPolicy) -> &[Message] {
        let start = usize::from(
            self.messages
                .first()
                .is_some_and(|v| v.role == MessageRole::System),
        );
        &self.messages[start..self.keep_tail_start(keep)]
    }

    /// The messages compression would keep verbatim.
    pub fn kept_tail(&self, keep: KeepPolicy) -> &[Message] {
        &self.messages[self.keep_tail_start(keep)..]
    }

    /// Estimated tokens of the messages compression would fold away.
    pub fn reclaimable_tokens(&self, keep: KeepPolicy) -> usize {
        self.model().total_tokens(self.foldable_messages(keep))
    }

    pub fn compressing(&self) -> bool {
        self.compressing
    }

    pub fn set_compressing(&mut self, compressing: bool) {
        self.compressing = compressing;
    }

    pub fn compress(&mut self, mut prompt: String, keep: KeepPolicy) {
        // Capture the session's original system prompt the first time we compress.
        // On subsequent compressions `messages[0]` is the summary message built
        // below; re-reading it would re-embed every prior recap+summary, growing
        // the message — and the post-compression token floor — without bound.
        if self.compressed_system_prompt.is_none() {
            let system_prompt = self
                .messages
                .first()
                .filter(|v| v.role == MessageRole::System)
                .map(|v| v.content.to_text())
                .unwrap_or_default();
            self.compressed_system_prompt = Some(system_prompt);
        }
        if let Some(system_prompt) = self
            .compressed_system_prompt
            .as_deref()
            .filter(|v| !v.is_empty())
        {
            prompt = format!("{system_prompt}\n\n{prompt}");
        }
        let messages_to_keep = self.messages.split_off(self.keep_tail_start(keep));
        // Archived turns are at most re-attached as a completed earlier
        // exchange, which never needs their thinking blocks. Sweeping the
        // existing archive as well keeps a session file written before this
        // strip from carrying every signature it ever produced.
        self.compressed_messages.append(&mut self.messages);
        for message in &mut self.compressed_messages {
            message.drop_thinking_blocks();
        }
        self.messages.push(Message::new(
            MessageRole::System,
            MessageContent::Text(prompt),
        ));
        self.messages.extend(messages_to_keep);
        self.dirty = true;
        self.update_tokens();
    }

    pub fn need_autoname(&self) -> bool {
        self.autoname.as_ref().map(|v| v.need()).unwrap_or_default()
    }

    pub fn set_autonaming(&mut self, naming: bool) {
        if let Some(v) = self.autoname.as_mut() {
            v.naming = naming;
        }
    }

    pub fn chat_history_for_autonaming(&self) -> Option<String> {
        self.autoname.as_ref().and_then(|v| v.chat_history.clone())
    }

    pub fn autoname(&self) -> Option<&str> {
        self.autoname.as_ref().and_then(|v| v.name.as_deref())
    }

    pub fn set_autoname(&mut self, value: &str) {
        let name = value
            .chars()
            .map(|v| if v.is_alphanumeric() { v } else { '-' })
            .collect();
        self.autoname = Some(AutoName::new(name));
    }

    pub fn exit(&mut self, session_dir: &Path, is_repl: bool) -> Result<()> {
        let save_session = self.effective_save_session();
        if self.dirty && save_session != Some(false) {
            let mut session_dir = session_dir.to_path_buf();
            let mut session_name = self.name().to_string();
            if save_session.is_none() {
                if !is_repl {
                    return Ok(());
                }
                let ans = Confirm::new("Save session?").with_default(false).prompt()?;
                if !ans {
                    return Ok(());
                }
                if session_name == TEMP_SESSION_NAME {
                    session_name = Text::new("Session name:")
                        .with_validator(|input: &str| {
                            let input = input.trim();
                            if input.is_empty() {
                                Ok(Validation::Invalid("This name is required".into()))
                            } else if input == TEMP_SESSION_NAME {
                                Ok(Validation::Invalid("This name is reserved".into()))
                            } else {
                                Ok(Validation::Valid)
                            }
                        })
                        .prompt()?;
                }
            } else if save_session == Some(true) && session_name == TEMP_SESSION_NAME {
                session_dir = session_dir.join("_");
                ensure_parent_exists(&session_dir).with_context(|| {
                    format!("Failed to create directory '{}'", session_dir.display())
                })?;

                let now = chrono::Local::now();
                session_name = now.format("%Y%m%dT%H%M%S").to_string();
                if let Some(autoname) = self.autoname() {
                    session_name = format!("{session_name}-{autoname}")
                }
            }
            let session_path = session_dir.join(format!("{session_name}.yaml"));
            self.save(&session_name, &session_path, is_repl)?;
        } else if is_repl
            && save_session == Some(true)
            && let Some(path) = &self.path
        {
            // The per-turn flush already wrote the file; confirm it the
            // way a dirty exit does, without writing again.
            print_saved_to(Path::new(path));
        }
        Ok(())
    }

    pub fn save(&mut self, session_name: &str, session_path: &Path, is_repl: bool) -> Result<()> {
        let seq = SAVE_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        self.save_with_temp_seq(session_name, session_path, is_repl, seq)
    }

    /// Number of `save` calls so far in this process; a test takes the delta
    /// across an operation to pin how many times it wrote. The delta is only
    /// meaningful under `#[serial]`: any concurrent `Session::save` in the
    /// test binary bumps it.
    #[cfg(test)]
    pub(crate) fn save_temp_seq() -> u64 {
        SAVE_TEMP_SEQ.load(Ordering::Relaxed)
    }

    fn save_with_temp_seq(
        &mut self,
        session_name: &str,
        session_path: &Path,
        is_repl: bool,
        seq: u64,
    ) -> Result<()> {
        ensure_parent_exists(session_path)?;

        let content = serde_yaml::to_string(&self)
            .with_context(|| format!("Failed to serde session '{}'", self.name))?;
        // Write through a sibling temp file and rename it over the target so a
        // symlink planted at the session path is replaced, never followed. The
        // temp file is opened with create_new so a symlink planted at the temp
        // path is not followed either; a stale entry there is removed and the
        // open retried once.
        let tmp_path = save_temp_path(session_path, seq);
        let write_context = || {
            format!(
                "Failed to write session '{}' to '{}'",
                self.name,
                session_path.display()
            )
        };
        let owner_only = self.owner_only;
        let open_tmp = || {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            if owner_only {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            #[cfg(not(unix))]
            let _ = owner_only;
            options.open(&tmp_path)
        };
        let mut tmp_file = match open_tmp() {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                remove_file(&tmp_path).with_context(write_context)?;
                open_tmp().with_context(write_context)?
            }
            Err(error) => return Err(error).with_context(write_context),
        };
        if let Err(error) = tmp_file
            .write_all(content.as_bytes())
            .and_then(|_| tmp_file.flush())
        {
            drop(tmp_file);
            let _ = remove_file(&tmp_path);
            return Err(error).with_context(write_context);
        }
        drop(tmp_file);
        if let Err(error) = rename(&tmp_path, session_path) {
            let _ = remove_file(&tmp_path);
            return Err(error).with_context(write_context);
        }
        if let Some(uid) = self.uid
            && let Err(error) = child_sessions::note_parent_saved(uid, session_path)
        {
            log::warn!(
                "Failed to record the save of session '{}' for its child sessions: {error:#}",
                self.name
            );
        }

        self.path = Some(session_path.display().to_string());

        if is_repl {
            print_saved_to(session_path);
        }

        if self.name() != session_name {
            self.name = session_name.to_string()
        }

        self.dirty = false;

        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }

        if let Some(path) = self.path.clone() {
            let name = self.name.clone();
            self.save(&name, Path::new(&path), false)?;
        }

        Ok(())
    }

    pub fn guard_empty(&self, prefix: CommandPrefix) -> Result<()> {
        if !self.is_empty() {
            bail!(
                "Cannot perform this operation because the session has messages, please `{}` first.",
                prefix.render_hints(".empty session")
            );
        }
        Ok(())
    }

    pub fn add_message(&mut self, input: &Input, output: &str) -> Result<()> {
        if input.continue_output().is_some() {
            if let Some(message) = self.messages.last_mut()
                && let MessageContent::Text(text) = &mut message.content
            {
                *text = format!("{text}{output}");
            }
        } else if input.regenerate() {
            if let Some(message) = self.messages.last_mut()
                && let MessageContent::Text(text) = &mut message.content
            {
                *text = output.to_string();
            }
        } else {
            if self.messages.is_empty() {
                if self.name == TEMP_SESSION_NAME && self.save_session == Some(true) {
                    let raw_input = input.raw();
                    let chat_history = format!("USER: {raw_input}\nASSISTANT: {output}\n");
                    self.autoname = Some(AutoName::new_from_chat_history(chat_history));
                }
                self.messages.extend(input.role().build_messages(input));
            } else {
                self.messages
                    .push(Message::new(MessageRole::User, input.message_content()));
            }
            self.data_urls.extend(input.data_urls());
            if let Some(tool_calls) = input.tool_calls() {
                self.messages.push(Message::new(
                    MessageRole::Tool,
                    MessageContent::ToolCalls(tool_calls.clone()),
                ))
            }
            self.messages.push(Message::new(
                MessageRole::Assistant,
                MessageContent::Text(output.to_string()),
            ));
        }
        self.dirty = true;
        self.update_tokens();
        Ok(())
    }

    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.compressed_messages.clear();
        self.compressed_system_prompt = None;
        self.data_urls.clear();
        self.autoname = None;
        self.last_run = None;
        self.dirty = true;
        self.update_tokens();
    }

    pub fn pop_last_exchange(&mut self) -> Option<String> {
        let user_idx = self.messages.iter().rposition(|m| m.role.is_user())?;
        let user_text = self.messages[user_idx].content.as_text()?.to_string();
        self.messages.truncate(user_idx);
        self.dirty = true;
        self.update_tokens();
        // Both checks are needed: the prompt alone would also match a later
        // exchange that repeats the run's text, and the length alone would
        // match any undo after compression shrank the history. One false
        // clear remains: the same prompt re-sent after `.compress session`
        // shrank the history below the recorded count, then undone, passes
        // both checks and drops a record its exchange did not produce.
        if self.last_run.as_ref().is_some_and(|run| {
            run.recorded_at_message_count > self.messages.len() && run.prompt == user_text
        }) {
            self.last_run = None;
        }
        Some(user_text)
    }

    pub fn echo_messages(&self, input: &Input) -> String {
        let messages = self.build_messages(input);
        serde_yaml::to_string(&messages).unwrap_or_else(|_| "Unable to echo message".into())
    }

    pub fn build_messages(&self, input: &Input) -> Vec<Message> {
        let mut messages = self.messages.clone();
        if input.continue_output().is_some() {
            return messages;
        }
        let mut need_add_msg = true;
        let len = messages.len();
        if len == 0 {
            messages = input.role().build_messages(input);
            need_add_msg = false;
        } else if len == 1
            && self.compressed_messages.len() >= 2
            && let Some(index) = self
                .compressed_messages
                .iter()
                .rposition(|v| v.role == MessageRole::User)
            && self.reattach_fits(&self.compressed_messages[index..], input.app_config())
        {
            // The re-attached exchange always ends in an Assistant text message
            // (`add_message`, the tool-loop checkpoint and the interrupted-turn
            // close all push Tool then Assistant), so an archived tool turn is
            // never the turn in progress and travels without thinking blocks.
            messages.extend(self.compressed_messages[index..].to_vec());
        }
        if need_add_msg {
            messages.push(Message::new(MessageRole::User, input.message_content()));
        }
        messages
    }

    pub fn keep_tail_token_budget(&self, app: &AppConfig) -> usize {
        keep_token_budget(
            self.compression_threshold
                .unwrap_or(app.compression_threshold),
            app.compression_safety_valve,
            self.model().max_input_tokens(),
        )
    }

    /// Re-attaching the last archived exchange after a full compression is
    /// subject to the same tail budget as compression itself; otherwise the
    /// one giant exchange compression just folded away comes straight back.
    fn reattach_fits(&self, exchange: &[Message], app: &AppConfig) -> bool {
        self.model().total_tokens(exchange) <= self.keep_tail_token_budget(app)
    }
}

impl RoleLike for Session {
    fn to_role(&self) -> Role {
        let role_name = self.role_name.as_deref().unwrap_or_default();
        let mut role = Role::new(role_name, &self.role_prompt);
        role.sync(self);
        role
    }

    fn model(&self) -> &Model {
        &self.model
    }

    fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    fn top_p(&self) -> Option<f64> {
        self.top_p
    }

    fn reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort.clone()
    }

    fn enabled_tools(&self) -> Option<Vec<String>> {
        self.enabled_tools.clone()
    }

    fn enabled_mcp_servers(&self) -> Option<Vec<String>> {
        self.enabled_mcp_servers.clone()
    }

    fn mcp_tools(&self) -> Option<IndexMap<String, Vec<String>>> {
        self.mcp_tools.clone()
    }

    fn set_model(&mut self, model: Model) {
        if self.model().id() != model.id() {
            self.model_id = model.id();
            self.model = model;
            self.dirty = true;
            self.update_tokens();
        }
    }

    fn set_temperature(&mut self, value: Option<f64>) {
        if self.temperature != value {
            self.temperature = value;
            self.dirty = true;
        }
    }

    fn set_top_p(&mut self, value: Option<f64>) {
        if self.top_p != value {
            self.top_p = value;
            self.dirty = true;
        }
    }

    fn set_reasoning_effort(&mut self, value: Option<String>) {
        if self.reasoning_effort != value {
            self.reasoning_effort = value;
            self.dirty = true;
        }
    }

    fn set_enabled_tools(&mut self, value: Option<Vec<String>>) {
        if self.enabled_tools != value {
            self.enabled_tools = value;
            self.dirty = true;
        }
    }

    fn set_enabled_mcp_servers(&mut self, value: Option<Vec<String>>) {
        if self.enabled_mcp_servers != value {
            self.enabled_mcp_servers = value;
            self.dirty = true;
        }
    }

    fn set_mcp_tools(&mut self, value: Option<IndexMap<String, Vec<String>>>) {
        if self.mcp_tools != value {
            self.mcp_tools = value;
            self.dirty = true;
        }
    }
}

#[derive(Debug, Clone, Default)]
struct AutoName {
    naming: bool,
    chat_history: Option<String>,
    name: Option<String>,
}

impl AutoName {
    pub fn new(name: String) -> Self {
        Self {
            name: Some(name),
            ..Default::default()
        }
    }
    pub fn new_from_chat_history(chat_history: String) -> Self {
        Self {
            chat_history: Some(chat_history),
            ..Default::default()
        }
    }
    pub fn need(&self) -> bool {
        !self.naming && self.chat_history.is_some() && self.name.is_none()
    }
}

fn save_temp_path(session_path: &Path, seq: u64) -> PathBuf {
    let mut tmp_name = session_path.as_os_str().to_os_string();
    tmp_name.push(format!(".{}.{seq}.tmp", process::id()));
    PathBuf::from(tmp_name)
}

fn print_saved_to(session_path: &Path) {
    println!("✓ Saved the session to '{}'.", session_path.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{
        Message, MessageContent, MessageContentToolCalls, MessageRole, Model, ModelData,
        ThinkingBlock, TokenUsage,
    };
    use crate::config::{AppConfig, AppState, RequestContext, WorkingMode};
    use crate::function::{Functions, ToolCall, ToolResult};
    use serial_test::serial;
    use std::sync::Arc;

    #[test]
    fn session_default_is_empty() {
        let session = Session::default();

        assert!(session.is_empty());
        assert_eq!(session.name(), "");
        assert_eq!(session.role_name(), None);
        assert!(!session.dirty());
    }

    #[test]
    fn set_role_captures_and_clear_role_drops_role_hooks() {
        let content = "---\nhooks:\n  turn.completed:\n    - name: role-probe\n      command: role-cmd\n---\nPrompt";
        let mut session = Session::default();

        session.set_role(Role::new("hooked", content));
        let hooks = session.role_hooks().expect("set_role must snapshot hooks");
        assert_eq!(hooks["turn.completed"][0].name, "role-probe");

        session.clear_role();
        assert!(session.role_hooks().is_none());

        session.set_role(Role::new("plain", "No frontmatter"));
        assert!(
            session.role_hooks().is_none(),
            "a role without hooks must not leave a stale snapshot"
        );
    }

    fn push_interrupted_turn(session: &mut Session) {
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("do things".to_string()),
        ));
        session.messages.push(Message::new(
            MessageRole::Tool,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![ToolResult::new(ToolCall::default(), json!("ok"))],
                String::new(),
            )),
        ));
        session.messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::Text(INTERRUPTED_RESPONSE_TEXT.to_string()),
        ));
    }

    #[test]
    fn session_has_interrupted_error_checkpoint_detects_sentinel() {
        let mut session = Session::default();
        assert!(!session.has_interrupted_error_checkpoint());

        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("hi".to_string()),
        ));
        session.messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::Text("hello".to_string()),
        ));
        assert!(
            !session.has_interrupted_error_checkpoint(),
            "a normal completed exchange is not an interruption"
        );

        push_interrupted_turn(&mut session);
        assert!(session.has_interrupted_error_checkpoint());
    }

    #[test]
    fn session_detects_checkpoint_appended_by_crashed_continue_turn() {
        // A crash during a `.continue` turn appends the sentinel to the
        // previous assistant text (add_message's continue_output branch)
        // instead of pushing a standalone checkpoint message.
        let mut session = Session::default();
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("hi".to_string()),
        ));
        session.messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::Text(format!("partial answer{INTERRUPTED_RESPONSE_TEXT}")),
        ));

        assert!(session.has_interrupted_error_checkpoint());
    }

    #[test]
    fn checkpointed_turn_is_not_treated_as_interruption() {
        // A mid-loop checkpoint closes its turn with TOOL_LOOP_CHECKPOINT_TEXT;
        // offering to resume it as a crashed turn would double-run the loop.
        let mut session = Session::default();
        push_interrupted_turn(&mut session);
        session.messages.pop();
        session.messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::Text(TOOL_LOOP_CHECKPOINT_TEXT.to_string()),
        ));

        assert!(!session.has_interrupted_error_checkpoint());
    }

    #[test]
    fn ctrl_c_turn_is_not_treated_as_interruption() {
        // A Ctrl-C mid-loop closes its turn with TURN_INTERRUPTED_TEXT; the
        // user chose to stop, so a reload must not offer it as a crashed turn.
        let mut session = Session::default();
        push_interrupted_turn(&mut session);
        session.messages.pop();
        session.messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::Text(TURN_INTERRUPTED_TEXT.to_string()),
        ));

        assert!(!session.has_interrupted_error_checkpoint());
    }

    #[test]
    fn session_interrupted_checkpoint_with_tool_calls_survives_yaml_round_trip() {
        let mut session = Session::default();
        push_interrupted_turn(&mut session);

        let yaml = serde_yaml::to_string(&session).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();

        assert!(
            reloaded.has_interrupted_error_checkpoint(),
            "interruption checkpoint must survive save/reload"
        );
        assert!(
            reloaded.messages.iter().any(|m| matches!(
                &m.content,
                MessageContent::ToolCalls(tc) if tc.tool_results.len() == 1
            )),
            "tool calls made before the crash must survive save/reload"
        );
    }

    #[test]
    fn session_compression_keep_last_survives_yaml_round_trip_and_defaults_to_none() {
        let session: Session = serde_yaml::from_str("model: provider:test\nmessages: []").unwrap();
        assert_eq!(session.compression_keep_last(), None);

        let mut session = Session::default();
        session.set_compression_keep_last(Some(3));
        assert!(session.dirty);

        let yaml = serde_yaml::to_string(&session).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(reloaded.compression_keep_last(), Some(3));
        assert!(
            rendered_lines(&session)
                .iter()
                .any(|line| line.starts_with("compression_keep_last") && line.ends_with('3'))
        );
    }

    #[test]
    fn session_chat_agent_fields_default_empty_and_stay_out_of_yaml() {
        let session: Session = serde_yaml::from_str("model: provider:test\nmessages: []").unwrap();
        assert_eq!(session.chat_agent, None);
        assert!(session.chat_agent_variables.is_empty());
        assert!(session.chat_agent_instructions.is_empty());

        let yaml = serde_yaml::to_string(&Session::default()).unwrap();
        assert!(!yaml.contains("chat_agent"), "{yaml}");
    }

    #[test]
    fn session_chat_agent_fields_survive_yaml_round_trip() {
        let session = Session {
            chat_agent: Some("helper".to_string()),
            chat_agent_variables: AgentVariables::from_iter([(
                "tone".to_string(),
                "terse".to_string(),
            )]),
            chat_agent_instructions: "Be terse.".to_string(),
            ..Session::default()
        };

        let yaml = serde_yaml::to_string(&session).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(reloaded.chat_agent, session.chat_agent);
        assert_eq!(reloaded.chat_agent_variables, session.chat_agent_variables);
        assert_eq!(
            reloaded.chat_agent_instructions,
            session.chat_agent_instructions
        );
    }

    #[test]
    fn sync_chat_agent_writes_only_the_role_and_delegate_fields() {
        let mut session = Session {
            model_id: "provider:distinct".to_string(),
            temperature: Some(0.3),
            top_p: Some(0.7),
            reasoning_effort: Some("high".to_string()),
            enabled_tools: Some(vec!["fs_read".to_string()]),
            enabled_mcp_servers: Some(vec!["srv".to_string()]),
            agent_variables: AgentVariables::from_iter([("lang".to_string(), "rust".to_string())]),
            agent_instructions: "graph agent instructions".to_string(),
            role_name: Some("role".to_string()),
            role_prompt: "role prompt".to_string(),
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("hello".to_string()),
            )],
            ..Session::default()
        };
        let before = session.clone();

        let mut agent = Agent::test_new(AgentConfig {
            name: "helper".to_string(),
            instructions: "Speak {{tone}}.".to_string(),
            compression_threshold: Some(4321),
            compression_keep_last: Some(7),
            ..AgentConfig::default()
        });
        agent.set_shared_variables(AgentVariables::from_iter([(
            "tone".to_string(),
            "tersely".to_string(),
        )]));

        session.sync_chat_agent(&agent);

        assert!(session.dirty);
        assert!(
            session.role_prompt.starts_with("Speak tersely."),
            "{}",
            session.role_prompt
        );
        assert_eq!(session.role_prompt, agent.interpolated_instructions());
        assert_eq!(session.role_name, None);
        assert!(session.role_hooks.is_none());
        assert_eq!(session.chat_agent.as_deref(), Some("helper"));
        assert_eq!(session.chat_agent_variables, *agent.variables());
        assert_eq!(session.chat_agent_instructions, session.role_prompt);
        assert_eq!(session.compression_threshold(), Some(4321));
        assert_eq!(session.compression_keep_last(), Some(7));

        assert_eq!(session.model_id, before.model_id);
        assert_eq!(session.temperature, before.temperature);
        assert_eq!(session.top_p, before.top_p);
        assert_eq!(session.reasoning_effort, before.reasoning_effort);
        assert_eq!(session.enabled_tools, before.enabled_tools);
        assert_eq!(session.enabled_mcp_servers, before.enabled_mcp_servers);
        assert_eq!(session.agent_variables, before.agent_variables);
        assert_eq!(session.agent_instructions, before.agent_instructions);
        assert_eq!(
            serde_json::to_value(&session.messages).unwrap(),
            serde_json::to_value(&before.messages).unwrap()
        );
    }

    #[test]
    fn sync_chat_agent_marks_dirty_only_through_compression_settings_like_sync_agent() {
        let agent = Agent::test_new(AgentConfig {
            name: "helper".to_string(),
            ..AgentConfig::default()
        });
        let mut session = Session::default();

        session.sync_chat_agent(&agent);

        assert!(!session.dirty);
    }

    #[test]
    fn sync_agent_clears_the_chat_agent_fields_and_sync_chat_agent_sets_them_again() {
        let graph_agent = Agent::test_new(AgentConfig {
            name: "graph-agent".to_string(),
            instructions: "graph".to_string(),
            ..AgentConfig::default()
        });
        let mut helper = Agent::test_new(AgentConfig {
            name: "helper".to_string(),
            instructions: "Speak {{tone}}.".to_string(),
            ..AgentConfig::default()
        });
        helper.set_shared_variables(AgentVariables::from_iter([(
            "tone".to_string(),
            "tersely".to_string(),
        )]));
        let mut session = Session::default();

        session.sync_chat_agent(&helper);
        session.sync_agent(&graph_agent);

        assert_eq!(session.chat_agent(), None);
        assert!(session.chat_agent_variables().is_empty());
        assert!(session.chat_agent_instructions().is_empty());
        assert_eq!(session.role_prompt, graph_agent.interpolated_instructions());

        session.sync_chat_agent(&helper);

        assert_eq!(session.chat_agent(), Some("helper"));
        assert_eq!(*session.chat_agent_variables(), *helper.variables());
        assert_eq!(
            session.chat_agent_instructions(),
            helper.interpolated_instructions()
        );
        assert_eq!(session.role_prompt, helper.interpolated_instructions());
        assert_eq!(*session.agent_variables(), *graph_agent.variables());
        assert_eq!(
            session.agent_instructions(),
            graph_agent.interpolated_instructions()
        );
    }

    #[test]
    fn session_enabled_macros_absent_is_none() {
        let session: Session = serde_yaml::from_str("model: provider:test\nmessages: []").unwrap();

        assert_eq!(session.enabled_macros, None);
    }

    #[test]
    fn session_token_usage_accumulates_and_survives_yaml_round_trip() {
        let mut session = Session::default();
        assert!(session.token_usage().is_empty());

        session.accumulate_token_usage(&TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(10),
            cache_creation_input_tokens: Some(50),
            cache_read_input_tokens: None,
        });
        session.accumulate_token_usage(&TokenUsage {
            input_tokens: Some(1),
            output_tokens: None,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(90),
        });
        assert!(session.dirty());
        assert_eq!(session.token_usage().input_tokens, Some(101));
        assert_eq!(session.token_usage().output_tokens, Some(10));
        assert_eq!(session.token_usage().cache_creation_input_tokens, Some(50));
        assert_eq!(session.token_usage().cache_read_input_tokens, Some(90));

        let yaml = serde_yaml::to_string(&session).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(reloaded.token_usage().input_tokens, Some(101));
        assert_eq!(reloaded.token_usage().cache_read_input_tokens, Some(90));
    }

    #[test]
    fn session_empty_token_usage_is_not_serialized() {
        let session = Session::default();
        let yaml = serde_yaml::to_string(&session).unwrap();
        assert!(!yaml.contains("token_usage"));
    }

    #[test]
    fn session_role_hooks_is_not_serialized() {
        let content = "---\nhooks:\n  turn.completed:\n    - name: role-probe\n      command: role-cmd\n---\nPrompt";
        let mut session = Session::default();
        session.set_role(Role::new("hooked", content));
        assert!(session.role_hooks().is_some());

        let yaml = serde_yaml::to_string(&session).unwrap();
        assert!(
            !yaml.contains("role_hooks"),
            "the role-hooks snapshot must never reach session YAML"
        );
        assert!(
            !yaml.contains("role-cmd"),
            "no hook command text may leak into session YAML under any key"
        );
    }

    #[test]
    fn session_scope_is_not_serialized() {
        let mut session = Session::default();
        session.set_scope(SessionScope::Workspace);

        let yaml = serde_yaml::to_string(&session).unwrap();

        assert!(!yaml.contains("scope"));
    }

    #[test]
    fn session_deserialization_ignores_injected_role_hooks() {
        // A session file is attacker-writable data: a `role_hooks:` key
        // crafted into it must never deserialize into executable hook
        // config. Hooks are only ever re-read from the role file itself.
        let yaml = "model: ''\nmessages: []\nrole_hooks:\n  session.started:\n    - name: injected\n      command: touch /tmp/pwned\n";
        let session: Session = serde_yaml::from_str(yaml).unwrap();
        assert!(
            session.role_hooks().is_none(),
            "injected role_hooks in session YAML must be ignored"
        );
    }

    #[test]
    fn session_cost_accumulates_and_survives_yaml_round_trip() {
        let mut session = Session::default();
        assert_eq!(session.cost(), 0.0);

        session.accumulate_cost(0.0025);
        session.accumulate_cost(0.001);
        assert!((session.cost() - 0.0035).abs() < 1e-12);
        assert!(session.dirty());

        let yaml = serde_yaml::to_string(&session).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();
        assert!((reloaded.cost() - 0.0035).abs() < 1e-12);
    }

    #[test]
    fn session_zero_cost_is_not_serialized() {
        let session = Session::default();
        let yaml = serde_yaml::to_string(&session).unwrap();
        assert!(!yaml.contains("cost"));
    }

    fn sample_last_run(prompt: &str) -> LastRun {
        LastRun {
            version: 1,
            completed_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            prompt: prompt.to_string(),
            output: "run output".to_string(),
            exposed: IndexMap::from([("n".to_string(), json!(2))]),
            trace: vec![crate::graph::TraceStep {
                node_id: "done".to_string(),
                node_type: "end".to_string(),
                visit: 1,
                outcome: crate::graph::TraceOutcome::End,
                elapsed_ms: 3,
                fell_back: false,
            }],
            recorded_at_message_count: 0,
            carried: IndexMap::new(),
        }
    }

    #[test]
    fn session_last_run_survives_yaml_round_trip_and_defaults_to_none() {
        let session: Session = serde_yaml::from_str("model: provider:test\nmessages: []").unwrap();
        assert_eq!(session.last_run(), None);

        let mut session = Session::default();
        session.messages.push(text(MessageRole::User, "run"));
        session
            .messages
            .push(text(MessageRole::Assistant, "run output"));
        session.set_last_run(Some(sample_last_run("run")));
        assert!(session.dirty);
        assert_eq!(session.last_run().unwrap().recorded_at_message_count, 2);

        let yaml = serde_yaml::to_string(&session).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(reloaded.last_run(), session.last_run());
    }

    #[test]
    fn session_last_run_trace_step_without_fell_back_loads_as_false() {
        let yaml = "\
model: provider:test
messages: []
last_run:
  version: 1
  completed_at: 1970-01-01T00:00:00Z
  prompt: run
  output: recovered
  exposed: {}
  trace:
  - node_id: crash
    node_type: script
    visit: 1
    outcome: !routed
      to: recover
    elapsed_ms: 3
  - node_id: recover
    node_type: end
    visit: 1
    outcome: end
    elapsed_ms: 0
    fell_back: true
";
        let session: Session = serde_yaml::from_str(yaml).unwrap();

        let trace = &session.last_run().unwrap().trace;
        assert_eq!(trace.len(), 2);
        assert!(!trace[0].fell_back, "a step without the key loads as false");
        assert!(trace[1].fell_back, "a step that carries the key keeps it");

        let yaml = serde_yaml::to_string(&session).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(reloaded.last_run(), session.last_run());
        assert_eq!(yaml.matches("fell_back: false").count(), 1, "{yaml}");
    }

    #[test]
    fn session_absent_last_run_is_not_serialized() {
        let session = Session::default();
        let yaml = serde_yaml::to_string(&session).unwrap();
        assert!(!yaml.contains("last_run"));

        let mut session = Session::default();
        session.set_last_run(Some(sample_last_run("run")));
        session.set_last_run(None);
        let yaml = serde_yaml::to_string(&session).unwrap();
        assert!(!yaml.contains("last_run"));
    }

    #[test]
    fn pop_last_exchange_clears_the_last_run_it_produced() {
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let mut input = Input::from_str(&ctx, "run", None).unwrap();
        input.set_patched_text("patched: run");
        let prompt = input.bare_text().to_string();
        assert_ne!(input.text(), prompt);

        let mut session = Session::default();
        input.clear_patch();
        session.add_message(&input, "run output").unwrap();
        let stored = session
            .messages
            .iter()
            .find(|m| m.role.is_user())
            .expect("add_message stores the user message");
        assert_eq!(stored.content.to_text(), "run");
        session.set_last_run(Some(sample_last_run(&prompt)));

        assert_eq!(session.pop_last_exchange().as_deref(), Some("run"));
        assert_eq!(session.last_run(), None);
        assert!(!session.has_user_messages());
    }

    #[test]
    fn clear_messages_drops_the_last_run() {
        let mut session = Session::default();
        session.messages.push(text(MessageRole::User, "run"));
        session
            .messages
            .push(text(MessageRole::Assistant, "run output"));
        session.set_last_run(Some(sample_last_run("run")));

        session.clear_messages();

        assert!(session.last_run().is_none());
        assert!(session.messages.is_empty());
    }

    #[test]
    fn pop_last_exchange_keeps_the_last_run_when_undoing_a_later_exchange() {
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let mut session = Session::default();
        let run = Input::from_str(&ctx, "run", None).unwrap();
        session.add_message(&run, "run output").unwrap();
        session.set_last_run(Some(sample_last_run("run")));
        let chat = Input::from_str(&ctx, "chat", None).unwrap();
        session.add_message(&chat, "reply").unwrap();

        assert_eq!(session.pop_last_exchange().as_deref(), Some("chat"));
        assert!(session.last_run().is_some());

        assert_eq!(session.pop_last_exchange().as_deref(), Some("run"));
        assert_eq!(session.last_run(), None);
    }

    #[test]
    fn pop_last_exchange_keeps_the_last_run_when_a_later_exchange_repeats_its_text() {
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let mut session = Session::default();
        let run = Input::from_str(&ctx, "run", None).unwrap();
        session.add_message(&run, "run output").unwrap();
        session.set_last_run(Some(sample_last_run("run")));
        let repeat = Input::from_str(&ctx, "run", None).unwrap();
        session.add_message(&repeat, "reply 2").unwrap();

        // Same text as the run, but the exchange was added after the run was
        // recorded, so the length condition must keep the run.
        assert_eq!(session.pop_last_exchange().as_deref(), Some("run"));
        assert!(session.last_run().is_some());

        assert_eq!(session.pop_last_exchange().as_deref(), Some("run"));
        assert_eq!(session.last_run(), None);
    }

    #[test]
    fn pop_last_exchange_keeps_the_last_run_after_compression_shrank_the_history() {
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let mut session = Session::default();
        session.messages.push(text(MessageRole::User, "earlier"));
        session
            .messages
            .push(text(MessageRole::Assistant, "before"));
        let run = Input::from_str(&ctx, "run", None).unwrap();
        session.add_message(&run, "run output").unwrap();
        session.set_last_run(Some(sample_last_run("run")));
        assert_eq!(session.last_run().unwrap().recorded_at_message_count, 4);

        session.compress("recap".to_string(), keep(0));
        assert_eq!(session.messages.len(), 1);
        let chat = Input::from_str(&ctx, "chat", None).unwrap();
        session.add_message(&chat, "reply").unwrap();

        assert_eq!(session.pop_last_exchange().as_deref(), Some("chat"));
        assert!(
            session.messages.len() < 4,
            "the length condition alone must not clear the run"
        );
        assert!(session.last_run().is_some());
    }

    #[test]
    fn session_export_includes_real_usage_only_when_reported() {
        let empty = Session::default().export(None).unwrap();
        assert!(!empty.contains("real_input_tokens"));
        assert!(empty.contains("scope: global"));

        let mut session = Session::default();
        session.set_scope(SessionScope::Workspace);
        session.accumulate_token_usage(&TokenUsage {
            input_tokens: Some(5),
            output_tokens: Some(7),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(3),
        });
        let exported = session.export(None).unwrap();
        assert!(exported.contains("scope: workspace"));
        assert!(exported.contains("real_input_tokens: 5"));
        assert!(exported.contains("real_output_tokens: 7"));
        assert!(exported.contains("cache_read_tokens: 3"));
        assert!(!exported.contains("cache_creation_tokens"));
    }

    fn sample_compression_info() -> CompressionInfo {
        CompressionInfo {
            threshold: 100_000,
            keep_last: 4,
            safety_valve: 0.8,
            model: None,
            valve_limit: Some(160_000),
            keep_tokens: 40_000,
            checkpoint_tokens: 50_000,
            min_reclaim: Some(20_000),
            last_prompt_tokens: Some(90_000),
            reclaimable_tokens: Some(30_000),
            kept_tail_messages: Some(3),
            kept_tail_tokens: Some(5_000),
            archived_messages: Some(12),
        }
    }

    #[test]
    fn session_export_includes_compression_block() {
        let exported = Session::default()
            .export(Some(&sample_compression_info()))
            .unwrap();
        assert!(exported.contains("compression:\n"));
        assert!(exported.contains("  threshold: 100000\n"));
        assert!(exported.contains("  keep_last: 4\n"));
        assert!(exported.contains("  safety_valve: 0.8\n"));
        assert!(exported.contains("  model: null\n"));
        assert!(exported.contains("  valve_limit: 160000\n"));
        assert!(exported.contains("  keep_tokens: 40000\n"));
        assert!(exported.contains("  checkpoint_tokens: 50000\n"));
        assert!(exported.contains("  min_reclaim: 20000\n"));
        assert!(exported.contains("  last_prompt_tokens: 90000\n"));
        assert!(exported.contains("  reclaimable: 30000\n"));
        assert!(exported.contains("  kept_tail_messages: 3\n"));
        assert!(exported.contains("  kept_tail_tokens: 5000\n"));
        assert!(exported.contains("  archived_messages: 12\n"));

        assert!(
            !Session::default()
                .export(None)
                .unwrap()
                .contains("compression")
        );
    }

    #[test]
    fn compression_info_formats_null_and_unlimited() {
        let info = CompressionInfo {
            threshold: 0,
            keep_last: 0,
            safety_valve: 0.0,
            model: None,
            valve_limit: valve_limit(0.0, None),
            keep_tokens: keep_token_budget(0, 0.0, None),
            checkpoint_tokens: 50_000,
            min_reclaim: min_reclaim_tokens(0),
            last_prompt_tokens: None,
            reclaimable_tokens: None,
            kept_tail_messages: None,
            kept_tail_tokens: None,
            archived_messages: None,
        };

        let rows = info.rows();
        let row = |key: &str| {
            rows.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.as_str())
                .unwrap_or_else(|| panic!("{key} row missing from {rows:?}"))
        };
        assert_eq!(row("compression_threshold"), "0");
        assert_eq!(row("compression_safety_valve"), "0");
        assert_eq!(row("compression_model"), "null");
        assert_eq!(row("compression_valve_limit"), "null");
        assert_eq!(row("compression_keep_tokens"), "unlimited");
        assert_eq!(row("compression_min_reclaim"), "null");
        assert_eq!(row("compression_last_prompt_tokens"), "null");
        assert_eq!(rows.len(), 9, "live rows are absent without a session");

        let json = info.to_json();
        assert_eq!(json["model"], serde_json::Value::Null);
        assert_eq!(json["valve_limit"], serde_json::Value::Null);
        assert_eq!(json["keep_tokens"], json!("unlimited"));
        assert_eq!(json["min_reclaim"], serde_json::Value::Null);
        assert_eq!(json["last_prompt_tokens"], serde_json::Value::Null);
        assert!(json.get("reclaimable").is_none());
        assert!(json.get("archived_messages").is_none());
    }

    #[test]
    fn compression_info_rows_keep_their_order() {
        let keys: Vec<&str> = sample_compression_info()
            .rows()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(
            keys,
            [
                "compression_threshold",
                "compression_keep_last",
                "compression_safety_valve",
                "compression_model",
                "compression_valve_limit",
                "compression_keep_tokens",
                "compression_checkpoint_tokens",
                "compression_min_reclaim",
                "compression_last_prompt_tokens",
                "compression_reclaimable",
                "compression_kept_tail_messages",
                "compression_kept_tail_tokens",
                "compression_archived_messages",
            ]
        );
    }

    fn rendered_lines_with(
        session: &Session,
        compression: Option<&CompressionInfo>,
    ) -> Vec<String> {
        let options = crate::render::RenderOptions {
            raw_markdown: true,
            ..Default::default()
        };
        let mut render = MarkdownRender::init(options).unwrap();
        session
            .render(&mut render, &None, compression)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn rendered_lines(session: &Session) -> Vec<String> {
        rendered_lines_with(session, None)
    }

    #[test]
    fn session_render_reports_estimated_tokens_like_export() {
        let mut session = Session::default();
        let mut data = ModelData::new("windowed");
        data.max_input_tokens = Some(1_000);
        session.set_model(Model::from_config("provider", &[data]).remove(0));
        session.messages.push(text(MessageRole::System, "system"));
        session
            .messages
            .push(text(MessageRole::User, "hello there"));
        session.update_tokens();
        let (tokens, percent) = session.tokens_usage();
        assert!(tokens > 0 && percent > 0.0);

        let lines = rendered_lines(&session);
        let row = |key: &str| {
            lines
                .iter()
                .find(|line| line.starts_with(key))
                .unwrap_or_else(|| panic!("{key} row missing from:\n{}", lines.join("\n")))
                .clone()
        };
        assert!(row("total_tokens").ends_with(&tokens.to_string()));
        assert!(row("max_input_tokens").ends_with("1000"));
        assert!(row("total/max").ends_with(&format!("{percent}%")));

        let exported = session.export(None).unwrap();
        assert!(exported.contains(&format!("total_tokens: {tokens}\n")));
        assert!(exported.contains(&format!("total/max: {percent}%\n")));
    }

    #[test]
    fn session_render_uses_compression_rows_when_given() {
        let mut session = Session::default();
        session.set_compression_threshold(Some(123));

        let plain = rendered_lines(&session);
        assert!(
            plain
                .iter()
                .any(|line| line.starts_with("compression_threshold") && line.ends_with("123"))
        );
        assert!(
            !plain
                .iter()
                .any(|line| line.starts_with("compression_keep_tokens"))
        );

        let full = rendered_lines_with(&session, Some(&sample_compression_info()));
        assert!(
            full.iter().any(|line| {
                line.starts_with("compression_threshold") && line.ends_with("100000")
            }),
            "the info block replaces the session override row"
        );
        assert!(
            full.iter()
                .any(|line| line.starts_with("compression_keep_tokens") && line.ends_with("40000"))
        );
        assert!(
            full.iter()
                .any(|line| line.starts_with("compression_archived_messages")
                    && line.ends_with("12"))
        );
        assert_eq!(
            full.iter()
                .filter(|line| line.starts_with("compression_"))
                .count(),
            13
        );
    }

    #[test]
    fn session_render_reports_scope_line() {
        let mut session = Session::default();
        assert!(
            rendered_lines(&session)
                .iter()
                .any(|line| line.starts_with("scope") && line.ends_with("global"))
        );

        session.set_scope(SessionScope::Workspace);
        assert!(
            rendered_lines(&session)
                .iter()
                .any(|line| line.starts_with("scope") && line.ends_with("workspace"))
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn session_save_replaces_a_symlink_instead_of_following_it() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("coyote-session-save-symlink-{unique}"));
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let target = dir.join("target.txt");
        std::fs::write(&target, "untouched").unwrap();
        let link = sessions_dir.join("link.yaml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut session = Session::default();
        session.save("link", &link, false).unwrap();

        assert_eq!(read_to_string(&target).unwrap(), "untouched");
        assert!(link.symlink_metadata().unwrap().is_file());
        assert!(read_to_string(&link).unwrap().contains("model:"));
        assert_eq!(session.name(), "link");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn session_save_does_not_follow_a_symlink_planted_at_the_temp_path() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("coyote-session-save-tmp-symlink-{unique}"));
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let target = dir.join("target.txt");
        std::fs::write(&target, "untouched").unwrap();
        let destination = sessions_dir.join("notes.yaml");
        let seq = SAVE_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_path = save_temp_path(&destination, seq);
        std::os::unix::fs::symlink(&target, &tmp_path).unwrap();

        let mut session = Session::default();
        session
            .save_with_temp_seq("notes", &destination, false, seq)
            .unwrap();

        assert_eq!(read_to_string(&target).unwrap(), "untouched");
        assert!(destination.symlink_metadata().unwrap().is_file());
        assert!(read_to_string(&destination).unwrap().contains("model:"));
        assert!(!tmp_path.exists(), "temp file must not be left behind");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[serial]
    fn session_save_temp_paths_differ_between_writes_in_one_process() {
        let destination = Path::new("/sessions/notes.yaml");
        let first = save_temp_path(destination, SAVE_TEMP_SEQ.fetch_add(1, Ordering::Relaxed));
        let second = save_temp_path(destination, SAVE_TEMP_SEQ.fetch_add(1, Ordering::Relaxed));

        assert_ne!(first, second);
        for path in [&first, &second] {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(
                name.starts_with(&format!("notes.yaml.{}.", process::id()))
                    && name.ends_with(".tmp"),
                "{name}"
            );
        }
    }

    #[test]
    fn session_uid_is_absent_until_set_and_round_trips_through_yaml() {
        let session: Session = serde_yaml::from_str("model: ''\nmessages: []\n").unwrap();
        assert_eq!(session.uid(), None);
        assert!(
            !serde_yaml::to_string(&Session::default())
                .unwrap()
                .contains("uid:")
        );

        let uid = Uuid::new_v4();
        let mut session = Session::default();
        assert!(!session.dirty());
        session.set_uid(Some(uid));
        assert!(session.dirty());

        let yaml = serde_yaml::to_string(&session).unwrap();
        assert!(yaml.contains("uid:"), "{yaml}");
        let mut reloaded: Session = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(reloaded.uid(), Some(uid));

        assert!(!reloaded.dirty());
        reloaded.set_uid(Some(uid));
        assert!(!reloaded.dirty(), "setting the same uid must not dirty");
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn session_save_owner_only_writes_a_0600_file_and_a_default_save_keeps_the_open_mode() {
        use std::os::unix::fs::PermissionsExt;

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("coyote-session-save-mode-{unique}"));
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let probe = sessions_dir.join("probe.txt");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .unwrap();
        let open_mode = probe.metadata().unwrap().permissions().mode() & 0o777;

        let shared = sessions_dir.join("shared.yaml");
        Session::default().save("shared", &shared, false).unwrap();
        assert_eq!(
            shared.metadata().unwrap().permissions().mode() & 0o777,
            open_mode
        );

        let private = sessions_dir.join("private.yaml");
        let mut session = Session::default();
        session.set_owner_only(true);
        session.save("private", &private, false).unwrap();
        assert_eq!(
            private.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[serial]
    fn session_set_path_names_the_file_flush_writes() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("coyote-session-set-path-{unique}"));
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let destination = sessions_dir.join("notes.yaml");

        let mut session = Session::default();
        session.set_path(destination.clone());
        session.flush().unwrap();
        assert!(
            !destination.exists(),
            "flush on a clean session must write nothing"
        );

        session.set_uid(Some(Uuid::new_v4()));
        assert!(session.dirty());
        session.flush().unwrap();
        assert!(read_to_string(&destination).unwrap().contains("model:"));
        assert!(!session.dirty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[serial]
    fn exit_on_a_clean_autosaving_named_session_writes_nothing() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("coyote-session-clean-exit-{unique}"));
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let destination = sessions_dir.join("notes.yaml");
        let mut session = Session::default();
        session.set_save_session(Some(true));
        session.save("notes", &destination, false).unwrap();
        assert!(!session.dirty());
        let before = read_to_string(&destination).unwrap();
        let modified_before = destination.metadata().unwrap().modified().unwrap();

        session.exit(&sessions_dir, true).unwrap();

        assert_eq!(read_to_string(&destination).unwrap(), before);
        assert_eq!(
            destination.metadata().unwrap().modified().unwrap(),
            modified_before
        );
        assert_eq!(session.path.as_deref(), destination.to_str());
        let mut entries: Vec<_> = std::fs::read_dir(&sessions_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(entries, vec![std::ffi::OsString::from("notes.yaml")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[serial]
    fn save_records_the_parent_path_and_time_in_the_child_session_root() {
        use crate::config::child_sessions::{ChildSessionRoot, RootFile};
        use crate::utils::process::ProcessStamp;
        use chrono::TimeZone;

        let cache = crate::testing::CacheDirGuard::new("session-save-note-parent");
        let sessions_dir = cache.root.join("sessions-dir");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        let seeded_at = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        root.ensure_dir(&RootFile::seed_at(seeded_at, ProcessStamp::current(), None))
            .unwrap();
        let a_path = sessions_dir.join("a.yaml");
        let other_path = sessions_dir.join("other.yaml");
        let mut session = Session::default();
        session.set_uid(Some(uid));

        let before_save = chrono::Utc::now();
        session.save("a", &a_path, false).unwrap();
        let record = RootFile::load(&root.root_file()).unwrap();
        assert_eq!(
            record.parent_paths,
            vec![std::fs::canonicalize(&a_path).unwrap()]
        );
        let first_saved_at = record.last_saved_at.unwrap();
        assert!(first_saved_at >= before_save);
        assert_eq!(record.created_at, seeded_at);

        session.mark_dirty();
        session.save("a", &a_path, false).unwrap();
        let record = RootFile::load(&root.root_file()).unwrap();
        assert_eq!(
            record.parent_paths,
            vec![std::fs::canonicalize(&a_path).unwrap()]
        );
        assert!(record.last_saved_at.unwrap() >= first_saved_at);

        session.save("other", &other_path, false).unwrap();
        let record = RootFile::load(&root.root_file()).unwrap();
        assert_eq!(
            record.parent_paths,
            vec![
                std::fs::canonicalize(&a_path).unwrap(),
                std::fs::canonicalize(&other_path).unwrap()
            ]
        );
        assert!(!session.dirty());
    }

    #[test]
    #[serial]
    fn save_without_a_child_session_root_dir_creates_nothing_under_the_cache() {
        let cache = crate::testing::CacheDirGuard::new("session-save-no-root");
        let sessions_dir = cache.root.join("sessions-dir");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("a.yaml");
        let mut session = Session::default();
        session.set_uid(Some(Uuid::new_v4()));

        session.save("a", &path, false).unwrap();

        assert!(path.is_file());
        let mut entries: Vec<_> = std::fs::read_dir(&cache.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(entries, vec![std::ffi::OsString::from("sessions-dir")]);

        let mut unidentified = Session::default();
        assert_eq!(unidentified.uid(), None);
        unidentified
            .save("b", &sessions_dir.join("b.yaml"), false)
            .unwrap();
        let mut entries: Vec<_> = std::fs::read_dir(&cache.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(entries, vec![std::ffi::OsString::from("sessions-dir")]);
    }

    #[test]
    fn usage_probe_effective_save_session_only_lifts_to_true() {
        for setting in [None, Some(false), Some(true)] {
            let mut session = Session::default();
            session.set_save_session(setting);
            assert_eq!(
                session.effective_save_session(),
                setting,
                "without the one-shot flag the setting passes through"
            );
            session.set_save_session_this_time();
            assert_eq!(
                session.effective_save_session(),
                Some(true),
                "the one-shot flag wins over {setting:?}"
            );
            assert_eq!(
                session.save_session(),
                setting,
                "the plain accessor is left alone"
            );
        }
    }

    /// Two in-process writers sharing one session path (they share a pid)
    /// saving the same file at once: every write lands, nothing is lost
    /// to a sibling removing an in-flight temp file, and no temp file is
    /// left behind. Under a pid-only temp name the second writer's
    /// `AlreadyExists` retry removes the first writer's temp file and the
    /// first rename then fails.
    #[test]
    #[serial]
    fn usage_probe_concurrent_in_process_writers_to_one_path_all_succeed() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("coyote-session-concurrent-{unique}"));
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let destination = sessions_dir.join("shared.yaml");
        const ROUNDS: usize = 40;
        const WRITERS: usize = 4;
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let input = Input::from_str(&ctx, "hello", None).unwrap();

        let barrier = std::sync::Barrier::new(WRITERS);
        let failures: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let barrier = &barrier;
                    let destination = destination.clone();
                    let input = input.clone();
                    scope.spawn(move || {
                        let mut session = Session::default();
                        session.set_save_session(Some(true));
                        let mut failures = Vec::new();
                        for round in 0..ROUNDS {
                            session.add_message(&input, "ok").unwrap();
                            barrier.wait();
                            if let Err(err) = session.save("shared", &destination, false) {
                                failures.push(format!("writer {writer} round {round}: {err:#}"));
                            }
                        }
                        failures
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });

        assert!(
            failures.is_empty(),
            "every concurrent save must succeed: {failures:#?}"
        );
        let on_disk: Session = serde_yaml::from_str(&read_to_string(&destination).unwrap())
            .expect("the file is whole: no torn write");
        assert!(!on_disk.messages().is_empty());
        let mut entries: Vec<_> = std::fs::read_dir(&sessions_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("shared.yaml")],
            "no temp file survives the concurrent writes"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn session_todo_state_survives_yaml_round_trip() {
        let mut session = Session::default();
        let mut todo_list = TodoList::new("Ship feature");
        todo_list.add("Write code");
        todo_list.add("Write tests");
        todo_list.mark_done(1);
        session.sync_todo_state(&todo_list, Some("Which database should tests target?"));
        assert!(session.dirty());

        let yaml = serde_yaml::to_string(&session).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(reloaded.todo_list, todo_list);
        assert_eq!(
            reloaded.auto_continue_paused.as_deref(),
            Some("Which database should tests target?")
        );
    }

    #[test]
    fn session_goal_only_todo_list_survives_yaml_round_trip() {
        // `todo__init` sets a goal before any items exist; the serde skip
        // predicate must not treat that as an empty list.
        let mut session = Session::default();
        session.sync_todo_state(&TodoList::new("Ship feature"), None);
        assert!(session.dirty());

        let yaml = serde_yaml::to_string(&session).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(reloaded.todo_list.goal, "Ship feature");
        assert!(reloaded.todo_list.todos.is_empty());
    }

    #[test]
    fn session_without_todo_fields_loads_defaults() {
        let session: Session = serde_yaml::from_str("model: provider:test\nmessages: []").unwrap();

        assert!(session.todo_list.is_empty());
        assert_eq!(session.auto_continue_paused, None);

        let yaml = serde_yaml::to_string(&session).unwrap();
        assert!(!yaml.contains("todo_list"));
        assert!(!yaml.contains("auto_continue_paused"));
    }

    #[test]
    fn session_mcp_tools_survives_yaml_round_trip() {
        let mut session = Session::default();
        let mut mcp_tools = IndexMap::new();
        mcp_tools.insert("github".to_string(), vec!["get_*".to_string()]);
        mcp_tools.insert("slack".to_string(), vec![]);
        session.set_mcp_tools(Some(mcp_tools.clone()));

        let yaml = serde_yaml::to_string(&session).unwrap();
        let reloaded: Session = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(reloaded.mcp_tools(), Some(mcp_tools));
    }

    #[test]
    fn session_set_role_does_not_copy_mcp_tools() {
        let role = Role::new(
            "test",
            "---\nmcp_tools:\n  github: [get_issue]\n---\nPrompt",
        );
        assert!(role.mcp_tools().is_some());
        let mut session = Session::default();

        session.set_role(role);

        assert_eq!(session.mcp_tools(), None);
    }

    #[test]
    fn session_set_mcp_tools_marks_dirty() {
        let mut session = Session::default();
        assert!(!session.dirty());

        session.set_mcp_tools(Some(IndexMap::new()));

        assert!(session.dirty());
        assert_eq!(session.mcp_tools(), Some(IndexMap::new()));
    }

    #[test]
    fn session_enabled_macros_empty_list_is_some_empty() {
        let session: Session =
            serde_yaml::from_str("model: provider:test\nenabled_macros: []\nmessages: []").unwrap();

        assert_eq!(session.enabled_macros, Some(vec![]));
    }

    #[test]
    fn session_enabled_macros_empty_string_is_some_empty() {
        let session: Session =
            serde_yaml::from_str("model: provider:test\nenabled_macros: \"\"\nmessages: []")
                .unwrap();

        assert_eq!(session.enabled_macros, Some(vec![]));
    }

    #[test]
    fn session_enabled_macros_csv_string() {
        let session: Session =
            serde_yaml::from_str("model: provider:test\nenabled_macros: \"a,b\"\nmessages: []")
                .unwrap();

        assert_eq!(
            session.enabled_macros,
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn session_serialize_omits_enabled_macros_when_none() {
        let session = Session::default();
        let yaml = serde_yaml::to_string(&session).unwrap();

        assert!(!yaml.contains("enabled_macros"));
    }

    #[test]
    fn session_new_from_ctx_captures_save_session() {
        let app_config = Arc::new(AppConfig::default());
        let app_state = Arc::new(AppState {
            config: app_config.clone(),
            vault: Arc::new(Vault::default()),
            mcp_factory: Arc::new(mcp_factory::McpFactory::default()),
            rag_cache: Arc::new(rag_cache::RagCache::default()),
            mcp_config: None,
            mcp_log_path: None,
            mcp_registry: None,
            functions: Functions::default(),
        });
        let ctx = RequestContext::new(app_state, WorkingMode::Cmd);

        let session = Session::new_from_ctx(&ctx, &app_config, "test-session").unwrap();

        assert_eq!(session.name(), "test-session");
        assert_eq!(session.save_session(), app_config.save_session);
        assert!(session.is_empty());
        assert!(!session.dirty());
    }

    #[test]
    fn session_new_from_ctx_with_in_flight_todo_state_is_dirty() {
        let app_config = Arc::new(AppConfig::default());
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.init_todo_list("Ship feature");

        let session = Session::new_from_ctx(&ctx, &app_config, "mid-task").unwrap();

        assert!(
            session.dirty(),
            "carried-in todo state must mark the session dirty, or a create-then-exit skips the only save of it"
        );
    }

    #[test]
    fn session_set_role_captures_role_info() {
        let mut session = Session::default();
        let content = "---\ntemperature: 0.5\n---\nYou are a coder";
        let mut role = Role::new("coder", content);
        role.set_model(Model::default());

        session.set_role(role);

        assert_eq!(session.role_name(), Some("coder"));
        assert_eq!(session.temperature(), Some(0.5));
        assert!(session.dirty());
    }

    #[test]
    fn session_clear_role() {
        let mut session = Session::default();
        let mut role = Role::new("test", "prompt");
        role.set_model(Model::default());
        session.set_role(role);

        assert_eq!(session.role_name(), Some("test"));

        session.clear_role();

        assert_eq!(session.role_name(), None);
    }

    #[test]
    fn session_guard_empty_passes_when_empty() {
        let session = Session::default();

        assert!(session.guard_empty(CommandPrefix::Dot).is_ok());
    }

    #[test]
    fn session_guard_empty_hint_follows_command_prefix() {
        let mut session = Session::default();
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("hi".to_string()),
        ));

        let err = session
            .guard_empty(CommandPrefix::Slash)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`/empty session`"), "{err}");
    }

    #[test]
    fn session_needs_compression_threshold() {
        let session = Session::default();

        assert!(!session.needs_compression(4000, keep(0)));
    }

    #[test]
    fn session_needs_compression_returns_false_when_compressing() {
        let mut session = Session::default();

        session.set_compressing(true);

        assert!(!session.needs_compression(0, keep(0)));
    }

    #[test]
    fn session_needs_compression_returns_false_when_threshold_zero() {
        let session = Session::default();

        assert!(!session.needs_compression(0, keep(0)));
    }

    #[test]
    fn session_needs_compression_requires_reclaimable_tokens() {
        let mut session = Session::default();
        session.messages.push(Message::new(
            MessageRole::System,
            MessageContent::Text("x".repeat(4000)),
        ));
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("y".repeat(4000)),
        ));
        session.messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::Text("ok".to_string()),
        ));
        session.update_tokens();

        assert!(session.tokens() > 100);
        assert!(!session.needs_compression(100, keep(2)));
        assert!(session.needs_compression(100, keep(1)));
        assert!(
            session.needs_compression(
                100,
                KeepPolicy {
                    last: 2,
                    tokens: 500
                }
            ),
            "a tail over the token budget shrinks until something is reclaimable"
        );
    }

    fn keep(last: usize) -> KeepPolicy {
        KeepPolicy {
            last,
            tokens: usize::MAX,
        }
    }

    fn giant_tool_message() -> Message {
        Message::new(
            MessageRole::Tool,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                (0..200)
                    .map(|i| {
                        ToolResult::new(
                            ToolCall::new("t".into(), json!({}), Some(format!("id-{i}"))),
                            json!("x".repeat(2000)),
                        )
                    })
                    .collect(),
                String::new(),
            )),
        )
    }

    fn text(role: MessageRole, text: &str) -> Message {
        Message::new(role, MessageContent::Text(text.to_string()))
    }

    fn messages_json(messages: &[Message]) -> String {
        serde_json::to_string(messages).unwrap()
    }

    /// `[System, User, Tool(giant), Assistant]`: one tool loop holding far
    /// more tokens than any sane window, on a model with the given limit.
    fn giant_tail_session(max_input_tokens: Option<usize>) -> Session {
        let mut session = Session::default();
        let mut data = ModelData::new("giant-model");
        data.max_input_tokens = max_input_tokens;
        session.set_model(Model::from_config("provider", &[data]).remove(0));
        session.messages.push(text(MessageRole::System, "system"));
        session
            .messages
            .push(text(MessageRole::User, "do everything"));
        session.messages.push(giant_tool_message());
        session.messages.push(text(MessageRole::Assistant, "done"));
        session.update_tokens();
        session
    }

    #[test]
    fn session_needs_compression_counts_giant_tail_message_as_reclaimable() {
        let session = giant_tail_session(None);
        let keep = KeepPolicy {
            last: 10,
            tokens: keep_token_budget(4000, 0.8, None),
        };

        assert!(session.tokens() > 100_000);
        assert!(session.needs_compression(4000, keep));
    }

    #[test]
    fn session_safety_valve_fires_when_keep_last_covers_a_giant_tail_message() {
        let session = giant_tail_session(Some(200_000));
        let keep = KeepPolicy {
            last: 10,
            tokens: keep_token_budget(4000, 0.8, Some(200_000)),
        };

        assert!(session.safety_valve_triggered(0.8, keep, Some(900_000)));
    }

    #[test]
    #[serial]
    fn compression_info_live_numbers_match_keep_policy() {
        let mut app_state = AppState::test_default();
        app_state.config = Arc::new(AppConfig {
            compression_threshold: 4000,
            compression_keep_last: 2,
            ..AppConfig::default()
        });
        let mut ctx = RequestContext::new(Arc::new(app_state), WorkingMode::Cmd);
        ctx.last_prompt_token_usage = Some(7_777);
        let mut session = giant_tail_session(Some(200_000));
        session
            .messages
            .push(text(MessageRole::User, "last question"));
        session
            .messages
            .push(text(MessageRole::Assistant, "last answer"));
        session.update_tokens();
        ctx.session = Some(session);
        let app = ctx.app.config.clone();

        let keep = ctx.compression_keep_policy(None);
        assert_eq!(keep.last, 2);
        assert_eq!(keep.tokens, 2000);
        let session = ctx.session.as_ref().unwrap();
        let tail_start = session.keep_tail_start(keep);
        assert_eq!(tail_start, 4, "keep_last 2 keeps the final exchange");

        let info = ctx.compression_info(&app).unwrap();

        assert_eq!(info.threshold, 4000);
        assert_eq!(info.keep_last, 2);
        assert_eq!(info.keep_tokens, 2000);
        assert_eq!(info.last_prompt_tokens, Some(7_777));
        assert_eq!(
            info.reclaimable_tokens,
            Some(
                session
                    .model()
                    .total_tokens(session.foldable_messages(keep))
            )
        );
        assert!(info.reclaimable_tokens.unwrap() > 100_000);
        assert_eq!(
            info.kept_tail_messages,
            Some(session.messages.len() - tail_start)
        );
        assert_eq!(
            info.kept_tail_tokens,
            Some(
                session
                    .model()
                    .total_tokens(&session.messages[tail_start..])
            )
        );
        assert_eq!(info.archived_messages, Some(0));

        ctx.session
            .as_mut()
            .unwrap()
            .compress("summary".to_string(), keep);
        let after = ctx.compression_info(&app).unwrap();
        assert_eq!(after.archived_messages, Some(tail_start));
        assert_eq!(after.kept_tail_messages, Some(2));
        assert_eq!(after.reclaimable_tokens, Some(0));
    }

    #[test]
    fn keep_token_budget_takes_the_smaller_defined_term() {
        assert_eq!(keep_token_budget(4000, 0.8, Some(200_000)), 2000);
        assert_eq!(keep_token_budget(400_000, 0.8, Some(200_000)), 40_000);
        assert_eq!(keep_token_budget(4000, 0.8, None), 2000);
        assert_eq!(keep_token_budget(0, 0.8, Some(200_000)), 40_000);
        assert_eq!(keep_token_budget(0, 0.0, Some(200_000)), usize::MAX);
        assert_eq!(keep_token_budget(0, f32::NAN, Some(200_000)), usize::MAX);
        assert_eq!(keep_token_budget(0, 0.8, None), usize::MAX);
    }

    fn two_exchange_session() -> Session {
        let mut session = Session::default();
        session.messages.push(text(MessageRole::System, "system"));
        session
            .messages
            .push(text(MessageRole::User, &"a".repeat(4000)));
        session.messages.push(text(MessageRole::Assistant, "first"));
        session
            .messages
            .push(text(MessageRole::User, &"b".repeat(4000)));
        session
            .messages
            .push(text(MessageRole::Assistant, "second"));
        session.update_tokens();
        session
    }

    #[test]
    fn keep_tail_start_snaps_to_user_boundary() {
        let session = two_exchange_session();

        assert_eq!(session.keep_tail_start(keep(0)), 5);
        assert_eq!(
            session.keep_tail_start(keep(1)),
            5,
            "a lone assistant reply has no user turn to anchor to"
        );
        assert_eq!(session.keep_tail_start(keep(2)), 3);
        assert_eq!(
            session.keep_tail_start(keep(3)),
            3,
            "a cut inside an exchange moves forward to the next user turn"
        );
        assert_eq!(session.keep_tail_start(keep(4)), 1);
        assert_eq!(session.keep_tail_start(keep(5)), 1);
        assert_eq!(session.keep_tail_start(keep(99)), 1);
    }

    #[test]
    fn keep_tail_start_shrinks_until_tail_fits_tokens() {
        let session = two_exchange_session();
        let last_exchange = session.model().total_tokens(&session.messages[3..]);
        assert!(last_exchange > 1000);

        assert_eq!(
            session.keep_tail_start(KeepPolicy {
                last: 4,
                tokens: last_exchange
            }),
            3
        );
        assert_eq!(
            session.keep_tail_start(KeepPolicy {
                last: 4,
                tokens: last_exchange - 1
            }),
            5
        );
        assert_eq!(
            session.keep_tail_start(KeepPolicy { last: 4, tokens: 0 }),
            5
        );
    }

    #[test]
    fn compress_and_foldable_agree_at_every_boundary() {
        let base = two_exchange_session();
        let len = base.messages.len();
        for last in 0..=len + 2 {
            for tokens in [usize::MAX, 5000, 1500, 300, 0] {
                let keep = KeepPolicy { last, tokens };
                let mut session = base.clone();
                let t = session.keep_tail_start(keep);
                let tail = messages_json(&base.messages[t..]);
                let archived = messages_json(&base.messages[..t]);
                assert_eq!(
                    messages_json(session.foldable_messages(keep)),
                    messages_json(&base.messages[1..t]),
                    "foldable mismatch for {keep:?}"
                );

                session.compress("recap".to_string(), keep);

                assert!(session.messages[0].role.is_system());
                assert_eq!(
                    messages_json(&session.messages[1..]),
                    tail,
                    "compress kept a different tail for {keep:?}"
                );
                assert_eq!(
                    messages_json(&session.compressed_messages),
                    archived,
                    "compress archived a different prefix for {keep:?}"
                );
            }
        }
    }

    #[test]
    fn compress_keep_last_at_or_above_len_keeps_all_non_system() {
        for last in [5, 7] {
            let mut session = two_exchange_session();
            let original = messages_json(&session.messages[1..]);

            session.compress("recap".to_string(), keep(last));

            assert_eq!(session.messages.len(), 5);
            assert_eq!(session.messages[0].content.to_text(), "system\n\nrecap");
            assert_eq!(messages_json(&session.messages[1..]), original);
            assert_eq!(session.compressed_messages.len(), 1);
        }
    }

    #[test]
    fn build_messages_skips_reattach_when_tail_exceeds_keep_tokens() {
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let input = Input::from_str(&ctx, "next", None).unwrap();

        let mut small = Session::default();
        small.messages.push(text(MessageRole::System, "system"));
        small.messages.push(text(MessageRole::User, "hello"));
        small.messages.push(text(MessageRole::Assistant, "hi"));
        small.compress("recap".to_string(), keep(0));
        assert_eq!(
            small.build_messages(&input).len(),
            4,
            "a small last exchange is re-attached after a full compression"
        );

        let mut giant = giant_tail_session(None);
        giant.compress("recap".to_string(), keep(0));
        let messages = giant.build_messages(&input);
        assert_eq!(
            messages.len(),
            2,
            "a last exchange over the keep budget must stay archived"
        );
        assert!(messages[0].role.is_system());
        assert!(messages[1].role.is_user());
    }

    /// A session with a system message, a large user message, and a small
    /// assistant reply, on a model with the given input limit. With
    /// `keep_last = 1` the user message is reclaimable; with `keep_last = 2`
    /// nothing is unless the token budget forces the tail to shrink.
    fn valve_session(max_input_tokens: Option<usize>) -> Session {
        let mut session = Session::default();
        let mut data = ModelData::new("valve-model");
        data.max_input_tokens = max_input_tokens;
        session.set_model(Model::from_config("provider", &[data]).remove(0));
        session.messages.push(Message::new(
            MessageRole::System,
            MessageContent::Text("x".repeat(400)),
        ));
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("y".repeat(4000)),
        ));
        session.messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::Text("ok".to_string()),
        ));
        session.update_tokens();
        session
    }

    #[test]
    fn session_safety_valve_fires_from_estimate() {
        let session = valve_session(Some(1000));

        assert!(session.tokens() >= 800);
        assert!(session.safety_valve_triggered(0.8, keep(1), None));
    }

    #[test]
    fn session_safety_valve_fires_from_real_usage_over_estimate() {
        let session = valve_session(Some(1_000_000));

        assert!(!session.safety_valve_triggered(0.8, keep(1), None));
        assert!(!session.safety_valve_triggered(0.8, keep(1), Some(700_000)));
        assert!(session.safety_valve_triggered(0.8, keep(1), Some(800_000)));
    }

    #[test]
    fn session_safety_valve_disabled_when_zero() {
        let session = valve_session(Some(1000));

        assert!(!session.safety_valve_triggered(0.0, keep(1), Some(1_000_000)));
    }

    #[test]
    fn session_safety_valve_disabled_when_nan() {
        let session = valve_session(Some(1000));

        assert!(!session.safety_valve_triggered(f32::NAN, keep(1), Some(1_000_000)));
    }

    #[test]
    fn session_safety_valve_respects_compressing() {
        let mut session = valve_session(Some(1000));
        session.set_compressing(true);

        assert!(!session.safety_valve_triggered(0.8, keep(1), Some(1_000_000)));
    }

    #[test]
    fn session_safety_valve_requires_reclaimable_tokens() {
        let session = valve_session(Some(1000));

        assert!(!session.safety_valve_triggered(0.8, keep(2), Some(1_000_000)));
        assert!(session.safety_valve_triggered(0.8, keep(1), Some(1_000_000)));
        assert!(session.safety_valve_triggered(
            0.8,
            KeepPolicy {
                last: 2,
                tokens: 500
            },
            Some(1_000_000)
        ));
    }

    #[test]
    fn session_safety_valve_requires_model_max_input_tokens() {
        let session = valve_session(None);

        assert!(!session.safety_valve_triggered(0.8, keep(1), Some(1_000_000)));
    }

    #[test]
    fn session_set_compressing_flag() {
        let mut session = Session::default();
        assert!(!session.compressing());
        session.set_compressing(true);
        assert!(session.compressing());
        session.set_compressing(false);
        assert!(!session.compressing());
    }

    #[test]
    fn session_set_save_session_this_time() {
        let mut session = Session::default();
        assert!(!session.save_session_this_time);
        session.set_save_session_this_time();
        assert!(session.save_session_this_time);
    }

    #[test]
    fn session_save_session_returns_configured_value() {
        let mut session = Session::default();
        assert_eq!(session.save_session(), None);
        session.set_save_session(Some(true));
        assert_eq!(session.save_session(), Some(true));
        session.set_save_session(Some(false));
        assert_eq!(session.save_session(), Some(false));
        session.set_save_session(None);
        assert_eq!(session.save_session(), None);
    }

    #[test]
    fn session_compress_moves_messages() {
        let mut session = Session::default();
        session.messages.push(Message::new(
            MessageRole::System,
            MessageContent::Text("system prompt".to_string()),
        ));
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("hello".to_string()),
        ));

        assert_eq!(session.messages.len(), 2);
        assert!(session.compressed_messages.is_empty());

        session.compress("Summary of conversation".to_string(), keep(0));

        assert!(!session.compressed_messages.is_empty());
        assert_eq!(session.messages.len(), 1);
        assert!(session.dirty());
    }

    /// A tool-call message as a long Claude session stores it: every result
    /// carrying a thinking block whose signature dwarfs its text, plus a
    /// redacted block and a Responses-API reasoning item.
    fn tool_message_with_thinking(id: &str) -> Message {
        let mut result = ToolResult::new(
            ToolCall::new("fs_cat".into(), serde_json::json!({}), Some(id.to_string())),
            serde_json::json!("contents"),
        );
        result.thinking = vec![
            ThinkingBlock::Thinking {
                thinking: String::new(),
                signature: "S".repeat(64),
            },
            ThinkingBlock::RedactedThinking {
                data: "R".repeat(64),
            },
            ThinkingBlock::Reasoning {
                id: "rs_1".into(),
                summary: serde_json::Value::Null,
                encrypted_content: Some("E".repeat(64)),
            },
        ];
        Message::new(
            MessageRole::Tool,
            MessageContent::ToolCalls(MessageContentToolCalls::new(vec![result], "reading".into())),
        )
    }

    fn thinking_of(message: &Message) -> &[ThinkingBlock] {
        match &message.content {
            MessageContent::ToolCalls(tool_calls) => &tool_calls.tool_results[0].thinking,
            _ => panic!("not a tool message"),
        }
    }

    #[test]
    fn session_compress_drops_thinking_blocks_from_the_archive_but_not_the_live_tail() {
        let mut session = Session::default();
        session.compressed_messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("earlier".to_string()),
        ));
        session
            .compressed_messages
            .push(tool_message_with_thinking("earlier"));
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("first".to_string()),
        ));
        session.messages.push(tool_message_with_thinking("old"));
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("second".to_string()),
        ));
        session.messages.push(tool_message_with_thinking("recent"));

        // Keep the last exchange live; archive the first.
        session.compress("recap".to_string(), keep(2));

        let archived: Vec<&Message> = session
            .compressed_messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert_eq!(
            archived.len(),
            2,
            "the prior archive and the newly archived turn"
        );
        for message in archived {
            assert!(
                matches!(thinking_of(message), [ThinkingBlock::Reasoning { id, .. }] if id == "rs_1"),
                "archived: thinking/redacted dropped, reasoning kept: {:?}",
                thinking_of(message)
            );
        }
        let live = session
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("the recent tool message stayed live");
        assert_eq!(
            thinking_of(live).len(),
            3,
            "the live tail keeps its blocks for replay"
        );
    }

    #[test]
    fn build_messages_reattaches_a_stripped_tool_turn_behind_its_assistant_close() {
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let input = Input::from_str(&ctx, "next", None).unwrap();

        // An exchange as `add_message` records it: User, Tool, Assistant text.
        let mut session = Session::default();
        session.messages.push(text(MessageRole::System, "system"));
        session.messages.push(text(MessageRole::User, "read it"));
        session.messages.push(tool_message_with_thinking("last"));
        session.messages.push(text(MessageRole::Assistant, "done"));
        session.compress("recap".to_string(), keep(0));

        let messages = session.build_messages(&input);

        // Summary, the re-attached exchange, the new user message.
        assert_eq!(messages.len(), 5, "{messages:#?}");
        assert!(messages[0].role.is_system());
        assert!(messages[1].role.is_user());
        assert_eq!(messages[2].role, MessageRole::Tool);
        assert!(
            matches!(thinking_of(&messages[2]), [ThinkingBlock::Reasoning { .. }]),
            "the re-attached tool turn travels without its thinking blocks"
        );
        assert!(
            messages[3].role.is_assistant()
                && matches!(messages[3].content, MessageContent::Text(_)),
            "the archived exchange ends in an Assistant text message, so the tool turn \
             is never the turn in progress: {:?}",
            messages[3]
        );
        assert!(messages[4].role.is_user());
    }

    #[test]
    fn session_compress_does_not_accumulate_summaries() {
        let mut session = Session::default();
        session.messages.push(Message::new(
            MessageRole::System,
            MessageContent::Text("SYSTEM PROMPT".to_string()),
        ));
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("hello".to_string()),
        ));

        session.compress("recap one".to_string(), keep(0));

        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("more".to_string()),
        ));
        session.messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::Text("reply".to_string()),
        ));

        session.compress("recap two".to_string(), keep(0));

        assert_eq!(
            session.messages[0].content.to_text(),
            "SYSTEM PROMPT\n\nrecap two"
        );
    }

    #[test]
    fn session_compress_without_system_message_does_not_capture_summary() {
        let mut session = Session::default();
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("hello".to_string()),
        ));

        session.compress("recap one".to_string(), keep(0));

        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("more".to_string()),
        ));

        session.compress("recap two".to_string(), keep(0));

        assert_eq!(session.messages[0].content.to_text(), "recap two");
    }

    #[test]
    fn session_is_not_empty_after_compress() {
        let mut session = Session::default();
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("hello".to_string()),
        ));

        session.compress("Summary".to_string(), keep(0));

        assert!(!session.is_empty());
    }

    #[test]
    fn session_need_autoname_default_false() {
        let session = Session::default();

        assert!(!session.need_autoname());
    }

    #[test]
    fn session_set_autonaming_doesnt_panic_without_autoname() {
        let mut session = Session::default();

        session.set_autonaming(true);

        assert!(!session.need_autoname());
    }

    #[test]
    fn session_set_name_updates_name() {
        let mut session = Session::default();

        session.set_name("my-fork".to_string());

        assert_eq!(session.name(), "my-fork");
    }
}
