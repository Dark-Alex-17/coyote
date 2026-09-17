use super::access_token::{get_access_token, get_access_token_account_id};
use super::oauth::{self, OAuthConfig, OAuthProvider};
use super::openai_oauth::OpenAIOAuthProvider;
use super::*;

use crate::utils::strip_think_tag;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

const API_BASE: &str = "https://api.openai.com/v1";
const CODEX_API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OpenAIConfig {
    pub name: Option<String>,
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    pub organization_id: Option<String>,
    pub auth: Option<String>,
    pub oauth: Option<Box<OAuthConfig>>,
    pub wire_api: Option<WireApi>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    pub patch: Option<RequestPatch>,
    pub extra: Option<ExtraConfig>,
}

impl OpenAIClient {
    config_get_fn!(api_key, get_api_key);
    config_get_fn!(api_base, get_api_base);

    create_oauth_supported_client_config!();
}

#[async_trait::async_trait]
impl Client for OpenAIClient {
    client_common_fns!();

    fn supports_oauth(&self) -> bool {
        self.config.auth.as_deref() == Some("oauth")
    }

    async fn chat_completions_inner(
        &self,
        client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput> {
        let (request_data, wire) = prepare_chat_completions(self, client, data).await?;
        let builder = self.request_builder(client, request_data);
        match wire {
            WireApi::Responses => openai_responses_chat_completions(builder, self.model()).await,
            WireApi::Chat => openai_chat_completions(builder, self.model()).await,
        }
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        let (request_data, wire) = prepare_chat_completions(self, client, data).await?;
        let builder = self.request_builder(client, request_data);

        match wire {
            WireApi::Responses => openai_responses_streaming(builder, handler).await,
            WireApi::Chat => {
                openai_chat_completions_streaming(builder, handler, self.model()).await
            }
        }
    }

    async fn embeddings_inner(
        &self,
        client: &ReqwestClient,
        data: &EmbeddingsData,
    ) -> Result<EmbeddingsOutput> {
        let request_data = prepare_embeddings(self, client, data).await?;
        let builder = self.request_builder(client, request_data);
        openai_embeddings(builder, self.model()).await
    }

    async fn rerank_inner(
        &self,
        client: &ReqwestClient,
        data: &RerankData,
    ) -> Result<RerankOutput> {
        let request_data = noop_prepare_rerank(self, data)?;
        let builder = self.request_builder(client, request_data);
        noop_rerank(builder, self.model()).await
    }
}

/// Codex only speaks the Responses API, so it forces the responses wire and
/// rejects an explicit `wire_api: chat`. Anywhere else an explicit `wire_api`
/// wins; without one, stock openai (api.openai.com, no custom `api_base`)
/// defaults to responses and everything else defaults to chat.
pub fn resolve_wire_api(
    explicit: Option<WireApi>,
    uses_codex: bool,
    stock_openai_without_api_base: bool,
) -> Result<WireApi> {
    match (uses_codex, explicit) {
        (true, Some(WireApi::Chat)) => bail!(
            "the Codex backend only speaks the Responses API; remove `wire_api: chat` or configure an `api_base`"
        ),
        (true, _) => Ok(WireApi::Responses),
        (false, Some(wire)) => Ok(wire),
        (false, None) if stock_openai_without_api_base => Ok(WireApi::Responses),
        (false, None) => Ok(WireApi::Chat),
    }
}

async fn prepare_chat_completions(
    self_: &OpenAIClient,
    client: &ReqwestClient,
    data: ChatCompletionsData,
) -> Result<(RequestData, WireApi)> {
    let uses_oauth = self_.config.auth.as_deref() == Some("oauth");

    if !uses_oauth && self_.config.oauth.is_some() {
        bail!(
            "'{}' has an `oauth:` block configured but `auth: oauth` is not set; the oauth block would be ignored. Set `auth: oauth` (and run 'coyote --authenticate {}') or remove the oauth block.",
            self_.name(),
            self_.name()
        );
    }

    let oauth_provider = if uses_oauth {
        Some(resolve_oauth_provider(self_)?)
    } else {
        None
    };
    let uses_stock_provider = matches!(oauth_provider, Some((_, true)));
    // Stock oauth with no `api_base` routes to the ChatGPT codex backend (Responses API).
    let uses_codex = uses_stock_provider && self_.get_api_base().is_err();
    // Stock-openai traffic means api.openai.com: an api-key client (no oauth
    // at all) or stock oauth, either way without a custom `api_base`. A
    // config-oauth gateway missing its required `api_base` must not qualify.
    let stock_openai_without_api_base =
        (oauth_provider.is_none() || uses_stock_provider) && self_.get_api_base().is_err();
    let wire = resolve_wire_api(
        self_.config.wire_api,
        uses_codex,
        stock_openai_without_api_base,
    )?;

    let url = if uses_codex {
        CODEX_API_ENDPOINT.to_string()
    } else {
        let api_base = resolve_api_base(self_)?;
        match wire {
            WireApi::Responses => format!("{}/responses", api_base.trim_end_matches('/')),
            WireApi::Chat => format!("{}/chat/completions", api_base.trim_end_matches('/')),
        }
    };

    let body = match wire {
        WireApi::Responses => openai_build_responses_body(data, &self_.model),
        WireApi::Chat => openai_build_chat_completions_body(data, &self_.model),
    };

    let mut request_data = RequestData::new(url, body).wire(wire);

    if let Some((provider, _)) = oauth_provider {
        let ready = oauth::prepare_oauth_access_token(client, &*provider, self_.name()).await?;

        if !ready {
            bail!(
                "OAuth configured but no tokens found for '{}'. Run: 'coyote --authenticate {}' or '.authenticate' in the REPL",
                self_.name(),
                self_.name()
            );
        }

        let token = get_access_token(self_.name())?;
        request_data.bearer_auth(token);

        if uses_stock_provider && let Some(account_id) = get_access_token_account_id(self_.name()) {
            request_data.header("ChatGPT-Account-Id", account_id);
        }

        for (key, value) in provider.extra_request_headers() {
            request_data.header(key, value);
        }
    } else if let Ok(api_key) = self_.get_api_key() {
        request_data.bearer_auth(api_key);
    } else {
        bail!(
            "No authentication configured for '{}'. Set `api_key` or use `auth: oauth` with `coyote --authenticate {}`.",
            self_.name(),
            self_.name()
        );
    }

    if let Some(organization_id) = &self_.config.organization_id {
        request_data.header("OpenAI-Organization", organization_id);
    }

    Ok((request_data, wire))
}

async fn prepare_embeddings(
    self_: &OpenAIClient,
    client: &ReqwestClient,
    data: &EmbeddingsData,
) -> Result<RequestData> {
    let uses_oauth = self_.config.auth.as_deref() == Some("oauth");

    if !uses_oauth && self_.config.oauth.is_some() {
        bail!(
            "'{}' has an `oauth:` block configured but `auth: oauth` is not set; the oauth block would be ignored. Set `auth: oauth` (and run 'coyote --authenticate {}') or remove the oauth block.",
            self_.name(),
            self_.name()
        );
    }

    let api_base = resolve_api_base(self_)?;

    let url = format!("{}/embeddings", api_base.trim_end_matches('/'));

    let body = openai_build_embeddings_body(data, &self_.model);

    let mut request_data = RequestData::new(url, body);

    if uses_oauth {
        let (provider, _) = resolve_oauth_provider(self_)?;
        let ready = oauth::prepare_oauth_access_token(client, &*provider, self_.name()).await?;

        if !ready {
            bail!(
                "OAuth configured but no tokens found for '{}'. Run: 'coyote --authenticate {}' or '.authenticate' in the REPL",
                self_.name(),
                self_.name()
            );
        }

        let token = get_access_token(self_.name())?;
        request_data.bearer_auth(token);

        for (key, value) in provider.extra_request_headers() {
            request_data.header(key, value);
        }
    } else if let Ok(api_key) = self_.get_api_key() {
        request_data.bearer_auth(api_key);
    } else {
        bail!(
            "No authentication configured for '{}'. Set `api_key` or use `auth: oauth` with `coyote --authenticate {}`.",
            self_.name(),
            self_.name()
        );
    }

    if let Some(organization_id) = &self_.config.organization_id {
        request_data.header("OpenAI-Organization", organization_id);
    }

    Ok(request_data)
}

fn resolve_api_base(self_: &OpenAIClient) -> Result<String> {
    resolve_api_base_against(self_, &ALL_PROVIDER_MODELS)
}

fn resolve_api_base_against(
    self_: &OpenAIClient,
    all_provider_models: &[ProviderModels],
) -> Result<String> {
    if let Ok(api_base) = self_.get_api_base() {
        return Ok(api_base);
    }
    let uses_config_oauth = self_.config.auth.as_deref() == Some("oauth")
        && match locate_client_config(self_) {
            Ok(cc) => oauth::config_oauth_for_client(cc, all_provider_models).is_some(),
            // Safe: on locate failure, provider resolution bails when an inline oauth block exists; otherwise the stock provider is used and issuer-stamped gateway tokens are rejected before attach.
            Err(_) => self_.config.oauth.is_some(),
        };
    if uses_config_oauth {
        bail!(
            "'{}' has a config-driven OAuth configuration (an `oauth:` block in the client entry or models catalog) but no `api_base`; refusing to fall back to {} (the oauth token would be sent to the wrong host). Set `api_base` on the '{}' client entry.",
            self_.name(),
            API_BASE,
            self_.name()
        );
    }
    Ok(API_BASE.to_string())
}

fn locate_client_config(self_: &OpenAIClient) -> Result<&ClientConfig> {
    let client_name = self_.name();
    self_
        .app_config()
        .clients
        .iter()
        .find(|cc| {
            matches!(
                cc,
                ClientConfig::OpenAIConfig(c)
                if c.name.as_deref().unwrap_or(OpenAIClient::NAME) == client_name
            )
        })
        .ok_or_else(|| anyhow!("Could not locate ClientConfig entry for '{}'", client_name))
}

fn resolve_oauth_provider(self_: &OpenAIClient) -> Result<(Box<dyn OAuthProvider>, bool)> {
    match locate_client_config(self_) {
        Ok(cc) => Ok(oauth::openai_oauth_provider_for_client(
            cc,
            &ALL_PROVIDER_MODELS,
        )),
        Err(_) if self_.config.oauth.is_none() => Ok((Box::new(OpenAIOAuthProvider), true)),
        Err(err) => Err(err),
    }
}

pub async fn openai_chat_completions(
    builder: RequestBuilder,
    _model: &Model,
) -> Result<ChatCompletionsOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let data: Value = res.json().await?;
    if !status.is_success() {
        catch_error(&data, status.as_u16())?;
    }

