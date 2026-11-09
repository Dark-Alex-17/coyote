use super::*;

use crate::client::structured_output::SchemaDialect;
use crate::client::{
    ChatCompletionsData, Client, ImageUrl, Message, MessageContent, MessageContentPart,
    MessageContentToolCalls, MessageRole, Model, init_client, patch_messages,
};
use crate::function::ToolResult;
use crate::utils::{
    AbortSignal, IMAGE_SLOT, UserInterrupt, is_loader_protocol, prepare_image_file_blocking,
    render_image_slots, sha256, strip_image_slots,
};

use anyhow::{Context, Result, bail};
use indexmap::IndexSet;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SUMMARY_MAX_WIDTH: usize = 80;
const EXTENDED_PROMPT_CACHE_TTL: &str = "1h";

pub const TOOL_LOOP_CHECKPOINT_TEXT: &str = "[Context checkpoint: the tool activity above was committed to session history mid-turn so it can be compressed. The task is not finished; it continues in the next turn.]";
pub const TOOL_LOOP_CONTINUE_TEXT: &str = "Continue exactly where you left off. This is an automatic context checkpoint, not a new instruction: the original request and your progress so far are in the history above. Do not restart or re-summarize; proceed with the next step.";

#[derive(Debug, Clone)]
pub struct Input {
    app_config: Arc<AppConfig>,
    stream_enabled: bool,
    session: Option<Session>,
    rag: Option<Arc<Rag>>,
    functions: Option<Vec<FunctionDeclaration>>,
    text: String,
    raw: (String, Vec<String>),
    patched_text: Option<String>,
    request_preamble: Option<String>,
    last_reply: Option<String>,
    continue_output: Option<String>,
    regenerate: bool,
    medias: Vec<String>,
    data_urls: HashMap<String, String>,
    tool_calls: Option<MessageContentToolCalls>,
    role: Role,
    rag_name: Option<String>,
    with_session: bool,
    with_agent: bool,
    long_lived: bool,
}

impl Input {
    /// Text from anywhere but the REPL image route has its `IMAGE_SLOT`s
    /// stripped, so a typed or loaded one cannot place an image.
    pub fn from_str(ctx: &RequestContext, text: &str, role: Option<Role>) -> Result<Self> {
        let text = strip_image_slots(text);
        let (role, with_session, with_agent) = resolve_role(ctx, role)?;
        let long_lived = is_long_lived(ctx, with_session);
        let captured = capture_input_config(ctx, &role);
        Ok(Self {
            app_config: Arc::clone(&ctx.app.config),
            stream_enabled: captured.stream_enabled,
            session: captured.session,
            rag: captured.rag,
            functions: captured.functions,
            text: text.clone(),
            raw: (text, vec![]),
            patched_text: None,
            request_preamble: None,
            last_reply: None,
            continue_output: None,
            regenerate: false,
            medias: Default::default(),
            data_urls: Default::default(),
            tool_calls: None,
            role,
            rag_name: None,
            with_session,
            with_agent,
            long_lived,
        })
    }

    /// Like `from_str` with an explicit role, except the input stays attached
    /// to the ctx session and agent, and `functions` is taken as given rather
    /// than selected from the role. For callers that build the role and tool
    /// set themselves but still want the exchange saved into the session.
    pub fn attached_with_role(
        ctx: &RequestContext,
        text: &str,
        role: Role,
        functions: Option<Vec<FunctionDeclaration>>,
    ) -> Result<Self> {
        let text = strip_image_slots(text);
        let with_session = ctx.session.is_some();
        Ok(Self {
            app_config: Arc::clone(&ctx.app.config),
            stream_enabled: ctx.app.config.stream,
            session: ctx.session.clone(),
            rag: ctx.rag.clone(),
            functions,
            text: text.clone(),
            raw: (text, vec![]),
            patched_text: None,
            request_preamble: None,
            last_reply: None,
            continue_output: None,
            regenerate: false,
            medias: Default::default(),
            data_urls: Default::default(),
            tool_calls: None,
            role,
            rag_name: None,
            with_session,
            with_agent: ctx.agent.is_some(),
            long_lived: is_long_lived(ctx, with_session),
        })
    }

    /// An input carrying only `text`: no session history, no RAG, and no
    /// function declarations. For auxiliary LLM calls (such as session
    /// summarization) whose requests must not grow with the session.
    pub fn detached_text(ctx: &RequestContext, text: &str) -> Self {
        let mut role = Role::new("", "");
        role.set_model(ctx.current_model().clone());
        Self::detached_with_role(ctx, text, role)
    }

    /// `detached_text` under a caller-built `role`. Unlike `from_str`, the
    /// role's tool filter is never consulted, so the agent's built-in
    /// functions are not declared either: auxiliary one-shots whose reply
    /// is read as data must not be able to call anything.
    pub fn detached_with_role(ctx: &RequestContext, text: &str, role: Role) -> Self {
        let text = strip_image_slots(text);
        Self {
            app_config: Arc::clone(&ctx.app.config),
            stream_enabled: ctx.app.config.stream,
            session: None,
            rag: None,
            functions: None,
            text: text.clone(),
            raw: (text, vec![]),
            patched_text: None,
            request_preamble: None,
            last_reply: None,
            continue_output: None,
            regenerate: false,
            medias: Default::default(),
            data_urls: Default::default(),
            tool_calls: None,
            role,
            rag_name: None,
            with_session: false,
            with_agent: false,
            long_lived: false,
        }
    }

    pub async fn from_files(
        ctx: &RequestContext,
        raw_text: &str,
        paths: Vec<String>,
        role: Option<Role>,
    ) -> Result<Self> {
        Self::load_files(ctx, &strip_image_slots(raw_text), paths, role).await
    }

    /// `from_files` keeping the `IMAGE_SLOT`s in `raw_text`. Only the REPL
    /// image route, which places every slot itself, may call it.
    pub(crate) async fn from_files_with_image_slots(
        ctx: &RequestContext,
        raw_text: &str,
        paths: Vec<String>,
        role: Option<Role>,
    ) -> Result<Self> {
        Self::load_files(ctx, raw_text, paths, role).await
    }

    async fn load_files(
        ctx: &RequestContext,
        raw_text: &str,
        paths: Vec<String>,
        role: Option<Role>,
    ) -> Result<Self> {
        let loaders = ctx.app.config.document_loaders.clone();
        let (raw_paths, local_paths, remote_urls, external_cmds, protocol_paths, with_last_reply) =
            resolve_paths(&loaders, paths)?;
        let mut last_reply = None;
        let (documents, medias, data_urls) = load_documents(
            &loaders,
            local_paths,
            remote_urls,
            external_cmds,
            protocol_paths,
        )
        .await
        .context("Failed to load files")?;
        let mut texts = vec![];
        if !raw_text.is_empty() {
            texts.push(raw_text.to_string());
        };
        if with_last_reply {
            if let Some(LastMessage { input, output, .. }) = ctx.last_message.as_ref() {
                if !output.is_empty() {
                    last_reply = Some(output.clone())
                } else if let Some(v) = input.last_reply.as_ref() {
                    last_reply = Some(v.clone());
                }
                if let Some(v) = last_reply.clone() {
                    texts.push(format!("\n{}", strip_image_slots(&v)));
                }
            }
            if last_reply.is_none() && documents.is_empty() && medias.is_empty() {
                bail!("No last reply found");
            }
        }
        let documents_len = documents.len();
        for (kind, path, contents) in documents {
            let path = strip_image_slots(&path);
            let contents = strip_image_slots(&contents);
            if documents_len == 1 && raw_text.is_empty() {
                texts.push(format!("\n{contents}"));
            } else {
                texts.push(format!(
                    "\n============ {kind}: {path} ============\n{contents}"
                ));
            }
        }
        let (role, with_session, with_agent) = resolve_role(ctx, role)?;
        let long_lived = is_long_lived(ctx, with_session);
        let captured = capture_input_config(ctx, &role);
        Ok(Self {
            app_config: Arc::clone(&ctx.app.config),
            stream_enabled: captured.stream_enabled,
            session: captured.session,
            rag: captured.rag,
            functions: captured.functions,
            text: texts.join("\n"),
            raw: (raw_text.to_string(), raw_paths),
            patched_text: None,
            request_preamble: None,
            last_reply,
            continue_output: None,
            regenerate: false,
            medias,
            data_urls,
            tool_calls: Default::default(),
            role,
            rag_name: None,
            with_session,
            with_agent,
            long_lived,
        })
    }

    pub async fn from_files_with_spinner(
        ctx: &RequestContext,
        raw_text: &str,
        paths: Vec<String>,
        role: Option<Role>,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        abortable_run_with_spinner(
            Input::from_files(ctx, raw_text, paths, role),
            "Loading files",
            abort_signal,
            UserInterrupt::Owned,
        )
        .await
    }

    pub fn is_empty(&self) -> bool {
        strip_image_slots(&self.text).is_empty() && self.medias.is_empty()
    }

    pub fn data_urls(&self) -> HashMap<String, String> {
        self.data_urls.clone()
    }

    pub fn declared_function_names(&self) -> HashSet<String> {
        self.functions
            .as_ref()
            .map(|functions| functions.iter().map(|f| f.name.clone()).collect())
            .unwrap_or_default()
    }

    pub fn tool_calls(&self) -> &Option<MessageContentToolCalls> {
        &self.tool_calls
    }

    pub fn text(&self) -> String {
        match self.patched_text.clone() {
            Some(text) => text,
            None => self.text.clone(),
        }
    }

    /// Whether the user's text says anything a retrieval query could use;
    /// a message of only images does not.
    fn has_search_text(&self) -> bool {
        !strip_image_slots(&self.text).trim().is_empty()
    }

    pub fn bare_text(&self) -> &str {
        &self.text
    }

    /// The text the model is sent: the request preamble, when one is set,
    /// ahead of `text()`.
    pub fn text_with_preamble(&self) -> String {
        match &self.request_preamble {
            Some(preamble) => format!("{}\n\n{}", preamble.trim_end(), self.text()),
            None => self.text(),
        }
    }

    pub fn clear_patch(&mut self) {
        self.patched_text = None;
        self.request_preamble = None;
    }

    pub fn set_request_preamble(&mut self, text: impl Into<String>) {
        self.request_preamble = Some(strip_image_slots(&text.into()));
    }

    /// Copies the request-only state of `source`. Its text comes along with
    /// its medias, since the `IMAGE_SLOT`s in it place those images and this
    /// input's own constructor stripped them.
    pub(crate) fn carry_patch_from(&mut self, source: &Input) {
        if !source.medias.is_empty() {
            self.text = source.text.clone();
        }
        self.patched_text = source.patched_text.clone();
        self.rag_name = source.rag_name.clone();
        self.raw = source.raw.clone();
        self.medias = source.medias.clone();
        self.data_urls = source.data_urls.clone();
        self.last_reply = source.last_reply.clone();
        self.continue_output = source.continue_output.clone();
    }

