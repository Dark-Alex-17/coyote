use std::collections::HashSet;
use std::mem;

use super::access_token::get_access_token;
use super::oauth::{self, OAuthConfig};
use super::*;

use crate::utils::strip_think_tag;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

const API_BASE: &str = "https://api.anthropic.com/v1";
const CLAUDE_CODE_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeConfig {
    pub name: Option<String>,
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    pub auth: Option<String>,
    pub oauth: Option<Box<OAuthConfig>>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    pub prompt_cache: Option<bool>,
    pub patch: Option<RequestPatch>,
    pub extra: Option<ExtraConfig>,
}

impl ClaudeClient {
    config_get_fn!(api_key, get_api_key);
    config_get_fn!(api_base, get_api_base);

    create_oauth_supported_client_config!();
}

#[async_trait::async_trait]
impl Client for ClaudeClient {
    client_common_fns!();

    fn supports_oauth(&self) -> bool {
        self.config.auth.as_deref() == Some("oauth")
    }

    async fn chat_completions_inner(
        &self,
        client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput> {
        let request_data = prepare_chat_completions(self, client, data).await?;
        let builder = self.request_builder(client, request_data);
        claude_chat_completions(builder, self.model()).await
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        let request_data = prepare_chat_completions(self, client, data).await?;
        let builder = self.request_builder(client, request_data);
        claude_chat_completions_streaming(builder, handler, self.model()).await
    }
}

async fn prepare_chat_completions(
    self_: &ClaudeClient,
    client: &ReqwestClient,
    data: ChatCompletionsData,
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

    let url = format!("{}/messages", api_base.trim_end_matches('/'));
    let body = claude_build_chat_completions_body(
        data,
        &self_.model,
        self_.config.prompt_cache.unwrap_or(true),
    )?;

    let mut request_data = RequestData::new(url, body);

    request_data.header("anthropic-version", "2023-06-01");

    if uses_oauth {
        let client_name = self_.name();
        let cc = locate_client_config(self_)?;
        let (provider, uses_stock_provider) =
            oauth::claude_oauth_provider_for_client(cc, &ALL_PROVIDER_MODELS);
        let ready = oauth::prepare_oauth_access_token(client, &*provider, client_name).await?;
        if !ready {
            bail!(
                "OAuth configured but no tokens found for '{}'. Run: 'coyote --authenticate {}' or '.authenticate' in the REPL",
                client_name,
                client_name
            );
        }
        let token = get_access_token(client_name)?;
        request_data.bearer_auth(token);
        for (key, value) in provider.extra_request_headers() {
            request_data.header(key, value);
        }
        if uses_stock_provider {
            inject_oauth_system_prompt(&mut request_data.body);
        }
    } else if let Ok(api_key) = self_.get_api_key() {
        request_data.header("x-api-key", api_key);
    } else {
        bail!(
            "No authentication configured for '{}'. Set `api_key` or use `auth: oauth` with `coyote --authenticate {}`.",
            self_.name(),
            self_.name()
        );
    }

    Ok(request_data)
}

fn resolve_api_base(self_: &ClaudeClient) -> Result<String> {
    resolve_api_base_against(self_, &ALL_PROVIDER_MODELS)
}

fn resolve_api_base_against(
    self_: &ClaudeClient,
    all_provider_models: &[ProviderModels],
) -> Result<String> {
    if let Ok(api_base) = self_.get_api_base() {
        return Ok(api_base);
    }
    let uses_config_oauth = self_.config.auth.as_deref() == Some("oauth")
        && match locate_client_config(self_) {
            Ok(cc) => oauth::config_oauth_for_client(cc, all_provider_models).is_some(),
            // Safe: oauth prepare paths bail on this locate failure before attaching any token.
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

fn locate_client_config(self_: &ClaudeClient) -> Result<&ClientConfig> {
    let client_name = self_.name();
    self_
        .app_config()
        .clients
        .iter()
        .find(|cc| {
            matches!(
                cc,
                ClientConfig::ClaudeConfig(c)
                if c.name.as_deref().unwrap_or(ClaudeClient::NAME) == client_name
            )
        })
        .ok_or_else(|| anyhow!("Could not locate ClientConfig entry for '{}'", client_name))
}

/// Anthropic requires OAuth-authenticated requests to include a Claude Code
/// system prompt prefix in order to consider a request body as "valid".
///
/// This behavior was discovered 2026-03-17.
///
/// The prefix must be in its **own** top-level system block. Concatenating it
/// with role / session content into a single block causes Anthropic to reject
/// the request with `rate_limit_error`. Any pre-existing system content is
/// preserved as additional blocks after the prefix.
fn inject_oauth_system_prompt(body: &mut Value) {
    let existing_blocks: Vec<Value> = match body.get("system") {
        Some(Value::String(s)) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![json!({ "type": "text", "text": s })]
            }
        }
        Some(Value::Array(blocks)) => blocks.clone(),
        _ => Vec::new(),
    };

    let already_injected = existing_blocks
        .first()
        .and_then(|b| b.get("text").and_then(|t| t.as_str()))
        .map(|t| t == CLAUDE_CODE_PREFIX)
        .unwrap_or(false);
    if already_injected {
        return;
    }

    let mut system = vec![json!({ "type": "text", "text": CLAUDE_CODE_PREFIX })];
    system.extend(existing_blocks);
    body["system"] = Value::Array(system);
}

pub async fn claude_chat_completions(
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
    claude_extract_chat_completions(&data)
}

pub async fn claude_chat_completions_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
    _model: &Model,
) -> Result<()> {
    let mut function_name = String::new();
    let mut function_arguments = String::new();
    let mut function_id = String::new();
    let mut reasoning_state = 0;
    let mut thinking_text = String::new();
    let mut thinking_signature = String::new();
    let handle = |message: SseMessage| -> Result<bool> {
        let data: Value = serde_json::from_str(&message.data)?;
        debug!("stream-data: {data}");
        if let Some(typ) = data["type"].as_str() {
            match typ {
                "content_block_start" => {
                    if let (Some("redacted_thinking"), Some(redacted_data)) = (
                        data["content_block"]["type"].as_str(),
                        data["content_block"]["data"].as_str(),
                    ) {
                        handler.thinking_block(ThinkingBlock::RedactedThinking {
                            data: redacted_data.to_string(),
                        });
                    }
                    if let (Some("tool_use"), Some(name), Some(id)) = (
                        data["content_block"]["type"].as_str(),
                        data["content_block"]["name"].as_str(),
                        data["content_block"]["id"].as_str(),
                    ) {
                        if !function_name.is_empty() {
                            let arguments: Value = if function_arguments.is_empty() {
                                json!({})
                            } else {
                                function_arguments.parse().with_context(|| {
                                    format!("Tool call '{function_name}' has non-JSON arguments '{function_arguments}'")
                                })?
                            };
                            handler.tool_call(ToolCall::new(
                                function_name.clone(),
                                arguments,
                                Some(function_id.clone()),
                            ))?;
                        }
                        function_name = name.into();
                        function_arguments.clear();
                        function_id = id.into();
                    }
                }
                "content_block_delta" => {
                    if let Some(text) = data["delta"]["text"].as_str() {
                        handler.text(text)?;
                    } else if let Some(text) = data["delta"]["thinking"].as_str() {
                        reasoning_state = 1;
                        thinking_text.push_str(text);
                    } else if let Some(signature) = data["delta"]["signature"].as_str() {
                        thinking_signature.push_str(signature);
                    } else if let (true, Some(partial_json)) = (
                        !function_name.is_empty(),
                        data["delta"]["partial_json"].as_str(),
                    ) {
                        function_arguments.push_str(partial_json);
                    }
                }
                "content_block_stop" => {
                    if reasoning_state == 1 {
                        reasoning_state = 0;
                        handler.thinking_block(ThinkingBlock::Thinking {
                            thinking: mem::take(&mut thinking_text),
                            signature: mem::take(&mut thinking_signature),
                        });
                    }
                    if !function_name.is_empty() {
                        let arguments: Value = if function_arguments.is_empty() {
                            json!({})
                        } else {
                            function_arguments.parse().with_context(|| {
                                format!("Tool call '{function_name}' has non-JSON arguments '{function_arguments}'")
                            })?
                        };
                        handler.tool_call(ToolCall::new(
                            mem::take(&mut function_name),
                            arguments,
                            Some(mem::take(&mut function_id)),
                        ))?;
                        function_arguments.clear();
                    }
                }
                "message_start" => {
                    if let Some(usage) = claude_parse_usage(&data["message"]["usage"]) {
                        debug!("token-usage: {usage:?}");
                        handler.usage(usage);
                    }
                }
                "message_delta" => {
                    if let Some(usage) = claude_parse_usage(&data["usage"]) {
                        debug!("token-usage: {usage:?}");
                        handler.usage(usage);
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    };

    sse_stream(builder, handle).await
}

pub fn claude_build_chat_completions_body(
    data: ChatCompletionsData,
    model: &Model,
    prompt_cache: bool,
) -> Result<Value> {
    let ChatCompletionsData {
        mut messages,
        temperature,
        top_p,
        reasoning_effort,
        functions,
        stream,
    } = data;

    let system_message = extract_system_message(&mut messages);

    let mut network_image_urls = vec![];

    let messages_len = messages.len();
    let messages: Vec<Value> = messages
        .into_iter()
        .enumerate()
        .flat_map(|(i, message)| {
            let Message { role, content } = message;
            match content {
                MessageContent::Text(text) if role.is_assistant() && i != messages_len - 1 => {
                    vec![json!({ "role": role, "content": strip_think_tag(&text) })]
                }
                MessageContent::Text(text) => vec![json!({
                    "role": role,
                    "content": text,
                })],
                MessageContent::Array(list) => {
                    let content: Vec<_> = list
                        .into_iter()
                        .map(|item| match item {
                            MessageContentPart::Text { text } => {
                                json!({"type": "text", "text": text})
                            }
                            MessageContentPart::ImageUrl {
                                image_url: ImageUrl { url },
                            } => {
                                if let Some((mime_type, data)) = url
                                    .strip_prefix("data:")
                                    .and_then(|v| v.split_once(";base64,"))
                                {
                                    json!({
                                        "type": "image",
                                        "source": {
                                            "type": "base64",
                                            "media_type": mime_type,
                                            "data": data,
                                        }
                                    })
                                } else {
                                    network_image_urls.push(url.clone());
                                    json!({ "url": url })
                                }
                            }
                        })
                        .collect();
                    vec![json!({
                        "role": role,
                        "content": content,
                    })]
                }
                MessageContent::ToolCalls(MessageContentToolCalls {
                    tool_results,
                    text,
                    sequence,
                }) => {
                    if !sequence {
                        let mut assistant_parts = vec![];
                        let mut user_parts = vec![];
                        for (index, tool_result) in tool_results.iter().enumerate() {
                            for block in &tool_result.thinking {
                                // Responses reasoning items are OpenAI-only;
                                // drop them when the session moves to Claude.
                                if !matches!(block, ThinkingBlock::Reasoning { .. }) {
                                    assistant_parts.push(json!(block));
                                }
                            }
                            let round_text = if index == 0 && !text.is_empty() {
                                Some(text.as_str())
                            } else {
                                tool_result.text.as_deref()
                            };
                            if let Some(round_text) = round_text {
                                let round_text = strip_think_tag(round_text);
                                let round_text = round_text.trim();
                                if !round_text.is_empty() {
                                    assistant_parts.push(json!({
                                        "type": "text",
                                        "text": round_text,
                                    }))
                                }
                            }
                            assistant_parts.push(json!({
                                "type": "tool_use",
                                "id": tool_result.call.id,
                                "name": tool_result.call.name,
                                "input": tool_result.call.arguments,
                            }));
                            user_parts.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_result.call.id,
                                "content": tool_result.output.to_string(),
                            }));
                        }
                        // Empty tool_results (reachable via deserialized sessions) must not emit an empty pair.
                        if assistant_parts.is_empty() {
                            vec![]
                        } else {
                            vec![
                                json!({ "role": "assistant", "content": assistant_parts }),
                                json!({ "role": "user", "content": user_parts }),
                            ]
                        }
                    } else {
                        // One pair per round: Claude can reuse tool_use IDs across API calls.
                        // A round boundary is detected by the presence of round text, but
                        // rounds where the model emitted only tool calls (no narration)
                        // carry no text marker. As a backstop, also split whenever a
                        // tool_use ID would repeat within the current assistant message —
                        // the API rejects duplicate tool_use IDs in a single message.
                        let mut messages = vec![];
                        let mut assistant_parts: Vec<serde_json::Value> = vec![];
                        let mut user_parts: Vec<serde_json::Value> = vec![];
                        let mut chunk_ids: HashSet<&str> = HashSet::new();
                        for (index, tool_result) in tool_results.iter().enumerate() {
                            let id_collision = tool_result
                                .call
                                .id
                                .as_deref()
                                .is_some_and(|id| chunk_ids.contains(id));
                            if index > 0 && (tool_result.text.is_some() || id_collision) {
                                messages.push(
                                    json!({ "role": "assistant", "content": assistant_parts }),
                                );
                                messages.push(json!({ "role": "user", "content": user_parts }));
                                assistant_parts = vec![];
                                user_parts = vec![];
                                chunk_ids.clear();
                            }
                            if let Some(id) = tool_result.call.id.as_deref() {
                                chunk_ids.insert(id);
                            }
                            for block in &tool_result.thinking {
                                if !matches!(block, ThinkingBlock::Reasoning { .. }) {
                                    assistant_parts.push(json!(block));
                                }
                            }
                            let round_text = if index == 0 && !text.is_empty() {
                                Some(text.as_str())
                            } else {
                                tool_result.text.as_deref()
                            };
                            if let Some(round_text) = round_text {
                                let round_text = strip_think_tag(round_text);
                                let round_text = round_text.trim();
                                if !round_text.is_empty() {
                                    assistant_parts.push(json!({
                                        "type": "text",
                                        "text": round_text,
                                    }))
                                }
                            }
                            assistant_parts.push(json!({
                                "type": "tool_use",
                                "id": tool_result.call.id,
                                "name": tool_result.call.name,
                                "input": tool_result.call.arguments,
                            }));
                            user_parts.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_result.call.id,
                                "content": tool_result.output.to_string(),
                            }));
                        }
                        if !assistant_parts.is_empty() {
                            messages
                                .push(json!({ "role": "assistant", "content": assistant_parts }));
                            messages.push(json!({ "role": "user", "content": user_parts }));
                        }
                        messages
                    }
                }
            }
        })
        .collect();

    if !network_image_urls.is_empty() {
        bail!(
            "The model does not support network images: {:?}",
            network_image_urls
        );
    }

    let mut body = json!({
        "model": model.real_name(),
        "messages": messages,
    });
    if let Some(v) = system_message {
        body["system"] = v.into();
    }
    if let Some(v) = model.max_tokens_param() {
        body["max_tokens"] = v.into();
    }
    if let Some(v) = temperature {
        body["temperature"] = v.into();
    }
    if let Some(v) = top_p {
        body["top_p"] = v.into();
    }
    if let Some(v) = reasoning_effort {
        body["output_config"] = json!({ "effort": v });
    }
    if stream {
        body["stream"] = true.into();
    }
    if let Some(functions) = functions {
        body["tools"] = functions
            .iter()
            .map(|v| {
								if v.parameters.is_empty_properties() {
									json!({
                    "name": v.name,
                    "description": v.description,
										"input_schema": { "type": "object", "properties": {}, "required": [] },
                	})
								} else {
									json!({
											"name": v.name,
											"description": v.description,
											"input_schema": v.parameters,
									})
								}
						})
					.collect();
    }
    if prompt_cache {
        apply_prompt_cache_breakpoints(&mut body);
    }
    Ok(body)
}

