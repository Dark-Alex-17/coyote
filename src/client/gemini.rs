use super::access_token::get_access_token;
use super::gemini_oauth::GeminiOAuthProvider;
use super::oauth;
use super::vertexai::*;
use super::*;

use anyhow::{Context, Result, bail};
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct GeminiConfig {
    pub name: Option<String>,
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    pub auth: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    pub patch: Option<RequestPatch>,
    pub extra: Option<ExtraConfig>,
}

impl GeminiClient {
    config_get_fn!(api_key, get_api_key);
    config_get_fn!(api_base, get_api_base);

    create_oauth_supported_client_config!();
}

#[async_trait::async_trait]
impl Client for GeminiClient {
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
        gemini_chat_completions(builder, self.model()).await
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        let request_data = prepare_chat_completions(self, client, data).await?;
        let builder = self.request_builder(client, request_data);
        gemini_chat_completions_streaming(builder, handler, self.model()).await
    }

    async fn embeddings_inner(
        &self,
        client: &ReqwestClient,
        data: &EmbeddingsData,
    ) -> Result<EmbeddingsOutput> {
        let request_data = prepare_embeddings(self, client, data).await?;
        let builder = self.request_builder(client, request_data);
        embeddings(builder, self.model()).await
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
    self_: &GeminiClient,
    client: &ReqwestClient,
    data: ChatCompletionsData,
) -> Result<RequestData> {
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let func = match data.stream {
        true => "streamGenerateContent",
        false => "generateContent",
    };

    let url = format!(
        "{}/models/{}:{}",
        api_base.trim_end_matches('/'),
        self_.model.real_name(),
        func
    );

    let body = gemini_build_chat_completions_body(data, &self_.model)?;
    let mut request_data = RequestData::new(url, body);

    let uses_oauth = self_.config.auth.as_deref() == Some("oauth");

    if uses_oauth {
        let provider = GeminiOAuthProvider;
        let ready = oauth::prepare_oauth_access_token(client, &provider, self_.name()).await?;
        if !ready {
            bail!(
                "OAuth configured but no tokens found for '{}'. Run: 'loki --authenticate {}' or '.authenticate' in the REPL",
                self_.name(),
                self_.name()
            );
        }
        let token = get_access_token(self_.name())?;
        request_data.bearer_auth(token);
    } else if let Ok(api_key) = self_.get_api_key() {
        request_data.header("x-goog-api-key", api_key);
    } else {
        bail!(
            "No authentication configured for '{}'. Set `api_key` or use `auth: oauth` with `loki --authenticate {}`.",
            self_.name(),
            self_.name()
        );
    }

    Ok(request_data)
}

async fn prepare_embeddings(
    self_: &GeminiClient,
    client: &ReqwestClient,
    data: &EmbeddingsData,
) -> Result<RequestData> {
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let uses_oauth = self_.config.auth.as_deref() == Some("oauth");

    let url = if uses_oauth {
        format!(
            "{}/models/{}:batchEmbedContents",
            api_base.trim_end_matches('/'),
            self_.model.real_name(),
        )
    } else {
        let api_key = self_.get_api_key()?;
        format!(
            "{}/models/{}:batchEmbedContents?key={}",
            api_base.trim_end_matches('/'),
            self_.model.real_name(),
            api_key
        )
    };

    let model_id = format!("models/{}", self_.model.real_name());

    let requests: Vec<_> = data
        .texts
        .iter()
        .map(|text| {
            json!({
                "model": model_id,
                "content": {
                    "parts": [{ "text": text }]
                },
            })
        })
        .collect();

    let body = json!({ "requests": requests });
    let mut request_data = RequestData::new(url, body);

    if uses_oauth {
        let provider = GeminiOAuthProvider;
        let ready = oauth::prepare_oauth_access_token(client, &provider, self_.name()).await?;
        if !ready {
            bail!(
                "OAuth configured but no tokens found for '{}'. Run: 'loki --authenticate {}' or '.authenticate' in the REPL",
                self_.name(),
                self_.name()
            );
        }
        let token = get_access_token(self_.name())?;
        request_data.bearer_auth(token);
    }

    Ok(request_data)
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
        .embeddings
        .into_iter()
        .map(|embedding| embedding.values)
        .collect();
    Ok(output)
}

#[derive(Deserialize)]
struct EmbeddingsResBody {
    embeddings: Vec<EmbeddingsResBodyEmbedding>,
}

#[derive(Deserialize)]
struct EmbeddingsResBodyEmbedding {
    values: Vec<f32>,
}
