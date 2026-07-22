use super::input::*;
use super::*;

use crate::client::{Message, MessageContent, MessageRole};
use crate::render::MarkdownRender;

use anyhow::{Context, Result, bail};
use fancy_regex::Regex;
use inquire::{Confirm, Text, validator::Validation};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::fs::{read_to_string, write};
use std::path::Path;
use std::sync::LazyLock;

static RE_AUTONAME_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{8}T\d{6}-").unwrap());

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
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::deserialize_csv_or_vec"
    )]
    enabled_skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    save_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compression_threshold: Option<usize>,
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
    messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    data_urls: HashMap<String, String>,

    #[serde(skip)]
    model: Model,
    #[serde(skip)]
    role_prompt: String,
    #[serde(skip)]
    name: String,
    #[serde(skip)]
    path: Option<String>,
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
            ..Default::default()
        };
        session.set_role(role);
        session.dirty = false;
        Ok(session)
    }

    pub fn load_from_ctx(
        ctx: &RequestContext,
        app: &AppConfig,
        name: &str,
        path: &Path,
    ) -> Result<Self> {
        let content = read_to_string(path)
            .with_context(|| format!("Failed to load session {} at {}", name, path.display()))?;
        let mut session: Self =
            serde_yaml::from_str(&content).with_context(|| format!("Invalid session {name}"))?;

        session.model = Model::retrieve_model(app, &session.model_id, ModelType::Chat)?;

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
        }

        session.update_tokens();

        Ok(session)
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.compressed_messages.is_empty()
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn compressed_messages(&self) -> &[Message] {
        &self.compressed_messages
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn clear_autoname(&mut self) {
        self.autoname = None;
    }

    pub fn role_name(&self) -> Option<&str> {
        self.role_name.as_deref()
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn save_session(&self) -> Option<bool> {
        self.save_session
    }

    pub fn tokens(&self) -> usize {
        self.tokens
    }

    pub fn update_tokens(&mut self) {
        self.tokens = self.model().total_tokens(&self.messages);
    }

    pub fn has_user_messages(&self) -> bool {
        self.messages.iter().any(|v| v.role.is_user())
    }

    pub fn user_messages_len(&self) -> usize {
        self.messages.iter().filter(|v| v.role.is_user()).count()
    }

    pub fn export(&self) -> Result<String> {
        let mut data = json!({
            "path": self.path,
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
        if let Some(skills_enabled) = self.skills_enabled() {
            data["skills_enabled"] = skills_enabled.into();
        }
        if let Some(enabled_skills) = self.enabled_skills() {
            data["enabled_skills"] = json!(enabled_skills);
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
        data["messages"] = json!(self.messages);

        let output = serde_yaml::to_string(&data)
            .with_context(|| format!("Unable to show info about session '{}'", self.name))?;
        Ok(output)
    }

    pub fn render(
        &self,
        render: &mut MarkdownRender,
        agent_info: &Option<(String, Vec<String>)>,
    ) -> Result<String> {
        let mut items = vec![];

        if let Some(path) = &self.path {
            items.push(("path", path.to_string()));
        }

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

        if let Some(skills_enabled) = self.skills_enabled() {
            items.push(("skills_enabled", skills_enabled.to_string()));
        }

        if let Some(enabled_skills) = self.enabled_skills() {
            items.push(("enabled_skills", enabled_skills.join(",")));
        }

        if let Some(save_session) = self.save_session() {
            items.push(("save_session", save_session.to_string()));
        }

        if let Some(compression_threshold) = self.compression_threshold {
            items.push(("compression_threshold", compression_threshold.to_string()));
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

        if let Some(max_input_tokens) = self.model().max_input_tokens() {
            items.push(("max_input_tokens", max_input_tokens.to_string()));
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
        self.dirty = true;
        self.update_tokens();
    }

    pub fn clear_role(&mut self) {
        self.role_name = None;
        self.role_prompt.clear();
    }

    pub fn sync_agent(&mut self, agent: &Agent) {
        self.role_name = None;
        self.role_prompt = agent.interpolated_instructions();
        self.agent_variables = agent.variables().clone();
        self.agent_instructions = self.role_prompt.clone();
        if let Some(threshold) = agent.compression_threshold() {
            self.set_compression_threshold(Some(threshold));
        }
    }

    pub fn agent_variables(&self) -> &AgentVariables {
        &self.agent_variables
    }

    pub fn agent_instructions(&self) -> &str {
        &self.agent_instructions
    }

    pub fn set_save_session(&mut self, value: Option<bool>) {
        if self.save_session != value {
            self.save_session = value;
            self.dirty = true;
        }
    }

    pub fn set_save_session_this_time(&mut self) {
        self.save_session_this_time = true;
    }

    pub fn set_compression_threshold(&mut self, value: Option<usize>) {
        if self.compression_threshold != value {
            self.compression_threshold = value;
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

    pub fn needs_compression(&self, global_compression_threshold: usize) -> bool {
        if self.compressing {
            return false;
        }
        let threshold = self
            .compression_threshold
            .unwrap_or(global_compression_threshold);
        if threshold < 1 {
            return false;
        }
        self.tokens() > threshold
    }

    pub fn compressing(&self) -> bool {
        self.compressing
    }

    pub fn set_compressing(&mut self, compressing: bool) {
        self.compressing = compressing;
    }

    pub fn compress(&mut self, mut prompt: String) {
        if let Some(system_prompt) = self.messages.first().and_then(|v| {
            if MessageRole::System == v.role {
                let content = v.content.to_text();
                if !content.is_empty() {
                    return Some(content);
                }
            }
            None
        }) {
            prompt = format!("{system_prompt}\n\n{prompt}",);
        }
        self.compressed_messages.append(&mut self.messages);
        self.messages.push(Message::new(
            MessageRole::System,
            MessageContent::Text(prompt),
        ));
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
        let mut save_session = self.save_session();
        if self.save_session_this_time {
            save_session = Some(true);
        }
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
        }
        Ok(())
    }

    pub fn save(&mut self, session_name: &str, session_path: &Path, is_repl: bool) -> Result<()> {
        ensure_parent_exists(session_path)?;

        self.path = Some(session_path.display().to_string());

        let content = serde_yaml::to_string(&self)
            .with_context(|| format!("Failed to serde session '{}'", self.name))?;
        write(session_path, content).with_context(|| {
            format!(
                "Failed to write session '{}' to '{}'",
                self.name,
                session_path.display()
            )
        })?;

        if is_repl {
            println!("✓ Saved the session to '{}'.", session_path.display());
        }

        if self.name() != session_name {
            self.name = session_name.to_string()
        }

        self.dirty = false;

        Ok(())
    }

    pub fn guard_empty(&self) -> Result<()> {
        if !self.is_empty() {
            bail!(
                "Cannot perform this operation because the session has messages, please `.empty session` first."
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
        self.data_urls.clear();
        self.autoname = None;
        self.dirty = true;
        self.update_tokens();
    }

    pub fn pop_last_exchange(&mut self) -> Option<String> {
        let user_idx = self.messages.iter().rposition(|m| m.role.is_user())?;
        let user_text = self.messages[user_idx].content.as_text()?.to_string();
        self.messages.truncate(user_idx);
        self.dirty = true;
        self.update_tokens();
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
        {
            messages.extend(self.compressed_messages[index..].to_vec());
        }
        if need_add_msg {
            messages.push(Message::new(MessageRole::User, input.message_content()));
        }
        messages
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Message, MessageContent, MessageRole, Model};
    use crate::config::{AppConfig, AppState, RequestContext, WorkingMode};
    use crate::function::Functions;
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
        assert!(session.guard_empty().is_ok());
    }

    #[test]
    fn session_needs_compression_threshold() {
        let session = Session::default();
        assert!(!session.needs_compression(4000));
    }

    #[test]
    fn session_needs_compression_returns_false_when_compressing() {
        let mut session = Session::default();
        session.set_compressing(true);
        assert!(!session.needs_compression(0));
    }

    #[test]
    fn session_needs_compression_returns_false_when_threshold_zero() {
        let session = Session::default();
        assert!(!session.needs_compression(0));
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

        session.compress("Summary of conversation".to_string());

        assert!(!session.compressed_messages.is_empty());
        assert_eq!(session.messages.len(), 1);
        assert!(session.dirty());
    }

    #[test]
    fn session_is_not_empty_after_compress() {
        let mut session = Session::default();
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text("hello".to_string()),
        ));

        session.compress("Summary".to_string());

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