    debug!("non-stream-data: {data}");
    openai_extract_chat_completions(&data)
}

pub async fn openai_chat_completions_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
    _model: &Model,
) -> Result<()> {
    let mut call_id = String::new();
    let mut function_name = String::new();
    let mut function_arguments = String::new();
    let mut function_id = String::new();
    let mut reasoning_state = 0;
    let handle = |message: SseMessage| -> Result<bool> {
        if message.data == "[DONE]" {
            if !function_name.is_empty() {
                if function_arguments.is_empty() {
                    function_arguments = String::from("{}");
                }
                let arguments: Value = function_arguments.parse().with_context(|| {
                    format!(
                        "Tool call '{function_name}' has non-JSON arguments '{function_arguments}'"
                    )
                })?;
                handler.tool_call(ToolCall::new(
                    function_name.clone(),
                    arguments,
                    normalize_function_id(&function_id),
                ))?;
            }
            return Ok(true);
        }
        let data: Value = serde_json::from_str(&message.data)?;
        debug!("stream-data: {data}");
        if let Some(text) = data["choices"][0]["delta"]["content"]
            .as_str()
            .filter(|v| !v.is_empty())
        {
            if reasoning_state == 1 {
                handler.text("\n</think>\n\n")?;
                reasoning_state = 0;
            }
            handler.text(text)?;
        } else if let Some(text) = data["choices"][0]["delta"]["reasoning_content"]
            .as_str()
            .or_else(|| data["choices"][0]["delta"]["reasoning"].as_str())
            .filter(|v| !v.is_empty())
        {
            if reasoning_state == 0 {
                handler.text("<think>\n")?;
                reasoning_state = 1;
            }
            handler.text(text)?;
        }
        if let (Some(function), index, id) = (
            data["choices"][0]["delta"]["tool_calls"][0]["function"].as_object(),
            data["choices"][0]["delta"]["tool_calls"][0]["index"].as_u64(),
            data["choices"][0]["delta"]["tool_calls"][0]["id"]
                .as_str()
                .filter(|v| !v.is_empty()),
        ) {
            if reasoning_state == 1 {
                handler.text("\n</think>\n\n")?;
                reasoning_state = 0;
            }
            let maybe_call_id = format!("{}/{}", id.unwrap_or_default(), index.unwrap_or_default());
            if maybe_call_id != call_id && maybe_call_id.len() >= call_id.len() {
                if !function_name.is_empty() {
                    if function_arguments.is_empty() {
                        function_arguments = String::from("{}");
                    }
                    let arguments: Value = function_arguments.parse().with_context(|| {
                        format!("Tool call '{function_name}' has non-JSON arguments '{function_arguments}'")
                    })?;
                    handler.tool_call(ToolCall::new(
                        function_name.clone(),
                        arguments,
                        normalize_function_id(&function_id),
                    ))?;
                }
                function_name.clear();
                function_arguments.clear();
                function_id.clear();
                call_id = maybe_call_id;
            }
            if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                if name.starts_with(&function_name) {
                    function_name = name.to_string();
                } else {
                    function_name.push_str(name);
                }
            }
            if let Some(arguments) = function.get("arguments").and_then(|v| v.as_str()) {
                function_arguments.push_str(arguments);
            }
            if let Some(id) = id {
                function_id = id.to_string();
            }
        }
        Ok(false)
    };

    sse_stream(builder, handle).await
}

pub async fn openai_embeddings(
    builder: RequestBuilder,
    _model: &Model,
) -> Result<EmbeddingsOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let data: Value = res.json().await?;
    if !status.is_success() {
        catch_error(&data, status.as_u16())?;
    }
    let res_body: EmbeddingsResBody =
        serde_json::from_value(data).context("Invalid embeddings data")?;
    let output = res_body.data.into_iter().map(|v| v.embedding).collect();
    Ok(output)
}

#[derive(Deserialize)]
struct EmbeddingsResBody {
    data: Vec<EmbeddingsResBodyEmbedding>,
}

#[derive(Deserialize)]
struct EmbeddingsResBodyEmbedding {
    embedding: Vec<f32>,
}