/// Anthropic allows at most 4 cache_control breakpoints; all four are spent
/// here: the last tool, the system prompt, and the last two user messages.
/// Two moving breakpoints guarantee a cache read every turn: turn N+1's
/// second-to-last breakpoint sits exactly where turn N's last one was.
fn apply_prompt_cache_breakpoints(body: &mut Value) {
    if let Some(text) = body["system"].as_str().map(str::to_string) {
        body["system"] = json!([{
            "type": "text",
            "text": text,
            "cache_control": { "type": "ephemeral" },
        }]);
    }
    if let Some(tool) = body
        .get_mut("tools")
        .and_then(|v| v.as_array_mut())
        .and_then(|v| v.last_mut())
    {
        tool["cache_control"] = json!({ "type": "ephemeral" });
    }
    let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return;
    };
    let mut remaining = 2;
    for message in messages.iter_mut().rev() {
        if remaining == 0 {
            break;
        }
        if message["role"] != "user" {
            continue;
        }
        if let Some(text) = message["content"].as_str().map(str::to_string) {
            message["content"] = json!([{ "type": "text", "text": text }]);
        }
        if let Some(block) = message["content"].as_array_mut().and_then(|v| v.last_mut()) {
            block["cache_control"] = json!({ "type": "ephemeral" });
            remaining -= 1;
        }
    }
}

