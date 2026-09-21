use super::access_token::get_access_token;
use super::gemini_oauth::GeminiOAuthProvider;
use super::oauth::{self, OAuthConfig, OAuthProvider};
use super::vertexai::*;
use super::*;

use anyhow::{Context, Result, anyhow, bail};
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
    pub oauth: Option<Box<OAuthConfig>>,
    #[serde(default)]
    pub models: Vec<ModelData>,
    #[serde(default)]
    pub extend_models: bool,
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
    let uses_oauth = self_.config.auth.as_deref() == Some("oauth");

    if !uses_oauth && self_.config.oauth.is_some() {
        bail!(
            "'{}' has an `oauth:` block configured but `auth: oauth` is not set; the oauth block would be ignored. Set `auth: oauth` (and run 'coyote --authenticate {}') or remove the oauth block.",
            self_.name(),
            self_.name()
        );
    }

    let api_base = resolve_api_base(self_)?;

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
        request_data.header("x-goog-api-key", api_key);
    } else {
        bail!(
            "No authentication configured for '{}'. Set `api_key` or use `auth: oauth` with `coyote --authenticate {}`.",
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
    let uses_oauth = self_.config.auth.as_deref() == Some("oauth");

    if !uses_oauth && self_.config.oauth.is_some() {
        bail!(
            "'{}' has an `oauth:` block configured but `auth: oauth` is not set; the oauth block would be ignored. Set `auth: oauth` (and run 'coyote --authenticate {}') or remove the oauth block.",
            self_.name(),
            self_.name()
        );
    }

    let api_base = resolve_api_base(self_)?;

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
    }

    Ok(request_data)
}

fn resolve_api_base(self_: &GeminiClient) -> Result<String> {
    resolve_api_base_against(self_, &ALL_PROVIDER_MODELS)
}

