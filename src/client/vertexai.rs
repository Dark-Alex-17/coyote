use super::access_token::*;
use super::claude::*;
use super::openai::*;
use super::structured_output::{SchemaDialect, StructuredOutputSpec, convert_schema};
use super::*;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Duration, Utc};
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, str::FromStr};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct VertexAIConfig {
    pub name: Option<String>,
    pub project_id: Option<String>,
    pub location: Option<String>,
    pub adc_file: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    #[serde(default)]
    pub extend_models: bool,
    pub prompt_cache: Option<bool>,
    pub patch: Option<RequestPatch>,
    pub extra: Option<ExtraConfig>,
}

impl VertexAIClient {
    config_get_fn!(project_id, get_project_id);
    config_get_fn!(location, get_location);

    create_client_config!([
        ("project_id", "Project ID", None, false),
        ("location", "Location", None, false),
    ]);
}

#[async_trait::async_trait]
impl Client for VertexAIClient {
    client_common_fns!();

    fn native_structured_output_dialect(&self) -> Option<SchemaDialect> {
        match ModelCategory::from_str(self.model().real_name()) {
            Ok(ModelCategory::Gemini) => Some(SchemaDialect::Gemini),
            Ok(ModelCategory::Claude) => Some(SchemaDialect::Anthropic),
            Ok(ModelCategory::Mistral) | Err(_) => None,
        }
    }