    /// Attaches images in the given order, keeping repeats, unlike
    /// `from_files`, which loads each distinct path once.
    pub async fn extend_image_medias(&mut self, paths: &[String]) -> Result<()> {
        for path in paths {
            let contents = prepare_image_file_blocking(path.into())
                .await
                .with_context(|| format!("Unable to read media '{path}'"))?
                .to_data_url();
            self.data_urls.insert(sha256(&contents), path.clone());
            self.medias.push(contents);
            self.raw.1.push(path.clone());
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn set_patched_text(&mut self, text: &str) {
        self.patched_text = Some(text.to_string());
    }

    pub fn medias(&self) -> &[String] {
        &self.medias
    }

    #[cfg(test)]
    pub fn last_reply(&self) -> Option<&str> {
        self.last_reply.as_deref()
    }

    pub fn set_text(&mut self, text: String) {
        self.text = strip_image_slots(&text);
    }

    pub fn stream(&self) -> bool {
        self.stream_enabled && !self.role().model().no_stream()
    }

    pub fn continue_output(&self) -> Option<&str> {
        self.continue_output.as_deref()
    }

    pub fn set_continue_output(&mut self, output: &str) {
        let output = match &self.continue_output {
            Some(v) => format!("{v}{output}"),
            None => output.to_string(),
        };
        self.continue_output = Some(output);
    }

    pub(crate) fn clear_continue_output(&mut self) {
        self.continue_output = None;
    }

    pub(crate) fn clear_continuation(&mut self) {
        self.continue_output = None;
        self.regenerate = false;
    }

    pub fn regenerate(&self) -> bool {
        self.regenerate
    }

    pub fn set_regenerate(&mut self, current_role: Role) {
        if current_role.name() == self.role().name() {
            self.role = current_role;
        }
        self.regenerate = true;
        self.tool_calls = None;
    }

    pub async fn use_embeddings(&mut self, abort_signal: AbortSignal) -> Result<()> {
        if !self.has_search_text() {
            return Ok(());
        }
        if let Some(rag) = &self.rag {
            let result = rag
                .search_with_template(
                    &self.app_config,
                    &self.text,
                    abort_signal,
                    UserInterrupt::Owned,
                )
                .await?;
            self.patched_text = Some(result);
            self.rag_name = Some(rag.name().to_string());
        }
        Ok(())
    }

    pub fn rag_name(&self) -> Option<&str> {
        self.rag_name.as_deref()
    }

    pub fn merge_tool_results(mut self, output: String, tool_results: Vec<ToolResult>) -> Self {
        match self.tool_calls.as_mut() {
            Some(exist_tool_results) => {
                exist_tool_results.merge(tool_results, output);
            }
            None => self.tool_calls = Some(MessageContentToolCalls::new(tool_results, output)),
        }
        self
    }

    pub fn into_checkpoint_continuation(mut self, ctx: &RequestContext) -> Self {
        self.tool_calls = None;
        self.text = TOOL_LOOP_CONTINUE_TEXT.into();
        self.raw = (TOOL_LOOP_CONTINUE_TEXT.into(), vec![]);
        self.patched_text = None;
        self.request_preamble = None;
        self.rag_name = None;
        self.medias.clear();
        self.data_urls.clear();
        self.last_reply = None;
        self.continue_output = None;
        self.regenerate = false;
        self.refresh_session(ctx);
        self
    }

    pub fn create_client(&self) -> Result<Box<dyn Client>> {
        init_client(&self.app_config, self.role().model().clone())
    }

    pub async fn fetch_chat_text(&self) -> Result<String> {
        let client = self.create_client()?;
        let text = client.chat_completions(self.clone()).await?.text;
        let text = strip_think_tag(&text).to_string();
        Ok(text)
    }

    pub fn prepare_completion_data(
        &self,
        model: &Model,
        stream: bool,
    ) -> Result<ChatCompletionsData> {
        guard_medias(&self.medias, model)?;
        let mut messages = self.build_messages()?;
        let current_user = if self.continue_output().is_none() {
            messages.iter().rposition(|message| {
                message.role == MessageRole::User
                    && !matches!(message.content, MessageContent::ToolCalls(_))
            })
        } else {
            None
        };
        limit_outgoing_images(&mut messages, model, current_user);
        patch_messages(&mut messages, model);
        model.guard_max_input_tokens(&messages)?;
        let (temperature, top_p) = (self.role().temperature(), self.role().top_p());
        let reasoning_effort = self
            .role()
            .reasoning_effort()
            .or_else(|| model.default_reasoning_effort().map(|s| s.to_string()));
        let functions = if model.supports_function_calling() {
            let fns = self.functions.clone();
            if let Some(vec) = &fns {
                for def in vec {
                    debug!("Function definition: {:?}", def.name);
                }
            }
            fns
        } else {
            None
        };
        let structured_output = if model.supports_structured_output() {
            self.role().structured_output().cloned()
        } else {
            None
        };
        let extended_ttl = matches!(
            self.app_config.prompt_cache_ttl.as_deref(),
            None | Some(EXTENDED_PROMPT_CACHE_TTL)
        );
        let prompt_cache_ttl =
            (self.long_lived && extended_ttl).then(|| EXTENDED_PROMPT_CACHE_TTL.to_string());
        Ok(ChatCompletionsData {
            messages,
            temperature,
            top_p,
            reasoning_effort,
            functions,
            stream,
            prompt_cache_ttl,
            structured_output,
        })
    }

    /// The dialect the request will be built in when the provider enforces
    /// the role's schema itself, or None when the caller must fall back.
    /// Gated on the client's model because that is the model
    /// `Client::chat_completions` passes to `prepare_completion_data`.
    #[cfg_attr(not(test), expect(dead_code, reason = "no production caller yet"))] // TASK-429
    pub fn native_structured_output(&self, client: &dyn Client) -> Option<SchemaDialect> {
        if self.role().structured_output().is_some() && client.model().supports_structured_output()
        {
            client.native_structured_output_dialect()
        } else {
            None
        }
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "no production caller yet"))] // TASK-429
    pub fn without_structured_output(&self) -> Input {
        let mut input = self.clone();
        input.role.set_structured_output(None);
        input
    }

    pub fn build_messages(&self) -> Result<Vec<Message>> {
        let mut messages = if let Some(session) = self.session(&self.session) {
            session.build_messages(self)
        } else {
            self.role().build_messages(self)
        };
        if let Some(tool_calls) = &self.tool_calls {
            messages.push(Message::new(
                MessageRole::Assistant,
                MessageContent::ToolCalls(tool_calls.clone()),
            ))
        }
        Ok(messages)
    }

    pub fn echo_messages(&self) -> String {
        if let Some(session) = self.session(&self.session) {
            session.echo_messages(self)
        } else {
            self.role().echo_messages(self)
        }
    }

    pub fn role(&self) -> &Role {
        &self.role
    }

    /// Refuses a message whose images the model cannot take or that exceed
    /// the per-request limits. Runs before anything is sent so a refusal
    /// never reaches the session.
    pub fn guard_images(&self) -> Result<()> {
        guard_medias(&self.medias, self.role().model())
    }

    pub fn app_config(&self) -> &AppConfig {
        &self.app_config
    }

    pub fn set_role_model(&mut self, model: Model) {
        self.role.set_model(model);
    }

    pub fn session<'a>(&self, session: &'a Option<Session>) -> Option<&'a Session> {
        if self.with_session {
            session.as_ref()
        } else {
            None
        }
    }

    /// Re-captures `ctx.session` in place of the snapshot taken at
    /// construction. Session compression mutates the ctx's session, not the
    /// snapshot, so an input that outlives a compression must refresh before
    /// building its next request or it re-sends the pre-compression history.
    pub fn refresh_session(&mut self, ctx: &RequestContext) {
        self.session = ctx.session.clone();
    }

    pub fn session_mut<'a>(&self, session: &'a mut Option<Session>) -> Option<&'a mut Session> {
        if self.with_session {
            session.as_mut()
        } else {
            None
        }
    }

    pub fn with_session(&self) -> bool {
        self.with_session
    }

    pub fn with_agent(&self) -> bool {
        self.with_agent
    }

    pub fn summary(&self) -> String {
        let text: String = render_image_slots(&self.text, self.medias.len())
            .trim()
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if text.width_cjk() > SUMMARY_MAX_WIDTH {
            let mut sum_width = 0;
            let mut chars = vec![];
            for c in text.chars() {
                sum_width += c.width_cjk().unwrap_or(1);
                if sum_width > SUMMARY_MAX_WIDTH - 3 {
                    chars.extend(['.', '.', '.']);
                    break;
                }
                chars.push(c);
            }
            chars.into_iter().collect()
        } else {
            text
        }
    }

    pub fn raw(&self) -> String {
        self.raw_with(render_image_slots)
    }

    /// `raw` with the image positions dropped rather than shown.
    pub fn raw_without_image_slots(&self) -> String {
        self.raw_with(|text, _| strip_image_slots(text))
    }

    fn raw_with(&self, show_slots: fn(&str, usize) -> String) -> String {
        let (text, files) = &self.raw;
        let mut segments: Vec<String> = files.iter().map(|f| strip_image_slots(f)).collect();
        if !segments.is_empty() {
            segments.insert(0, ".file".into());
        }
        if !text.is_empty() {
            if !segments.is_empty() {
                segments.push("--".into());
            }
            segments.push(show_slots(text, self.medias.len()));
        }
        segments.join(" ")
    }

    pub fn render(&self) -> String {
        let text = render_image_slots(&self.text(), self.medias.len());
        if self.medias.is_empty() {
            return text;
        }
        let tail_text = if text.is_empty() {
            String::new()
        } else {
            format!(" -- {text}")
        };
        let files: Vec<String> = self
            .medias
            .iter()
            .cloned()
            .map(|url| resolve_data_url(&self.data_urls, url))
            .collect();
        format!(".file {}{}", files.join(" "), tail_text)
    }

    /// Splits the text at each `IMAGE_SLOT`, the i-th slot taking the i-th
    /// image. Slots beyond the images are dropped; images without a slot
    /// follow the text, as `.file` has always sent them.
    pub fn message_content(&self) -> MessageContent {
        let text = self.text_with_preamble();
        if self.medias.is_empty() {
            return MessageContent::Text(strip_image_slots(&text));
        }
        let image_part = |url: &String| MessageContentPart::ImageUrl {
            image_url: ImageUrl { url: url.clone() },
        };
        let mut medias = self.medias.iter();
        let mut parts = Vec::new();
        let mut pending = String::new();
        for (index, piece) in text.split(IMAGE_SLOT).enumerate() {
            if index > 0
                && let Some(url) = medias.next()
            {
                push_trimmed_text(&mut parts, &pending);
                pending.clear();
                parts.push(image_part(url));
            }
            pending.push_str(piece);
        }
        if !parts.is_empty() {
            push_trimmed_text(&mut parts, &pending);
        } else if !self.text.is_empty() || self.request_preamble.is_some() {
            parts.push(MessageContentPart::Text { text: pending });
        }
        parts.extend(medias.map(image_part));
        MessageContent::Array(parts)
    }
}

