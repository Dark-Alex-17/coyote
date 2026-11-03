use super::access_token::*;
use super::claude::*;
use super::openai::*;
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

fn prepare_chat_completions(
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
            }
            if let Some(category) = gemini_refusal_category(&data) {
                return Err(ProviderRefusal {
                    category: Some(category.to_string()),
                    explanation: None,
                    partial_output: handler.has_received_visible_output(),
                }
                .into());
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
        if let Some(category) = gemini_refusal_category(data) {
            return Err(ProviderRefusal {
                category: Some(category.to_string()),
                ..ProviderRefusal::default()
            }
            .into());
        }
        bail!("Invalid response data: {data}");
    }
    let output = ChatCompletionsOutput {
        text,
        tool_calls,
        ..Default::default()
    };
    Ok(output)
}

/// Gemini blocks a prompt with `promptFeedback.blockReason`, whatever the reason, and
/// stops a reply on its safety filters with `finishReason: SAFETY`. Either is a
/// refusal, and the reason names its category.
fn gemini_refusal_category(data: &Value) -> Option<&str> {
    data["promptFeedback"]["blockReason"].as_str().or_else(|| {
        data["candidates"][0]["finishReason"]
            .as_str()
            .filter(|reason| *reason == "SAFETY")
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
                            vec![
                                json!({ "role": "model", "parts": model_parts }),
                                json!({ "role": "function", "parts": function_parts }),
                            ]
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
enum ModelCategory {
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
        };

        let body = gemini_build_chat_completions_body(data, &Model::new("vertexai", "gemini-test"))
            .unwrap();

        let contents = body["contents"].as_array().unwrap();

        assert_eq!(contents.len(), 1, "body: {body}");
    }

    mod safety_refusal_tests {
        use super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        /// Serves one canned JSON response to the first request, then closes.
        async fn serve(status: &'static str, body: Value) -> RequestBuilder {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let body = body.to_string();
            tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(response.as_bytes()).await.unwrap();
            });
            ReqwestClient::builder()
                .no_proxy()
                .build()
                .unwrap()
                .post(format!("http://{addr}/"))
        }

        async fn stream(body: Value) -> (Result<()>, SseHandler) {
            let builder = serve("200 OK", body).await;
            let (sender, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut handler = SseHandler::new(sender, crate::utils::create_abort_signal());
            handler.set_silent(true);
            let res = gemini_chat_completions_streaming(
                builder,
                &mut handler,
                &Model::new("vertexai", "gemini-test"),
            )
            .await;
            (res, handler)
        }

        fn refusal_category(err: &anyhow::Error) -> Option<&str> {
            err.downcast_ref::<ProviderRefusal>()
                .unwrap_or_else(|| panic!("not a provider refusal: {err:#}"))
                .category
                .as_deref()
        }

        #[test]
        fn a_blocked_prompt_is_a_refusal_named_by_its_block_reason() {
            let data = json!({"promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}});

            let err = gemini_extract_chat_completions_text(&data).unwrap_err();

            assert_eq!(refusal_category(&err), Some("PROHIBITED_CONTENT"));
        }

        #[test]
        fn a_safety_finish_with_no_content_is_a_refusal() {
            let data = json!({"candidates": [{"finishReason": "SAFETY"}]});

            let err = gemini_extract_chat_completions_text(&data).unwrap_err();

            assert_eq!(refusal_category(&err), Some("SAFETY"));
            assert_eq!(
                format!("{err:#}"),
                "the model declined to answer (provider refusal: SAFETY)"
            );
        }

        #[test]
        fn an_empty_reply_with_another_finish_reason_is_not_a_refusal() {
            let data = json!({"candidates": [{"finishReason": "RECITATION"}]});

            let err = gemini_extract_chat_completions_text(&data).unwrap_err();

            assert!(err.downcast_ref::<ProviderRefusal>().is_none(), "{err:#}");
        }

        #[tokio::test]
        async fn an_api_error_saying_blocked_due_to_safety_is_not_a_refusal() {
            let builder = serve(
                "400 Bad Request",
                json!({"error": {"code": 400, "message": "Blocked due to safety", "status": "INVALID_ARGUMENT"}}),
            )
            .await;

            let err = gemini_chat_completions(builder, &Model::new("vertexai", "gemini-test"))
                .await
                .unwrap_err();

            assert!(err.downcast_ref::<ProviderRefusal>().is_none(), "{err:#}");
            assert!(err.downcast_ref::<ApiStatusError>().is_some(), "{err:#}");
        }

        #[tokio::test]
        async fn a_streamed_block_before_any_text_is_a_refusal_without_partial_output() {
            let (res, _) = stream(json!([{"promptFeedback": {"blockReason": "SAFETY"}}])).await;

            let err = res.unwrap_err();
            assert_eq!(refusal_category(&err), Some("SAFETY"));
            assert!(!err.downcast_ref::<ProviderRefusal>().unwrap().partial_output);
        }

        #[tokio::test]
        async fn a_streamed_safety_stop_after_text_is_flagged_partial_and_keeps_the_text() {
            let (res, handler) = stream(json!([
                {"candidates": [{"content": {"parts": [{"text": "partial"}]}}]},
                {"candidates": [{"finishReason": "SAFETY"}]}
            ]))
            .await;

            let err = res.unwrap_err();
            assert!(err.downcast_ref::<ProviderRefusal>().unwrap().partial_output);
            let (text, _, _, _) = handler.take();
            assert_eq!(text, "partial");
        }
    }
}