    async fn chat_completions_inner(
        &self,
        client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput> {
        prepare_gcloud_access_token(client, self.name(), &self.config.adc_file).await?;
        let model = self.model();
        let model_category = ModelCategory::from_str(model.real_name())?;
        let request_data = prepare_chat_completions(self, data, &model_category)?;
        let builder = self.request_builder(client, request_data);
        match model_category {
            ModelCategory::Gemini => gemini_chat_completions(builder, model).await,
            ModelCategory::Claude => claude_chat_completions(builder, model).await,
            ModelCategory::Mistral => openai_chat_completions(builder, model).await,
        }
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        prepare_gcloud_access_token(client, self.name(), &self.config.adc_file).await?;
        let model = self.model();
        let model_category = ModelCategory::from_str(model.real_name())?;
        let request_data = prepare_chat_completions(self, data, &model_category)?;
        let builder = self.request_builder(client, request_data);
        match model_category {
            ModelCategory::Gemini => {
                gemini_chat_completions_streaming(builder, handler, model).await
            }
            ModelCategory::Claude => {
                claude_chat_completions_streaming(builder, handler, model).await
            }
            ModelCategory::Mistral => {
                openai_chat_completions_streaming(builder, handler, model).await
            }
        }
    }

    async fn embeddings_inner(
        &self,
        client: &ReqwestClient,
        data: &EmbeddingsData,
    ) -> Result<Vec<Vec<f32>>> {
        prepare_gcloud_access_token(client, self.name(), &self.config.adc_file).await?;
        let request_data = prepare_embeddings(self, data)?;
        let builder = self.request_builder(client, request_data);
        embeddings(builder, self.model()).await
    }
}

pub(super) fn prepare_chat_completions(
    self_: &VertexAIClient,
    data: ChatCompletionsData,
    model_category: &ModelCategory,
) -> Result<RequestData> {
    let project_id = self_.get_project_id()?;
    let location = self_.get_location()?;
    let access_token = get_access_token(self_.name())?;

    let base_url = if location == "global" {
        format!(
            "https://aiplatform.googleapis.com/v1/projects/{project_id}/locations/global/publishers"
        )
    } else {
        format!(
            "https://{location}-aiplatform.googleapis.com/v1/projects/{project_id}/locations/{location}/publishers"
        )
    };

    let model_name = self_.model.real_name();

    let url = match model_category {
        ModelCategory::Gemini => {
            let func = match data.stream {
                true => "streamGenerateContent",
                false => "generateContent",
            };
            format!("{base_url}/google/models/{model_name}:{func}")
        }
        ModelCategory::Claude => {
            let func = match data.stream {
                true => "streamRawPredict",
                false => "rawPredict",
            };
            format!("{base_url}/anthropic/models/{model_name}:{func}")
        }
        ModelCategory::Mistral => {
            let func = match data.stream {
                true => "streamRawPredict",
                false => "rawPredict",
            };
            format!("{base_url}/mistralai/models/{model_name}:{func}")
        }
    };

    let body = match model_category {
        ModelCategory::Gemini => gemini_build_chat_completions_body(data, &self_.model)?,
        ModelCategory::Claude => {
            let mut body = claude_build_chat_completions_body(
                data,
                &self_.model,
                self_.config.prompt_cache.unwrap_or(true),
            )?;
            if let Some(body_obj) = body.as_object_mut() {
                body_obj.remove("model");
            }
            body["anthropic_version"] = "vertex-2023-10-16".into();
            body
        }
        ModelCategory::Mistral => {
            let mut body = openai_build_chat_completions_body(data, &self_.model);
            if let Some(body_obj) = body.as_object_mut() {
                body_obj["model"] = strip_model_version(self_.model.real_name()).into();
                // Vertex Mistral has no native structured-output dialect.
                body_obj.remove("response_format");
            }
            body
        }
    };

    let mut request_data = RequestData::new(url, body);

    request_data.bearer_auth(access_token);

    Ok(request_data)
}

fn prepare_embeddings(self_: &VertexAIClient, data: &EmbeddingsData) -> Result<RequestData> {
    let project_id = self_.get_project_id()?;
    let location = self_.get_location()?;
    let access_token = get_access_token(self_.name())?;

    let base_url = if location == "global" {
        format!(
            "https://aiplatform.googleapis.com/v1/projects/{project_id}/locations/global/publishers"
        )
    } else {
        format!(
            "https://{location}-aiplatform.googleapis.com/v1/projects/{project_id}/locations/{location}/publishers"
        )
    };
    let url = format!(
        "{base_url}/google/models/{}:predict",
        self_.model.real_name()
    );

    let instances: Vec<_> = data.texts.iter().map(|v| json!({"content": v})).collect();

    let body = json!({
        "instances": instances,
    });

    let mut request_data = RequestData::new(url, body);

    request_data.bearer_auth(access_token);

    Ok(request_data)
}

pub async fn gemini_chat_completions(
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
    gemini_extract_chat_completions_text(&data)
}

pub async fn gemini_chat_completions_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
    _model: &Model,
) -> Result<()> {
    let res = builder.send().await?;
    let status = res.status();
    if !status.is_success() {
        let data: Value = res.json().await?;
        catch_error(&data, status.as_u16())?;
    } else {
        let handle = |value: &str| -> Result<()> {
            let data: Value = serde_json::from_str(value)?;
            debug!("stream-data: {data}");
            // Each chunk's `usageMetadata` is cumulative and the handler keeps
            // the latest values, so chunks must never be summed.
            if let Some(usage) = gemini_parse_usage(&data["usageMetadata"]) {
                debug!("token-usage: {usage:?}");
                handler.usage(usage);
            }
            if let Some(parts) = data["candidates"][0]["content"]["parts"].as_array() {
                for (i, part) in parts.iter().enumerate() {
                    if let Some(text) = part["text"].as_str() {
                        if i > 0 {
                            handler.text("\n\n")?;
                        }
                        handler.text(text)?;
                    } else if let (Some(name), Some(args)) = (
                        part["functionCall"]["name"].as_str(),
                        part["functionCall"]["args"].as_object(),
                    ) {
                        let thought_signature = part["thoughtSignature"]
                            .as_str()
                            .or_else(|| part["thought_signature"].as_str())
                            .map(|s| s.to_string());
                        handler.tool_call(
                            ToolCall::new(name.to_string(), json!(args), None)
                                .with_thought_signature(thought_signature),
                        )?;
                    }
                }
            } else if let Some("SAFETY") = data["promptFeedback"]["blockReason"]
                .as_str()
                .or_else(|| data["candidates"][0]["finishReason"].as_str())
            {
                bail!("Blocked due to safety")
            }

            Ok(())
        };
        json_stream(res.bytes_stream(), handle).await?;
    }
    Ok(())
}

async fn embeddings(builder: RequestBuilder, _model: &Model) -> Result<EmbeddingsOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let data: Value = res.json().await?;
    if !status.is_success() {
        catch_error(&data, status.as_u16())?;
    }
    let res_body: EmbeddingsResBody =
        serde_json::from_value(data).context("Invalid embeddings data")?;
    let output = res_body
        .predictions
        .into_iter()
        .map(|v| v.embeddings.values)
        .collect();
    Ok(output)
}