fn push_trimmed_text(parts: &mut Vec<MessageContentPart>, text: &str) {
    let text = text.trim();
    if !text.is_empty() {
        parts.push(MessageContentPart::Text { text: text.into() });
    }
}

fn resolve_role(ctx: &RequestContext, role: Option<Role>) -> Result<(Role, bool, bool)> {
    match role {
        Some(v) => Ok((v, false, false)),
        None => Ok((
            ctx.extract_role(ctx.app.config.as_ref())?,
            ctx.session.is_some(),
            ctx.agent.is_some(),
        )),
    }
}

/// A root context seated on a session is paced by a human, so its turns are
/// often more than five minutes apart. Spawned children and detached
/// one-shots run back to back and keep the provider's default cache TTL.
fn is_long_lived(ctx: &RequestContext, with_session: bool) -> bool {
    ctx.self_agent_id.is_none() && with_session
}

struct CapturedInputConfig {
    stream_enabled: bool,
    session: Option<Session>,
    rag: Option<Arc<Rag>>,
    functions: Option<Vec<FunctionDeclaration>>,
}

fn capture_input_config(ctx: &RequestContext, role: &Role) -> CapturedInputConfig {
    CapturedInputConfig {
        stream_enabled: ctx.app.config.stream,
        session: ctx.session.clone(),
        rag: ctx.rag.clone(),
        functions: ctx.select_functions(role),
    }
}

type ResolvePathsOutput = (
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    bool,
);

fn resolve_paths(
    loaders: &HashMap<String, String>,
    paths: Vec<String>,
) -> Result<ResolvePathsOutput> {
    let mut raw_paths = IndexSet::new();
    let mut local_paths = IndexSet::new();
    let mut remote_urls = IndexSet::new();
    let mut external_cmds = IndexSet::new();
    let mut protocol_paths = IndexSet::new();
    let mut with_last_reply = false;
    for path in paths {
        if path == "%%" {
            with_last_reply = true;
            raw_paths.insert(path);
        } else if path.starts_with('`') && path.len() > 2 && path.ends_with('`') {
            external_cmds.insert(path[1..path.len() - 1].to_string());
            raw_paths.insert(path);
        } else if is_url(&path) {
            if path.strip_suffix("**").is_some() {
                bail!("Invalid website '{path}'");
            }
            remote_urls.insert(path.clone());
            raw_paths.insert(path);
        } else if is_loader_protocol(loaders, &path) {
            protocol_paths.insert(path.clone());
            raw_paths.insert(path);
        } else {
            let resolved_path = resolve_home_dir(&path);
            let absolute_path = to_absolute_path(&resolved_path)
                .with_context(|| format!("Invalid path '{path}'"))?;
            local_paths.insert(resolved_path);
            raw_paths.insert(absolute_path);
        }
    }
    Ok((
        raw_paths.into_iter().collect(),
        local_paths.into_iter().collect(),
        remote_urls.into_iter().collect(),
        external_cmds.into_iter().collect(),
        protocol_paths.into_iter().collect(),
        with_last_reply,
    ))
}

async fn load_documents(
    loaders: &HashMap<String, String>,
    local_paths: Vec<String>,
    remote_urls: Vec<String>,
    external_cmds: Vec<String>,
    protocol_paths: Vec<String>,
) -> Result<(
    Vec<(&'static str, String, String)>,
    Vec<String>,
    HashMap<String, String>,
)> {
    let mut files = vec![];
    let mut medias = vec![];
    let mut data_urls = HashMap::new();

    for cmd in external_cmds {
        let output = duct::cmd(&SHELL.cmd, &[&SHELL.arg, &cmd])
            .stderr_to_stdout()
            .unchecked()
            .read()
            .unwrap_or_else(|err| err.to_string());
        files.push(("CMD", cmd, output));
    }

    let local_files = expand_glob_paths(&local_paths, true).await?;
    for file_path in local_files {
        if is_supported_image_path(&file_path) {
            let contents = prepare_image_file_blocking(file_path.as_str().into())
                .await
                .with_context(|| format!("Unable to read media '{file_path}'"))?
                .to_data_url();
            data_urls.insert(sha256(&contents), file_path);
            medias.push(contents)
        } else {
            let document = load_file(loaders, &file_path)
                .await
                .with_context(|| format!("Unable to read file '{file_path}'"))?;
            files.push(("FILE", file_path, document.contents));
        }
    }

    for file_url in remote_urls {
        let (contents, extension) = fetch_with_loaders(loaders, &file_url, true)
            .await
            .with_context(|| format!("Failed to load url '{file_url}'"))?;
        if extension == MEDIA_URL_EXTENSION {
            data_urls.insert(sha256(&contents), file_url);
            medias.push(contents)
        } else {
            files.push(("URL", file_url, contents));
        }
    }

    for protocol_path in protocol_paths {
        let documents = load_protocol_path(loaders, &protocol_path)
            .with_context(|| format!("Failed to load from '{protocol_path}'"))?;
        files.extend(
            documents
                .into_iter()
                .map(|document| ("FROM", document.path, document.contents)),
        );
    }

    Ok((files, medias, data_urls))
}

pub fn resolve_data_url(data_urls: &HashMap<String, String>, data_url: String) -> String {
    if data_url.starts_with("data:") {
        let hash = sha256(&data_url);
        if let Some(path) = data_urls.get(&hash) {
            return path.to_string();
        }
        data_url
    } else {
        data_url
    }
}

pub fn vision_unsupported_message(model: &Model) -> String {
    format!(
        "The model '{}' does not accept images. Switch to a vision-capable model, or set 'supports_vision: true' on this model in your config if it does support them.",
        model.id()
    )
}

fn guard_medias(medias: &[String], model: &Model) -> Result<()> {
    if medias.is_empty() {
        return Ok(());
    }
    if !model.supports_vision() {
        bail!("{}", vision_unsupported_message(model));
    }
    let base64_bytes: usize = medias.iter().map(|url| data_url_base64_len(url)).sum();
    if medias.len() > MAX_REQUEST_IMAGES || base64_bytes > MAX_REQUEST_IMAGE_BASE64_BYTES {
        const MB: f64 = 1024.0 * 1024.0;
        bail!(
            "This message carries {} images ({:.1} MB); the limit is {MAX_REQUEST_IMAGES} images and {} MB per request.",
            medias.len(),
            base64_bytes as f64 / MB,
            MAX_REQUEST_IMAGE_BASE64_BYTES as f64 / MB
        );
    }
    Ok(())
}

fn data_url_base64_len(url: &str) -> usize {
    url.split_once(',')
        .map_or(url.len(), |(_, data)| data.len())
}

const IMAGES_UNSUPPORTED_REASON: &str = "the current model does not accept images";
const IMAGES_OVER_BUDGET_REASON: &str = "older image dropped to stay within the request size limit";

/// The number of oldest history images still to refuse, consumed as history
/// images are visited oldest first.
struct ImageBudget {
    to_drop: usize,
}

impl ImageBudget {
    fn admit(&mut self) -> bool {
        if self.to_drop == 0 {
            return true;
        }
        self.to_drop -= 1;
        false
    }
}

/// Base64 sizes of the images a message carries, in the order they are sent.
fn image_sizes(message: &Message) -> Vec<usize> {
    match &message.content {
        MessageContent::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                MessageContentPart::ImageUrl { image_url } => {
                    Some(data_url_base64_len(&image_url.url))
                }
                MessageContentPart::Text { .. } => None,
            })
            .collect(),
        MessageContent::ToolCalls(tool_calls) => tool_calls
            .tool_results
            .iter()
            .flat_map(|result| &result.attachments)
            .map(|attachment| attachment.data.len())
            .collect(),
        MessageContent::Text(_) => vec![],
    }
}

/// Counts every image in the request, the current message's included, and
/// picks how many of the oldest history images go. The cut depends only on
/// the totals, so the same history always yields the same request.
fn oldest_history_images_to_drop(messages: &[Message], current_user: Option<usize>) -> usize {
    let mut history = Vec::new();
    let mut count = 0;
    let mut bytes = 0;
    for (idx, message) in messages.iter().enumerate() {
        let sizes = image_sizes(message);
        count += sizes.len();
        bytes += sizes.iter().sum::<usize>();
        let is_history_image_message = Some(idx) != current_user
            && (message.role == MessageRole::User
                || matches!(message.content, MessageContent::ToolCalls(_)));
        if is_history_image_message {
            history.extend(sizes);
        }
    }

    let count_to_drop = history_images_to_drop(count);
    let bytes_to_drop = history_image_bytes_to_drop(bytes);
    let mut dropped = 0;
    let mut dropped_bytes = 0;
    for size in history {
        if dropped >= count_to_drop && dropped_bytes >= bytes_to_drop {
            break;
        }
        dropped += 1;
        dropped_bytes += size;
    }
    dropped
}

