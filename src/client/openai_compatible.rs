use super::access_token::get_access_token;
use super::oauth;
use super::openai::*;
use super::*;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use oauth::OAuthConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAICompatibleConfig {
    pub name: Option<String>,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub auth: Option<String>,
    pub oauth: Option<Box<OAuthConfig>>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    pub patch: Option<RequestPatch>,
    pub extra: Option<ExtraConfig>,
}

impl OpenAICompatibleClient {
    config_get_fn!(api_base, get_api_base);
    config_get_fn!(api_key, get_api_key);

    create_client_config!([]);
}

#[async_trait::async_trait]
impl Client for OpenAICompatibleClient {
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
        
        openai_chat_completions(builder, self.model()).await
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        let request_data = prepare_chat_completions(self, client, data).await?;
        let builder = self.request_builder(client, request_data);
        
        openai_chat_completions_streaming(builder, handler, self.model()).await
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
        let request_data = prepare_rerank(self, client, data).await?;
        let builder = self.request_builder(client, request_data);
        
        generic_rerank(builder, self.model()).await
    }
}

async fn prepare_chat_completions(
    self_: &OpenAICompatibleClient,
    client: &ReqwestClient,
    data: ChatCompletionsData,
) -> Result<RequestData> {
    let api_base = get_api_base_ext(self_)?;
    let url = format!("{api_base}/chat/completions");
    let body = openai_build_chat_completions_body(data, &self_.model);
    let mut request_data = RequestData::new(url, body);
    
    apply_auth(self_, client, &mut request_data).await?;
    
    Ok(request_data)
}

async fn prepare_embeddings(
    self_: &OpenAICompatibleClient,
    client: &ReqwestClient,
    data: &EmbeddingsData,
) -> Result<RequestData> {
    let api_base = get_api_base_ext(self_)?;
    let url = format!("{api_base}/embeddings");
    let body = openai_build_embeddings_body(data, &self_.model);
    let mut request_data = RequestData::new(url, body);
    
    apply_auth(self_, client, &mut request_data).await?;
    
    Ok(request_data)
}

async fn prepare_rerank(
    self_: &OpenAICompatibleClient,
    client: &ReqwestClient,
    data: &RerankData,
) -> Result<RequestData> {
    let api_base = get_api_base_ext(self_)?;
    let url = if self_.name().starts_with("ernie") {
        format!("{api_base}/rerankers")
    } else {
        format!("{api_base}/rerank")
    };
    let body = generic_build_rerank_body(data, &self_.model);
    let mut request_data = RequestData::new(url, body);
    
    apply_auth(self_, client, &mut request_data).await?;
    
    Ok(request_data)
}

async fn apply_auth(
    self_: &OpenAICompatibleClient,
    client: &ReqwestClient,
    request_data: &mut RequestData,
) -> Result<()> {
    if self_.config.auth.as_deref() == Some("oauth") {
        let client_name = self_.name();
        let app_config = self_.app_config();
        let cc = app_config
            .clients
            .iter()
            .find(|cc| {
                matches!(
                    cc,
                    ClientConfig::OpenAICompatibleConfig(c)
                    if c.name.as_deref().unwrap_or("openai-compatible") == client_name
                )
            })
            .ok_or_else(|| {
                anyhow!("Could not locate ClientConfig entry for '{}'", client_name)
            })?;
        let provider = oauth::get_oauth_provider_for_client(cc, &ALL_PROVIDER_MODELS)
            .ok_or_else(|| {
                anyhow!(
                    "OAuth configured for '{}' but no oauth block resolved (missing from both models.yaml and user config)",
                    client_name
                )
            })?;
        
        let ready = oauth::prepare_oauth_access_token(client, &*provider, client_name).await?;
        if !ready {
            bail!(
                "OAuth configured for '{}' but no tokens found. Run: 'coyote --authenticate {}' or '.authenticate' in the REPL",
                client_name,
                client_name
            );
        }
        
        let token = get_access_token(client_name)?;
        request_data.bearer_auth(token);
        
        for (key, value) in provider.extra_request_headers() {
            request_data.header(key, value);
        }
    } else if let Ok(api_key) = self_.get_api_key() {
        request_data.bearer_auth(api_key);
    }
    Ok(())
}

fn get_api_base_ext(self_: &OpenAICompatibleClient) -> Result<String> {
    let api_base = match self_.get_api_base() {
        Ok(v) => v,
        Err(err) => {
            match OPENAI_COMPATIBLE_PROVIDERS
                .into_iter()
                .find_map(|(name, api_base)| {
                    if name == self_.model.client_name() {
                        Some(api_base.to_string())
                    } else {
                        None
                    }
                }) {
                Some(v) => v,
                None => return Err(err),
            }
        }
    };
    Ok(api_base.trim_end_matches('/').to_string())
}

pub async fn generic_rerank(builder: RequestBuilder, _model: &Model) -> Result<RerankOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let mut data: Value = res.json().await?;
    if !status.is_success() {
        catch_error(&data, status.as_u16())?;
    }
    if data.get("results").is_none()
        && data.get("data").is_some()
        && let Some(data_obj) = data.as_object_mut()
        && let Some(value) = data_obj.remove("data")
    {
        data_obj.insert("results".to_string(), value);
    }
    let res_body: GenericRerankResBody =
        serde_json::from_value(data).context("Invalid rerank data")?;
    Ok(res_body.results)
}

#[derive(Deserialize)]
pub struct GenericRerankResBody {
    pub results: RerankOutput,
}

pub fn generic_build_rerank_body(data: &RerankData, model: &Model) -> Value {
    let RerankData {
        query,
        documents,
        top_n,
    } = data;

    let mut body = json!({
        "model": model.real_name(),
        "query": query,
        "documents": documents,
    });
    if model.client_name().starts_with("voyageai") {
        body["top_k"] = (*top_n).into()
    } else {
        body["top_n"] = (*top_n).into()
    }
    body
}
