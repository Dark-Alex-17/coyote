use super::*;

use crate::client::{
    init_client, patch_messages, ChatCompletionsData, Client, ImageUrl, Message,
    MessageContent, MessageContentPart, MessageContentToolCalls, MessageRole, Model,
};
use crate::function::ToolResult;
use crate::utils::{base64_encode, is_loader_protocol, sha256, AbortSignal};

use anyhow::{bail, Context, Result};
use indexmap::IndexSet;
use std::{collections::HashMap, fs::File, io::Read, sync::Arc};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const IMAGE_EXTS: [&str; 5] = ["png", "jpeg", "jpg", "webp", "gif"];
const SUMMARY_MAX_WIDTH: usize = 80;

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
}

impl Input {
    pub fn from_str(ctx: &RequestContext, text: &str, role: Option<Role>) -> Self {
        let (role, with_session, with_agent) = resolve_role(ctx, role);
        let captured = capture_input_config(ctx, &role);
        Self {
            app_config: Arc::clone(&ctx.app.config),
            stream_enabled: captured.stream_enabled,
            session: captured.session,
            rag: captured.rag,
            functions: captured.functions,
            text: text.to_string(),
            raw: (text.to_string(), vec![]),
            patched_text: None,
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
        }
    }

    pub async fn from_files(
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
                    texts.push(format!("\n{v}"));
                }
            }
            if last_reply.is_none() && documents.is_empty() && medias.is_empty() {
                bail!("No last reply found");
            }
        }
        let documents_len = documents.len();
        for (kind, path, contents) in documents {
            if documents_len == 1 && raw_text.is_empty() {
                texts.push(format!("\n{contents}"));
            } else {
                texts.push(format!(
                    "\n============ {kind}: {path} ============\n{contents}"
                ));
            }
        }
        let (role, with_session, with_agent) = resolve_role(ctx, role);
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
        )
        .await
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.medias.is_empty()
    }

    pub fn data_urls(&self) -> HashMap<String, String> {
        self.data_urls.clone()
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

    pub fn clear_patch(&mut self) {
        self.patched_text = None;
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
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
        if self.text.is_empty() {
            return Ok(());
        }
        if let Some(rag) = &self.rag {
            let result = rag
                .search_with_template(&self.app_config, &self.text, abort_signal)
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
        let mut messages = self.build_messages()?;
        patch_messages(&mut messages, model);
        model.guard_max_input_tokens(&messages)?;
        let (temperature, top_p) = (self.role().temperature(), self.role().top_p());
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
        Ok(ChatCompletionsData {
            messages,
            temperature,
            top_p,
            functions,
            stream,
        })
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

    pub fn session<'a>(&self, session: &'a Option<Session>) -> Option<&'a Session> {
        if self.with_session {
            session.as_ref()
        } else {
            None
        }
    }

    pub fn session_mut<'a>(&self, session: &'a mut Option<Session>) -> Option<&'a mut Session> {
        if self.with_session {
            session.as_mut()
        } else {
            None
        }
    }

    pub fn with_agent(&self) -> bool {
        self.with_agent
    }

    pub fn summary(&self) -> String {
        let text: String = self
            .text
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
        let (text, files) = &self.raw;
        let mut segments = files.to_vec();
        if !segments.is_empty() {
            segments.insert(0, ".file".into());
        }
        if !text.is_empty() {
            if !segments.is_empty() {
                segments.push("--".into());
            }
            segments.push(text.clone());
        }
        segments.join(" ")
    }

    pub fn render(&self) -> String {
        let text = self.text();
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

    pub fn message_content(&self) -> MessageContent {
        if self.medias.is_empty() {
            MessageContent::Text(self.text())
        } else {
            let mut list: Vec<MessageContentPart> = self
                .medias
                .iter()
                .cloned()
                .map(|url| MessageContentPart::ImageUrl {
                    image_url: ImageUrl { url },
                })
                .collect();
            if !self.text.is_empty() {
                list.insert(0, MessageContentPart::Text { text: self.text() });
            }
            MessageContent::Array(list)
        }
    }
}