#[derive(Deserialize)]
struct EmbeddingsResBody {
    predictions: Vec<EmbeddingsResBodyPrediction>,
}

#[derive(Deserialize)]
struct EmbeddingsResBodyPrediction {
    embeddings: EmbeddingsResBodyPredictionEmbeddings,
}

#[derive(Deserialize)]
struct EmbeddingsResBodyPredictionEmbeddings {
    values: Vec<f32>,
}

fn gemini_extract_chat_completions_text(data: &Value) -> Result<ChatCompletionsOutput> {
    let mut text_parts = vec![];
    let mut tool_calls = vec![];
    if let Some(parts) = data["candidates"][0]["content"]["parts"].as_array() {
        for part in parts {
            if let Some(text) = part["text"].as_str() {
                text_parts.push(text);
            }
            if let (Some(name), Some(args)) = (
                part["functionCall"]["name"].as_str(),
                part["functionCall"]["args"].as_object(),
            ) {
                let thought_signature = part["thoughtSignature"]
                    .as_str()
                    .or_else(|| part["thought_signature"].as_str())
                    .map(|s| s.to_string());
                tool_calls.push(
                    ToolCall::new(name.to_string(), json!(args), None)
                        .with_thought_signature(thought_signature),
                );
            }
        }
    }

    let text = text_parts.join("\n\n");
    if text.is_empty() && tool_calls.is_empty() {
        if let Some("SAFETY") = data["promptFeedback"]["blockReason"]
            .as_str()
            .or_else(|| data["candidates"][0]["finishReason"].as_str())
        {
            bail!("Blocked due to safety")
        } else {
            bail!("Invalid response data: {data}");
        }
    }
    let output = ChatCompletionsOutput {
        text,
        tool_calls,
        usage: gemini_parse_usage(&data["usageMetadata"]),
        ..Default::default()
    };
    Ok(output)
}

/// `promptTokenCount` includes cached content, so cached tokens are
/// subtracted to keep the buckets disjoint. Thinking tokens are billed as
/// output but reported apart from `candidatesTokenCount`. Gemini has no
/// cache-creation counter.
pub fn gemini_parse_usage(usage: &Value) -> Option<TokenUsage> {
    if !usage.is_object() {
        return None;
    }

    let cached_tokens = usage["cachedContentTokenCount"].as_u64();
    let input_tokens = usage["promptTokenCount"].as_u64().map(|v| {
        v.saturating_add(usage["toolUsePromptTokenCount"].as_u64().unwrap_or(0))
            .saturating_sub(cached_tokens.unwrap_or(0))
    });
    let output_tokens = match (
        usage["candidatesTokenCount"].as_u64(),
        usage["thoughtsTokenCount"].as_u64(),
    ) {
        (None, None) => None,
        (candidates, thoughts) => {
            Some(candidates.unwrap_or(0).saturating_add(thoughts.unwrap_or(0)))
        }
    };
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: cached_tokens,
    })
}