pub fn openai_build_chat_completions_body(data: ChatCompletionsData, model: &Model) -> Value {
    let ChatCompletionsData {
        messages,
        temperature,
        top_p,
        reasoning_effort,
        functions,
        stream,
    } = data;

    let messages_len = messages.len();
    let messages: Vec<Value> =
        messages
            .into_iter()
            .enumerate()
            .flat_map(|(i, message)| {
                let Message { role, content } = message;
                match content {
                    MessageContent::ToolCalls(MessageContentToolCalls {
                        tool_results,
                        text,
                        sequence,
                        ..
                    }) => {
                        // Empty tool_results (reachable via deserialized sessions) must not emit empty tool_calls.
                        if tool_results.is_empty() {
                            vec![]
                        } else if !sequence {
                            let tool_calls: Vec<_> = tool_results
                                .iter()
                                .map(|tool_result| {
                                    json!({
                                        "id": tool_result.call.id,
                                        "type": "function",
                                        "function": {
                                            "name": tool_result.call.name,
                                            "arguments": tool_result.call.arguments.to_string(),
                                        },
                                    })
                                })
                                .collect();
                            let mut assistant_message =
                                json!({ "role": MessageRole::Assistant, "tool_calls": tool_calls });
                            if !text.is_empty() {
                                assistant_message["content"] = strip_think_tag(&text).into();
                            }
                            let mut messages = vec![assistant_message];
                            for tool_result in tool_results {
                                messages.push(json!({
                                    "role": "tool",
                                    "content": tool_result.output.to_string(),
                                    "tool_call_id": tool_result.call.id,
                                }));
                            }
                            messages
                        } else {
                            tool_results.into_iter().enumerate().flat_map(|(index, tool_result)| {
                            let round_text = if index == 0 && !text.is_empty() {
                                Some(text.clone())
                            } else {
                                tool_result.text.clone()
                            };
                            let mut assistant_message = json!({
                                "role": MessageRole::Assistant,
                                "tool_calls": [
                                    {
                                        "id": tool_result.call.id,
                                        "type": "function",
                                        "function": {
                                            "name": tool_result.call.name,
                                            "arguments": tool_result.call.arguments.to_string(),
                                        },
                                    }
                                ]
                            });
                            if let Some(round_text) = round_text {
                                assistant_message["content"] = strip_think_tag(&round_text).into();
                            }
                            vec![
                                assistant_message,
                                json!({
                                    "role": "tool",
                                    "content": tool_result.output.to_string(),
                                    "tool_call_id": tool_result.call.id,
                                })
                            ]

                        }).collect()
                        }
                    }
                    MessageContent::Text(text) if role.is_assistant() && i != messages_len - 1 => {
                        vec![json!({ "role": role, "content": strip_think_tag(&text) }
                        )]
                    }
                    _ => vec![json!({ "role": role, "content": content })],
                }
            })
            .collect();

    let mut body = json!({
        "model": &model.real_name(),
        "messages": messages,
    });

    if let Some(v) = model.max_tokens_param() {
        if model
            .patch()
            .and_then(|v| v.get("body").and_then(|v| v.get("max_tokens")))
            == Some(&Value::Null)
        {
            body["max_completion_tokens"] = v.into();
        } else {
            body["max_tokens"] = v.into();
        }
    }
    if let Some(v) = temperature {
        body["temperature"] = v.into();
    }
    if let Some(v) = top_p {
        body["top_p"] = v.into();
    }
    if let Some(v) = reasoning_effort {
        body["reasoning_effort"] = v.into();
    }
    if stream {
        body["stream"] = true.into();
    }
    if let Some(functions) = functions {
        body["tools"] = functions
            .iter()
            .map(|v| {
                json!({
                    "type": "function",
                    "function": v,
                })
            })
            .collect();
    }
    body
}

pub fn openai_build_embeddings_body(data: &EmbeddingsData, model: &Model) -> Value {
    json!({
        "input": data.texts,
        "model": model.real_name()
    })
}

pub fn openai_extract_chat_completions(data: &Value) -> Result<ChatCompletionsOutput> {
    let text = data["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default();

    let reasoning = data["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .or_else(|| data["choices"][0]["message"]["reasoning"].as_str())
        .unwrap_or_default()
        .trim();

    let mut tool_calls = vec![];
    if let Some(calls) = data["choices"][0]["message"]["tool_calls"].as_array() {
        for call in calls {
            if let (Some(name), Some(arguments), Some(id)) = (
                call["function"]["name"].as_str(),
                call["function"]["arguments"].as_str(),
                call["id"].as_str(),
            ) {
                let arguments: Value = arguments.parse().with_context(|| {
                    format!("Tool call '{name}' has non-JSON arguments '{arguments}'")
                })?;
                tool_calls.push(ToolCall::new(
                    name.to_string(),
                    arguments,
                    Some(id.to_string()),
                ));
            }
        }
    };

    if text.is_empty() && tool_calls.is_empty() {
        bail!("Invalid response data: {data}");
    }
    let text = if !reasoning.is_empty() {
        format!("<think>\n{reasoning}\n</think>\n\n{text}")
    } else {
        text.to_string()
    };
    let output = ChatCompletionsOutput {
        text,
        tool_calls,
        ..Default::default()
    };
    Ok(output)
}

fn normalize_function_id(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

pub fn openai_build_responses_body(data: ChatCompletionsData, model: &Model) -> Value {
    let ChatCompletionsData {
        messages,
        temperature,
        top_p,
        reasoning_effort,
        functions,
        stream,
    } = data;

    let messages_len = messages.len();
    let input: Vec<Value> = messages
        .into_iter()
        .enumerate()
        .flat_map(|(i, message)| {
            let Message { role, content } = message;
            match content {
                MessageContent::ToolCalls(MessageContentToolCalls {
                    tool_results,
                    text,
                    ..
                }) => tool_results
                    .into_iter()
                    .enumerate()
                    .flat_map(|(index, tool_result)| {
                        let round_text = if index == 0 && !text.is_empty() {
                            Some(text.clone())
                        } else {
                            tool_result.text.clone()
                        };
                        let mut items = vec![];
                        for block in &tool_result.thinking {
                            if let ThinkingBlock::Reasoning {
                                id,
                                summary,
                                encrypted_content,
                            } = block
                            {
                                // Under `store: false` the API 400s on an
                                // id-only reasoning item, which would wedge a
                                // persisted session; it also rejects a null
                                // summary, so normalize it to an empty array.
                                if encrypted_content.is_none() {
                                    debug!("dropping reasoning item '{id}': no encrypted_content");
                                    continue;
                                }
                                let mut item = json!(block);
                                if summary.is_null() {
                                    item["summary"] = json!([]);
                                }
                                items.push(item);
                            }
                        }
                        if let Some(round_text) = round_text {
                            items.push(json!({
                                "role": MessageRole::Assistant,
                                "content": strip_think_tag(&round_text),
                            }));
                        }
                        items.push(json!({
                            "type": "function_call",
                            "call_id": tool_result.call.id,
                            "name": tool_result.call.name,
                            "arguments": tool_result.call.arguments.to_string(),
                        }));
                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_result.call.id,
                            "output": tool_result.output.to_string(),
                        }));
                        items
                    })
                    .collect(),
                MessageContent::Text(text) if role.is_assistant() && i != messages_len - 1 => {
                    vec![json!({ "role": role, "content": strip_think_tag(&text) })]
                }
                _ => vec![json!({ "role": role, "content": content })],
            }
        })
        .collect();

    let mut body = json!({
        "model": &model.real_name(),
        "input": input,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });

    if let Some(v) = model.max_tokens_param() {
        body["max_output_tokens"] = v.into();
    }
    if let Some(v) = temperature {
        body["temperature"] = v.into();
    }
    if let Some(v) = top_p {
        body["top_p"] = v.into();
    }
    if let Some(v) = reasoning_effort {
        body["reasoning"] = json!({ "effort": v });
    }
    if stream {
        body["stream"] = true.into();
    }
    if let Some(functions) = functions {
        body["tools"] = functions
            .iter()
            .map(|v| {
                let mut tool = serde_json::to_value(v).unwrap_or_default();
                tool["type"] = "function".into();
                tool["strict"] = false.into();
                tool
            })
            .collect();
    }
    body
}

