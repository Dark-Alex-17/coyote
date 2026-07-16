use super::access_token::{get_access_token, get_access_token_account_id};
use super::oauth::{self, OAuthProvider};
use super::openai_oauth::OpenAIOAuthProvider;
use super::*;

use crate::utils::strip_think_tag;

use anyhow::{Context, Result, bail};
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
        let uses_codex =
            self.config.auth.as_deref() == Some("oauth") && self.get_api_base().is_err();
        let request_data = prepare_chat_completions(self, client, data).await?;
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
        let uses_codex =
            self.config.auth.as_deref() == Some("oauth") && self.get_api_base().is_err();
        let request_data = prepare_chat_completions(self, client, data).await?;
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
) -> Result<RequestData> {
    let uses_oauth = self_.config.auth.as_deref() == Some("oauth");
    let has_custom_base = self_.get_api_base().is_ok();

    let uses_codex = uses_oauth && !has_custom_base;

    let url = if uses_codex {
        CODEX_API_ENDPOINT.to_string()
    } else {
        let api_base = self_
            .get_api_base()
            .unwrap_or_else(|_| API_BASE.to_string());
        format!("{}/chat/completions", api_base.trim_end_matches('/'))
    };

    let body = if uses_codex {
        openai_build_responses_body(data, &self_.model)
    } else {
        openai_build_chat_completions_body(data, &self_.model)
    };

    let mut request_data = RequestData::new(url, body);

    if uses_oauth {
        let provider = OpenAIOAuthProvider;
        let ready = oauth::prepare_oauth_access_token(client, &provider, self_.name()).await?;

        if !ready {
            bail!(
                "OAuth configured but no tokens found for '{}'. Run: 'coyote --authenticate {}' or '.authenticate' in the REPL",
                self_.name(),
                self_.name()
            );
        }

        let token = get_access_token(self_.name())?;
        request_data.bearer_auth(token);

        if let Some(account_id) = get_access_token_account_id(self_.name()) {
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

    Ok(request_data)
}

async fn prepare_embeddings(
    self_: &OpenAIClient,
    client: &ReqwestClient,
    data: &EmbeddingsData,
) -> Result<RequestData> {
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let url = format!("{api_base}/embeddings");

    let body = openai_build_embeddings_body(data, &self_.model);

    let mut request_data = RequestData::new(url, body);

    if self_.config.auth.as_deref() == Some("oauth") {
        let provider = OpenAIOAuthProvider;
        let ready = oauth::prepare_oauth_access_token(client, &provider, self_.name()).await?;

        if !ready {
            bail!(
                "OAuth configured but no tokens found for '{}'. Run: 'coyote --authenticate {}' or '.authenticate' in the REPL",
                self_.name(),
                self_.name()
            );
        }

        let token = get_access_token(self_.name())?;
        request_data.bearer_auth(token);
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
    let messages: Vec<Value> = messages
        .into_iter()
        .enumerate()
        .flat_map(|(i, message)| {
            let Message { role, content } = message;
            match content {
                MessageContent::ToolCalls(MessageContentToolCalls {
                    tool_results,
                    text: _,
                    sequence,
                }) => {
                    if !sequence {
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
                        let mut messages = vec![
                            json!({ "role": MessageRole::Assistant, "tool_calls": tool_calls }),
                        ];
                        for tool_result in tool_results {
                            messages.push(json!({
                                "role": "tool",
                                "content": tool_result.output.to_string(),
                                "tool_call_id": tool_result.call.id,
                            }));
                        }
                        messages
                    } else {
                        tool_results.into_iter().flat_map(|tool_result| {
                            vec![
                                json!({
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
                                }),
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
    let output = ChatCompletionsOutput { text, tool_calls };
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
                    text: _,
                    sequence: _,
                }) => tool_results
                    .into_iter()
                    .flat_map(|tool_result| {
                        vec![
                            json!({
                                "type": "function_call",
                                "call_id": tool_result.call.id,
                                "name": tool_result.call.name,
                                "arguments": tool_result.call.arguments.to_string(),
                            }),
                            json!({
                                "type": "function_call_output",
                                "call_id": tool_result.call.id,
                                "output": tool_result.output.to_string(),
                            }),
                        ]
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
    Ok(ChatCompletionsOutput { text, tool_calls })
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
