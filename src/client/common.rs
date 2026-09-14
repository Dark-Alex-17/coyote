use super::*;

use super::access_token::{distrust_access_token, get_access_token};
use crate::config::{RenderMode, paths};
use crate::{
    config::{AppConfig, Input, RequestContext},
    function::{FunctionDeclaration, ToolCall, ToolResult, eval_tool_calls},
    render::render_stream,
    utils::*,
};

use crate::vault::Vault;
use anyhow::{Context, Result, bail};
use fancy_regex::Regex;
use indexmap::IndexMap;
use inquire::{
    MultiSelect, Select, Text, list_option::ListOption, required, validator::Validation,
};
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;

pub const MODELS_YAML: &str = include_str!("../../models.yaml");

pub static ALL_PROVIDER_MODELS: LazyLock<Vec<ProviderModels>> = LazyLock::new(|| {
    paths::local_models_override()
        .ok()
        .unwrap_or_else(|| serde_yaml::from_str(MODELS_YAML).unwrap())
});

static EMBEDDING_MODEL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"((^|/)(bge-|e5-|uae-|gte-|text-)|embed|multilingual|minilm)").unwrap()
});

static ESCAPE_SLASH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?<!\\)/").unwrap());

#[async_trait::async_trait]
pub trait Client: Sync + Send {
    fn app_config(&self) -> &AppConfig;

    fn extra_config(&self) -> Option<&ExtraConfig>;

    fn patch_config(&self) -> Option<&RequestPatch>;

    fn name(&self) -> &str;

    fn model(&self) -> &Model;

    fn supports_oauth(&self) -> bool {
        false
    }

    fn build_client(&self) -> Result<ReqwestClient> {
        let mut builder = ReqwestClient::builder();
        let extra = self.extra_config();
        let timeout = extra.and_then(|v| v.connect_timeout).unwrap_or(10);
        let read_timeout = extra.and_then(|v| v.read_timeout).unwrap_or(300);
        if let Some(proxy) = extra.and_then(|v| v.proxy.as_deref()) {
            builder = set_proxy(builder, proxy)?;
        }
        if let Some(user_agent) = self.app_config().user_agent.as_ref() {
            builder = builder.user_agent(user_agent);
        }
        if read_timeout > 0 {
            builder = builder.read_timeout(Duration::from_secs(read_timeout));
        }
        let client = builder
            .connect_timeout(Duration::from_secs(timeout))
            .build()
            .with_context(|| "Failed to build client")?;
        Ok(client)
    }

    /// On a 401 the cached access token is distrusted and the call retried
    /// exactly once; the retry re-runs the per-client prepare step, which
    /// sees the rejection marker, force-refreshes the token, and rebuilds
    /// the whole request. A second 401 propagates the original error; any
    /// other retry failure propagates as-is.
    async fn chat_completions(&self, input: Input) -> Result<ChatCompletionsOutput> {
        if self.app_config().dry_run {
            let content = input.echo_messages();
            return Ok(ChatCompletionsOutput::new(&content));
        }
        let client = self.build_client()?;
        let data = input.prepare_completion_data(self.model(), false)?;
        let err = match self.chat_completions_inner(&client, data).await {
            Ok(output) => return Ok(output),
            Err(err) => err,
        };
        let ret = if should_retry_auth(&err, self.name()) {
            debug!(
                "provider '{}' rejected access token (401); refreshing and retrying once",
                self.name()
            );
            let data = input.prepare_completion_data(self.model(), false)?;
            match self.chat_completions_inner(&client, data).await {
                Err(retry_err) if is_auth_error(&retry_err) => Err(err),
                ret => ret,
            }
        } else {
            Err(err)
        };
        ret.with_context(|| "Failed to call chat-completions api")
    }

    /// Same retry-once-on-401 semantics as [`Self::chat_completions`], but
    /// only while the handler has received nothing yet: retrying after
    /// partial output has streamed would render it to the user twice. The
    /// retry lives inside the same `select!` arm so abort stays responsive.
    async fn chat_completions_streaming(
        &self,
        input: &Input,
        handler: &mut SseHandler,
    ) -> Result<()> {
        let abort_signal = handler.abort();
        let input = input.clone();
        tokio::select! {
            ret = async {
                if self.app_config().dry_run {
                    let content = input.echo_messages();
                    handler.text(&content)?;
                    return Ok(());
                }
                let client = self.build_client()?;
                let data = input.prepare_completion_data(self.model(), true)?;
                let err = match self.chat_completions_streaming_inner(&client, handler, data).await {
                    Ok(()) => return Ok(()),
                    Err(err) => err,
                };
                if handler.has_received_content() || !should_retry_auth(&err, self.name()) {
                    return Err(err);
                }
                debug!(
                    "provider '{}' rejected access token (401); refreshing and retrying once",
                    self.name()
                );
                let data = input.prepare_completion_data(self.model(), true)?;
                match self.chat_completions_streaming_inner(&client, handler, data).await {
                    Err(retry_err) if is_auth_error(&retry_err) => Err(err),
                    ret => ret,
                }
            } => {
                handler.done();
                ret.with_context(|| "Failed to call chat-completions api")
            }
            _ = wait_abort_signal(&abort_signal) => {
                handler.done();
                Ok(())
            },
        }
    }