fn claude_parse_usage(usage: &Value) -> Option<TokenUsage> {
    if !usage.is_object() {
        return None;
    }

    Some(TokenUsage {
        input_tokens: usage["input_tokens"].as_u64(),
        output_tokens: usage["output_tokens"].as_u64(),
        cache_creation_input_tokens: usage["cache_creation_input_tokens"].as_u64(),
        cache_read_input_tokens: usage["cache_read_input_tokens"].as_u64(),
    })
}

pub fn claude_extract_chat_completions(data: &Value) -> Result<ChatCompletionsOutput> {
    let mut text = String::new();
    let mut tool_calls = vec![];
    let mut thinking = vec![];
    if let Some(list) = data["content"].as_array() {
        for item in list {
            match item["type"].as_str() {
                Some("thinking") => {
                    if let Some(v) = item["thinking"].as_str() {
                        thinking.push(ThinkingBlock::Thinking {
                            thinking: v.to_string(),
                            signature: item["signature"].as_str().unwrap_or_default().to_string(),
                        });
                    }
                }
                Some("redacted_thinking") => {
                    if let Some(v) = item["data"].as_str() {
                        thinking.push(ThinkingBlock::RedactedThinking {
                            data: v.to_string(),
                        });
                    }
                }
                Some("text") => {
                    if let Some(v) = item["text"].as_str() {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(v);
                    }
                }
                Some("tool_use") => {
                    if let (Some(name), Some(input), Some(id)) = (
                        item["name"].as_str(),
                        item.get("input"),
                        item["id"].as_str(),
                    ) {
                        tool_calls.push(ToolCall::new(
                            name.to_string(),
                            input.clone(),
                            Some(id.to_string()),
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

    let output = ChatCompletionsOutput {
        text: text.to_string(),
        tool_calls,
        thinking,
        usage: claude_parse_usage(&data["usage"]),
    };
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::access_token::set_access_token;
    use crate::config::AppConfig;
    use crate::function::{FunctionDeclaration, ToolCall, ToolResult};
    use chrono::Utc;
    use std::sync::Arc;

    fn tool_result(id: &str, text: Option<&str>) -> ToolResult {
        ToolResult {
            call: ToolCall::new("fs_read".into(), json!({"path": "x"}), Some(id.into())),
            output: json!("ok"),
            text: text.map(|t| t.to_string()),
            thinking: vec![],
        }
    }

    fn build_body_with(
        tool_results: Vec<ToolResult>,
        text: &str,
        sequence: bool,
        prompt_cache: bool,
    ) -> Value {
        let data = ChatCompletionsData {
            messages: vec![
                Message::new(MessageRole::User, MessageContent::Text("hello".to_string())),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::ToolCalls(MessageContentToolCalls {
                        tool_results,
                        text: text.to_string(),
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
        claude_build_chat_completions_body(data, &Model::new("claude", "claude-test"), prompt_cache)
            .unwrap()
    }

    fn build_body(tool_results: Vec<ToolResult>, prompt_cache: bool) -> Value {
        build_body_with(tool_results, "", true, prompt_cache)
    }

    fn multi_turn_data(functions: Option<Vec<FunctionDeclaration>>) -> ChatCompletionsData {
        ChatCompletionsData {
            messages: vec![
                Message::new(MessageRole::System, MessageContent::Text("sys".to_string())),
                Message::new(MessageRole::User, MessageContent::Text("first".to_string())),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::Text("reply".to_string()),
                ),
                Message::new(
                    MessageRole::User,
                    MessageContent::Text("second".to_string()),
                ),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::Text("reply2".to_string()),
                ),
                Message::new(MessageRole::User, MessageContent::Text("third".to_string())),
            ],
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            functions,
            stream: false,
        }
    }

    fn function_declaration(name: &str) -> FunctionDeclaration {
        FunctionDeclaration {
            name: name.to_string(),
            description: format!("{name} description"),
            parameters: Default::default(),
            agent: false,
        }
    }

    fn assert_unique_tool_use_ids_per_message(body: &Value) {
        for message in body["messages"].as_array().unwrap() {
            let Some(content) = message["content"].as_array() else {
                continue;
            };
            let mut seen = HashSet::new();
            for block in content {
                if block["type"] == "tool_use" {
                    let id = block["id"].as_str().unwrap();
                    assert!(
                        seen.insert(id.to_string()),
                        "duplicate tool_use id `{id}` within a single assistant message: {message}"
                    );
                }
            }
        }
    }

    #[test]
    fn sequence_splits_on_round_text() {
        let body = build_body(
            vec![
                tool_result("toolu_A", None),
                tool_result("toolu_B", None),
                tool_result("toolu_C", Some("running another tool")),
            ],
            false,
        );

        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 5, "body: {body}");
        assert_unique_tool_use_ids_per_message(&body);
    }

    #[test]
    fn sequence_splits_on_reused_id_in_textless_round() {
        let body = build_body(
            vec![
                tool_result("toolu_A", None),
                tool_result("toolu_B", None),
                tool_result("toolu_A", None),
            ],
            false,
        );

        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 5, "body: {body}");
        assert_unique_tool_use_ids_per_message(&body);
    }

    #[test]
    fn sequence_keeps_textless_rounds_merged_when_ids_are_unique() {
        let body = build_body(
            vec![
                tool_result("toolu_A", None),
                tool_result("toolu_B", None),
                tool_result("toolu_C", None),
            ],
            false,
        );

        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 3, "body: {body}");
        assert_unique_tool_use_ids_per_message(&body);
    }

    #[test]
    fn non_sequence_emits_assistant_user_pair() {
        let body = build_body_with(
            vec![tool_result("toolu_A", None), tool_result("toolu_B", None)],
            "",
            false,
            false,
        );

        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 3, "body: {body}");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn non_sequence_empty_tool_results_emits_no_messages() {
        let body = build_body_with(vec![], "leftover text", false, false);

        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 1, "body: {body}");
    }

    #[test]
    fn sequence_empty_tool_results_emits_no_messages() {
        let body = build_body_with(vec![], "leftover text", true, false);

        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 1, "body: {body}");
    }

    #[test]
    fn skips_openai_reasoning_blocks_in_replay() {
        let reasoning = ToolResult {
            call: ToolCall::new(
                "fs_read".into(),
                json!({"path": "x"}),
                Some("toolu_A".into()),
            ),
            output: json!("ok"),
            text: None,
            thinking: vec![ThinkingBlock::Reasoning {
                id: "rs_1".into(),
                summary: json!([{ "type": "summary_text", "text": "thinking" }]),
                encrypted_content: Some("enc123".into()),
            }],
        };

        for sequence in [false, true] {
            let body = build_body_with(vec![reasoning.clone()], "", sequence, false);

            let content = body["messages"][1]["content"].as_array().unwrap();
            assert_eq!(content.len(), 1, "body: {body}");
            assert_eq!(content[0]["type"], "tool_use", "body: {body}");
        }
    }

    #[test]
    fn prompt_cache_places_breakpoints() {
        let functions = vec![function_declaration("a"), function_declaration("b")];
        let body = claude_build_chat_completions_body(
            multi_turn_data(Some(functions)),
            &Model::new("claude", "claude-test"),
            true,
        )
        .unwrap();

        let cache_control = json!({ "type": "ephemeral" });
        let tools = body["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none(), "body: {body}");
        assert_eq!(tools[1]["cache_control"], cache_control);

        let system = body["system"].as_array().unwrap();
        assert_eq!(system.last().unwrap()["text"], "sys");
        assert_eq!(system.last().unwrap()["cache_control"], cache_control);

        let messages = body["messages"].as_array().unwrap();
        assert!(
            messages[0]["content"].is_string(),
            "older user messages stay untouched: {body}"
        );
        for idx in [2, 4] {
            let blocks = messages[idx]["content"].as_array().unwrap();
            let last = blocks.last().unwrap();
            assert_eq!(last["type"], "text", "body: {body}");
            assert_eq!(last["cache_control"], cache_control, "body: {body}");
        }
    }

    #[test]
    fn prompt_cache_marks_last_tool_result_block() {
        let body = build_body(
            vec![tool_result("toolu_A", None), tool_result("toolu_B", None)],
            true,
        );

        assert!(body.get("system").is_none(), "body: {body}");
        assert!(body.get("tools").is_none(), "body: {body}");

        let cache_control = json!({ "type": "ephemeral" });
        let messages = body["messages"].as_array().unwrap();
        let last_blocks = messages.last().unwrap()["content"].as_array().unwrap();
        let last_block = last_blocks.last().unwrap();
        assert_eq!(last_block["type"], "tool_result", "body: {body}");
        assert_eq!(last_block["cache_control"], cache_control);
        assert!(last_blocks[0].get("cache_control").is_none());

        let first_blocks = messages[0]["content"].as_array().unwrap();
        assert_eq!(first_blocks.last().unwrap()["type"], "text");
        assert_eq!(first_blocks.last().unwrap()["cache_control"], cache_control);
    }

    #[test]
    fn prompt_cache_disabled_leaves_body_unchanged() {
        let functions = vec![function_declaration("a")];
        let body = claude_build_chat_completions_body(
            multi_turn_data(Some(functions)),
            &Model::new("claude", "claude-test"),
            false,
        )
        .unwrap();

        assert!(body["system"].is_string(), "body: {body}");
        assert!(!body.to_string().contains("cache_control"), "body: {body}");
    }

    #[test]
    fn extract_populates_usage() {
        let data = json!({
            "content": [{ "type": "text", "text": "hi" }],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_creation_input_tokens": 100,
                "cache_read_input_tokens": 200,
            }
        });

        let output = claude_extract_chat_completions(&data).unwrap();

        let usage = output.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.cache_creation_input_tokens, Some(100));
        assert_eq!(usage.cache_read_input_tokens, Some(200));
    }

    fn claude_config(name: &str, auth: Option<&str>, oauth: Option<OAuthConfig>) -> ClaudeConfig {
        ClaudeConfig {
            name: Some(name.into()),
            api_key: None,
            api_base: oauth
                .as_ref()
                .map(|_| "https://gateway.example/v1".to_string()),
            auth: auth.map(str::to_string),
            oauth: oauth.map(Box::new),
            models: vec![],
            prompt_cache: None,
            patch: None,
            extra: None,
        }
    }

    fn minimal_oauth_config() -> OAuthConfig {
        serde_yaml::from_str("client_id: gateway\ntoken_url: https://gateway.example/token")
            .unwrap()
    }

    fn make_client(config: ClaudeConfig, clients: Vec<ClientConfig>) -> ClaudeClient {
        ClaudeClient {
            app_config: Arc::new(AppConfig {
                clients,
                ..AppConfig::default()
            }),
            config,
            model: Model::new("claude", "claude-test"),
        }
    }

    fn prepare(client: &ClaudeClient) -> Result<RequestData> {
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

    #[test]
    fn oauth_stock_provider_applies_claude_code_spoof() {
        let name = "claude-gate-stock-test";
        let config = claude_config(name, Some("oauth"), None);
        let client = make_client(config.clone(), vec![ClientConfig::ClaudeConfig(config)]);
        set_access_token(name, "stock-at".into(), Utc::now().timestamp() + 3600, None);

        let request_data = prepare(&client).unwrap();

        assert_eq!(
            request_data
                .headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer stock-at")
        );
        assert_eq!(
            request_data
                .headers
                .get("anthropic-beta")
                .map(String::as_str),
            Some("oauth-2025-04-20")
        );
        assert_eq!(request_data.body["system"][0]["text"], CLAUDE_CODE_PREFIX);
    }

    #[test]
    fn oauth_config_provider_skips_claude_code_spoof() {
        let name = "claude-gate-gateway-test";
        let config = claude_config(name, Some("oauth"), Some(minimal_oauth_config()));
        let client = make_client(config.clone(), vec![ClientConfig::ClaudeConfig(config)]);
        set_access_token(
            name,
            "gateway-at".into(),
            Utc::now().timestamp() + 3600,
            None,
        );

        let request_data = prepare(&client).unwrap();

        assert_eq!(
            request_data
                .headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer gateway-at")
        );
        assert!(
            !request_data.headers.contains_key("anthropic-beta"),
            "headers: {:?}",
            request_data.headers
        );
        assert!(
            request_data.body.get("system").is_none(),
            "body: {}",
            request_data.body
        );
    }

    #[test]
    fn api_key_auth_sets_x_api_key_without_spoof() {
        let name = "claude-gate-apikey-test";
        let mut config = claude_config(name, None, None);
        config.api_key = Some("sk-test".into());
        let client = make_client(config.clone(), vec![ClientConfig::ClaudeConfig(config)]);

        let request_data = prepare(&client).unwrap();

        assert_eq!(
            request_data.headers.get("x-api-key").map(String::as_str),
            Some("sk-test")
        );
        assert!(!request_data.headers.contains_key("authorization"));
        assert!(!request_data.headers.contains_key("anthropic-beta"));
        assert!(
            request_data.body.get("system").is_none(),
            "body: {}",
            request_data.body
        );
    }

    #[test]
    fn oauth_block_without_auth_oauth_is_rejected() {
        let name = "claude-gate-contradiction-test";
        let mut config = claude_config(name, None, Some(minimal_oauth_config()));
        config.api_key = Some("sk-test".into());
        let client = make_client(config.clone(), vec![ClientConfig::ClaudeConfig(config)]);

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
    fn oauth_block_without_api_base_is_rejected() {
        let name = "claude-gate-apibase-missing-test";
        let mut config = claude_config(name, Some("oauth"), Some(minimal_oauth_config()));
        config.api_base = None;
        let client = make_client(config.clone(), vec![ClientConfig::ClaudeConfig(config)]);

        let err = resolve_api_base(&client).unwrap_err().to_string();

        assert!(
            err.contains(name) && err.contains("refusing to fall back"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn catalog_only_oauth_block_without_api_base_is_rejected() {
        let name = "claude-gate-catalog-oauth-test";
        let config = claude_config(name, Some("oauth"), None);
        let client = make_client(config.clone(), vec![ClientConfig::ClaudeConfig(config)]);
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
    fn catalog_oauth_block_with_api_key_auth_falls_back_to_stock_api_base() {
        let name = "claude-gate-catalog-apikey-test";
        let mut config = claude_config(name, None, None);
        config.api_key = Some("sk-test".into());
        let client = make_client(config.clone(), vec![ClientConfig::ClaudeConfig(config)]);
        let catalog = vec![ProviderModels {
            provider: name.into(),
            oauth: Some(minimal_oauth_config()),
            models: vec![],
        }];

        let api_base = resolve_api_base_against(&client, &catalog).unwrap();

        assert_eq!(api_base, API_BASE);
    }

    #[test]
    fn auth_mismatch_bails_before_missing_api_base_guard() {
        let name = "claude-gate-bail-order-test";
        let mut config = claude_config(name, None, Some(minimal_oauth_config()));
        config.api_base = None;
        let client = make_client(config.clone(), vec![ClientConfig::ClaudeConfig(config)]);

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
    fn oauth_block_with_api_base_resolves_it() {
        let name = "claude-gate-apibase-set-test";
        let config = claude_config(name, Some("oauth"), Some(minimal_oauth_config()));
        let client = make_client(config.clone(), vec![ClientConfig::ClaudeConfig(config)]);

        let api_base = resolve_api_base(&client).unwrap();

        assert_eq!(api_base, "https://gateway.example/v1");
    }

    #[test]
    fn no_oauth_block_falls_back_to_stock_api_base() {
        let name = "claude-gate-apibase-fallback-test";
        let config = claude_config(name, None, None);
        let client = make_client(config.clone(), vec![ClientConfig::ClaudeConfig(config)]);

        let api_base = resolve_api_base(&client).unwrap();

        assert_eq!(api_base, API_BASE);
    }

    #[test]
    fn oauth_lookup_resolves_entry_matching_client_name() {
        let name = "claude-gate-lookup-test";
        // The sibling entry resolves to the stock provider; if the lookup
        // ignored the name it would apply the Claude Code spoof here.
        let sibling = claude_config("claude-gate-lookup-sibling-test", Some("oauth"), None);
        let config = claude_config(name, Some("oauth"), Some(minimal_oauth_config()));
        let client = make_client(
            config.clone(),
            vec![
                ClientConfig::ClaudeConfig(sibling),
                ClientConfig::ClaudeConfig(config),
            ],
        );
        set_access_token(
            name,
            "lookup-at".into(),
            Utc::now().timestamp() + 3600,
            None,
        );

        let request_data = prepare(&client).unwrap();

        assert_eq!(
            request_data
                .headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer lookup-at")
        );
        assert!(!request_data.headers.contains_key("anthropic-beta"));
        assert!(request_data.body.get("system").is_none());
    }

    #[test]
    fn oauth_lookup_falls_back_to_default_name_for_unnamed_entry() {
        // An entry with `name: None` must match a client named "claude" via
        // the `unwrap_or(ClaudeClient::NAME)` fallback.
        let sibling = claude_config("claude-gate-fallback-sibling-test", Some("oauth"), None);
        let mut config = claude_config("ignored", Some("oauth"), Some(minimal_oauth_config()));
        config.name = None;
        let client = make_client(
            config.clone(),
            vec![
                ClientConfig::ClaudeConfig(sibling),
                ClientConfig::ClaudeConfig(config),
            ],
        );
        set_access_token(
            ClaudeClient::NAME,
            "fallback-at".into(),
            Utc::now().timestamp() + 3600,
            None,
        );

        let request_data = prepare(&client).unwrap();

        assert_eq!(
            request_data
                .headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer fallback-at")
        );
        assert!(!request_data.headers.contains_key("anthropic-beta"));
        assert!(request_data.body.get("system").is_none());
    }

    #[test]
    fn oauth_lookup_errors_when_config_entry_missing() {
        let config = claude_config("claude-gate-missing-test", Some("oauth"), None);
        let client = make_client(config, vec![]);

        let err = match prepare(&client) {
            Ok(_) => panic!("expected the lookup to fail"),
            Err(err) => err.to_string(),
        };

        assert!(
            err.contains("Could not locate ClientConfig entry"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn shipped_catalog_resolves_stock_claude_provider() {
        let cc = ClientConfig::ClaudeConfig(claude_config("claude", Some("oauth"), None));
        let bundled: Vec<ProviderModels> = serde_yaml::from_str(MODELS_YAML).unwrap();

        let (_, is_stock) = oauth::claude_oauth_provider_for_client(&cc, &bundled);

        assert!(
            is_stock,
            "bundled models.yaml must not carry a claude oauth block: it would silently drop the Claude Code spoof for stock Pro/Max users"
        );
    }
}