fn resolve_role(ctx: &RequestContext, role: Option<Role>) -> (Role, bool, bool) {
    match role {
        Some(v) => (v, false, false),
        None => (
            ctx.extract_role(ctx.app.config.as_ref()),
            ctx.session.is_some(),
            ctx.agent.is_some(),
        ),
    }
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
        if is_image(&file_path) {
            let contents = read_media_to_data_url(&file_path)
                .with_context(|| format!("Unable to read media '{file_path}'"))?;
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

fn is_image(path: &str) -> bool {
    get_patch_extension(path)
        .map(|v| IMAGE_EXTS.contains(&v.as_str()))
        .unwrap_or_default()
}

fn read_media_to_data_url(image_path: &str) -> Result<String> {
    let extension = get_patch_extension(image_path).unwrap_or_default();
    let mime_type = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => bail!("Unexpected media type"),
    };
    let mut file = File::open(image_path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    let encoded_image = base64_encode(buffer);
    let data_url = format!("data:{mime_type};base64,{encoded_image}");

    Ok(data_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::request_context::RequestContext;
    use crate::config::{AppState, WorkingMode};
    use std::sync::Arc;

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
        let (resolved, with_session, with_agent) = resolve_role(&ctx, Some(role));
        assert_eq!(resolved.name(), "custom");
        assert!(!with_session);
        assert!(!with_agent);
    }

    #[test]
    fn resolve_role_without_role_no_session_no_agent() {
        let ctx = create_test_ctx();
        let (resolved, with_session, with_agent) = resolve_role(&ctx, None);
        assert_eq!(resolved.name(), "");
        assert!(!with_session);
        assert!(!with_agent);
    }

    #[test]
    fn resolve_role_without_role_with_session() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let (_resolved, with_session, with_agent) = resolve_role(&ctx, None);
        assert!(with_session);
        assert!(!with_agent);
    }

    #[test]
    fn resolve_role_explicit_role_overrides_session_flag() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let role = Role::new("explicit", "prompt");
        let (_resolved, with_session, _with_agent) = resolve_role(&ctx, Some(role));
        assert!(!with_session);
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
        let input = Input::from_str(&ctx, "hello world", None);
        assert_eq!(input.text(), "hello world");
    }

    #[test]
    fn input_from_str_with_explicit_role() {
        let ctx = create_test_ctx();
        let role = Role::new("pirate", "you are a pirate");
        let input = Input::from_str(&ctx, "ahoy", Some(role));
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
        let input = Input::from_str(&ctx, "test", None);
        assert!(!input.stream_enabled);
    }

    #[test]
    fn input_is_empty_with_no_text_and_no_medias() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "", None);
        assert!(input.is_empty());
    }

    #[test]
    fn input_is_not_empty_with_text() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "hello", None);
        assert!(!input.is_empty());
    }

    #[test]
    fn input_set_text_changes_text() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "original", None);
        input.set_text("modified".to_string());
        assert_eq!(input.text(), "modified");
    }

    #[test]
    fn input_text_returns_patched_when_set() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "original", None);
        input.patched_text = Some("patched".to_string());
        assert_eq!(input.text(), "patched");
    }

    #[test]
    fn input_clear_patch_restores_original() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "original", None);
        input.patched_text = Some("patched".to_string());
        input.clear_patch();
        assert_eq!(input.text(), "original");
    }

    #[test]
    fn input_set_continue_output_accumulates() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "test", None);
        assert!(input.continue_output().is_none());
        input.set_continue_output("first ");
        assert_eq!(input.continue_output(), Some("first "));
        input.set_continue_output("second");
        assert_eq!(input.continue_output(), Some("first second"));
    }

    #[test]
    fn input_set_regenerate_sets_flag_and_clears_tool_calls() {
        let ctx = create_test_ctx();
        let mut input = Input::from_str(&ctx, "test", None);
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
        let input = Input::from_str(&ctx, &long_text, None);
        let summary = input.summary();
        assert!(summary.len() < 200);
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn input_summary_preserves_short_text() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "short", None);
        assert_eq!(input.summary(), "short");
    }

    #[test]
    fn input_raw_with_no_files() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "hello", None);
        assert_eq!(input.raw(), "hello");
    }

    #[test]
    fn input_render_with_no_medias() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "hello", None);
        assert_eq!(input.render(), "hello");
    }

    #[test]
    fn input_with_agent_false_when_no_agent() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "test", None);
        assert!(!input.with_agent());
    }

    #[test]
    fn input_session_returns_none_when_with_session_false() {
        let ctx = create_test_ctx();
        let input = Input::from_str(&ctx, "test", Some(Role::new("r", "p")));
        let session = Some(Session::default());
        assert!(input.session(&session).is_none());
    }

    #[test]
    fn input_session_returns_some_when_with_session_true() {
        let mut ctx = create_test_ctx();
        ctx.session = Some(Session::default());
        let input = Input::from_str(&ctx, "test", None);
        let session = Some(Session::default());
        assert!(input.session(&session).is_some());
    }

    #[test]
    fn is_image_recognizes_image_extensions() {
        assert!(is_image("photo.png"));
        assert!(is_image("photo.jpeg"));
        assert!(is_image("photo.jpg"));
        assert!(is_image("photo.webp"));
        assert!(is_image("photo.gif"));
    }

    #[test]
    fn is_image_rejects_non_image_extensions() {
        assert!(!is_image("file.txt"));
        assert!(!is_image("file.rs"));
        assert!(!is_image("file.pdf"));
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
}