    /// Same retry-once-on-401 semantics as [`Self::chat_completions`]
    /// (gemini OAuth embeddings route here).
    async fn embeddings(&self, data: &EmbeddingsData) -> Result<Vec<Vec<f32>>> {
        let client = self.build_client()?;
        let err = match self.embeddings_inner(&client, data).await {
            Ok(output) => return Ok(output),
            Err(err) => err,
        };
        let ret = if should_retry_auth(&err, self.name()) {
            debug!(
                "provider '{}' rejected access token (401); refreshing and retrying once",
                self.name()
            );
            match self.embeddings_inner(&client, data).await {
                Err(retry_err) if is_auth_error(&retry_err) => Err(err),
                ret => ret,
            }
        } else {
            Err(err)
        };
        ret.context("Failed to call embeddings api")
    }

    async fn rerank(&self, data: &RerankData) -> Result<RerankOutput> {
        let client = self.build_client()?;
        self.rerank_inner(&client, data)
            .await
            .context("Failed to call rerank api")
    }

    async fn chat_completions_inner(
        &self,
        client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput>;

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()>;

    async fn embeddings_inner(
        &self,
        _client: &ReqwestClient,
        _data: &EmbeddingsData,
    ) -> Result<EmbeddingsOutput> {
        bail!("The client doesn't support embeddings api")
    }

    async fn rerank_inner(
        &self,
        _client: &ReqwestClient,
        _data: &RerankData,
    ) -> Result<RerankOutput> {
        bail!("The client doesn't support rerank api")
    }

    fn request_builder(
        &self,
        client: &reqwest::Client,
        mut request_data: RequestData,
    ) -> RequestBuilder {
        self.patch_request_data(&mut request_data);
        request_data.into_builder(client)
    }

    fn patch_request_data(&self, request_data: &mut RequestData) {
        let model_type = self.model().model_type();
        if let Some(patch) = self.model().patch() {
            request_data.apply_patch(patch.clone());
        }

        let patch_map = std::env::var(get_env_name(&format!(
            "patch_{}_{}",
            self.model().client_name(),
            model_type.api_name(),
        )))
        .ok()
        .and_then(|v| serde_json::from_str(&v).ok())
        .or_else(|| {
            self.patch_config()
                .and_then(|v| model_type.extract_patch(v))
                .cloned()
        });
        let patch_map = match patch_map {
            Some(v) => v,
            _ => return,
        };
        for (key, patch) in patch_map {
            let key = ESCAPE_SLASH_RE.replace_all(&key, r"\/");
            if let Ok(regex) = Regex::new(&format!("^({key})$"))
                && let Ok(true) = regex.is_match(self.model().name())
            {
                request_data.apply_patch(patch);
                return;
            }
        }
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self::OpenAIConfig(OpenAIConfig::default())
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ExtraConfig {
    pub proxy: Option<String>,
    pub connect_timeout: Option<u64>,
    pub read_timeout: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RequestPatch {
    pub chat_completions: Option<ApiPatch>,
    pub embeddings: Option<ApiPatch>,
    pub rerank: Option<ApiPatch>,
}

pub type ApiPatch = IndexMap<String, Value>;

pub struct RequestData {
    pub url: String,
    pub headers: IndexMap<String, String>,
    pub body: Value,
}

impl RequestData {
    pub fn new<T>(url: T, body: Value) -> Self
    where
        T: std::fmt::Display,
    {
        Self {
            url: url.to_string(),
            headers: Default::default(),
            body,
        }
    }

    pub fn bearer_auth<T>(&mut self, auth: T)
    where
        T: std::fmt::Display,
    {
        self.headers
            .insert("authorization".into(), format!("Bearer {auth}"));
    }

    pub fn header<K, V>(&mut self, key: K, value: V)
    where
        K: std::fmt::Display,
        V: std::fmt::Display,
    {
        self.headers.insert(key.to_string(), value.to_string());
    }

    pub fn into_builder(self, client: &ReqwestClient) -> RequestBuilder {
        let RequestData { url, headers, body } = self;
        debug!("Request {url} {body}");

        let mut builder = client.post(url);
        for (key, value) in headers {
            builder = builder.header(key, value);
        }
        builder = builder.json(&body);
        builder
    }

    pub fn apply_patch(&mut self, patch: Value) {
        if let Some(patch_url) = patch["url"].as_str() {
            self.url = patch_url.into();
        }
        if let Some(patch_body) = patch.get("body") {
            json_patch::merge(&mut self.body, patch_body)
        }
        if let Some(patch_headers) = patch["headers"].as_object() {
            for (key, value) in patch_headers {
                if let Some(value) = value.as_str() {
                    self.header(key, value)
                } else if value.is_null() {
                    self.headers.swap_remove(key);
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct ChatCompletionsData {
    pub messages: Vec<Message>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub reasoning_effort: Option<String>,
    pub functions: Option<Vec<FunctionDeclaration>>,
    pub stream: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ChatCompletionsOutput {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub thinking: Vec<ThinkingBlock>,
}

impl ChatCompletionsOutput {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug)]
pub struct EmbeddingsData {
    pub texts: Vec<String>,
    pub query: bool,
}

impl EmbeddingsData {
    pub fn new(texts: Vec<String>, query: bool) -> Self {
        Self { texts, query }
    }
}

pub type EmbeddingsOutput = Vec<Vec<f32>>;

#[derive(Debug)]
pub struct RerankData {
    pub query: String,
    pub documents: Vec<String>,
    pub top_n: usize,
}

impl RerankData {
    pub fn new(query: String, documents: Vec<String>, top_n: usize) -> Self {
        Self {
            query,
            documents,
            top_n,
        }
    }
}

pub type RerankOutput = Vec<RerankResult>;

#[derive(Debug, Deserialize)]
pub struct RerankResult {
    pub index: usize,
}

pub type PromptAction<'a> = (&'a str, &'a str, Option<&'a str>, bool);

pub async fn create_config(
    prompts: &[PromptAction<'static>],
    client: &str,
    vault: &Vault,
) -> Result<(String, Value)> {
    let mut config = json!({
        "type": client,
    });
    for (key, desc, help_message, is_secret) in prompts {
        let env_name = format!("{client}-{key}")
            .to_ascii_uppercase()
            .replace("_", "-");
        let required = std::env::var(&env_name).is_err();
        let value = if !is_secret {
            prompt_input_string(desc, required, *help_message)?
        } else {
            vault.add_secret(&env_name)?;
            format!("{{{{{}}}}}", env_name)
        };
        if !value.is_empty() {
            config[key] = value.into();
        }
    }
    let model = set_client_models_config(&mut config, client).await?;
    let clients = json!(vec![config]);
    Ok((model, clients))
}

pub async fn create_openai_compatible_client_config(
    client: &str,
) -> Result<Option<(String, Value)>> {
    let api_base = OPENAI_COMPATIBLE_PROVIDERS
        .into_iter()
        .find(|(name, _)| client == *name)
        .map(|(_, api_base)| api_base)
        .unwrap_or("http(s)://{API_ADDR}/v1");

    let name = if client == OpenAICompatibleClient::NAME {
        let value = prompt_input_string("Provider Name", true, None)?;
        value.replace(' ', "-")
    } else {
        client.to_string()
    };

    let mut config = json!({
        "type": OpenAICompatibleClient::NAME,
        "name": &name,
    });

    let api_base = if api_base.contains('{') {
        prompt_input_string("API Base", true, Some(&format!("e.g. {api_base}")))?
    } else {
        api_base.to_string()
    };
    config["api_base"] = api_base.into();

    let has_bundled_oauth = ALL_PROVIDER_MODELS
        .iter()
        .any(|p| p.provider == client && p.oauth.is_some());

    let use_oauth = if has_bundled_oauth {
        let choice = Select::new("Authentication method:", vec!["API Key", "OAuth"]).prompt()?;
        choice == "OAuth"
    } else {
        false
    };

    if use_oauth {
        config["auth"] = "oauth".into();
    } else {
        let api_key = prompt_input_string("API Key", false, None)?;
        if !api_key.is_empty() {
            config["api_key"] = api_key.into();
        }
    }

    let model = set_client_models_config(&mut config, &name).await?;
    let clients = json!(vec![config]);
    Ok(Some((model, clients)))
}

pub async fn call_chat_completions(
    input: &Input,
    print: bool,
    extract_code: bool,
    client: &dyn Client,
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
) -> Result<(String, Vec<ToolResult>)> {
    let is_child_agent = ctx.current_depth > 0;
    let suppress_spinner = is_child_agent || ctx.render_mode == RenderMode::Silent;
    let spinner_message = if suppress_spinner { "" } else { "Generating" };
    let ret = abortable_run_with_spinner(
        client.chat_completions(input.clone()),
        spinner_message,
        abort_signal,
    )
    .await;

    match ret {
        Ok(ret) => {
            let ChatCompletionsOutput {
                mut text,
                tool_calls,
                thinking,
                ..
            } = ret;
            if !text.is_empty() {
                if extract_code {
                    text = extract_code_block(&strip_think_tag(&text)).to_string();
                }
                if print {
                    ctx.app.config.print_markdown(&text)?;
                }
            }
            finish_completion(ctx, text, tool_calls, thinking).await
        }
        Err(err) => Err(err),
    }
}

pub async fn call_chat_completions_streaming(
    input: &Input,
    client: &dyn Client,
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
) -> Result<(String, Vec<ToolResult>)> {
    let (tx, rx) = unbounded_channel();
    let mut handler = SseHandler::new(tx, abort_signal.clone());
    let silent = ctx.render_mode == RenderMode::Silent;
    if silent {
        handler.set_silent(true);
    }

    let (send_ret, render_ret) = tokio::join!(
        client.chat_completions_streaming(input, &mut handler),
        render_stream(rx, client.app_config(), abort_signal.clone(), silent),
    );

    let aborted_ctrlc = handler.abort().aborted_ctrlc();
    let aborted_ctrld = handler.abort().aborted_ctrld();

    if aborted_ctrld {
        bail!("Aborted.");
    }

    render_ret?;

    let (text, tool_calls, thinking) = handler.take();

    if aborted_ctrlc {
        if !ctx.working_mode.is_repl() || ctx.session.is_none() {
            bail!("Aborted.");
        }

        if text.is_empty() {
            if !silent && *IS_STDOUT_TERMINAL {
                println!();
                eprintln!("{}", error_text("Response interrupted"));
            }

            return Ok(("".to_string(), vec![]));
        }

        if !silent && *IS_STDOUT_TERMINAL {
            println!();
            eprintln!("{}", error_text("Response interrupted"));
        }

        return Ok((text, vec![]));
    }

    match send_ret {
        Ok(_) => {
            if !silent && !text.is_empty() && !text.ends_with('\n') {
                println!();
            }
            finish_completion(ctx, text, tool_calls, thinking).await
        }
        Err(err) => {
            if !silent && !text.is_empty() {
                println!();
            }
            Err(err)
        }
    }
}

/// Streaming transport for graph `llm` nodes: accumulates the reply and the
/// transport itself renders nothing; no spinner, delta, or reasoning
/// (tool-call rendering inside `eval_tool_calls` is unchanged from the
/// non-streaming path). Because chunks arrive as they are generated, the
/// reqwest `read_timeout` bounds only stalls between chunks; the caller's
/// own deadline bounds the whole generation. Parity with the non-streaming
/// transport is kept deliberately: the handler's call-loop detection is off
/// (that path never had it), and a model the catalog marks `no_stream`
/// takes the non-streaming request, raced against the abort. An abort
/// observed after the provider call fails outright. Partial output is
/// never returned as success.
pub async fn call_chat_completions_streaming_quiet(
    input: &Input,
    client: &dyn Client,
    ctx: &mut RequestContext,
    abort_signal: AbortSignal,
) -> Result<(String, Vec<ToolResult>)> {
    if client.model().no_stream() {
        let output = tokio::select! {
            ret = client.chat_completions(input.clone()) => ret?,
            _ = wait_abort_signal(&abort_signal) => bail!("Aborted."),
        };
        if abort_signal.aborted() {
            bail!("Aborted.");
        }
        let ChatCompletionsOutput {
            text,
            tool_calls,
            thinking,
        } = output;
        return finish_completion(ctx, text, tool_calls, thinking).await;
    }

    // `_rx` outlives the call so `handler.done()` still finds a receiver.
    let (tx, _rx) = unbounded_channel();
    let mut handler = SseHandler::new(tx, abort_signal.clone());
    handler.set_silent(true);
    handler.set_call_loop_detection(false);

    let send_ret = client.chat_completions_streaming(input, &mut handler).await;

    if abort_signal.aborted() {
        bail!("Aborted.");
    }
    send_ret?;

    let (text, tool_calls, thinking) = handler.take();
    finish_completion(ctx, text, tool_calls, thinking).await
}

async fn finish_completion(
    ctx: &mut RequestContext,
    text: String,
    tool_calls: Vec<ToolCall>,
    thinking: Vec<ThinkingBlock>,
) -> Result<(String, Vec<ToolResult>)> {
    let mut tool_results = eval_tool_calls(ctx, tool_calls).await?;
    if let Some(first) = tool_results.first_mut() {
        first.thinking = thinking;
    }
    tool_results
        .iter()
        .for_each(|res| ctx.tool_scope.tool_tracker.record_call(res.call.clone()));
    Ok((text, tool_results))
}

pub fn noop_prepare_rerank<T>(_client: &T, _data: &RerankData) -> Result<RequestData> {
    bail!("The client doesn't support rerank api")
}

pub async fn noop_rerank(_builder: RequestBuilder, _model: &Model) -> Result<RerankOutput> {
    bail!("The client doesn't support rerank api")
}

#[derive(Debug)]
pub struct ApiStatusError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ApiStatusError {}

/// True when the error chain bottoms out in an [`ApiStatusError`] with
/// status 401 EXACTLY. 403 (entitlement) and 429 (rate limit) are never
/// auth failures, and message text is never inspected.
fn is_auth_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiStatusError>()
        .is_some_and(|api_err| api_err.status == 401)
}

/// Decides whether a 401 from `client_name` warrants a single retry after a
/// forced token refresh: the error must be a 401 [`ApiStatusError`], and the
/// client must have a cached access token to distrust (API-key clients have
/// none and never retry). Distrusting marks the exact rejected token so the
/// retry's prepare step force-refreshes it. There is deliberately no backoff:
/// the blast radius is bounded at one extra request per user-visible call.
///
/// Note: vertexai shares the ACCESS_TOKENS cache, so a 401 there also
/// triggers distrust+retry — deliberate.
fn should_retry_auth(err: &anyhow::Error, client_name: &str) -> bool {
    if !is_auth_error(err) {
        return false;
    }
    let Ok(token) = get_access_token(client_name) else {
        return false;
    };
    distrust_access_token(client_name, &token)
}

pub fn catch_error(data: &Value, status: u16) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    debug!("Invalid response, status: {status}, data: {data}");
    let api_error = |message: String| anyhow::Error::new(ApiStatusError { status, message });
    if let Some(error) = data["error"].as_object() {
        if let (Some(typ), Some(message)) = (
            json_str_from_map(error, "type"),
            json_str_from_map(error, "message"),
        ) {
            return Err(api_error(format!("{message} (type: {typ})")));
        } else if let (Some(typ), Some(message)) = (
            json_str_from_map(error, "code"),
            json_str_from_map(error, "message"),
        ) {
            return Err(api_error(format!("{message} (code: {typ})")));
        }
    } else if let Some(error) = data["errors"][0].as_object() {
        if let (Some(code), Some(message)) = (
            error.get("code").and_then(|v| v.as_u64()),
            json_str_from_map(error, "message"),
        ) {
            return Err(api_error(format!("{message} (status: {code})")));
        }
    } else if let Some(error) = data[0]["error"].as_object() {
        if let (Some(status), Some(message)) = (
            json_str_from_map(error, "status"),
            json_str_from_map(error, "message"),
        ) {
            return Err(api_error(format!("{message} (status: {status})")));
        }
    } else if let (Some(detail), Some(status)) = (data["detail"].as_str(), data["status"].as_i64())
    {
        return Err(api_error(format!("{detail} (status: {status})")));
    } else if let Some(error) = data["error"].as_str() {
        return Err(api_error(error.to_string()));
    } else if let Some(message) = data["message"].as_str() {
        return Err(api_error(message.to_string()));
    }
    Err(api_error(format!(
        "Invalid response data: {data} (status: {status})"
    )))
}

pub fn json_str_from_map<'a>(
    map: &'a serde_json::Map<String, Value>,
    field_name: &str,
) -> Option<&'a str> {
    map.get(field_name).and_then(|v| v.as_str())
}

pub async fn set_client_models_config(client_config: &mut Value, client: &str) -> Result<String> {
    if let Some(provider) = ALL_PROVIDER_MODELS.iter().find(|v| v.provider == client) {
        let models: Vec<String> = provider
            .models
            .iter()
            .filter(|v| v.model_type == "chat")
            .map(|v| v.name.clone())
            .collect();
        let model_name = select_model(models)?;
        return Ok(format!("{client}:{model_name}"));
    }
    let mut model_names = vec![];
    if let (Some(true), Some(api_base), api_key) = (
        client_config["type"]
            .as_str()
            .map(|v| v == OpenAICompatibleClient::NAME),
        client_config["api_base"].as_str(),
        client_config["api_key"]
            .as_str()
            .map(|v| v.to_string())
            .or_else(|| {
                let env_name = format!("{client}_api_key").to_ascii_uppercase();
                std::env::var(&env_name).ok()
            }),
    ) {
        match abortable_run_with_spinner(
            fetch_models(api_base, api_key.as_deref()),
            "Fetching models",
            create_abort_signal(),
        )
        .await
        {
            Ok(fetched_models) => {
                model_names = MultiSelect::new("LLMs to include (required):", fetched_models)
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
            }
            Err(err) => {
                eprintln!("✗ Fetch models failed: {err}");
            }
        }
    }
    if model_names.is_empty() {
        model_names = prompt_input_string(
            "LLMs to add",
            true,
            Some("Separated by commas, e.g. llama3.3,qwen2.5"),
        )?
        .split(',')
        .filter_map(|v| {
            let v = v.trim();
            if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            }
        })
        .collect::<Vec<_>>();
    }
    if model_names.is_empty() {
        bail!("No models");
    }
    let models: Vec<Value> = model_names
        .iter()
        .map(|v| {
            let l = v.to_lowercase();
            if l.contains("rank") {
                json!({
                    "name": v,
                    "type": "reranker",
                })
            } else if let Ok(true) = EMBEDDING_MODEL_RE.is_match(&l) {
                json!({
                    "name": v,
                    "type": "embedding",
                    "default_chunk_size": 1000,
                    "max_batch_size": 100
                })
            } else if v.contains("vision") {
                json!({
                    "name": v,
                    "supports_vision": true
                })
            } else {
                json!({
                    "name": v,
                })
            }
        })
        .collect();
    client_config["models"] = models.into();
    let model_name = select_model(model_names)?;
    Ok(format!("{client}:{model_name}"))
}

fn select_model(model_names: Vec<String>) -> Result<String> {
    if model_names.is_empty() {
        bail!("No models");
    }
    let model = if model_names.len() == 1 {
        model_names[0].clone()
    } else {
        Select::new("Default Model (required):", model_names).prompt()?
    };
    Ok(model)
}

fn prompt_input_string(desc: &str, required: bool, help_message: Option<&str>) -> Result<String> {
    let desc = if required {
        format!("{desc} (required):")
    } else {
        format!("{desc} (optional):")
    };
    let mut text = Text::new(&desc);
    if required {
        text = text.with_validator(required!("This field is required"))
    }
    if let Some(help_message) = help_message {
        text = text.with_help_message(help_message);
    }
    let text = text.prompt()?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::access_token::{is_rejected, set_access_token};
    use crate::config::{AppState, WorkingMode};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::Instant;

    fn catch_error_message(data: &Value, status: u16) -> String {
        catch_error(data, status).unwrap_err().to_string()
    }

    #[test]
    fn test_catch_error_display_json_with_type() {
        let data = json!({"error": {"type": "invalid_request_error", "message": "Bad request"}});
        assert_eq!(
            catch_error_message(&data, 400),
            "Bad request (type: invalid_request_error)"
        );
    }

    #[test]
    fn test_catch_error_display_json_with_code() {
        let data = json!({"error": {"code": "rate_limited", "message": "Too many requests"}});
        assert_eq!(
            catch_error_message(&data, 429),
            "Too many requests (code: rate_limited)"
        );
    }

    #[test]
    fn test_catch_error_display_errors_array() {
        let data = json!({"errors": [{"code": 7000, "message": "No route"}]});
        assert_eq!(catch_error_message(&data, 404), "No route (status: 7000)");
    }

    #[test]
    fn test_catch_error_display_array_error_status() {
        let data = json!([{"error": {"status": "PERMISSION_DENIED", "message": "Denied"}}]);
        assert_eq!(
            catch_error_message(&data, 403),
            "Denied (status: PERMISSION_DENIED)"
        );
    }

    #[test]
    fn test_catch_error_display_detail_status() {
        let data = json!({"detail": "Not found", "status": 404});
        assert_eq!(catch_error_message(&data, 404), "Not found (status: 404)");
    }

    #[test]
    fn test_catch_error_display_error_string() {
        let data = json!({"error": "Something went wrong"});
        assert_eq!(catch_error_message(&data, 500), "Something went wrong");
    }

    #[test]
    fn test_catch_error_display_message_string() {
        let data = json!({"message": "Unauthorized"});
        assert_eq!(catch_error_message(&data, 401), "Unauthorized");
    }

    #[test]
    fn test_catch_error_display_fallback() {
        let data = json!({"unexpected": true});
        assert_eq!(
            catch_error_message(&data, 500),
            format!("Invalid response data: {data} (status: 500)")
        );
    }

    #[test]
    fn test_catch_error_ok_on_success_status() {
        let data = json!({"error": {"type": "x", "message": "y"}});
        assert!(catch_error(&data, 200).is_ok());
        assert!(catch_error(&data, 299).is_ok());
    }

    #[test]
    fn test_catch_error_downcast_through_context_chain() {
        let data = json!({"error": {"type": "authentication_error", "message": "Invalid key"}});
        let err = catch_error(&data, 401)
            .context("Failed to call chat-completions api")
            .unwrap_err();
        let api_err = err
            .downcast_ref::<ApiStatusError>()
            .expect("should downcast through context chain");
        assert_eq!(api_err.status, 401);
        assert_eq!(api_err.message, "Invalid key (type: authentication_error)");
    }

    #[test]
    fn test_catch_error_preserves_status() {
        let data = json!({"message": "Unauthorized"});
        let err = catch_error(&data, 401).unwrap_err();
        assert_eq!(err.downcast_ref::<ApiStatusError>().unwrap().status, 401);

        let data = json!({"detail": "Rate limited", "status": 429});
        let err = catch_error(&data, 429).unwrap_err();
        assert_eq!(err.downcast_ref::<ApiStatusError>().unwrap().status, 429);

        // The struct carries the outer HTTP status even when the body embeds another code
        let data = json!({"errors": [{"code": 7000, "message": "No route"}]});
        let err = catch_error(&data, 429).unwrap_err();
        assert_eq!(err.downcast_ref::<ApiStatusError>().unwrap().status, 429);
    }

    /// Wrapped in `.context(...)` so every test below proves the downcast
    /// works through an anyhow context chain, as in the trait methods.
    fn api_status_error(status: u16) -> anyhow::Error {
        anyhow::Error::new(ApiStatusError {
            status,
            message: format!("error (status: {status})"),
        })
        .context("Failed to call chat-completions api")
    }

    fn cache_token(client: &str, token: &str) {
        set_access_token(
            client,
            token.into(),
            chrono::Utc::now().timestamp() + 3600,
            None,
        );
    }

    #[test]
    fn test_should_retry_auth_401_with_cached_token() {
        let client = "should-retry-auth-401";
        cache_token(client, "at-1");

        assert!(should_retry_auth(&api_status_error(401), client));
        assert!(is_rejected(client, "at-1"), "rejected marker not set");
    }

    #[test]
    fn test_should_retry_auth_non_401_statuses() {
        let client = "should-retry-auth-non-401";
        cache_token(client, "at-1");

        for status in [403, 429, 500] {
            assert!(
                !should_retry_auth(&api_status_error(status), client),
                "retried on {status}"
            );
        }
        assert_eq!(get_access_token(client).unwrap(), "at-1");
        assert!(!is_rejected(client, "at-1"), "marker set without a 401");
    }

    #[test]
    fn test_should_retry_auth_non_api_status_error() {
        let client = "should-retry-auth-non-api";
        cache_token(client, "at-1");

        let err = anyhow::anyhow!("connection reset").context("Failed to call embeddings api");
        assert!(!should_retry_auth(&err, client));
        assert_eq!(get_access_token(client).unwrap(), "at-1");
        assert!(!is_rejected(client, "at-1"));
    }

    #[test]
    fn test_should_retry_auth_401_without_cached_token() {
        let client = "should-retry-auth-no-token";

        assert!(!should_retry_auth(&api_status_error(401), client));
        assert!(!is_rejected(client, "at-1"));
    }

    /// Fake provider whose streaming path emits `PACED_DELTAS` text chunks
    /// 60s apart, then one thinking block and one tool call, and whose
    /// non-streaming path returns that same reply in one piece when
    /// `nonstream_replies` is set (and bails otherwise). Exercises the real
    /// trait defaults (build_client, prepare_completion_data, abort
    /// `select!`) with no network.
    struct PacedStreamClient {
        config: AppConfig,
        model: Model,
        stall_forever: bool,
        nonstream_replies: bool,
        streaming_calls: AtomicUsize,
    }

    const PACED_DELTAS: usize = 10;
    const PACED_GAP: Duration = Duration::from_secs(60);

    fn paced_text() -> String {
        (0..PACED_DELTAS).map(|i| format!("chunk{i} ")).collect()
    }

    fn paced_tool_call() -> ToolCall {
        ToolCall::new(
            "lookup".to_string(),
            json!({"q": 1}),
            Some("call-1".to_string()),
        )
    }

    fn paced_thinking() -> ThinkingBlock {
        ThinkingBlock::Thinking {
            thinking: "hmm".to_string(),
            signature: "sig".to_string(),
        }
    }

    #[async_trait::async_trait]
    impl Client for PacedStreamClient {
        fn app_config(&self) -> &AppConfig {
            &self.config
        }

        fn extra_config(&self) -> Option<&ExtraConfig> {
            None
        }

        fn patch_config(&self) -> Option<&RequestPatch> {
            None
        }

        fn name(&self) -> &str {
            "paced-stream"
        }

        fn model(&self) -> &Model {
            &self.model
        }

        async fn chat_completions_inner(
            &self,
            _client: &ReqwestClient,
            data: ChatCompletionsData,
        ) -> Result<ChatCompletionsOutput> {
            assert!(!data.stream, "non-streaming path must not request a stream");
            if !self.nonstream_replies {
                bail!("the quiet transport must not take the non-streaming path");
            }
            if self.stall_forever {
                std::future::pending::<()>().await;
            }
            Ok(ChatCompletionsOutput {
                text: paced_text(),
                tool_calls: vec![paced_tool_call()],
                thinking: vec![paced_thinking()],
            })
        }

        async fn chat_completions_streaming_inner(
            &self,
            _client: &ReqwestClient,
            handler: &mut SseHandler,
            data: ChatCompletionsData,
        ) -> Result<()> {
            self.streaming_calls.fetch_add(1, Ordering::SeqCst);
            assert!(data.stream, "trait default must request a stream");
            for i in 0..PACED_DELTAS {
                tokio::time::sleep(PACED_GAP).await;
                handler.text(&format!("chunk{i} "))?;
            }
            if self.stall_forever {
                std::future::pending::<()>().await;
            }
            handler.thinking_block(paced_thinking());
            handler.tool_call(paced_tool_call())
        }
    }

    fn paced_client(stall_forever: bool) -> PacedStreamClient {
        PacedStreamClient {
            config: AppConfig::default(),
            model: Model::default(),
            stall_forever,
            nonstream_replies: false,
            streaming_calls: AtomicUsize::new(0),
        }
    }

    fn no_stream_model() -> Model {
        let mut data = ModelData::new("");
        data.no_stream = true;
        Model::from_config("", &[data]).remove(0)
    }

    fn assert_paced_reply(text: &str, results: &[ToolResult]) {
        assert_eq!(text, paced_text());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].call.name, "lookup");
        assert_eq!(results[0].call.id.as_deref(), Some("call-1"));
        assert!(
            matches!(&results[0].thinking[..], [ThinkingBlock::Thinking { thinking, .. }] if thinking == "hmm")
        );
    }

    fn quiet_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    /// 600s of virtual generation completes and yields the same shape the
    /// non-streaming transport would: full text, tool results with the
    /// thinking stapled onto the first, and the tracker fed each call. The
    /// fake ignores the reqwest client, so reqwest's `read_timeout` is not in
    /// the loop here; the real-socket tests in graph/mod.rs pin that it
    /// fires on a stall (`sse_read_timeout_stall_is_transient`) and not on a
    /// slow but continuous stream (`sse_slow_but_continuous_stream_outlives_read_timeout`).
    #[tokio::test(start_paused = true)]
    async fn quiet_streaming_accumulates_across_a_long_generation() {
        let mut ctx = quiet_ctx();
        let input = Input::from_str(&ctx, "hi", None).unwrap();
        let client = paced_client(false);
        let started = Instant::now();

        let (text, results) =
            call_chat_completions_streaming_quiet(&input, &client, &mut ctx, create_abort_signal())
                .await
                .unwrap();

        assert!(started.elapsed() >= PACED_GAP * PACED_DELTAS as u32);
        assert_paced_reply(&text, &results);
        assert!(
            results[0].output["tool_call_error"].is_string(),
            "an undeclared tool evaluates to a tool_call_error, not a panic: {}",
            results[0].output
        );
    }

    /// An abort mid-stream cancels the provider call promptly and fails the
    /// call outright; the ten chunks already buffered are never returned.
    #[tokio::test(start_paused = true)]
    async fn quiet_streaming_abort_discards_partial_output() {
        let mut ctx = quiet_ctx();
        let input = Input::from_str(&ctx, "hi", None).unwrap();
        let client = paced_client(true);
        let abort = create_abort_signal();
        let trigger = abort.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PACED_GAP * PACED_DELTAS as u32 + Duration::from_secs(30)).await;
            trigger.set_ctrlc();
        });
        let started = Instant::now();