pub async fn openai_responses_chat_completions(
    builder: RequestBuilder,
    _model: &Model,
) -> Result<ChatCompletionsOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let data: Value = res.json().await?;

    if !status.is_success() {
        catch_error(&data, status.as_u16())?;
    }

    debug!("non-stream-data: {data}");
    openai_extract_responses(&data)
}

pub fn openai_extract_responses(data: &Value) -> Result<ChatCompletionsOutput> {
    let mut text = String::new();
    let mut tool_calls = vec![];
    let mut thinking = vec![];

    if let Some(output) = data["output"].as_array() {
        for item in output {
            match item["type"].as_str() {
                Some("message") => {
                    if let Some(content) = item["content"].as_array() {
                        for part in content {
                            if part["type"].as_str() == Some("output_text")
                                && let Some(t) = part["text"].as_str()
                            {
                                text.push_str(t);
                            }
                        }
                    }
                }
                Some("function_call") => {
                    if let (Some(name), Some(arguments_str), Some(call_id)) = (
                        item["name"].as_str(),
                        item["arguments"].as_str(),
                        item["call_id"].as_str(),
                    ) {
                        let arguments: Value = arguments_str.parse().with_context(|| {
                            format!("Tool call '{name}' has non-JSON arguments '{arguments_str}'")
                        })?;
                        tool_calls.push(ToolCall::new(
                            name.to_string(),
                            arguments,
                            Some(call_id.to_string()),
                        ));
                    }
                }
                Some("reasoning") => {
                    if let Some(block) = reasoning_thinking_block(item) {
                        thinking.push(block);
                    }
                }
                _ => {}
            }
        }
    }

    if text.is_empty() && tool_calls.is_empty() {
        if data["status"].as_str() == Some("incomplete") {
            match data["incomplete_details"]["reason"].as_str() {
                Some(reason) => bail!("The response was cut off: {reason}"),
                None => bail!("The response was cut off: {data}"),
            }
        }
        bail!("Invalid response data: {data}");
    }
    Ok(ChatCompletionsOutput {
        text,
        tool_calls,
        thinking,
        usage: openai_parse_responses_usage(&data["usage"]),
    })
}

/// Captures a Responses reasoning item verbatim (summary included) so it can
/// be replayed in later tool-loop rounds; `store: false` makes the replayed
/// `encrypted_content` the model's only access to its prior reasoning.
fn reasoning_thinking_block(item: &Value) -> Option<ThinkingBlock> {
    Some(ThinkingBlock::Reasoning {
        id: item["id"].as_str()?.to_string(),
        summary: item["summary"].clone(),
        encrypted_content: item["encrypted_content"].as_str().map(|v| v.to_string()),
    })
}

/// OpenAI reports cached input under `input_tokens_details` and has no
/// cache-creation concept, so that field stays `None`. The reported
/// `input_tokens` total already includes cached reads, so cached tokens are
/// subtracted here to keep the buckets disjoint like other providers.
fn openai_parse_responses_usage(usage: &Value) -> Option<TokenUsage> {
    if !usage.is_object() {
        return None;
    }

    let cached_tokens = usage["input_tokens_details"]["cached_tokens"].as_u64();
    Some(TokenUsage {
        input_tokens: usage["input_tokens"]
            .as_u64()
            .map(|v| v.saturating_sub(cached_tokens.unwrap_or(0))),
        output_tokens: usage["output_tokens"].as_u64(),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: cached_tokens,
    })
}

pub async fn openai_responses_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
) -> Result<()> {
    let handle = |message: SseMessage| -> Result<bool> {
        if message.data == "[DONE]" {
            return Ok(true);
        }
        let data: Value = serde_json::from_str(&message.data)?;
        debug!("stream-data: {data}");
        openai_responses_handle_event(&data, handler)
    };

    sse_stream(builder, handle).await
}