pub fn gemini_build_chat_completions_body(
    data: ChatCompletionsData,
    model: &Model,
) -> Result<Value> {
    let ChatCompletionsData {
        mut messages,
        temperature,
        top_p,
        reasoning_effort,
        functions,
        stream: _,
        prompt_cache_ttl: _,
        structured_output,
    } = data;

    let system_message = extract_system_message(&mut messages);

    let mut network_image_urls = vec![];
    let contents: Vec<Value> = messages
        .into_iter()
        .flat_map(|message| {
            let Message { role, content } = message;
            let role = match role {
                MessageRole::User => "user",
                _ => "model",
            };
               match content {
                    MessageContent::Text(text) => vec![json!({
                        "role": role,
                        "parts": [{ "text": text }]
                    })],
                    MessageContent::Array(list) => {
                        let parts: Vec<Value> = list
                            .into_iter()
                            .map(|item| match item {
                                MessageContentPart::Text { text } => json!({"text": text}),
                                MessageContentPart::ImageUrl { image_url: ImageUrl { url } } => {
                                    if let Some((mime_type, data)) = url.strip_prefix("data:").and_then(|v| v.split_once(";base64,")) {
                                        json!({ "inline_data": { "mime_type": mime_type, "data": data } })
                                    } else {
                                        network_image_urls.push(url.clone());
                                        json!({ "url": url })
                                    }
                                },
                            })
                            .collect();
                        vec![json!({ "role": role, "parts": parts })]
                    },
                    MessageContent::ToolCalls(MessageContentToolCalls { tool_results, text, .. }) => {
                        let mut model_parts: Vec<Value> = vec![];
                        if !text.is_empty() {
                            model_parts.push(json!({ "text": text }));
                        }
                        for tool_result in tool_results.iter() {
                            if let Some(round_text) = &tool_result.text {
                                model_parts.push(json!({ "text": round_text }));
                            }
                            let mut part = json!({
                                "functionCall": {
                                    "name": tool_result.call.name,
                                    "args": tool_result.call.arguments,
                                }
                            });
                            if let Some(sig) = &tool_result.call.thought_signature {
                                part["thoughtSignature"] = json!(sig);
                            }
                            model_parts.push(part);
                        }
                        let image_parts: Vec<Value> = tool_results
                            .iter()
                            .flat_map(|tool_result| {
                                tool_result.attachments.iter().flat_map(move |attachment| {
                                    [
                                        json!({
                                            "text": format!(
                                                "Image output from tool call {}: {}",
                                                tool_result.call.name, attachment.source
                                            )
                                        }),
                                        json!({ "inline_data": { "mime_type": attachment.mime_type, "data": attachment.data } }),
                                    ]
                                })
                            })
                            .collect();
                        let function_parts: Vec<Value> = tool_results.into_iter().map(|tool_result| {
                            json!({
                                "functionResponse": {
                                    "name": tool_result.call.name,
                                    "response": {
                                        "name": tool_result.call.name,
                                        "content": tool_result.output,
                                    }
                                }
                            })
                        }).collect();
                        // Empty tool_results (reachable via deserialized sessions) must not emit an empty pair.
                        if function_parts.is_empty() {
                            vec![]
                        } else {
                            let mut turns = vec![
                                json!({ "role": "model", "parts": model_parts }),
                                json!({ "role": "user", "parts": function_parts }),
                            ];
                            // Gemini rejects image parts inside the function-response turn,
                            // so tool images follow it in their own user turn.
                            if !image_parts.is_empty() {
                                turns.push(json!({ "role": "user", "parts": image_parts }));
                            }
                            turns
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

    let mut body = json!({ "contents": contents, "generationConfig": {} });

    if let Some(v) = system_message {
        body["systemInstruction"] = json!({ "parts": [{"text": v }] });
    }

    if let Some(v) = model.max_tokens_param() {
        body["generationConfig"]["maxOutputTokens"] = v.into();
    }
    if let Some(v) = temperature {
        body["generationConfig"]["temperature"] = v.into();
    }
    if let Some(v) = top_p {
        body["generationConfig"]["topP"] = v.into();
    }
    if let Some(v) = reasoning_effort {
        body["generationConfig"]["thinking_config"] = json!({"thinking_level": v});
    }
    if let Some(StructuredOutputSpec { schema, .. }) = structured_output {
        body["generationConfig"]["responseMimeType"] = "application/json".into();
        body["generationConfig"]["responseJsonSchema"] =
            convert_schema(&schema, SchemaDialect::Gemini);
    }

    if let Some(functions) = functions {
        // Gemini doesn't support functions with parameters that have empty properties, so we need to patch it.
        let function_declarations: Vec<_> = functions
            .into_iter()
            .map(|function| {
                if function.parameters.is_empty_properties() {
                    json!({
                        "name": function.name,
                        "description": function.description,
                    })
                } else {
                    json!(function)
                }
            })
            .collect();
        body["tools"] = json!([{ "functionDeclarations": function_declarations }]);
    }

    Ok(body)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModelCategory {
    Gemini,
    Claude,
    Mistral,
}

impl FromStr for ModelCategory {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.starts_with("gemini") {
            Ok(ModelCategory::Gemini)
        } else if s.starts_with("claude") {
            Ok(ModelCategory::Claude)
        } else if s.starts_with("mistral") || s.starts_with("codestral") {
            Ok(ModelCategory::Mistral)
        } else {
            unsupported_model!(s)
        }
    }
}

pub async fn prepare_gcloud_access_token(
    client: &reqwest::Client,
    client_name: &str,
    adc_file: &Option<String>,
) -> Result<()> {
    if !is_valid_access_token(client_name) {
        let (token, expires_in) = fetch_access_token(client, adc_file)
            .await
            .with_context(|| "Failed to fetch access token")?;
        let expires_at = Utc::now()
            + Duration::try_seconds(expires_in)
                .ok_or_else(|| anyhow!("Failed to parse expires_in of access_token"))?;
        set_access_token(client_name, token, expires_at.timestamp(), None)
    }
    Ok(())
}

async fn fetch_access_token(
    client: &reqwest::Client,
    file: &Option<String>,
) -> Result<(String, i64)> {
    let credentials = load_adc(file).await?;
    let value: Value = client
        .post("https://oauth2.googleapis.com/token")
        .json(&credentials)
        .send()
        .await?
        .json()
        .await?;

    if let (Some(access_token), Some(expires_in)) =
        (value["access_token"].as_str(), value["expires_in"].as_i64())
    {
        Ok((access_token.to_string(), expires_in))
    } else if let Some(err_msg) = value["error_description"].as_str() {
        bail!("{err_msg}")
    } else {
        bail!("Invalid response data: {value}")
    }
}

async fn load_adc(file: &Option<String>) -> Result<Value> {
    let adc_file = file
        .as_ref()
        .map(PathBuf::from)
        .or_else(default_adc_file)
        .ok_or_else(|| anyhow!("No application_default_credentials.json"))?;
    let data = tokio::fs::read_to_string(adc_file).await?;
    let data: Value = serde_json::from_str(&data)?;
    if let (Some(client_id), Some(client_secret), Some(refresh_token)) = (
        data["client_id"].as_str(),
        data["client_secret"].as_str(),
        data["refresh_token"].as_str(),
    ) {
        Ok(json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "refresh_token": refresh_token,
            "grant_type": "refresh_token",
        }))
    } else {
        bail!("Invalid application_default_credentials.json")
    }
}

#[cfg(not(windows))]
fn default_adc_file() -> Option<PathBuf> {
    let mut path = dirs::home_dir()?;
    path.push(".config");
    path.push("gcloud");
    path.push("application_default_credentials.json");
    Some(path)
}

#[cfg(windows)]
fn default_adc_file() -> Option<PathBuf> {
    let mut path = dirs::config_dir()?;
    path.push("gcloud");
    path.push("application_default_credentials.json");
    Some(path)
}

fn strip_model_version(name: &str) -> &str {
    match name.split_once('@') {
        Some((v, _)) => v,
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::function::{ImageAttachment, ToolCall, ToolResult};

    #[test]
    fn gemini_empty_tool_results_emits_no_messages() {
        let data = ChatCompletionsData {
            messages: vec![
                Message::new(MessageRole::User, MessageContent::Text("hello".to_string())),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::ToolCalls(MessageContentToolCalls {
                        tool_results: vec![],
                        text: "leftover text".to_string(),
                        sequence: false,
                        round_starts: vec![],
                    }),
                ),
            ],
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            functions: None,
            stream: false,
            prompt_cache_ttl: None,
            structured_output: None,
        };

        let body = gemini_build_chat_completions_body(data, &Model::new("vertexai", "gemini-test"))
            .unwrap();

        let contents = body["contents"].as_array().unwrap();

        assert_eq!(contents.len(), 1, "body: {body}");
    }

    #[test]
    fn gemini_interleaved_user_parts_keep_their_order() {
        let body = gemini_build_chat_completions_body(
            interleaved_user_data(),
            &Model::new("vertexai", "gemini-test"),
        )
        .unwrap();

        assert_eq!(
            body["contents"][0]["parts"],
            json!([
                { "text": "before" },
                { "inline_data": { "mime_type": "image/png", "data": "QQ==" } },
                { "text": "after" },
                { "inline_data": { "mime_type": "image/jpeg", "data": "Qg==" } },
            ]),
            "body: {body}"
        );
    }

    fn gemini_body_for(tool_results: Vec<ToolResult>) -> Value {
        let data = ChatCompletionsData {
            messages: vec![
                Message::new(MessageRole::User, MessageContent::Text("hello".to_string())),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::ToolCalls(MessageContentToolCalls {
                        tool_results,
                        text: String::new(),
                        sequence: false,
                        round_starts: vec![],
                    }),
                ),
            ],
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            functions: None,
            stream: false,
            prompt_cache_ttl: None,
            structured_output: None,
        };
        gemini_build_chat_completions_body(data, &Model::new("vertexai", "gemini-test")).unwrap()
    }

    fn fs_read_result(id: &str) -> ToolResult {
        ToolResult::new(
            ToolCall::new("fs_read".into(), json!({"path": "x"}), Some(id.into())),
            json!("ok"),
        )
    }

    fn function_response_json() -> Value {
        json!({
            "functionResponse": {
                "name": "fs_read",
                "response": { "name": "fs_read", "content": "ok" },
            }
        })
    }

    #[test]
    fn gemini_tool_images_follow_function_responses_in_a_user_turn() {
        let body = gemini_body_for(vec![
            fs_read_result("call_A"),
            fs_read_result("call_B").with_attachments(vec![ImageAttachment {
                source: "/tmp/b.png".into(),
                mime_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }]),
        ]);

        let function_content = &body["contents"][2];
        assert_eq!(function_content["role"], "user", "body: {body}");
        assert_eq!(
            function_content["parts"],
            json!([function_response_json(), function_response_json()]),
            "body: {body}"
        );
        let image_content = &body["contents"][3];
        assert_eq!(image_content["role"], "user", "body: {body}");
        assert_eq!(
            image_content["parts"],
            json!([
                { "text": "Image output from tool call fs_read: /tmp/b.png" },
                { "inline_data": { "mime_type": "image/png", "data": "aGVsbG8=" } },
            ]),
            "body: {body}"
        );
        assert_eq!(body["contents"].as_array().unwrap().len(), 4, "body: {body}");
    }

    #[test]
    fn gemini_plain_tool_result_round_sends_function_responses_in_a_user_turn() {
        let body = gemini_body_for(vec![fs_read_result("call_A")]);

        assert_eq!(
            body["contents"],
            json!([
                { "role": "user", "parts": [{ "text": "hello" }] },
                {
                    "role": "model",
                    "parts": [{ "functionCall": { "name": "fs_read", "args": { "path": "x" } } }],
                },
                { "role": "user", "parts": [function_response_json()] },
            ]),
            "body: {body}"
        );
    }

    #[test]
    fn gemini_tool_result_round_keeps_thought_signature_on_model_turn() {
        let signed_call = ToolCall::new("fs_read".into(), json!({"path": "x"}), Some("call_A".into()))
            .with_thought_signature(Some("sig".into()));
        let body = gemini_body_for(vec![ToolResult::new(signed_call, json!("ok"))]);

        assert_eq!(
            body["contents"],
            json!([
                { "role": "user", "parts": [{ "text": "hello" }] },
                {
                    "role": "model",
                    "parts": [{
                        "functionCall": { "name": "fs_read", "args": { "path": "x" } },
                        "thoughtSignature": "sig",
                    }],
                },
                { "role": "user", "parts": [function_response_json()] },
            ]),
            "body: {body}"
        );
    }

    #[test]
    fn gemini_images_omitted_result_has_no_image_parts() {
        let mut result = fs_read_result("call_A");
        result.output = json!({ "result": "ok", "images_omitted": "model has no vision" });

        let body = gemini_body_for(vec![result]);

        let parts = body["contents"][2]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1, "body: {body}");
        assert!(parts[0].get("functionResponse").is_some(), "body: {body}");
    }

    #[test]
    fn claude_route_carries_tool_result_images() {
        let result = ToolResult::new(
            ToolCall::new("fs_read".into(), json!({"path": "x"}), Some("toolu_A".into())),
            json!("ok"),
        )
        .with_attachments(vec![ImageAttachment {
            source: "/tmp/a.png".into(),
            mime_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        }]);
        let data = ChatCompletionsData {
            messages: vec![
                Message::new(MessageRole::User, MessageContent::Text("hello".to_string())),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::ToolCalls(MessageContentToolCalls {
                        tool_results: vec![result],
                        text: String::new(),
                        sequence: true,
                        round_starts: vec![],
                    }),
                ),
            ],
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            functions: None,
            stream: false,
            prompt_cache_ttl: None,
            structured_output: None,
        };

        let body = claude_build_chat_completions_body(
            data,
            &Model::new("vertexai", "claude-test"),
            false,
        )
        .unwrap();

        assert_eq!(
            body["messages"][2]["content"][0]["content"],
            json!([
                {"type": "text", "text": "\"ok\""},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
            ]),
            "body: {body}"
        );
    }

    #[test]
    fn gemini_extract_parses_usage_metadata() {
        let data = json!({
            "candidates": [{ "content": { "parts": [{ "text": "hi" }] } }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "cachedContentTokenCount": 30,
                "candidatesTokenCount": 20,
                "thoughtsTokenCount": 5,
                "toolUsePromptTokenCount": 4,
            }
        });

        let usage = gemini_extract_chat_completions_text(&data)
            .unwrap()
            .usage
            .unwrap();

        assert_eq!(usage.input_tokens, Some(74));
        assert_eq!(usage.output_tokens, Some(25));
        assert_eq!(usage.cache_read_input_tokens, Some(30));
        assert_eq!(usage.cache_creation_input_tokens, None);
    }

    #[test]
    fn gemini_extract_usage_without_cached_content_has_no_cache_read() {
        let data = json!({
            "candidates": [{ "content": { "parts": [{ "text": "hi" }] } }],
            "usageMetadata": { "promptTokenCount": 100, "candidatesTokenCount": 20 }
        });

        let usage = gemini_extract_chat_completions_text(&data)
            .unwrap()
            .usage
            .unwrap();

        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.cache_read_input_tokens, None);
        assert_eq!(usage.cache_creation_input_tokens, None);
    }

    #[test]
    fn gemini_extract_without_usage_metadata_leaves_it_none() {
        let data = json!({ "candidates": [{ "content": { "parts": [{ "text": "hi" }] } }] });

        assert!(
            gemini_extract_chat_completions_text(&data)
                .unwrap()
                .usage
                .is_none()
        );
    }

    #[tokio::test]
    async fn gemini_stream_keeps_the_last_cumulative_usage() {
        let chunks = json!([
            {
                "candidates": [{ "content": { "parts": [{ "text": "a" }] } }],
                "usageMetadata": { "promptTokenCount": 100, "cachedContentTokenCount": 30, "candidatesTokenCount": 3, "toolUsePromptTokenCount": 4 }
            },
            {
                "candidates": [{ "content": { "parts": [{ "text": "b" }] } }],
                "usageMetadata": { "promptTokenCount": 100, "cachedContentTokenCount": 30, "candidatesTokenCount": 10, "thoughtsTokenCount": 5, "toolUsePromptTokenCount": 4 }
            },
            {
                "candidates": [{ "content": { "parts": [{ "text": "c" }] }, "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 100, "cachedContentTokenCount": 30, "candidatesTokenCount": 20, "thoughtsTokenCount": 5, "toolUsePromptTokenCount": 4 }
            }
        ]);
        let url =
            super::super::openai::test_server::serve_once("application/json", chunks.to_string())
                .await;
        let builder = super::super::openai::test_server::client().post(url);
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut handler = SseHandler::new(sender, crate::utils::create_abort_signal());

        gemini_chat_completions_streaming(
            builder,
            &mut handler,
            &Model::new("vertexai", "gemini-test"),
        )
        .await
        .unwrap();

        let (text, _, _, usage) = handler.take();
        assert_eq!(text, "abc");
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, Some(74));
        assert_eq!(usage.output_tokens, Some(25));
        assert_eq!(usage.cache_read_input_tokens, Some(30));
        assert_eq!(usage.cache_creation_input_tokens, None);
    }

    fn mistral_client(name: &str) -> VertexAIClient {
        set_access_token(
            name,
            "vertex-at".into(),
            chrono::Utc::now().timestamp() + 3600,
            None,
        );
        VertexAIClient {
            app_config: std::sync::Arc::new(crate::config::AppConfig::default()),
            config: VertexAIConfig {
                name: Some(name.into()),
                project_id: Some("project".into()),
                location: Some("us-central1".into()),
                ..Default::default()
            },
            model: Model::new(name, "mistral-large"),
        }
    }

    fn hello_data(stream: bool) -> ChatCompletionsData {
        ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("hello".to_string()),
            )],
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            functions: None,
            stream,
            prompt_cache_ttl: None,
            structured_output: None,
        }
    }

    #[test]
    fn native_structured_output_dialect_follows_model_category() {
        let cases = [
            ("gemini-2.5-pro", Some(SchemaDialect::Gemini)),
            ("claude-sonnet-4", Some(SchemaDialect::Anthropic)),
            ("mistral-large", None),
            ("llama-3", None),
        ];
        for (real_name, expected) in cases {
            let client = VertexAIClient {
                app_config: std::sync::Arc::new(crate::config::AppConfig::default()),
                config: VertexAIConfig::default(),
                model: Model::new("vertexai", real_name),
            };

            assert_eq!(
                client.native_structured_output_dialect(),
                expected,
                "{real_name}"
            );
        }
    }

    #[test]
    fn mistral_bodies_omit_stream_options() {
        let client = mistral_client("vertex-mistral-stream-usage-test");

        for stream in [true, false] {
            let request_data =
                prepare_chat_completions(&client, hello_data(stream), &ModelCategory::Mistral)
                    .unwrap();

            assert!(
                request_data.body.get("stream_options").is_none(),
                "body: {}",
                request_data.body
            );
            assert_eq!(request_data.body.get("stream").is_some(), stream);
        }
    }

    fn structured_output_schema() -> Value {
        structured_output_data().structured_output.unwrap().schema
    }

    #[test]
    fn gemini_spec_joins_existing_generation_config() {
        let mut model = Model::new("vertexai", "gemini-test");
        model.set_max_tokens(Some(256), true);
        let mut data = structured_output_data();
        data.temperature = Some(0.5);
        data.top_p = Some(0.9);
        data.reasoning_effort = Some("high".into());

        let body = gemini_build_chat_completions_body(data, &model).unwrap();

        assert_eq!(
            body["generationConfig"],
            json!({
                "maxOutputTokens": 256,
                "temperature": 0.5,
                "topP": 0.9,
                "thinking_config": { "thinking_level": "high" },
                "responseMimeType": "application/json",
                "responseJsonSchema": convert_schema(&structured_output_schema(), SchemaDialect::Gemini),
            })
        );
    }

    #[test]
    fn gemini_without_spec_body_is_unchanged() {
        let body = gemini_build_chat_completions_body(
            hello_data(false),
            &Model::new("vertexai", "gemini-test"),
        )
        .unwrap();

        assert_eq!(
            body.to_string(),
            r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{}}"#
        );
    }

    #[test]
    fn vertex_gemini_body_carries_the_spec() {
        let client = VertexAIClient {
            model: Model::new("vertex-gemini-structured-test", "gemini-2.5-pro"),
            ..mistral_client("vertex-gemini-structured-test")
        };

        let request_data =
            prepare_chat_completions(&client, structured_output_data(), &ModelCategory::Gemini)
                .unwrap();

        assert_eq!(
            request_data.body["generationConfig"]["responseJsonSchema"],
            convert_schema(&structured_output_schema(), SchemaDialect::Gemini)
        );
        assert_eq!(
            request_data.body["generationConfig"]["responseMimeType"],
            "application/json"
        );
    }

    #[test]
    fn vertex_claude_body_has_the_claude_output_config() {
        let client = VertexAIClient {
            model: Model::new("vertex-claude-structured-test", "claude-sonnet-4"),
            ..mistral_client("vertex-claude-structured-test")
        };
        let mut data = structured_output_data();
        data.reasoning_effort = Some("high".into());

        let request_data =
            prepare_chat_completions(&client, data, &ModelCategory::Claude).unwrap();

        assert_eq!(
            request_data.body["output_config"],
            json!({
                "effort": "high",
                "format": {
                    "type": "json_schema",
                    "schema": convert_schema(&structured_output_schema(), SchemaDialect::Anthropic),
                },
            })
        );
    }

    #[test]
    fn mistral_body_drops_the_spec() {
        let client = mistral_client("vertex-mistral-structured-test");

        let request_data =
            prepare_chat_completions(&client, structured_output_data(), &ModelCategory::Mistral)
                .unwrap();

        assert!(
            request_data.body.get("response_format").is_none(),
            "body: {}",
            request_data.body
        );
    }
}
