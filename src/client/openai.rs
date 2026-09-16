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
        let (request_data, uses_codex) = prepare_chat_completions(self, client, data).await?;
        let builder = self.request_builder(client, request_data);
        if uses_codex {
            openai_responses_chat_completions(builder, self.model()).await
        } else {
            openai_chat_completions(builder, self.model()).await
        }
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        let (request_data, uses_codex) = prepare_chat_completions(self, client, data).await?;
        let builder = self.request_builder(client, request_data);

        if uses_codex {
            openai_responses_streaming(builder, handler).await
        } else {
            openai_chat_completions_streaming(builder, handler, self.model()).await
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

async fn prepare_chat_completions(
    self_: &OpenAIClient,
    client: &ReqwestClient,
    data: ChatCompletionsData,
) -> Result<(RequestData, bool)> {
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

    let url = if uses_codex {
        CODEX_API_ENDPOINT.to_string()
    } else {
        let api_base = resolve_api_base(self_)?;
        format!("{}/chat/completions", api_base.trim_end_matches('/'))
    };

    let body = if uses_codex {
        openai_build_responses_body(data, &self_.model)
    } else {
        openai_build_chat_completions_body(data, &self_.model)
    };

    let mut request_data = RequestData::new(url, body);

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

    Ok((request_data, uses_codex))
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
                    sequence: _,
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
                _ => {}
            }
        }
    }

    if text.is_empty() && tool_calls.is_empty() {
        bail!("Invalid response data: {data}");
    }
    Ok(ChatCompletionsOutput {
        text,
        tool_calls,
        ..Default::default()
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

        match data["type"].as_str() {
            Some("response.output_text.delta") => {
                if let Some(delta) = data["delta"].as_str().filter(|v| !v.is_empty()) {
                    handler.text(delta)?;
                }
            }
            Some("response.output_item.done") => {
                let item = &data["item"];
                if item["type"].as_str() == Some("function_call")
                    && let (Some(name), Some(arguments_str), Some(call_id)) = (
                        item["name"].as_str(),
                        item["arguments"].as_str(),
                        item["call_id"].as_str(),
                    )
                {
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
            Some("response.completed") => {
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    };

    sse_stream(builder, handle).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::access_token::set_access_token;
    use crate::config::AppConfig;
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

    fn prepare(client: &OpenAIClient) -> Result<(RequestData, bool)> {
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("hello".to_string()),
            )],
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            functions: None,
            stream: false,
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

        let (request_data, uses_codex) = prepare(&client).unwrap();

        assert!(uses_codex);
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

        let (request_data, uses_codex) = prepare(&client).unwrap();

        assert!(!uses_codex);
        assert_eq!(
            request_data.url,
            "https://gateway.example/v1/chat/completions"
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
}