fn openai_responses_handle_event(data: &Value, handler: &mut SseHandler) -> Result<bool> {
    match data["type"].as_str() {
        Some("response.output_text.delta") => {
            if let Some(delta) = data["delta"].as_str().filter(|v| !v.is_empty()) {
                handler.text(delta)?;
            }
        }
        Some("response.output_item.done") => {
            let item = &data["item"];
            match item["type"].as_str() {
                Some("function_call") => {
                    if let (Some(name), Some(arguments_str), Some(call_id)) = (
                        item["name"].as_str(),
                        item["arguments"].as_str(),
                        item["call_id"].as_str(),
                    ) {
                        let arguments: Value = arguments_str.parse().with_context(|| {
                            format!("Tool call '{name}' has non-JSON arguments '{arguments_str}'")
                        })?;
                        handler.tool_call(ToolCall::new(
                            name.to_string(),
                            arguments,
                            Some(call_id.to_string()),
                        ))?;
                    }
                }
                Some("reasoning") => {
                    if let Some(block) = reasoning_thinking_block(item) {
                        handler.thinking_block(block);
                    }
                }
                _ => {}
            }
        }
        Some("response.completed") => {
            if let Some(usage) = openai_parse_responses_usage(&data["response"]["usage"]) {
                debug!("token-usage: {usage:?}");
                handler.usage(usage);
            }
            return Ok(true);
        }
        Some("response.failed") => match data["response"]["error"]["message"].as_str() {
            Some(message) => bail!("Response failed: {message}"),
            None => bail!("Response failed: {data}"),
        },
        Some("response.incomplete") => {
            // Truncated turns are the most expensive ones; record their
            // billed usage before deciding how to surface the truncation.
            if let Some(usage) = openai_parse_responses_usage(&data["response"]["usage"]) {
                debug!("token-usage: {usage:?}");
                handler.usage(usage);
            }
            // Parity with the non-streaming path (`openai_extract_responses`):
            // partial text or tool calls are returned rather than discarded
            // by a hard error; only an empty truncated response bails.
            if handler.has_received_visible_output() {
                debug!(
                    "response truncated ({}); keeping partial output",
                    data["response"]["incomplete_details"]["reason"]
                );
                return Ok(true);
            }
            match data["response"]["incomplete_details"]["reason"].as_str() {
                Some(reason) => bail!("The response was cut off: {reason}"),
                None => bail!("The response was cut off: {data}"),
            }
        }
        Some("error") => match data["message"].as_str() {
            Some(message) => bail!("Stream error: {message}"),
            None => bail!("Stream error: {data}"),
        },
        _ => {}
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::access_token::set_access_token;
    use crate::config::AppConfig;
    use crate::function::{FunctionDeclaration, ToolResult};
    use chrono::Utc;
    use std::sync::Arc;

    fn build_body(sequence: bool) -> Value {
        let data = ChatCompletionsData {
            messages: vec![
                Message::new(MessageRole::User, MessageContent::Text("hello".to_string())),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::ToolCalls(MessageContentToolCalls {
                        tool_results: vec![],
                        text: "leftover text".to_string(),
                        sequence,
                        round_starts: vec![],
                    }),
                ),
            ],
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            functions: None,
            stream: false,
        };
        openai_build_chat_completions_body(data, &Model::new("openai", "gpt-test"))
    }

    #[test]
    fn non_sequence_empty_tool_results_emits_no_messages() {
        let body = build_body(false);

        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 1, "body: {body}");
    }

    #[test]
    fn sequence_empty_tool_results_emits_no_messages() {
        let body = build_body(true);

        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 1, "body: {body}");
    }

    fn reasoning_block(id: &str) -> ThinkingBlock {
        ThinkingBlock::Reasoning {
            id: id.to_string(),
            summary: json!([{ "type": "summary_text", "text": "thinking" }]),
            encrypted_content: Some("enc123".to_string()),
        }
    }

    fn responses_tool_result(
        id: &str,
        text: Option<&str>,
        thinking: Vec<ThinkingBlock>,
    ) -> ToolResult {
        ToolResult {
            call: ToolCall::new("fs_read".into(), json!({"path": "x"}), Some(id.into())),
            output: json!("ok"),
            text: text.map(|t| t.to_string()),
            thinking,
        }
    }

    fn build_responses_body(
        tool_results: Vec<ToolResult>,
        functions: Option<Vec<FunctionDeclaration>>,
    ) -> Value {
        let data = ChatCompletionsData {
            messages: vec![
                Message::new(MessageRole::User, MessageContent::Text("hello".to_string())),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::ToolCalls(MessageContentToolCalls {
                        tool_results,
                        text: "first round".to_string(),
                        sequence: false,
                        round_starts: vec![],
                    }),
                ),
            ],
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            functions,
            stream: false,
        };
        openai_build_responses_body(data, &Model::new("openai", "gpt-test"))
    }

    #[test]
    fn responses_body_sets_include_and_strict_false() {
        let functions = vec![
            FunctionDeclaration {
                name: "a".to_string(),
                description: "a description".to_string(),
                parameters: Default::default(),
                agent: false,
            },
            FunctionDeclaration {
                name: "b".to_string(),
                description: "b description".to_string(),
                parameters: Default::default(),
                agent: false,
            },
        ];
        let body = build_responses_body(vec![], Some(functions));

        assert_eq!(
            body["include"],
            json!(["reasoning.encrypted_content"]),
            "body: {body}"
        );
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2, "body: {body}");
        for tool in tools {
            assert_eq!(tool["strict"], json!(false), "body: {body}");
        }
    }

    #[test]
    fn responses_replay_orders_reasoning_first_per_round() {
        let body = build_responses_body(
            vec![
                responses_tool_result("call_A", None, vec![reasoning_block("rs_1")]),
                responses_tool_result("call_B", Some("round two"), vec![reasoning_block("rs_2")]),
            ],
            None,
        );

        let input = body["input"].as_array().unwrap();
        let types: Vec<_> = input
            .iter()
            .map(|item| item["type"].as_str().unwrap_or("text"))
            .collect();
        assert_eq!(
            types,
            [
                "text",
                "reasoning",
                "text",
                "function_call",
                "function_call_output",
                "reasoning",
                "text",
                "function_call",
                "function_call_output",
            ],
            "body: {body}"
        );
        assert_eq!(input[1]["id"], "rs_1", "body: {body}");
        assert_eq!(input[1]["encrypted_content"], "enc123", "body: {body}");
        assert_eq!(input[2]["content"], "first round", "body: {body}");
        assert_eq!(input[3]["call_id"], "call_A", "body: {body}");
        assert_eq!(input[5]["id"], "rs_2", "body: {body}");
        assert_eq!(input[6]["content"], "round two", "body: {body}");
        assert_eq!(input[7]["call_id"], "call_B", "body: {body}");
    }

    #[test]
    fn responses_body_skips_anthropic_thinking_blocks() {
        let body = build_responses_body(
            vec![responses_tool_result(
                "call_A",
                None,
                vec![
                    ThinkingBlock::Thinking {
                        thinking: "hmm".to_string(),
                        signature: "sig123".to_string(),
                    },
                    ThinkingBlock::RedactedThinking {
                        data: "b64data".to_string(),
                    },
                ],
            )],
            None,
        );

        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 4, "body: {body}");
        assert!(
            input.iter().all(|item| !matches!(
                item["type"].as_str(),
                Some("thinking" | "redacted_thinking" | "reasoning")
            )),
            "body: {body}"
        );
    }

    #[test]
    fn responses_replay_skips_reasoning_without_encrypted_content() {
        let body = build_responses_body(
            vec![responses_tool_result(
                "call_A",
                None,
                vec![ThinkingBlock::Reasoning {
                    id: "rs_1".to_string(),
                    summary: json!([]),
                    encrypted_content: None,
                }],
            )],
            None,
        );

        let input = body["input"].as_array().unwrap();
        assert!(
            input.iter().all(|item| item["type"] != "reasoning"),
            "body: {body}"
        );
    }

    #[test]
    fn responses_replay_normalizes_null_summary_to_empty_array() {
        let body = build_responses_body(
            vec![responses_tool_result(
                "call_A",
                None,
                vec![ThinkingBlock::Reasoning {
                    id: "rs_1".to_string(),
                    summary: Value::Null,
                    encrypted_content: Some("enc123".to_string()),
                }],
            )],
            None,
        );

        let input = body["input"].as_array().unwrap();
        let item = input
            .iter()
            .find(|item| item["type"] == "reasoning")
            .unwrap();
        assert_eq!(item["summary"], json!([]), "body: {body}");
    }

    #[test]
    fn extract_responses_captures_reasoning_items() {
        let data = json!({
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{ "type": "summary_text", "text": "thinking" }],
                    "encrypted_content": "enc123",
                },
                { "type": "message", "content": [{ "type": "output_text", "text": "answer" }] },
            ]
        });

        let output = openai_extract_responses(&data).unwrap();

        assert_eq!(output.text, "answer");
        assert_eq!(output.thinking.len(), 1);
        match &output.thinking[0] {
            ThinkingBlock::Reasoning {
                id,
                summary,
                encrypted_content,
            } => {
                assert_eq!(id, "rs_1");
                assert_eq!(
                    summary,
                    &json!([{ "type": "summary_text", "text": "thinking" }])
                );
                assert_eq!(encrypted_content.as_deref(), Some("enc123"));
            }
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn extract_responses_parses_usage() {
        let data = json!({
            "output": [
                { "type": "message", "content": [{ "type": "output_text", "text": "answer" }] },
            ],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "input_tokens_details": { "cached_tokens": 60 },
            }
        });

        let output = openai_extract_responses(&data).unwrap();

        let usage = output.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(40));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.cache_creation_input_tokens, None);
        assert_eq!(usage.cache_read_input_tokens, Some(60));
    }

    #[test]
    fn extract_responses_usage_without_cached_details_keeps_input_intact() {
        let data = json!({
            "output": [
                { "type": "message", "content": [{ "type": "output_text", "text": "answer" }] },
            ],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
            }
        });

        let output = openai_extract_responses(&data).unwrap();

        let usage = output.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.cache_read_input_tokens, None);
    }

    #[test]
    fn extract_responses_without_usage_leaves_it_none() {
        let data = json!({
            "output": [
                { "type": "message", "content": [{ "type": "output_text", "text": "answer" }] },
            ]
        });

        assert!(openai_extract_responses(&data).unwrap().usage.is_none());
    }

    #[test]
    fn extract_responses_incomplete_status_surfaces_reason() {
        let data = json!({
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "output": []
        });

        let err = openai_extract_responses(&data).unwrap_err().to_string();

        assert!(
            err.contains("cut off") && err.contains("max_output_tokens"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_responses_incomplete_without_reason_still_bails() {
        let data = json!({ "status": "incomplete", "output": [] });

        let err = openai_extract_responses(&data).unwrap_err().to_string();

        assert!(err.contains("cut off"), "unexpected error: {err}");
    }

    #[test]
    fn responses_stream_delivers_reasoning_block() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let abort_signal = crate::utils::create_abort_signal();
        let mut handler = SseHandler::new(sender, abort_signal);
        let event = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{ "type": "summary_text", "text": "thinking" }],
                "encrypted_content": "enc123",
            }
        });

        let done = openai_responses_handle_event(&event, &mut handler).unwrap();

        assert!(!done);
        let (_, _, thinking, _) = handler.take();
        assert_eq!(thinking.len(), 1);
        assert!(matches!(
            &thinking[0],
            ThinkingBlock::Reasoning { id, encrypted_content, .. }
                if id == "rs_1" && encrypted_content.as_deref() == Some("enc123")
        ));
    }

    #[test]
    fn responses_stream_completed_delivers_usage() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let abort_signal = crate::utils::create_abort_signal();
        let mut handler = SseHandler::new(sender, abort_signal);
        let event = json!({
            "type": "response.completed",
            "response": {
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "input_tokens_details": { "cached_tokens": 60 },
                }
            }
        });

        let done = openai_responses_handle_event(&event, &mut handler).unwrap();

        assert!(done);
        let (_, _, _, usage) = handler.take();
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, Some(40));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.cache_creation_input_tokens, None);
        assert_eq!(usage.cache_read_input_tokens, Some(60));
    }

    #[test]
    fn responses_stream_failed_event_bails_with_message() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let abort_signal = crate::utils::create_abort_signal();
        let mut handler = SseHandler::new(sender, abort_signal);
        let event = json!({
            "type": "response.failed",
            "response": {
                "error": { "code": "server_error", "message": "The model had an issue" }
            }
        });

        let err = openai_responses_handle_event(&event, &mut handler)
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("The model had an issue"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn responses_stream_error_event_bails_with_message() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let abort_signal = crate::utils::create_abort_signal();
        let mut handler = SseHandler::new(sender, abort_signal);
        let event = json!({
            "type": "error",
            "code": "rate_limit_exceeded",
            "message": "Rate limit reached",
        });

        let err = openai_responses_handle_event(&event, &mut handler)
            .unwrap_err()
            .to_string();

        assert!(err.contains("Rate limit reached"), "unexpected error: {err}");
    }

    #[test]
    fn responses_stream_incomplete_event_bails_with_reason() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let abort_signal = crate::utils::create_abort_signal();
        let mut handler = SseHandler::new(sender, abort_signal);
        let event = json!({
            "type": "response.incomplete",
            "response": {
                "incomplete_details": { "reason": "max_output_tokens" }
            }
        });

        let err = openai_responses_handle_event(&event, &mut handler)
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("The response was cut off: max_output_tokens"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn responses_stream_incomplete_without_reason_still_bails() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let abort_signal = crate::utils::create_abort_signal();
        let mut handler = SseHandler::new(sender, abort_signal);
        let event = json!({
            "type": "response.incomplete",
            "response": {}
        });

        let err = openai_responses_handle_event(&event, &mut handler)
            .unwrap_err()
            .to_string();

        assert!(err.contains("The response was cut off"), "unexpected error: {err}");
    }

    #[test]
    fn responses_stream_incomplete_with_partial_output_ends_stream_and_records_usage() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let abort_signal = crate::utils::create_abort_signal();
        let mut handler = SseHandler::new(sender, abort_signal);
        handler.set_silent(true);
        handler.text("partial answer").unwrap();
        let event = json!({
            "type": "response.incomplete",
            "response": {
                "incomplete_details": { "reason": "max_output_tokens" },
                "usage": {
                    "input_tokens": 50,
                    "output_tokens": 10,
                    "input_tokens_details": { "cached_tokens": 25 }
                }
            }
        });

        let done = openai_responses_handle_event(&event, &mut handler).unwrap();

        assert!(done, "partial output should end the stream, not bail");
        let (text, _, _, usage) = handler.take();
        assert_eq!(text, "partial answer");
        let usage = usage.expect("usage should be recorded for truncated turns");
        assert_eq!(usage.input_tokens, Some(25));
        assert_eq!(usage.output_tokens, Some(10));
        assert_eq!(usage.cache_read_input_tokens, Some(25));
    }

    fn openai_config(name: &str, auth: Option<&str>, oauth: Option<OAuthConfig>) -> OpenAIConfig {
        OpenAIConfig {
            name: Some(name.into()),
            api_base: oauth
                .as_ref()
                .map(|_| "https://gateway.example/v1".to_string()),
            auth: auth.map(str::to_string),
            oauth: oauth.map(Box::new),
            ..Default::default()
        }
    }

    fn make_client(config: OpenAIConfig, clients: Vec<ClientConfig>) -> OpenAIClient {
        OpenAIClient {
            app_config: Arc::new(AppConfig {
                clients,
                ..AppConfig::default()
            }),
            config,
            model: Model::new("openai", "gpt-test"),
        }
    }

    fn minimal_oauth_config() -> OAuthConfig {
        serde_yaml::from_str("client_id: gateway\ntoken_url: https://gateway.example/token")
            .unwrap()
    }

    fn prepare(client: &OpenAIClient) -> Result<(RequestData, WireApi)> {
        prepare_with_stream(client, false)
    }

    fn prepare_with_stream(client: &OpenAIClient, stream: bool) -> Result<(RequestData, WireApi)> {
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("hello".to_string()),
            )],
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            functions: None,
            stream,
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(prepare_chat_completions(
                client,
                &ReqwestClient::new(),
                data,
            ))
    }

    fn prepare_embed(client: &OpenAIClient) -> Result<RequestData> {
        let data = EmbeddingsData::new(vec!["hello".to_string()], false);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(prepare_embeddings(client, &ReqwestClient::new(), &data))
    }

    #[test]
    fn oauth_block_without_api_base_is_rejected() {
        let name = "openai-gate-apibase-missing-test";
        let mut config = openai_config(name, Some("oauth"), Some(minimal_oauth_config()));
        config.api_base = None;
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let err = resolve_api_base(&client).unwrap_err().to_string();

        assert!(
            err.contains(name) && err.contains("refusing to fall back"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn catalog_only_oauth_block_without_api_base_is_rejected() {
        let name = "openai-gate-catalog-oauth-test";
        let config = openai_config(name, Some("oauth"), None);
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);
        let catalog = vec![ProviderModels {
            provider: name.into(),
            oauth: Some(minimal_oauth_config()),
            models: vec![],
        }];

        let err = resolve_api_base_against(&client, &catalog)
            .unwrap_err()
            .to_string();

        assert!(
            err.contains(name) && err.contains("refusing to fall back"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn oauth_block_with_api_base_resolves_it() {
        let name = "openai-gate-apibase-set-test";
        let config = openai_config(name, Some("oauth"), Some(minimal_oauth_config()));
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let api_base = resolve_api_base(&client).unwrap();

        assert_eq!(api_base, "https://gateway.example/v1");
    }

    #[test]
    fn no_oauth_block_falls_back_to_stock_api_base() {
        let name = "openai-gate-apibase-fallback-test";
        let config = openai_config(name, None, None);
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let api_base = resolve_api_base(&client).unwrap();

        assert_eq!(api_base, API_BASE);
    }

    #[test]
    fn stock_oauth_without_api_base_routes_to_codex() {
        let name = "openai-gate-codex-test";
        let config = openai_config(name, Some("oauth"), None);
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);
        set_access_token(name, "codex-at".into(), Utc::now().timestamp() + 3600, None);

        let (request_data, wire) = prepare(&client).unwrap();

        assert_eq!(wire, WireApi::Responses);
        assert_eq!(request_data.url, CODEX_API_ENDPOINT);
    }

    #[test]
    fn config_oauth_block_skips_codex_routing() {
        let name = "openai-gate-gateway-codex-test";
        let config = openai_config(name, Some("oauth"), Some(minimal_oauth_config()));
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);
        set_access_token(
            name,
            "gateway-at".into(),
            Utc::now().timestamp() + 3600,
            None,
        );

        let (request_data, wire) = prepare(&client).unwrap();

        assert_eq!(wire, WireApi::Chat);
        assert_eq!(
            request_data.url,
            "https://gateway.example/v1/chat/completions"
        );
    }

    /// A config-oauth gateway missing its required `api_base` must never
    /// count as stock-openai traffic: its provider resolves as non-stock, so
    /// the wire resolver sees `stock=false`, and the request dies on the
    /// missing-api_base config error instead of adopting codex routing.
    #[test]
    fn config_oauth_without_api_base_is_not_stock_openai_traffic() {
        let name = "openai-gate-no-apibase-wire-test";
        let mut config = openai_config(name, Some("oauth"), Some(minimal_oauth_config()));
        config.api_base = None;
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let (_, is_stock) = resolve_oauth_provider(&client).unwrap();
        assert!(!is_stock, "an inline oauth block is a config provider");

        let err = prepare(&client).unwrap_err().to_string();
        assert!(
            err.contains("refusing to fall back"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn oauth_block_without_auth_oauth_is_rejected() {
        let name = "openai-gate-contradiction-test";
        let mut config = openai_config(name, None, Some(minimal_oauth_config()));
        config.api_key = Some("sk-test".into());
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let err = match prepare(&client) {
            Ok(_) => panic!("expected the contradictory config to be rejected"),
            Err(err) => err.to_string(),
        };

        assert!(
            err.contains("has an `oauth:` block configured but `auth: oauth` is not set"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn config_oauth_extra_request_headers_reach_request() {
        let name = "openai-gate-extra-headers-test";
        let oauth: OAuthConfig = serde_yaml::from_str(
            "client_id: gateway\ntoken_url: https://gateway.example/token\nextra_request_headers:\n  x-gateway-tenant: acme",
        )
        .unwrap();
        let config = openai_config(name, Some("oauth"), Some(oauth));
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);
        set_access_token(
            name,
            "gateway-at".into(),
            Utc::now().timestamp() + 3600,
            None,
        );

        let (request_data, _) = prepare(&client).unwrap();

        assert_eq!(
            request_data
                .headers
                .get("x-gateway-tenant")
                .map(String::as_str),
            Some("acme")
        );
    }

    #[test]
    fn config_oauth_embeddings_url_trims_trailing_slash() {
        let name = "openai-gate-embed-oauth-test";
        let mut config = openai_config(name, Some("oauth"), Some(minimal_oauth_config()));
        config.api_base = Some("https://gateway.example/v1/".into());
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);
        set_access_token(
            name,
            "gateway-at".into(),
            Utc::now().timestamp() + 3600,
            None,
        );

        let request_data = prepare_embed(&client).unwrap();

        assert_eq!(request_data.url, "https://gateway.example/v1/embeddings");
        assert_eq!(
            request_data
                .headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer gateway-at")
        );
    }

    #[test]
    fn oauth_block_without_api_base_is_rejected_in_embeddings() {
        let name = "openai-gate-embed-apibase-missing-test";
        let mut config = openai_config(name, Some("oauth"), Some(minimal_oauth_config()));
        config.api_base = None;
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let err = match prepare_embed(&client) {
            Ok(_) => panic!("expected the missing api_base to be rejected"),
            Err(err) => err.to_string(),
        };

        assert!(
            err.contains(name) && err.contains("refusing to fall back"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn catalog_oauth_block_with_api_key_auth_falls_back_to_stock_api_base() {
        let name = "openai-gate-catalog-apikey-test";
        let mut config = openai_config(name, None, None);
        config.api_key = Some("sk-test".into());
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);
        let catalog = vec![ProviderModels {
            provider: name.into(),
            oauth: Some(minimal_oauth_config()),
            models: vec![],
        }];

        let api_base = resolve_api_base_against(&client, &catalog).unwrap();

        assert_eq!(api_base, API_BASE);
    }

    #[test]
    fn missing_config_entry_without_inline_oauth_resolves_stock_provider() {
        let config = openai_config("openai-gate-missing-entry-test", Some("oauth"), None);
        let client = make_client(config, vec![]);

        let (_, is_stock) = resolve_oauth_provider(&client).unwrap();

        assert!(is_stock);
    }

    #[test]
    fn missing_config_entry_with_inline_oauth_is_a_hard_error() {
        let config = openai_config(
            "openai-gate-missing-entry-inline-test",
            Some("oauth"),
            Some(minimal_oauth_config()),
        );
        let client = make_client(config, vec![]);

        let err = match resolve_oauth_provider(&client) {
            Ok(_) => panic!("expected the missing ClientConfig entry to be a hard error"),
            Err(err) => err.to_string(),
        };

        assert!(
            err.contains("Could not locate ClientConfig entry"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn stock_oauth_attaches_chatgpt_account_id_header() {
        let name = "openai-gate-account-id-stock-test";
        let config = openai_config(name, Some("oauth"), None);
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);
        set_access_token(
            name,
            "codex-at".into(),
            Utc::now().timestamp() + 3600,
            Some("acct-123".into()),
        );

        let (request_data, _) = prepare(&client).unwrap();

        assert_eq!(
            request_data
                .headers
                .get("ChatGPT-Account-Id")
                .map(String::as_str),
            Some("acct-123")
        );
    }

    #[test]
    fn config_oauth_block_omits_chatgpt_account_id_header() {
        let name = "openai-gate-account-id-gateway-test";
        let config = openai_config(name, Some("oauth"), Some(minimal_oauth_config()));
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);
        set_access_token(
            name,
            "gateway-at".into(),
            Utc::now().timestamp() + 3600,
            Some("acct-123".into()),
        );

        let (request_data, _) = prepare(&client).unwrap();

        assert!(
            !request_data.headers.contains_key("ChatGPT-Account-Id"),
            "headers: {:?}",
            request_data.headers
        );
    }

    #[test]
    fn auth_mismatch_bails_before_missing_api_base_guard() {
        let name = "openai-gate-bail-order-test";
        let mut config = openai_config(name, None, Some(minimal_oauth_config()));
        config.api_base = None;
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let err = match prepare(&client) {
            Ok(_) => panic!("expected the contradictory config to be rejected"),
            Err(err) => err.to_string(),
        };

        assert!(
            err.contains("has an `oauth:` block configured but `auth: oauth` is not set")
                && !err.contains("refusing to fall back"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn shipped_catalog_resolves_stock_openai_provider() {
        let cc = ClientConfig::OpenAIConfig(openai_config("openai", Some("oauth"), None));
        let bundled: Vec<ProviderModels> = serde_yaml::from_str(MODELS_YAML).unwrap();

        let (_, is_stock) = oauth::openai_oauth_provider_for_client(&cc, &bundled);

        assert!(
            is_stock,
            "bundled models.yaml must not carry an openai oauth block: it would silently disable codex routing for stock ChatGPT oauth users"
        );
    }

    #[test]
    fn shipped_catalog_o_series_selects_responses_sub_patches() {
        let bundled: Vec<ProviderModels> = serde_yaml::from_str(MODELS_YAML).unwrap();
        let openai = bundled
            .iter()
            .find(|p| p.provider == "openai")
            .expect("bundled models.yaml must carry an openai provider");

        for (name, effort) in [
            ("o4-mini", None),
            ("o3", None),
            ("o3-mini", None),
            ("o4-mini-high", Some("high")),
            ("o3-high", Some("high")),
            ("o3-mini-high", Some("high")),
        ] {
            let data = openai
                .models
                .iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("model '{name}' missing from bundled models.yaml"));
            let config = api_key_config("openai", None);
            let mut client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);
            client.model = Model::from_config("openai", std::slice::from_ref(data)).remove(0);
            let mut request_data = RequestData::new(
                format!("{API_BASE}/responses"),
                json!({ "model": name, "temperature": 0.5, "top_p": 0.9 }),
            )
            .wire(WireApi::Responses);

            client.patch_request_data(&mut request_data);

            assert!(
                request_data.body.get("temperature").is_none(),
                "{name}: {}",
                request_data.body
            );
            assert!(
                request_data.body.get("top_p").is_none(),
                "{name}: {}",
                request_data.body
            );
            match effort {
                Some(effort) => assert_eq!(
                    request_data.body["reasoning"]["effort"],
                    json!(effort),
                    "{name}: {}",
                    request_data.body
                ),
                None => assert!(
                    request_data.body.get("reasoning").is_none(),
                    "{name}: {}",
                    request_data.body
                ),
            }
        }
    }

    #[test]
    fn wire_api_deserializes_on_openai_config() {
        let config: OpenAIConfig = serde_yaml::from_str("wire_api: responses").unwrap();
        assert_eq!(config.wire_api, Some(WireApi::Responses));

        let config: OpenAIConfig = serde_yaml::from_str("wire_api: chat").unwrap();
        assert_eq!(config.wire_api, Some(WireApi::Chat));
    }

    #[test]
    fn bogus_wire_api_is_a_config_error() {
        let err = serde_yaml::from_str::<OpenAIConfig>("wire_api: bogus")
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("responses") && err.contains("chat"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolver_codex_defaults_to_responses() {
        assert_eq!(
            resolve_wire_api(None, true, false).unwrap(),
            WireApi::Responses
        );
        assert_eq!(
            resolve_wire_api(Some(WireApi::Responses), true, false).unwrap(),
            WireApi::Responses
        );
    }

    #[test]
    fn resolver_rejects_explicit_chat_on_codex() {
        let err = resolve_wire_api(Some(WireApi::Chat), true, false)
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("only speaks the Responses API"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolver_explicit_wins_off_codex() {
        for stock in [false, true] {
            assert_eq!(
                resolve_wire_api(Some(WireApi::Responses), false, stock).unwrap(),
                WireApi::Responses
            );
            assert_eq!(
                resolve_wire_api(Some(WireApi::Chat), false, stock).unwrap(),
                WireApi::Chat
            );
        }
    }

    #[test]
    fn resolver_defaults_stock_openai_to_responses() {
        assert_eq!(
            resolve_wire_api(None, false, true).unwrap(),
            WireApi::Responses
        );
    }

    #[test]
    fn resolver_defaults_to_chat_off_stock() {
        assert_eq!(resolve_wire_api(None, false, false).unwrap(), WireApi::Chat);
    }

    fn api_key_config(name: &str, wire_api: Option<WireApi>) -> OpenAIConfig {
        OpenAIConfig {
            name: Some(name.into()),
            api_key: Some("sk-test".into()),
            wire_api,
            ..Default::default()
        }
    }

    #[test]
    fn api_key_openai_defaults_to_the_responses_wire() {
        let config = api_key_config("openai-wire-default-test", None);
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let (request_data, wire) = prepare(&client).unwrap();

        assert_eq!(wire, WireApi::Responses);
        assert_eq!(request_data.url, format!("{API_BASE}/responses"));
        assert!(
            request_data.body.get("input").is_some() && request_data.body.get("messages").is_none(),
            "body: {}",
            request_data.body
        );
    }

    #[test]
    fn api_key_openai_with_custom_api_base_defaults_to_the_chat_wire() {
        let mut config = api_key_config("openai-wire-custom-base-test", None);
        config.api_base = Some("https://gateway.example/v1".into());
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let (request_data, wire) = prepare(&client).unwrap();

        assert_eq!(wire, WireApi::Chat);
        assert_eq!(
            request_data.url,
            "https://gateway.example/v1/chat/completions"
        );
        assert!(
            request_data.body.get("messages").is_some(),
            "body: {}",
            request_data.body
        );
    }

    #[test]
    fn explicit_responses_wire_posts_a_responses_body_to_the_responses_endpoint() {
        let config = api_key_config("openai-wire-responses-test", Some(WireApi::Responses));
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let (request_data, wire) = prepare(&client).unwrap();

        assert_eq!(wire, WireApi::Responses);
        assert_eq!(request_data.url, format!("{API_BASE}/responses"));
        assert_eq!(request_data.body["store"], json!(false));
        assert!(
            request_data.body.get("input").is_some() && request_data.body.get("messages").is_none(),
            "body: {}",
            request_data.body
        );
    }

    #[test]
    fn explicit_responses_wire_forks_the_streaming_path_too() {
        let config = api_key_config("openai-wire-responses-stream-test", Some(WireApi::Responses));
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let (request_data, wire) = prepare_with_stream(&client, true).unwrap();

        assert_eq!(wire, WireApi::Responses);
        assert_eq!(request_data.url, format!("{API_BASE}/responses"));
        assert_eq!(request_data.body["stream"], json!(true));
        assert!(
            request_data.body.get("input").is_some(),
            "body: {}",
            request_data.body
        );
    }

    #[test]
    fn codex_with_explicit_chat_wire_is_rejected() {
        let name = "openai-codex-chat-wire-test";
        let mut config = openai_config(name, Some("oauth"), None);
        config.wire_api = Some(WireApi::Chat);
        let client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);

        let err = prepare(&client).unwrap_err().to_string();

        assert!(
            err.contains("only speaks the Responses API"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn codex_responses_body_ignores_a_chat_shaped_model_patch() {
        let name = "openai-codex-chat-patch-test";
        let config = openai_config(name, Some("oauth"), None);
        let mut model_data = ModelData::new("gpt-test");
        model_data.patch = Some(json!({
            "body": {
                "max_tokens": null,
                "temperature": null,
                "top_p": null,
                "reasoning_effort": "high",
            }
        }));
        let mut client = make_client(config.clone(), vec![ClientConfig::OpenAIConfig(config)]);
        client.model = Model::from_config("openai", &[model_data]).remove(0);
        set_access_token(name, "codex-at".into(), Utc::now().timestamp() + 3600, None);

        let (mut request_data, wire) = prepare(&client).unwrap();
        assert_eq!(wire, WireApi::Responses);
        let body_before = request_data.body.clone();

        client.patch_request_data(&mut request_data);

        assert_eq!(request_data.body, body_before, "a chat-shaped model patch must not merge into a responses body");
    }
}
