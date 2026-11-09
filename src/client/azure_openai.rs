use super::openai::*;
use super::*;

use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AzureOpenAIConfig {
    pub name: Option<String>,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    #[serde(default)]
    pub extend_models: bool,
    pub patch: Option<RequestPatch>,
    pub extra: Option<ExtraConfig>,
}

impl AzureOpenAIClient {
    config_get_fn!(api_base, get_api_base);
    config_get_fn!(api_key, get_api_key);

    create_client_config!([
        (
            "api_base",
            "API Base",
            Some("e.g. https://{RESOURCE}.openai.azure.com"),
            false,
        ),
        ("api_key", "API Key", None, true),
    ]);
}

impl_client_trait!(
    AzureOpenAIClient,
    (
        prepare_chat_completions,
        openai_chat_completions,
        openai_chat_completions_streaming
    ),
    (prepare_embeddings, openai_embeddings),
    (noop_prepare_rerank, noop_rerank),
);

fn prepare_chat_completions(
    self_: &AzureOpenAIClient,
    data: ChatCompletionsData,
) -> Result<RequestData> {
    let api_base = self_.get_api_base()?;
    let api_key = self_.get_api_key()?;

    let url = format!(
        "{}/openai/deployments/{}/chat/completions?api-version=2024-12-01-preview",
        api_base,
        self_.model.real_name()
    );

    let stream = data.stream;
    let mut body = openai_build_chat_completions_body(data, &self_.model);
    openai_request_stream_usage(&mut body, stream);

    let mut request_data = RequestData::new(url, body);

    request_data.header("api-key", api_key);

    Ok(request_data)
}

fn prepare_embeddings(self_: &AzureOpenAIClient, data: &EmbeddingsData) -> Result<RequestData> {
    let api_base = self_.get_api_base()?;
    let api_key = self_.get_api_key()?;

    let url = format!(
        "{}/openai/deployments/{}/embeddings?api-version=2024-10-21",
        api_base,
        self_.model.real_name()
    );

    let body = openai_build_embeddings_body(data, &self_.model);

    let mut request_data = RequestData::new(url, body);

    request_data.header("api-key", api_key);

    Ok(request_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use serde_json::json;
    use std::sync::Arc;

    fn make_client() -> AzureOpenAIClient {
        AzureOpenAIClient {
            app_config: Arc::new(AppConfig::default()),
            config: AzureOpenAIConfig {
                name: Some("azure-test".into()),
                api_base: Some("https://resource.openai.azure.com".into()),
                api_key: Some("azure-key".into()),
                ..Default::default()
            },
            model: Model::new("azure-test", "gpt-test"),
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
        }
    }

    #[test]
    fn streaming_chat_requests_usage() {
        let client = make_client();

        let request_data = prepare_chat_completions(&client, hello_data(true)).unwrap();

        let mut expected = openai_build_chat_completions_body(hello_data(true), &client.model);
        expected["stream_options"] = json!({ "include_usage": true });
        assert_eq!(request_data.body, expected);
    }

    #[test]
    fn non_streaming_chat_body_is_unchanged() {
        let client = make_client();

        let request_data = prepare_chat_completions(&client, hello_data(false)).unwrap();

        assert_eq!(
            request_data.body,
            openai_build_chat_completions_body(hello_data(false), &client.model)
        );
    }
}