/// Keeps history images within what the model and the per-request limits
/// accept, replacing each dropped image with a short note. Over a limit, the
/// oldest images go in blocks (see `history_images_to_drop`) so the prompt
/// prefix providers cache does not shift on every turn. It rewrites only the
/// outgoing copy and never fails: an error here would be written into the
/// session as a failed turn. The current user message's images are never
/// dropped; `guard_medias` has already checked them on their own.
fn limit_outgoing_images(messages: &mut [Message], model: &Model, current_user: Option<usize>) {
    let vision = model.supports_vision();
    let reason = if vision {
        IMAGES_OVER_BUDGET_REASON
    } else {
        IMAGES_UNSUPPORTED_REASON
    };
    let mut budget = ImageBudget {
        to_drop: if vision {
            oldest_history_images_to_drop(messages, current_user)
        } else {
            usize::MAX
        },
    };

    for (idx, message) in messages.iter_mut().enumerate() {
        if Some(idx) == current_user {
            continue;
        }
        match &mut message.content {
            MessageContent::Array(parts) if message.role == MessageRole::User => {
                let total = parts.len();
                parts.retain(|part| {
                    !matches!(part, MessageContentPart::ImageUrl { .. }) || budget.admit()
                });
                let dropped = total - parts.len();
                if dropped > 0 {
                    parts.push(MessageContentPart::Text {
                        text: format!("[{dropped} image(s) omitted: {reason}]"),
                    });
                }
            }
            MessageContent::ToolCalls(tool_calls) => {
                for result in tool_calls.tool_results.iter_mut() {
                    let total = result.attachments.len();
                    result.attachments.retain(|_| budget.admit());
                    let dropped = total - result.attachments.len();
                    if dropped > 0 {
                        result.output = json!({
                            "result": result.output.take(),
                            "images_omitted": format!(
                                "{dropped} image(s) from this tool were omitted: {reason}"
                            ),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::structured_output::StructuredOutputSpec;
    use crate::client::{ClientConfig, ModelData};
    use crate::config::request_context::RequestContext;
    use crate::config::session::KeepPolicy;
    use crate::config::{AppState, WorkingMode};
    use crate::function::{ImageAttachment, ToolCall};
    use serde_json::json;
    use std::fs;
    use std::sync::Arc;
    use std::time::SystemTime;

    fn default_app_state() -> Arc<AppState> {
        Arc::new(AppState::test_default())
    }

    fn create_test_ctx() -> RequestContext {
        RequestContext::new(default_app_state(), WorkingMode::Cmd)
    }

    #[test]
    fn resolve_role_with_explicit_role() {
        let ctx = create_test_ctx();
        let role = Role::new("custom", "be helpful");
        let (resolved, with_session, with_agent) = resolve_role(&ctx, Some(role)).unwrap();
        assert_eq!(resolved.name(), "custom");
        assert!(!with_session);
        assert!(!with_agent);
    }

    #[test]
    fn resolve_role_without_role_no_session_no_agent() {
        let ctx = create_test_ctx();
        let (resolved, with_session, with_agent) = resolve_role(&ctx, None).unwrap();
        assert_eq!(resolved.name(), "");
        assert!(!with_session);
        assert!(!with_agent);
    }

    #[test]
    fn resolve_role_without_role_with_session() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let (_resolved, with_session, with_agent) = resolve_role(&ctx, None).unwrap();
        assert!(with_session);
        assert!(!with_agent);
    }

    #[test]
    fn resolve_role_explicit_role_overrides_session_flag() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let role = Role::new("explicit", "prompt");
        let (_resolved, with_session, _with_agent) = resolve_role(&ctx, Some(role)).unwrap();
        assert!(!with_session);
    }

    #[test]
    fn detached_text_input_carries_no_session_history_or_functions() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let earlier = Input::from_str(&ctx, "earlier question", None).unwrap();
        ctx.session
            .as_mut()
            .unwrap()
            .add_message(&earlier, "earlier answer")
            .unwrap();

        let detached = Input::detached_text(&ctx, "summarize the above");
        let messages = detached.build_messages().unwrap();
        assert_eq!(messages.len(), 1, "must contain only the detached text");
        assert!(messages[0].role.is_user());
        assert_eq!(messages[0].content.to_text(), "summarize the above");
        assert!(detached.declared_function_names().is_empty());
        assert!(!detached.with_session());
        assert!(!detached.with_agent());
    }

    #[test]
    fn detached_with_role_keeps_the_role_but_declares_no_functions() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let mut role = Role::new("classifier", "answer in json");
        role.set_model(Model::from_config("openai", &[ModelData::new("router")]).remove(0));

        let detached = Input::detached_with_role(&ctx, "decide", role);

        assert_eq!(detached.role().name(), "classifier");
        assert_eq!(detached.role().prompt(), "answer in json");
        assert_eq!(detached.role().model().id(), "openai:router");
        assert!(detached.functions.is_none());
        assert!(detached.declared_function_names().is_empty());
        assert!(!detached.with_session());
        assert!(!detached.with_agent());
        let messages = detached.build_messages().unwrap();
        assert_eq!(messages.len(), 2, "system prompt plus the detached text");
        assert_eq!(messages[1].content.to_text(), "decide");
    }

    #[test]
    fn refresh_session_rebuilds_from_compressed_session() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        for i in 0..4 {
            let question =
                Input::from_str(&ctx, &format!("question {i}: {}", "x".repeat(500)), None).unwrap();
            ctx.session
                .as_mut()
                .unwrap()
                .add_message(&question, &"y".repeat(500))
                .unwrap();
        }

        let mut input = Input::from_str(&ctx, "retry me", None).unwrap();
        let stale = serde_json::to_string(&input.build_messages().unwrap()).unwrap();

        ctx.session.as_mut().unwrap().compress(
            "recap".into(),
            KeepPolicy {
                last: 1,
                tokens: usize::MAX,
            },
        );

        assert_eq!(
            serde_json::to_string(&input.build_messages().unwrap()).unwrap(),
            stale,
            "the snapshot captured at construction must be untouched by compression"
        );

        input.refresh_session(&ctx);
        let refreshed = serde_json::to_string(&input.build_messages().unwrap()).unwrap();
        assert!(
            refreshed.len() < stale.len(),
            "a refreshed input must build a strictly smaller request after compression"
        );
    }

    #[test]
    fn resolve_paths_detects_last_reply_syntax() {
        let loaders = HashMap::new();
        let (_, _, _, _, _, with_last_reply) =
            resolve_paths(&loaders, vec!["%%".to_string()]).unwrap();
        assert!(with_last_reply);
    }

    #[test]
    fn resolve_paths_detects_url() {
        let loaders = HashMap::new();
        let (_, local, remote, _, _, _) =
            resolve_paths(&loaders, vec!["https://example.com".to_string()]).unwrap();
        assert!(local.is_empty());
        assert_eq!(remote, vec!["https://example.com"]);
    }

    #[test]
    fn resolve_paths_detects_external_command() {
        let loaders = HashMap::new();
        let (_, _, _, external, _, _) =
            resolve_paths(&loaders, vec!["`echo hello`".to_string()]).unwrap();
        assert_eq!(external, vec!["echo hello"]);
    }

    #[test]
    fn resolve_paths_empty_input() {
        let loaders = HashMap::new();
        let (raw, local, remote, external, protocol, with_last) =
            resolve_paths(&loaders, vec![]).unwrap();
        assert!(raw.is_empty());
        assert!(local.is_empty());
        assert!(remote.is_empty());
        assert!(external.is_empty());
        assert!(protocol.is_empty());
        assert!(!with_last);
    }

    #[test]
    fn resolve_paths_rejects_url_with_glob_suffix() {
        let loaders = HashMap::new();
        let result = resolve_paths(&loaders, vec!["https://example.com**".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_paths_mixed_inputs() {
        let loaders = HashMap::new();
        let paths = vec![
            "%%".to_string(),
            "https://example.com".to_string(),
            "`ls`".to_string(),
        ];
        let (_, _, remote, external, _, with_last) = resolve_paths(&loaders, paths).unwrap();
        assert!(with_last);
        assert_eq!(remote.len(), 1);
        assert_eq!(external.len(), 1);
    }

    #[test]
    fn input_from_str_captures_text() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "hello world", None).unwrap();
        assert_eq!(input.text(), "hello world");
    }

    #[test]
    fn input_from_str_with_explicit_role() {
        let ctx = create_test_ctx();
        let role = Role::new("pirate", "you are a pirate");
        let input = Input::from_str(&ctx, "ahoy", Some(role)).unwrap();
        assert_eq!(input.role().name(), "pirate");
        assert!(!input.with_agent());
    }

    #[test]
    fn input_from_str_captures_stream_from_config() {
        let mut state = AppState::test_default();
        let mut config = (*state.config).clone();
        config.stream = false;
        state.config = Arc::new(config);
        let ctx = RequestContext::new(Arc::new(state), WorkingMode::Cmd);
        let input = Input::from_str(&ctx, "test", None).unwrap();
        assert!(!input.stream_enabled);
    }

    #[test]
    fn input_is_empty_with_no_text_and_no_medias() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "", None).unwrap();
        assert!(input.is_empty());
    }

    #[test]
    fn input_is_not_empty_with_text() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "hello", None).unwrap();
        assert!(!input.is_empty());
    }

    #[test]
    fn input_set_text_changes_text() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "original", None).unwrap();
        input.set_text("modified".to_string());
        assert_eq!(input.text(), "modified");
    }

    #[test]
    fn input_text_returns_patched_when_set() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "original", None).unwrap();
        input.patched_text = Some("patched".to_string());
        assert_eq!(input.text(), "patched");
    }

    #[test]
    fn input_clear_patch_restores_original() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "original", None).unwrap();
        input.patched_text = Some("patched".to_string());
        input.clear_patch();
        assert_eq!(input.text(), "original");
    }

    #[test]
    fn input_bare_text_ignores_patch() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "original", None).unwrap();
        input.set_patched_text("patched");
        assert_eq!(input.text(), "patched");
        assert_eq!(input.bare_text(), "original");
    }

    #[test]
    fn input_set_continue_output_accumulates() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "test", None).unwrap();
        assert!(input.continue_output().is_none());
        input.set_continue_output("first ");
        assert_eq!(input.continue_output(), Some("first "));
        input.set_continue_output("second");
        assert_eq!(input.continue_output(), Some("first second"));
    }

    #[test]
    fn input_set_regenerate_sets_flag_and_clears_tool_calls() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "test", None).unwrap();
        let role = input.role().clone();
        assert!(!input.regenerate());
        input.set_regenerate(role);
        assert!(input.regenerate());
        assert!(input.tool_calls().is_none());
    }

    #[test]
    fn input_summary_truncates_long_text() {
        let ctx = create_test_ctx();
        let long_text = "a".repeat(200);
        let input = Input::from_str(&ctx, &long_text, None).unwrap();
        let summary = input.summary();
        assert!(summary.len() < 200);
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn input_summary_preserves_short_text() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "short", None).unwrap();
        assert_eq!(input.summary(), "short");
    }

    #[test]
    fn input_raw_with_no_files() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "hello", None).unwrap();
        assert_eq!(input.raw(), "hello");
    }

    #[test]
    fn input_render_with_no_medias() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "hello", None).unwrap();
        assert_eq!(input.render(), "hello");
    }

    #[test]
    fn input_with_agent_false_when_no_agent() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "test", None).unwrap();
        assert!(!input.with_agent());
    }

    #[test]
    fn input_session_returns_none_when_with_session_false() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "test", Some(Role::new("r", "p"))).unwrap();
        let session = Some(Session::default());
        assert!(input.session(&session).is_none());
    }

    #[test]
    fn input_session_returns_some_when_with_session_true() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let input = Input::from_str(&ctx, "test", None).unwrap();
        let session = Some(Session::default());
        assert!(input.session(&session).is_some());
    }

    fn prompt_cache_ttl_of(input: &Input) -> Option<String> {
        input
            .prepare_completion_data(input.role().model(), false)
            .unwrap()
            .prompt_cache_ttl
    }

    #[test]
    fn root_session_turn_requests_the_extended_prompt_cache_ttl() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let input = Input::from_str(&ctx, "test", None).unwrap();
        assert_eq!(prompt_cache_ttl_of(&input).as_deref(), Some("1h"));
    }

    #[test]
    fn sessionless_and_detached_inputs_keep_the_default_prompt_cache_ttl() {
        let mut ctx = create_test_ctx();
        let sessionless = Input::from_str(&ctx, "test", None).unwrap();
        assert_eq!(prompt_cache_ttl_of(&sessionless), None);

        ctx.session = Some(Session::default());
        let detached = Input::detached_with_role(&ctx, "test", Role::new("r", "p"));
        assert_eq!(prompt_cache_ttl_of(&detached), None);
    }

    #[test]
    fn spawned_child_turn_keeps_the_default_prompt_cache_ttl() {
        let parent = create_test_ctx();
        let mut child = RequestContext::new_for_child(
            Arc::clone(&parent.app),
            &parent,
            1,
            Arc::new(crate::supervisor::mailbox::Inbox::new()),
            "agent_test_1".to_string(),
        );
        child.session = Some(Session::default());
        let input = Input::from_str(&child, "test", None).unwrap();
        assert!(input.with_session());
        assert_eq!(prompt_cache_ttl_of(&input), None);
    }

    #[test]
    fn prompt_cache_ttl_knob_set_to_5m_keeps_the_default_ttl() {
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            prompt_cache_ttl: Some("5m".to_string()),
            ..AppConfig::default()
        });
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        ctx.session = Some(Session::default());
        let input = Input::from_str(&ctx, "test", None).unwrap();
        assert_eq!(prompt_cache_ttl_of(&input), None);
    }

    fn structured_spec() -> StructuredOutputSpec {
        StructuredOutputSpec {
            name: "answer".to_string(),
            schema: json!({"type": "object"}),
        }
    }

    fn structured_model(supports_structured_output: bool) -> Model {
        let mut data = ModelData::new("structured");
        data.supports_function_calling = true;
        data.supports_structured_output = supports_structured_output;
        Model::from_config("openai", &[data]).remove(0)
    }

    fn structured_input(
        spec: Option<StructuredOutputSpec>,
        supports_structured_output: bool,
        functions: Option<Vec<FunctionDeclaration>>,
    ) -> Input {
        let mut input = Input::from_str(&create_test_ctx(), "test", None).unwrap();
        input.set_role_model(structured_model(supports_structured_output));
        input.role.set_structured_output(spec);
        input.functions = functions;
        input
    }

    #[test]
    fn prepare_completion_data_gates_structured_output_on_spec_and_model_flag() {
        let cases = [
            (None, true, None),
            (Some(structured_spec()), false, None),
            (Some(structured_spec()), true, Some(structured_spec())),
        ];
        for (spec, flag, expected) in cases {
            for functions in [None, Some(vec![declaration("lookup")])] {
                let input = structured_input(spec.clone(), flag, functions);
                let model = input.role().model().clone();

                let data = input.prepare_completion_data(&model, false).unwrap();
                let baseline = input
                    .without_structured_output()
                    .prepare_completion_data(&model, false)
                    .unwrap();

                assert_eq!(
                    data.structured_output, expected,
                    "spec {spec:?} flag {flag}"
                );
                assert_eq!(
                    format!("{:?}", data.functions),
                    format!("{:?}", baseline.functions)
                );
                assert!(baseline.structured_output.is_none());
            }
        }
    }

    fn structured_clients() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            clients: vec![
                ClientConfig::OpenAIConfig(Default::default()),
                ClientConfig::CohereConfig(Default::default()),
            ],
            ..AppConfig::default()
        })
    }

    #[test]
    fn native_structured_output_requires_spec_model_flag_and_client_dialect() {
        let app_config = structured_clients();
        let client_for = |client_name: &str, flag: bool| {
            let mut data = ModelData::new("structured");
            data.supports_structured_output = flag;
            init_client(
                &app_config,
                Model::from_config(client_name, &[data]).remove(0),
            )
            .unwrap()
        };
        let with_spec = structured_input(Some(structured_spec()), true, None);
        let without_spec = structured_input(None, true, None);

        assert_eq!(
            with_spec.native_structured_output(client_for("openai", true).as_ref()),
            Some(SchemaDialect::OpenAiStrict)
        );
        assert_eq!(
            with_spec.native_structured_output(client_for("openai", false).as_ref()),
            None
        );
        assert_eq!(
            without_spec.native_structured_output(client_for("openai", true).as_ref()),
            None
        );
        assert_eq!(
            with_spec.native_structured_output(client_for("cohere", true).as_ref()),
            None
        );
    }

    #[test]
    fn without_structured_output_clears_only_the_spec() {
        let input = structured_input(Some(structured_spec()), true, Some(vec![declaration("f")]));

        let stripped = input.without_structured_output();

        assert!(stripped.role().structured_output().is_none());
        assert_eq!(input.role().structured_output(), Some(&structured_spec()));
        assert_eq!(stripped.role().model().id(), input.role().model().id());
        assert_eq!(
            format!("{:?}", stripped.functions),
            format!("{:?}", input.functions)
        );
        assert_eq!(
            format!("{:?}", stripped.build_messages().unwrap()),
            format!("{:?}", input.build_messages().unwrap())
        );
        assert_eq!(stripped.text(), input.text());
    }

    #[test]
    fn set_role_model_swaps_model_preserving_with_session() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let mut input = Input::from_str(&ctx, "test", None).unwrap();
        assert!(input.with_session());
        input.set_role_model(Model::new("provider", "compress-model"));
        assert_eq!(input.role().model().id(), "provider:compress-model");
        assert!(input.with_session());
    }

    #[test]
    fn is_supported_image_path_recognizes_image_extensions() {
        assert!(is_supported_image_path("photo.png"));
        assert!(is_supported_image_path("photo.jpeg"));
        assert!(is_supported_image_path("photo.jpg"));
        assert!(is_supported_image_path("photo.webp"));
        assert!(is_supported_image_path("photo.gif"));
        assert!(is_supported_image_path("PHOTO.PNG"));
    }

    #[test]
    fn is_supported_image_path_rejects_non_image_extensions() {
        assert!(!is_supported_image_path("file.txt"));
        assert!(!is_supported_image_path("file.rs"));
        assert!(!is_supported_image_path("file.pdf"));
    }

    #[test]
    fn resolve_data_url_returns_path_for_known_hash() {
        let mut data_urls = HashMap::new();
        let data_url = "data:image/png;base64,abc123";
        let hash = sha256(data_url);
        data_urls.insert(hash, "/path/to/image.png".to_string());
        let result = resolve_data_url(&data_urls, data_url.to_string());
        assert_eq!(result, "/path/to/image.png");
    }

    #[test]
    fn resolve_data_url_returns_original_for_non_data_url() {
        let data_urls = HashMap::new();
        let result = resolve_data_url(&data_urls, "https://example.com/image.png".to_string());
        assert_eq!(result, "https://example.com/image.png");
    }

    fn run_async<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn from_files_loads_single_text_file() {
        let dir = env::temp_dir().join(format!(
            "coyote-input-test-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        fs::write(&file_path, "file content here").unwrap();

        let ctx = create_test_ctx();
        let input = run_async(Input::from_files(
            &ctx,
            "question",
            vec![file_path.to_string_lossy().to_string()],
            None,
        ))
        .unwrap();

        assert!(input.text().contains("file content here"));
        assert!(input.text().contains("question"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_files_strips_image_slots_from_loaded_documents() {
        let dir = env::temp_dir().join(format!(
            "coyote-input-slot-test-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        create_dir_all(&dir).unwrap();
        let file_path = dir.join("doc.txt");
        fs::write(&file_path, format!("before{IMAGE_SLOT}after")).unwrap();

        let ctx = create_test_ctx();
        let input = run_async(Input::from_files(
            &ctx,
            "question",
            vec![file_path.to_string_lossy().to_string()],
            None,
        ))
        .unwrap();

        assert!(input.text().contains("beforeafter"));
        assert!(!input.text().contains(IMAGE_SLOT));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_files_loads_multiple_files() {
        let dir = env::temp_dir().join(format!(
            "coyote-input-test-multi-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "content A").unwrap();
        fs::write(dir.join("b.txt"), "content B").unwrap();

        let ctx = create_test_ctx();
        let input = run_async(Input::from_files(
            &ctx,
            "question",
            vec![
                dir.join("a.txt").to_string_lossy().to_string(),
                dir.join("b.txt").to_string_lossy().to_string(),
            ],
            None,
        ))
        .unwrap();

        assert!(input.text().contains("content A"));
        assert!(input.text().contains("content B"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_files_with_no_paths_just_text() {
        let ctx = create_test_ctx();
        let input = run_async(Input::from_files(&ctx, "just text", vec![], None)).unwrap();
        assert_eq!(input.text(), "just text");
    }

    #[test]
    fn from_files_rejects_text_file_named_as_image() {
        let dir = env::temp_dir().join(format!(
            "coyote-input-test-fake-image-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        create_dir_all(&dir).unwrap();
        let file_path = dir.join("x.png");
        fs::write(&file_path, "not an image").unwrap();

        let ctx = create_test_ctx();
        let err = run_async(Input::from_files(
            &ctx,
            "question",
            vec![file_path.to_string_lossy().to_string()],
            None,
        ))
        .unwrap_err();

        assert!(format!("{err:#}").contains(&file_path.display().to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_files_with_external_command() {
        let ctx = create_test_ctx();
        let input = run_async(Input::from_files(
            &ctx,
            "question",
            vec!["`echo hello from cmd`".to_string()],
            None,
        ))
        .unwrap();
        assert!(input.text().contains("hello from cmd"));
    }

    #[test]
    fn from_files_nonexistent_file_errors() {
        let ctx = create_test_ctx();
        let result = run_async(Input::from_files(
            &ctx,
            "question",
            vec!["/nonexistent/path/xyz.txt".to_string()],
            None,
        ));
        assert!(result.is_err());
    }

    fn tool_result(id: &str, output: &str) -> ToolResult {
        ToolResult::new(
            ToolCall::new("t".into(), json!({}), Some(id.to_string())),
            json!(output),
        )
    }

    #[test]
    fn merge_tool_results_first_merge_creates_container() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "test", None).unwrap();

        let input =
            input.merge_tool_results("assistant text".into(), vec![tool_result("id-1", "ok")]);

        let tool_calls = input.tool_calls().as_ref().unwrap();
        assert_eq!(tool_calls.text, "assistant text");
        assert!(!tool_calls.sequence);
        assert_eq!(tool_calls.tool_results.len(), 1);
        assert!(tool_calls.tool_results[0].text.is_none());
        assert!(tool_calls.round_starts.is_empty());
    }

    #[test]
    fn merge_tool_results_second_merge_marks_sequence_and_tags_text() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "test", None)
            .unwrap()
            .merge_tool_results("assistant text".into(), vec![tool_result("id-1", "ok")]);

        let input =
            input.merge_tool_results("second text".into(), vec![tool_result("id-2", "ok2")]);

        let tool_calls = input.tool_calls().as_ref().unwrap();
        assert!(tool_calls.sequence);
        assert_eq!(tool_calls.tool_results.len(), 2);
        assert_eq!(tool_calls.text, "assistant text");
        assert!(tool_calls.tool_results[0].text.is_none());
        assert_eq!(
            tool_calls.tool_results[1].text,
            Some("second text".to_string())
        );
        assert_eq!(tool_calls.round_starts, vec![1]);
    }

    /// Regression: rounds where the model emitted only tool calls (no
    /// narration text) carry no text marker, so `round_starts` is the only
    /// record of the round boundary — it must be set on every merge.
    #[test]
    fn merge_tool_results_records_round_starts_for_textless_rounds() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "test", None)
            .unwrap()
            .merge_tool_results(String::new(), vec![tool_result("id-1", "ok")])
            .merge_tool_results(
                String::new(),
                vec![tool_result("id-2", "ok2"), tool_result("id-3", "ok3")],
            )
            .merge_tool_results(String::new(), vec![tool_result("id-4", "ok4")]);

        let tool_calls = input.tool_calls().as_ref().unwrap();
        assert!(tool_calls.sequence);
        assert_eq!(tool_calls.tool_results.len(), 4);
        assert!(tool_calls.tool_results.iter().all(|t| t.text.is_none()));
        assert_eq!(tool_calls.round_starts, vec![1, 3]);
    }

    #[test]
    fn build_messages_wraps_tool_results_in_single_assistant_message() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "test", None)
            .unwrap()
            .merge_tool_results("assistant text".into(), vec![tool_result("id-1", "ok")])
            .merge_tool_results("second text".into(), vec![tool_result("id-2", "ok2")]);

        let messages = input.build_messages().unwrap();

        let tool_call_messages: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m.content, MessageContent::ToolCalls(_)))
            .collect();
        assert_eq!(tool_call_messages.len(), 1);
        let message = tool_call_messages[0];
        assert!(matches!(message.role, MessageRole::Assistant));
        let MessageContent::ToolCalls(tool_calls) = &message.content else {
            unreachable!();
        };
        assert_eq!(tool_calls.tool_results.len(), 2);
    }

    #[test]
    fn into_checkpoint_continuation_resets_turn_state_keeps_role_session_functions() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let mut input = Input::from_str(&ctx, "do things", None)
            .unwrap()
            .merge_tool_results("round one".into(), vec![tool_result("id-1", "ok")]);
        input.patched_text = Some("patched".into());
        input.rag_name = Some("docs".into());
        input.medias.push("data:image/png;base64,abc".into());
        input.data_urls.insert("hash".into(), "/img.png".into());
        input.last_reply = Some("earlier".into());
        let role_name = input.role().name().to_string();
        let functions = input.declared_function_names();

        let earlier = Input::from_str(&ctx, "earlier", None).unwrap();
        ctx.session
            .as_mut()
            .unwrap()
            .add_message(&earlier, "answer")
            .unwrap();

        let next = input.into_checkpoint_continuation(&ctx);

        assert!(next.tool_calls().is_none());
        assert_eq!(next.text(), TOOL_LOOP_CONTINUE_TEXT);
        assert_eq!(next.raw(), TOOL_LOOP_CONTINUE_TEXT);
        assert!(next.patched_text.is_none());
        assert!(next.rag_name().is_none());
        assert!(next.medias.is_empty());
        assert!(next.data_urls().is_empty());
        assert!(next.last_reply.is_none());
        assert!(next.continue_output().is_none());
        assert!(!next.regenerate());
        assert_eq!(next.role().name(), role_name);
        assert_eq!(next.declared_function_names(), functions);
        assert!(next.with_session());
        assert_eq!(
            next.session.as_ref().unwrap().messages().len(),
            ctx.session.as_ref().unwrap().messages().len(),
            "the continuation must carry the ctx session, not the stale snapshot"
        );
    }

    #[test]
    fn clear_continuation_drops_continue_output_and_regenerate_only() {
        let ctx = create_test_ctx();
        let role = ctx.extract_role(&ctx.app.config).unwrap();
        let mut input = Input::from_str(&ctx, "go on", Some(role.clone())).unwrap();
        input.set_continue_output("the first half");
        input.set_regenerate(role);
        input.set_patched_text("go on (with retrieved context)");
        assert!(input.continue_output().is_some());
        assert!(input.regenerate());

        input.clear_continuation();

        assert!(input.continue_output().is_none());
        assert!(!input.regenerate());
        assert_eq!(input.bare_text(), "go on");
        assert_eq!(
            input.patched_text.as_deref(),
            Some("go on (with retrieved context)")
        );
    }

    fn declaration(name: &str) -> FunctionDeclaration {
        FunctionDeclaration {
            name: name.to_string(),
            description: String::new(),
            parameters: Default::default(),
            agent: false,
        }
    }

    fn session_texts(ctx: &RequestContext) -> Vec<(bool, String)> {
        ctx.session
            .as_ref()
            .unwrap()
            .messages()
            .iter()
            .map(|m| (m.role.is_user(), m.content.to_text()))
            .collect()
    }

    #[test]
    fn attached_with_role_keeps_session_and_given_functions_where_from_str_detaches() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());

        let attached = Input::attached_with_role(
            &ctx,
            "hi",
            Role::new("r", ""),
            Some(vec![declaration("fs_read")]),
        )
        .unwrap();
        assert!(attached.with_session());
        assert_eq!(
            attached.declared_function_names(),
            HashSet::from(["fs_read".to_string()])
        );
        assert_eq!(attached.role().name(), "r");

        let detached = Input::from_str(&ctx, "hi", Some(Role::new("r", ""))).unwrap();
        assert!(!detached.with_session());
    }

    #[test]
    fn attached_with_role_exchange_is_saved_into_the_session() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let app = ctx.app.config.clone();
        let input = Input::attached_with_role(&ctx, "hi", Role::new("r", ""), None).unwrap();

        ctx.save_message(&app, &input, "answer").unwrap();

        assert_eq!(
            session_texts(&ctx),
            vec![(true, "hi".to_string()), (false, "answer".to_string())]
        );
    }

    #[test]
    fn request_preamble_prefixes_the_request_but_never_the_stored_or_displayed_text() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let app = ctx.app.config.clone();
        let mut input = Input::attached_with_role(&ctx, "hi", Role::new("r", ""), None).unwrap();
        input.set_request_preamble("Last graph run\n");

        assert_eq!(
            input.message_content().to_text(),
            "Last graph run\n\nhi",
            "a rendered preamble's trailing newline must not widen the gap"
        );
        assert_eq!(input.text(), "hi");
        assert_eq!(input.raw(), "hi");
        assert_eq!(input.summary(), "hi");
        assert_eq!(input.render(), "hi");

        ctx.save_message(&app, &input, "answer").unwrap();
        assert_eq!(
            session_texts(&ctx),
            vec![(true, "hi".to_string()), (false, "answer".to_string())]
        );
    }

    #[test]
    fn request_preamble_is_emitted_as_a_text_part_even_without_typed_text() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "", None).unwrap();
        input.medias.push("data:image/png;base64,abc".into());
        input.set_request_preamble("Last graph run");

        let MessageContent::Array(parts) = input.message_content() else {
            panic!("multimodal input must build an array");
        };
        assert_eq!(parts.len(), 2);
        assert!(matches!(
            &parts[0],
            MessageContentPart::Text { text } if text.starts_with("Last graph run")
        ));
    }

    #[test]
    fn text_with_preamble_is_the_text_part_of_the_request() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "hi", None).unwrap();
        input.set_patched_text("hi (with retrieved context)");
        assert_eq!(
            input.text_with_preamble(),
            input.message_content().to_text()
        );
        assert_eq!(input.text_with_preamble(), "hi (with retrieved context)");

        input.set_request_preamble("Last graph run\n");
        assert_eq!(
            input.text_with_preamble(),
            input.message_content().to_text()
        );
        assert_eq!(
            input.text_with_preamble(),
            "Last graph run\n\nhi (with retrieved context)"
        );

        input.medias.push("data:image/png;base64,abc".into());
        let MessageContent::Array(parts) = input.message_content() else {
            panic!("multimodal input must build an array");
        };
        assert!(matches!(
            &parts[0],
            MessageContentPart::Text { text } if *text == input.text_with_preamble()
        ));
    }

    #[test]
    fn into_checkpoint_continuation_drops_request_preamble() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let mut input = Input::attached_with_role(&ctx, "hi", Role::new("r", ""), None).unwrap();
        input.set_request_preamble("Last graph run");

        let next = input.into_checkpoint_continuation(&ctx);

        assert_eq!(next.message_content().to_text(), TOOL_LOOP_CONTINUE_TEXT);
    }

    #[test]
    fn carry_patch_from_copies_the_patch_rag_name_files_medias_and_last_reply() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let mut source = Input::from_str(&ctx, "hi", None).unwrap();
        source.set_patched_text("hi (with retrieved context)");
        source.rag_name = Some("docs".into());
        source.raw = ("hi".into(), vec!["/img.png".into(), "%%".into()]);
        source.medias.push("data:image/png;base64,abc".into());
        source
            .data_urls
            .insert(sha256("data:image/png;base64,abc"), "/img.png".into());
        source.last_reply = Some("earlier".into());
        source.set_request_preamble("source preamble");

        let mut input = Input::attached_with_role(&ctx, "hi", Role::new("r", ""), None).unwrap();
        input.set_request_preamble("builder preamble");
        input.carry_patch_from(&source);

        assert_eq!(input.text(), source.text());
        assert_eq!(input.bare_text(), source.bare_text());
        assert_eq!(input.rag_name(), Some("docs"));
        assert_eq!(input.raw(), ".file /img.png %% -- hi");
        assert_eq!(input.medias(), source.medias());
        assert_eq!(input.data_urls(), source.data_urls());
        assert_eq!(
            input.render(),
            ".file /img.png -- hi (with retrieved context)"
        );
        assert_eq!(input.last_reply(), Some("earlier"));
        assert_eq!(input.request_preamble.as_deref(), Some("builder preamble"));

        input.clear_patch();
        assert_eq!(input.text(), "hi");
        assert!(input.request_preamble.is_none());
    }

    #[test]
    fn carry_patch_from_copies_the_continue_output() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let mut source = Input::from_str(&ctx, "hi", None).unwrap();
        source.set_continue_output("partial");

        let mut input = Input::attached_with_role(&ctx, "hi", Role::new("r", ""), None).unwrap();
        assert!(input.continue_output().is_none());
        input.carry_patch_from(&source);

        assert_eq!(input.continue_output(), Some("partial"));
    }

    fn model_with_vision(supports_vision: bool) -> Model {
        let mut data = ModelData::new("eyes");
        data.supports_vision = supports_vision;
        Model::from_config("openai", &[data]).remove(0)
    }

    fn input_with_images(supports_vision: bool, medias: Vec<String>) -> Input {
        let mut input = Input::from_str(&create_test_ctx(), "look", None).unwrap();
        input.set_role_model(model_with_vision(supports_vision));
        input.medias = medias;
        input
    }

    fn png_data_url(base64_len: usize) -> String {
        format!("data:image/png;base64,{}", "A".repeat(base64_len))
    }

    /// Text parts as their text and image parts as `image:<url>`.
    fn part_list(content: MessageContent) -> Vec<String> {
        let MessageContent::Array(parts) = content else {
            panic!("expected a part list: {content:?}");
        };
        parts
            .into_iter()
            .map(|part| match part {
                MessageContentPart::Text { text } => text,
                MessageContentPart::ImageUrl { image_url } => format!("image:{}", image_url.url),
            })
            .collect()
    }

    fn input_with_text_and_images(text: &str, medias: Vec<String>) -> Input {
        let mut input = input_with_images(true, medias);
        input.text = text.into();
        input
    }

    #[test]
    fn message_content_places_each_image_at_its_slot() {
        let (one, two) = (png_data_url(1), png_data_url(2));
        let input = input_with_text_and_images(
            &format!("Something here {IMAGE_SLOT} Something else {IMAGE_SLOT}"),
            vec![one.clone(), two.clone()],
        );

        assert_eq!(
            part_list(input.message_content()),
            vec![
                "Something here".to_string(),
                format!("image:{one}"),
                "Something else".to_string(),
                format!("image:{two}"),
            ]
        );
    }

    #[test]
    fn message_content_appends_images_without_a_slot() {
        let (one, two) = (png_data_url(1), png_data_url(2));
        let input = input_with_text_and_images(
            &format!("see {IMAGE_SLOT} then"),
            vec![one.clone(), two.clone()],
        );

        assert_eq!(
            part_list(input.message_content()),
            vec![
                "see".to_string(),
                format!("image:{one}"),
                "then".to_string(),
                format!("image:{two}"),
            ]
        );
    }

    #[test]
    fn message_content_drops_slots_beyond_the_images() {
        let one = png_data_url(1);
        let input = input_with_text_and_images(
            &format!("{IMAGE_SLOT} a{IMAGE_SLOT} here"),
            vec![one.clone()],
        );

        assert_eq!(
            part_list(input.message_content()),
            vec![format!("image:{one}"), "a here".to_string()]
        );
    }

    #[test]
    fn message_content_keeps_typed_markers_as_text() {
        let one = png_data_url(1);
        let input =
            input_with_text_and_images(&format!("[Image #1] a {IMAGE_SLOT} b"), vec![one.clone()]);

        assert_eq!(
            part_list(input.message_content()),
            vec![
                "[Image #1] a".to_string(),
                format!("image:{one}"),
                "b".to_string()
            ]
        );
    }

    #[test]
    fn message_content_with_only_slots_has_no_text_parts() {
        let one = png_data_url(1);
        let input = input_with_text_and_images(&format!(" {IMAGE_SLOT} "), vec![one.clone()]);

        assert_eq!(
            part_list(input.message_content()),
            vec![format!("image:{one}")]
        );
    }

    #[test]
    fn message_content_without_images_drops_stray_slots() {
        let input = input_with_text_and_images(&format!("a{IMAGE_SLOT}b"), vec![]);

        assert!(matches!(input.message_content(), MessageContent::Text(text) if text == "ab"));
    }

    #[test]
    fn preamble_and_retrieved_text_cannot_move_the_users_image() {
        let one = png_data_url(1);
        let mut input =
            input_with_text_and_images(&format!("look {IMAGE_SLOT} here"), vec![one.clone()]);
        input.set_request_preamble(format!("run said [Image #1] and {IMAGE_SLOT}"));

        assert_eq!(
            part_list(input.message_content()),
            vec![
                "run said [Image #1] and\n\nlook".to_string(),
                format!("image:{one}"),
                "here".to_string(),
            ]
        );
    }

    #[test]
    fn display_shows_each_slot_as_its_marker() {
        let one = png_data_url(1);
        let mut input =
            input_with_text_and_images(&format!("see {IMAGE_SLOT} now"), vec![one.clone()]);
        input.raw = (format!("see {IMAGE_SLOT} now"), vec!["/a.png".into()]);

        assert_eq!(input.summary(), "see [Image #1] now");
        assert_eq!(input.raw(), ".file /a.png -- see [Image #1] now");
        assert_eq!(input.raw_without_image_slots(), ".file /a.png -- see  now");
        assert!(
            input.render().ends_with(" -- see [Image #1] now"),
            "{}",
            input.render()
        );
    }

    #[test]
    fn display_drops_slots_past_the_images() {
        let one = png_data_url(1);
        let text = format!("see {IMAGE_SLOT} and {IMAGE_SLOT} now");
        let mut input = input_with_text_and_images(&text, vec![one.clone()]);
        input.raw = (text, vec![]);

        assert_eq!(input.summary(), "see [Image #1] and  now");
        assert_eq!(input.raw(), "see [Image #1] and  now");
        assert!(
            input.render().ends_with(" -- see [Image #1] and  now"),
            "{}",
            input.render()
        );
    }

    #[test]
    fn image_only_text_has_nothing_to_search() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "", None).unwrap();
        input.text = format!("{IMAGE_SLOT} {IMAGE_SLOT}");
        assert!(!input.has_search_text());
        input.text = format!("{IMAGE_SLOT} what is this?");
        assert!(input.has_search_text());
    }

    #[test]
    fn typed_image_slots_are_stripped_at_every_constructor() {
        let ctx = create_test_ctx();
        let typed = format!("a{IMAGE_SLOT}b");

        let from_str = Input::from_str(&ctx, &typed, None).unwrap();
        let attached = Input::attached_with_role(&ctx, &typed, Role::new("", ""), None).unwrap();
        let detached = Input::detached_with_role(&ctx, &typed, Role::new("", ""));
        let from_files = run_async(Input::from_files(&ctx, &typed, vec![], None)).unwrap();

        for input in [from_str, attached, detached, from_files] {
            assert_eq!(input.text(), "ab");
            assert_eq!(input.raw(), "ab");
        }
    }

    #[test]
    fn set_text_strips_image_slots() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "before", None).unwrap();

        input.set_text(format!("a{IMAGE_SLOT}b"));

        assert_eq!(input.text(), "ab");
    }

    #[test]
    fn only_the_image_route_constructor_keeps_slots() {
        let ctx = create_test_ctx();
        let text = format!("a{IMAGE_SLOT}b");

        let input = run_async(Input::from_files_with_image_slots(
            &ctx,
            &text,
            vec![],
            None,
        ))
        .unwrap();

        assert_eq!(input.text(), text);
    }

    #[test]
    fn a_lone_typed_slot_is_empty_input() {
        let input = Input::from_str(&create_test_ctx(), &IMAGE_SLOT.to_string(), None).unwrap();

        assert!(input.is_empty());
    }

    #[test]
    fn file_images_still_follow_the_text() {
        let dir = env::temp_dir().join(format!(
            "coyote-input-test-file-image-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        create_dir_all(&dir).unwrap();
        let shot = dir.join("a.png");
        image::DynamicImage::new_rgba8(2, 2)
            .save_with_format(&shot, image::ImageFormat::Png)
            .unwrap();
        let ctx = create_test_ctx();

        let input = run_async(Input::from_files(
            &ctx,
            "hi",
            vec![shot.to_string_lossy().to_string()],
            None,
        ))
        .unwrap();

        let parts = part_list(input.message_content());
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "hi");
        assert!(parts[1].starts_with("image:data:image/png"), "{parts:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn guard_images_allows_text_only_on_non_vision_model() {
        input_with_images(false, vec![]).guard_images().unwrap();
    }

    #[test]
    fn guard_images_refuses_images_on_non_vision_model() {
        let err = input_with_images(false, vec![png_data_url(8)])
            .guard_images()
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            "The model 'openai:eyes' does not accept images. Switch to a vision-capable model, or set 'supports_vision: true' on this model in your config if it does support them."
        );
    }

    #[test]
    fn guard_images_allows_the_maximum_image_count() {
        let medias = vec![png_data_url(8); MAX_REQUEST_IMAGES];
        input_with_images(true, medias).guard_images().unwrap();
    }

    #[test]
    fn guard_images_refuses_too_many_images() {
        let medias = vec![png_data_url(8); MAX_REQUEST_IMAGES + 1];
        let err = input_with_images(true, medias).guard_images().unwrap_err();

        assert_eq!(
            err.to_string(),
            "This message carries 21 images (0.0 MB); the limit is 20 images and 20 MB per request."
        );
    }

    #[test]
    fn guard_images_refuses_oversized_base64_total() {
        let half = MAX_REQUEST_IMAGE_BASE64_BYTES / 2;
        let medias = vec![png_data_url(half), png_data_url(half + 1)];
        let err = input_with_images(true, medias).guard_images().unwrap_err();

        assert_eq!(
            err.to_string(),
            "This message carries 2 images (20.0 MB); the limit is 20 images and 20 MB per request."
        );
    }

    #[test]
    fn prepare_completion_data_refuses_images_for_a_non_vision_model() {
        let input = input_with_images(true, vec![png_data_url(8)]);

        let err = input
            .prepare_completion_data(&model_with_vision(false), false)
            .unwrap_err();

        assert!(err.to_string().contains("does not accept images"), "{err}");
    }

    const OVER_BUDGET_NOTE: &str = "older image dropped to stay within the request size limit";

    fn image_part(base64_len: usize) -> MessageContentPart {
        MessageContentPart::ImageUrl {
            image_url: ImageUrl {
                url: png_data_url(base64_len),
            },
        }
    }

    fn user_images(count: usize, base64_len: usize) -> Message {
        let mut parts = vec![MessageContentPart::Text {
            text: "see these".into(),
        }];
        parts.extend((0..count).map(|_| image_part(base64_len)));
        Message::new(MessageRole::User, MessageContent::Array(parts))
    }

    fn attachment(source: &str, base64_len: usize) -> ImageAttachment {
        ImageAttachment {
            source: source.into(),
            mime_type: "image/png".into(),
            data: "A".repeat(base64_len),
        }
    }

    /// One tool result per entry of `per_result`, each carrying that many
    /// attachments labelled `<label>-<result>-<index>`.
    fn tool_images(
        label: &str,
        per_result: &[usize],
        base64_len: usize,
    ) -> MessageContentToolCalls {
        let results = per_result
            .iter()
            .enumerate()
            .map(|(r, &count)| {
                ToolResult::new(
                    ToolCall::new("fs_read".into(), json!({}), Some(format!("{label}-{r}"))),
                    json!("read"),
                )
                .with_attachments(
                    (0..count)
                        .map(|i| attachment(&format!("{label}-{r}-{i}"), base64_len))
                        .collect(),
                )
            })
            .collect();
        MessageContentToolCalls::new(results, String::new())
    }

    fn tool_message(tool_calls: MessageContentToolCalls) -> Message {
        Message::new(
            MessageRole::Assistant,
            MessageContent::ToolCalls(tool_calls),
        )
    }

    fn text_message(text: &str) -> Message {
        Message::new(MessageRole::User, MessageContent::Text(text.into()))
    }

    fn image_count(message: &Message) -> usize {
        match &message.content {
            MessageContent::Array(parts) => parts
                .iter()
                .filter(|part| matches!(part, MessageContentPart::ImageUrl { .. }))
                .count(),
            MessageContent::ToolCalls(tool_calls) => tool_calls
                .tool_results
                .iter()
                .map(|result| result.attachments.len())
                .sum(),
            MessageContent::Text(_) => 0,
        }
    }

    fn last_text_part(message: &Message) -> &str {
        match &message.content {
            MessageContent::Array(parts) => match parts.last() {
                Some(MessageContentPart::Text { text }) => text,
                other => panic!("last part is not text: {other:?}"),
            },
            other => panic!("not a part list: {other:?}"),
        }
    }

    fn tool_results(message: &Message) -> &[ToolResult] {
        match &message.content {
            MessageContent::ToolCalls(tool_calls) => &tool_calls.tool_results,
            other => panic!("not a tool message: {other:?}"),
        }
    }

    fn as_json(messages: &[Message]) -> serde_json::Value {
        serde_json::to_value(messages).unwrap()
    }

    #[test]
    fn limit_outgoing_images_drops_the_oldest_ten_of_twenty_five() {
        let mut messages = vec![
            user_images(3, 8),
            tool_message(tool_images("b", &[4], 8)),
            user_images(8, 8),
            tool_message(tool_images("d", &[5, 5], 8)),
            text_message("and now?"),
        ];
        let stored = messages.clone();

        limit_outgoing_images(&mut messages, &model_with_vision(true), Some(4));

        assert_eq!(image_count(&messages[0]), 0);
        assert_eq!(
            last_text_part(&messages[0]),
            format!("[3 image(s) omitted: {OVER_BUDGET_NOTE}]")
        );
        let emptied = &tool_results(&messages[1])[0];
        assert!(emptied.attachments.is_empty());
        assert_eq!(
            emptied.output,
            json!({
                "result": "read",
                "images_omitted": format!("4 image(s) from this tool were omitted: {OVER_BUDGET_NOTE}"),
            })
        );
        assert_eq!(image_count(&messages[2]), 5);
        assert_eq!(
            last_text_part(&messages[2]),
            format!("[3 image(s) omitted: {OVER_BUDGET_NOTE}]")
        );
        assert_eq!(as_json(&messages[3..]), as_json(&stored[3..]));
        assert_eq!(messages.iter().map(image_count).sum::<usize>(), 15);
    }

    /// `n` user turns carrying one image each, then a text-only current turn.
    fn image_turns(n: usize, base64_len: usize) -> Vec<Message> {
        let mut messages: Vec<_> = (0..n)
            .map(|i| {
                Message::new(
                    MessageRole::User,
                    MessageContent::Array(vec![
                        MessageContentPart::Text {
                            text: format!("turn {i}"),
                        },
                        image_part(base64_len),
                    ]),
                )
            })
            .collect();
        messages.push(text_message("and now?"));
        messages
    }

    fn dropped_turns(messages: &[Message]) -> usize {
        messages
            .iter()
            .take_while(|message| {
                matches!(&message.content, MessageContent::Array(_)) && image_count(message) == 0
            })
            .count()
    }

    #[test]
    fn limit_outgoing_images_drops_the_oldest_in_blocks_of_ten() {
        for (n, expected_dropped) in [(20, 0), (21, 10), (25, 10), (30, 10), (31, 20)] {
            let mut messages = image_turns(n, 8);

            limit_outgoing_images(&mut messages, &model_with_vision(true), Some(n));

            assert_eq!(dropped_turns(&messages), expected_dropped, "n = {n}");
            assert_eq!(
                messages.iter().map(image_count).sum::<usize>(),
                n - expected_dropped,
                "n = {n}"
            );
        }
    }

    #[test]
    fn limit_outgoing_images_keeps_the_prefix_stable_as_images_are_added() {
        let mut fewer = image_turns(21, 8);
        let mut more = image_turns(22, 8);

        limit_outgoing_images(&mut fewer, &model_with_vision(true), Some(21));
        limit_outgoing_images(&mut more, &model_with_vision(true), Some(22));

        assert_eq!(image_count(&fewer[10]), 1);
        assert_eq!(
            serde_json::to_string(&fewer[..11]).unwrap(),
            serde_json::to_string(&more[..11]).unwrap()
        );
    }

    #[test]
    fn limit_outgoing_images_drops_the_oldest_in_byte_blocks() {
        let two_mb = 2 * 1024 * 1024;
        for (n, expected_dropped) in [(10, 0), (11, 5), (15, 5), (16, 10)] {
            let mut messages = image_turns(n, two_mb);

            limit_outgoing_images(&mut messages, &model_with_vision(true), Some(n));

            assert_eq!(dropped_turns(&messages), expected_dropped, "n = {n}");
            assert_eq!(
                messages.iter().map(image_count).sum::<usize>(),
                n - expected_dropped,
                "n = {n}"
            );
        }
    }

    #[test]
    fn limit_outgoing_images_counts_interleaved_parts_in_order() {
        let interleaved = Message::new(
            MessageRole::User,
            MessageContent::Array(vec![
                image_part(8),
                MessageContentPart::Text {
                    text: "between".into(),
                },
                image_part(8),
                image_part(8),
            ]),
        );
        let mut messages = vec![
            user_images(9, 8),
            interleaved,
            user_images(9, 8),
            text_message("and now?"),
        ];

        limit_outgoing_images(&mut messages, &model_with_vision(true), Some(3));

        assert_eq!(image_count(&messages[0]), 0);
        let MessageContent::Array(parts) = &messages[1].content else {
            panic!("{:?}", messages[1].content);
        };
        let shape: Vec<_> = parts
            .iter()
            .map(|part| match part {
                MessageContentPart::Text { text } => text.as_str(),
                MessageContentPart::ImageUrl { .. } => "image",
            })
            .collect();
        let note = format!("[1 image(s) omitted: {OVER_BUDGET_NOTE}]");
        assert_eq!(shape, ["between", "image", "image", note.as_str()]);
        assert_eq!(image_count(&messages[2]), 9);
    }

    #[test]
    fn limit_outgoing_images_within_budget_leaves_messages_unchanged() {
        let mut messages = vec![
            user_images(2, 64),
            tool_message(tool_images("b", &[2, 1], 64)),
            user_images(1, 64),
        ];
        let stored = messages.clone();

        limit_outgoing_images(&mut messages, &model_with_vision(true), Some(2));

        assert_eq!(as_json(&messages), as_json(&stored));
    }

    #[test]
    fn limit_outgoing_images_strips_every_history_image_for_a_non_vision_model() {
        let image_only = Message::new(
            MessageRole::User,
            MessageContent::Array(vec![image_part(8), image_part(8)]),
        );
        let mut messages = vec![
            image_only,
            tool_message(tool_images("b", &[1], 8)),
            text_message("and now?"),
        ];

        limit_outgoing_images(&mut messages, &model_with_vision(false), Some(2));

        let MessageContent::Array(parts) = &messages[0].content else {
            panic!("{:?}", messages[0].content);
        };
        assert_eq!(parts.len(), 1);
        assert_eq!(
            last_text_part(&messages[0]),
            "[2 image(s) omitted: the current model does not accept images]"
        );
        let result = &tool_results(&messages[1])[0];
        assert!(result.attachments.is_empty());
        assert_eq!(
            result.output,
            json!({
                "result": "read",
                "images_omitted": "1 image(s) from this tool were omitted: the current model does not accept images",
            })
        );
    }

    #[test]
    fn limit_outgoing_images_never_drops_the_current_message_images() {
        let mut messages = vec![
            tool_message(tool_images("b", &[1], 8)),
            user_images(MAX_REQUEST_IMAGES, 8),
        ];

        limit_outgoing_images(&mut messages, &model_with_vision(true), Some(1));

        assert_eq!(image_count(&messages[0]), 0);
        assert_eq!(image_count(&messages[1]), MAX_REQUEST_IMAGES);
    }

    #[test]
    fn limit_outgoing_images_keeps_the_current_message_images_when_history_overflows() {
        let mut messages = image_turns(25, 8);
        messages.pop();
        messages.push(user_images(5, 8));

        limit_outgoing_images(&mut messages, &model_with_vision(true), Some(25));

        assert_eq!(dropped_turns(&messages), 10);
        assert_eq!(image_count(&messages[25]), 5);
    }

    #[test]
    fn prepare_completion_data_strips_tool_attachments_for_a_non_vision_model() {
        let mut input = input_with_images(false, vec![]);
        input.tool_calls = Some(tool_images("b", &[1], 8));
        let stored = serde_json::to_value(&input.tool_calls).unwrap();

        let data = input
            .prepare_completion_data(&model_with_vision(false), false)
            .unwrap();

        let result = &tool_results(data.messages.last().unwrap())[0];
        assert!(result.attachments.is_empty());
        assert_eq!(result.output["result"], json!("read"));
        assert!(result.output["images_omitted"].is_string());
        assert_eq!(serde_json::to_value(&input.tool_calls).unwrap(), stored);
    }
}