        let err = call_chat_completions_streaming_quiet(&input, &client, &mut ctx, abort)
            .await
            .expect_err("an aborted stream must not succeed");

        assert_eq!(err.to_string(), "Aborted.");
        assert!(started.elapsed() < PACED_GAP * (PACED_DELTAS as u32 + 1));
    }

    /// The quiet transport and the non-streaming transport hand the node the
    /// same `(text, tool_results)` for the same provider reply.
    #[tokio::test(start_paused = true)]
    async fn quiet_streaming_matches_the_non_streaming_reply() {
        let mut ctx = quiet_ctx();
        ctx.render_mode = RenderMode::Silent;
        let input = Input::from_str(&ctx, "hi", None).unwrap();
        let mut client = paced_client(false);
        client.nonstream_replies = true;

        let (streamed_text, streamed) =
            call_chat_completions_streaming_quiet(&input, &client, &mut ctx, create_abort_signal())
                .await
                .unwrap();
        let (plain_text, plain) = call_chat_completions(
            &input,
            false,
            false,
            &client,
            &mut ctx,
            create_abort_signal(),
        )
        .await
        .unwrap();

        assert_eq!(streamed_text, plain_text);
        assert_paced_reply(&streamed_text, &streamed);
        assert_paced_reply(&plain_text, &plain);
        assert_eq!(streamed[0].output, plain[0].output);
    }

    /// A model the catalog marks `no_stream` never sees a streaming request;
    /// the quiet transport takes the non-streaming path and returns the same
    /// shape.
    #[tokio::test]
    async fn quiet_transport_honours_no_stream_models() {
        let mut ctx = quiet_ctx();
        let input = Input::from_str(&ctx, "hi", None).unwrap();
        let mut client = paced_client(false);
        client.model = no_stream_model();
        client.nonstream_replies = true;

        let (text, results) =
            call_chat_completions_streaming_quiet(&input, &client, &mut ctx, create_abort_signal())
                .await
                .unwrap();

        assert_eq!(client.streaming_calls.load(Ordering::SeqCst), 0);
        assert_paced_reply(&text, &results);
    }

    /// The `no_stream` fallback is raced against the abort like the
    /// streaming path: a hung non-streaming request is cancelled promptly.
    #[tokio::test(start_paused = true)]
    async fn quiet_transport_no_stream_fallback_honours_abort() {
        let mut ctx = quiet_ctx();
        let input = Input::from_str(&ctx, "hi", None).unwrap();
        let mut client = paced_client(true);
        client.model = no_stream_model();
        client.nonstream_replies = true;
        let abort = create_abort_signal();
        let trigger = abort.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            trigger.set_ctrlc();
        });
        let started = Instant::now();

        let err = call_chat_completions_streaming_quiet(&input, &client, &mut ctx, abort)
            .await
            .expect_err("an aborted non-streaming fallback must not succeed");

        assert_eq!(err.to_string(), "Aborted.");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// The user-level `stream` toggle is a rendering preference; the quiet
    /// transport still streams with it off.
    #[tokio::test(start_paused = true)]
    async fn quiet_transport_ignores_the_user_stream_toggle() {
        let app = AppState {
            config: Arc::new(AppConfig {
                stream: false,
                ..AppConfig::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        let input = Input::from_str(&ctx, "hi", None).unwrap();
        assert!(!input.stream(), "fixture must have streaming disabled");
        let client = paced_client(false);

        let (text, results) =
            call_chat_completions_streaming_quiet(&input, &client, &mut ctx, create_abort_signal())
                .await
                .unwrap();

        assert_eq!(client.streaming_calls.load(Ordering::SeqCst), 1);
        assert_paced_reply(&text, &results);
    }
}