fn resolve_api_base_against(
    self_: &GeminiClient,
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

fn locate_client_config(self_: &GeminiClient) -> Result<&ClientConfig> {
    let client_name = self_.name();
    self_
        .app_config()
        .clients
        .iter()
        .find(|cc| {
            matches!(
                cc,
                ClientConfig::GeminiConfig(c)
                if c.name.as_deref().unwrap_or(GeminiClient::NAME) == client_name
            )
        })
        .ok_or_else(|| anyhow!("Could not locate ClientConfig entry for '{}'", client_name))
}

fn resolve_oauth_provider(self_: &GeminiClient) -> Result<(Box<dyn OAuthProvider>, bool)> {
    match locate_client_config(self_) {
        Ok(cc) => Ok(oauth::gemini_oauth_provider_for_client(
            cc,
            &ALL_PROVIDER_MODELS,
        )),
        Err(_) if self_.config.oauth.is_none() => Ok((Box::new(GeminiOAuthProvider), true)),
        Err(err) => Err(err),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::access_token::set_access_token;
    use crate::config::AppConfig;
    use chrono::Utc;
    use std::sync::Arc;

    fn gemini_config(name: &str, auth: Option<&str>, oauth: Option<OAuthConfig>) -> GeminiConfig {
        GeminiConfig {
            name: Some(name.into()),
            api_base: oauth
                .as_ref()
                .map(|_| "https://gateway.example/v1".to_string()),
            auth: auth.map(str::to_string),
            oauth: oauth.map(Box::new),
            ..Default::default()
        }
    }

    fn make_client(config: GeminiConfig, clients: Vec<ClientConfig>) -> GeminiClient {
        GeminiClient {
            app_config: Arc::new(AppConfig {
                clients,
                ..AppConfig::default()
            }),
            config,
            model: Model::new("gemini", "gemini-test"),
        }
    }

    fn minimal_oauth_config() -> OAuthConfig {
        serde_yaml::from_str("client_id: gateway\ntoken_url: https://gateway.example/token")
            .unwrap()
    }

    fn prepare_chat(client: &GeminiClient) -> Result<RequestData> {
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

    fn prepare_embed(client: &GeminiClient) -> Result<RequestData> {
        let data = EmbeddingsData::new(vec!["hello".to_string()], false);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(prepare_embeddings(client, &ReqwestClient::new(), &data))
    }

    #[test]
    fn oauth_block_without_api_base_is_rejected() {
        let name = "gemini-gate-apibase-missing-test";
        let mut config = gemini_config(name, Some("oauth"), Some(minimal_oauth_config()));
        config.api_base = None;
        let client = make_client(config.clone(), vec![ClientConfig::GeminiConfig(config)]);

        let err = resolve_api_base(&client).unwrap_err().to_string();

        assert!(
            err.contains(name) && err.contains("refusing to fall back"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn catalog_only_oauth_block_without_api_base_is_rejected() {
        let name = "gemini-gate-catalog-oauth-test";
        let config = gemini_config(name, Some("oauth"), None);
        let client = make_client(config.clone(), vec![ClientConfig::GeminiConfig(config)]);
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
        let name = "gemini-gate-apibase-set-test";
        let config = gemini_config(name, Some("oauth"), Some(minimal_oauth_config()));
        let client = make_client(config.clone(), vec![ClientConfig::GeminiConfig(config)]);

        let api_base = resolve_api_base(&client).unwrap();

        assert_eq!(api_base, "https://gateway.example/v1");
    }

    #[test]
    fn no_oauth_block_falls_back_to_stock_api_base() {
        let name = "gemini-gate-apibase-fallback-test";
        let config = gemini_config(name, None, None);
        let client = make_client(config.clone(), vec![ClientConfig::GeminiConfig(config)]);

        let api_base = resolve_api_base(&client).unwrap();

        assert_eq!(api_base, API_BASE);
    }

    #[test]
    fn oauth_block_without_auth_oauth_is_rejected() {
        let name = "gemini-gate-contradiction-test";
        let mut config = gemini_config(name, None, Some(minimal_oauth_config()));
        config.api_key = Some("sk-test".into());
        let client = make_client(config.clone(), vec![ClientConfig::GeminiConfig(config)]);

        let err = match prepare_chat(&client) {
            Ok(_) => panic!("expected the contradictory config to be rejected"),
            Err(err) => err.to_string(),
        };

        assert!(
            err.contains("has an `oauth:` block configured but `auth: oauth` is not set"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn config_oauth_embeddings_url_omits_key_param() {
        let name = "gemini-gate-embed-oauth-test";
        let config = gemini_config(name, Some("oauth"), Some(minimal_oauth_config()));
        let client = make_client(config.clone(), vec![ClientConfig::GeminiConfig(config)]);
        set_access_token(
            name,
            "gateway-at".into(),
            Utc::now().timestamp() + 3600,
            None,
        );

        let request_data = prepare_embed(&client).unwrap();

        assert_eq!(
            request_data.url,
            "https://gateway.example/v1/models/gemini-test:batchEmbedContents"
        );
        assert_eq!(
            request_data
                .headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer gateway-at")
        );
    }

    #[test]
    fn api_key_embeddings_url_appends_key_param() {
        let name = "gemini-gate-embed-apikey-test";
        let mut config = gemini_config(name, None, None);
        config.api_key = Some("sk-test".into());
        let client = make_client(config.clone(), vec![ClientConfig::GeminiConfig(config)]);

        let request_data = prepare_embed(&client).unwrap();

        assert_eq!(
            request_data.url,
            format!("{API_BASE}/models/gemini-test:batchEmbedContents?key=sk-test")
        );
    }

    #[test]
    fn config_oauth_extra_request_headers_reach_request() {
        let name = "gemini-gate-extra-headers-test";
        let oauth: OAuthConfig = serde_yaml::from_str(
            "client_id: gateway\ntoken_url: https://gateway.example/token\nextra_request_headers:\n  x-gateway-tenant: acme",
        )
        .unwrap();
        let config = gemini_config(name, Some("oauth"), Some(oauth));
        let client = make_client(config.clone(), vec![ClientConfig::GeminiConfig(config)]);
        set_access_token(
            name,
            "gateway-at".into(),
            Utc::now().timestamp() + 3600,
            None,
        );

        let request_data = prepare_chat(&client).unwrap();

        assert_eq!(
            request_data
                .headers
                .get("x-gateway-tenant")
                .map(String::as_str),
            Some("acme")
        );
    }

    #[test]
    fn catalog_oauth_block_with_api_key_auth_falls_back_to_stock_api_base() {
        let name = "gemini-gate-catalog-apikey-test";
        let mut config = gemini_config(name, None, None);
        config.api_key = Some("sk-test".into());
        let client = make_client(config.clone(), vec![ClientConfig::GeminiConfig(config)]);
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
        let config = gemini_config("gemini-gate-missing-entry-test", Some("oauth"), None);
        let client = make_client(config, vec![]);

        let (_, is_stock) = resolve_oauth_provider(&client).unwrap();

        assert!(is_stock);
    }

    #[test]
    fn missing_config_entry_with_inline_oauth_is_a_hard_error() {
        let config = gemini_config(
            "gemini-gate-missing-entry-inline-test",
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
    fn auth_mismatch_bails_before_missing_api_base_guard() {
        let name = "gemini-gate-bail-order-test";
        let mut config = gemini_config(name, None, Some(minimal_oauth_config()));
        config.api_base = None;
        let client = make_client(config.clone(), vec![ClientConfig::GeminiConfig(config)]);

        let err = match prepare_chat(&client) {
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
    fn shipped_catalog_resolves_stock_gemini_provider() {
        let cc = ClientConfig::GeminiConfig(gemini_config("gemini", Some("oauth"), None));
        let bundled: Vec<ProviderModels> = serde_yaml::from_str(MODELS_YAML).unwrap();

        let (_, is_stock) = oauth::gemini_oauth_provider_for_client(&cc, &bundled);

        assert!(
            is_stock,
            "bundled models.yaml must not carry a gemini oauth block: it would silently replace the stock Google oauth provider"
        );
    }
}
