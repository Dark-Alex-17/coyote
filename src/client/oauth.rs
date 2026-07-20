use super::access_token::{is_valid_access_token, set_access_token};
use super::openai_compatible_oauth::OpenAICompatibleOAuthProvider;
use super::{ClientConfig, ProviderModels};
use crate::config::paths;
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use indexmap::IndexMap;
use inquire::Text;
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenRequestFormat {
    Json,
    FormUrlEncoded,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OAuthFlow {
    #[default]
    Pkce,
    ClientCredentials,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
    pub client_id: String,
    pub token_url: String,
    #[serde(default)]
    pub flow: OAuthFlow,

    pub client_secret: Option<String>,
    pub authorize_url: Option<String>,
    pub redirect_uri: Option<String>,
    pub redirect_port: Option<u16>,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub token_request_format: Option<TokenRequestFormat>,
    #[serde(default)]
    pub extra_authorize_params: IndexMap<String, String>,
    #[serde(default)]
    pub extra_token_headers: IndexMap<String, String>,
    #[serde(default)]
    pub extra_request_headers: IndexMap<String, String>,
    #[serde(default)]
    pub echo_pkce_in_token_exchange: bool,
    #[serde(default = "default_true")]
    pub include_state_in_token_exchange: bool,
}

fn default_true() -> bool {
    true
}

impl OAuthConfig {
    pub fn merge(mut self, override_cfg: OAuthConfig) -> Self {
        self.client_id = override_cfg.client_id;
        self.token_url = override_cfg.token_url;
        self.flow = override_cfg.flow;
        if override_cfg.client_secret.is_some() {
            self.client_secret = override_cfg.client_secret;
        }
        if override_cfg.authorize_url.is_some() {
            self.authorize_url = override_cfg.authorize_url;
        }
        if override_cfg.redirect_uri.is_some() {
            self.redirect_uri = override_cfg.redirect_uri;
        }
        if override_cfg.redirect_port.is_some() {
            self.redirect_port = override_cfg.redirect_port;
        }
        if !override_cfg.scopes.is_empty() {
            self.scopes = override_cfg.scopes;
        }
        if override_cfg.token_request_format.is_some() {
            self.token_request_format = override_cfg.token_request_format;
        }
        if !override_cfg.extra_authorize_params.is_empty() {
            self.extra_authorize_params
                .extend(override_cfg.extra_authorize_params);
        }
        if !override_cfg.extra_token_headers.is_empty() {
            self.extra_token_headers
                .extend(override_cfg.extra_token_headers);
        }
        if !override_cfg.extra_request_headers.is_empty() {
            self.extra_request_headers
                .extend(override_cfg.extra_request_headers);
        }

        self.echo_pkce_in_token_exchange = override_cfg.echo_pkce_in_token_exchange;
        self.include_state_in_token_exchange = override_cfg.include_state_in_token_exchange;

        self
    }
}

pub trait OAuthProvider: Send + Sync {
    fn provider_name(&self) -> &str;
    fn client_id(&self) -> &str;
    fn authorize_url(&self) -> &str;
    fn token_url(&self) -> &str;
    fn redirect_uri(&self) -> &str;
    fn scopes(&self) -> String;

    fn client_secret(&self) -> Option<&str> {
        None
    }

    fn extra_authorize_params(&self) -> Vec<(&str, &str)> {
        vec![]
    }

    fn token_request_format(&self) -> TokenRequestFormat {
        TokenRequestFormat::Json
    }

    fn uses_localhost_redirect(&self) -> bool {
        false
    }

    fn extra_token_headers(&self) -> Vec<(&str, &str)> {
        vec![]
    }

    fn extra_request_headers(&self) -> Vec<(&str, &str)> {
        vec![]
    }

    fn fixed_redirect_uri(&self) -> Option<String> {
        None
    }

    fn extract_account_id(&self, _response: &Value) -> Option<String> {
        None
    }

    fn include_state_in_token_exchange(&self) -> bool {
        true
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::Pkce
    }

    fn echo_pkce_in_token_exchange(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    #[serde(default)]
    pub account_id: Option<String>,
}

pub async fn run_oauth_flow(provider: &dyn OAuthProvider, client_name: &str) -> Result<()> {
    match provider.flow() {
        OAuthFlow::Pkce => run_pkce_flow(provider, client_name).await,
        OAuthFlow::ClientCredentials => run_client_credentials_flow(provider, client_name).await,
    }
}

async fn run_pkce_flow(provider: &dyn OAuthProvider, client_name: &str) -> Result<()> {
    let random_bytes: [u8; 32] = rand::random::<[u8; 32]>();
    let code_verifier = URL_SAFE_NO_PAD.encode(random_bytes);

    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

    let state = Uuid::new_v4().to_string();

    let (redirect_uri, use_callback_listener) = if let Some(fixed) = provider.fixed_redirect_uri() {
        (fixed, true)
    } else if provider.uses_localhost_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let uri = format!("http://127.0.0.1:{port}/callback");
        drop(listener);
        (uri, true)
    } else {
        (provider.redirect_uri().to_string(), false)
    };

    let scopes = provider.scopes();
    let encoded_scopes = urlencoding::encode(&scopes);
    let encoded_redirect = urlencoding::encode(&redirect_uri);

    let mut authorize_url = format!(
        "{}?client_id={}&response_type=code&scope={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        provider.authorize_url(),
        provider.client_id(),
        encoded_scopes,
        encoded_redirect,
        code_challenge,
        state
    );

    for (key, value) in provider.extra_authorize_params() {
        authorize_url.push_str(&format!(
            "&{}={}",
            urlencoding::encode(key),
            urlencoding::encode(value)
        ));
    }

    println!(
        "\nOpen this URL to authenticate with {} (client '{}'):\n",
        provider.provider_name(),
        client_name
    );
    println!("  {authorize_url}\n");

    let _ = open::that(&authorize_url);

    let (code, returned_state) = if use_callback_listener {
        listen_for_oauth_callback(&redirect_uri)?
    } else {
        let input = Text::new("Paste the authorization code:").prompt()?;
        let parts: Vec<&str> = input.splitn(2, '#').collect();
        if parts.len() != 2 {
            bail!("Invalid authorization code format. Expected format: <code>#<state>");
        }
        (parts[0].to_string(), parts[1].to_string())
    };

    if returned_state != state {
        bail!(
            "OAuth state mismatch: expected '{state}', got '{returned_state}'. \
             This may indicate a CSRF attack or a stale authorization attempt."
        );
    }

    let client = ReqwestClient::new();
    let mut token_params = vec![
        ("grant_type", "authorization_code"),
        ("client_id", provider.client_id()),
        ("code", code.as_str()),
        ("code_verifier", code_verifier.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
    ];
    if provider.include_state_in_token_exchange() {
        token_params.push(("state", state.as_str()));
    }
    if provider.echo_pkce_in_token_exchange() {
        token_params.push(("code_challenge", code_challenge.as_str()));
        token_params.push(("code_challenge_method", "S256"));
    }
    let request = build_token_request(&client, provider, &token_params);

    let response: Value = request.send().await?.json().await?;

    let access_token = response["access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing access_token in response: {response}"))?
        .to_string();
    let refresh_token = response["refresh_token"].as_str().map(|s| s.to_string());
    let expires_in = response["expires_in"]
        .as_i64()
        .ok_or_else(|| anyhow!("Missing expires_in in response: {response}"))?;

    let expires_at = Utc::now().timestamp() + expires_in;

    let account_id = provider.extract_account_id(&response);

    let tokens = OAuthTokens {
        access_token,
        refresh_token,
        expires_at,
        account_id,
    };

    save_oauth_tokens(client_name, &tokens)?;

    println!(
        "Successfully authenticated client '{}' with {} via OAuth. Tokens saved.",
        client_name,
        provider.provider_name()
    );

    Ok(())
}

async fn run_client_credentials_flow(
    provider: &dyn OAuthProvider,
    client_name: &str,
) -> Result<()> {
    let client = ReqwestClient::new();
    let scopes = provider.scopes();
    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "client_credentials"),
        ("client_id", provider.client_id()),
    ];
    if !scopes.is_empty() {
        params.push(("scope", scopes.as_str()));
    }

    let request = build_token_request(&client, provider, &params);
    let response: Value = request.send().await?.json().await?;

    let access_token = response["access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing access_token in client_credentials response: {response}"))?
        .to_string();
    let expires_in = response["expires_in"]
        .as_i64()
        .ok_or_else(|| anyhow!("Missing expires_in in client_credentials response: {response}"))?;
    let expires_at = Utc::now().timestamp() + expires_in;

    let tokens = OAuthTokens {
        access_token,
        refresh_token: None,
        expires_at,
        account_id: provider.extract_account_id(&response),
    };
    save_oauth_tokens(client_name, &tokens)?;
    println!(
        "Successfully authenticated client '{}' with {} via OAuth (client_credentials). Tokens saved.",
        client_name,
        provider.provider_name()
    );

    Ok(())
}

pub fn load_oauth_tokens(client_name: &str) -> Option<OAuthTokens> {
    let path = paths::token_file(client_name);
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_oauth_tokens(client_name: &str, tokens: &OAuthTokens) -> Result<()> {
    let path = paths::token_file(client_name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(tokens)?;
    fs::write(path, json)?;
    Ok(())
}

pub async fn refresh_oauth_token(
    client: &ReqwestClient,
    provider: &dyn OAuthProvider,
    client_name: &str,
    tokens: &OAuthTokens,
) -> Result<OAuthTokens> {
    let refresh_token_val = tokens.refresh_token.as_deref().ok_or_else(|| {
        anyhow!(
            "No refresh token available for '{}'. Please re-authenticate.",
            client_name
        )
    })?;
    let request = build_token_request(
        client,
        provider,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", provider.client_id()),
            ("refresh_token", refresh_token_val),
        ],
    );

    let response: Value = request.send().await?.json().await?;

    let access_token = response["access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing access_token in refresh response: {response}"))?
        .to_string();
    let refresh_token = response["refresh_token"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| tokens.refresh_token.clone());
    let expires_in = response["expires_in"]
        .as_i64()
        .ok_or_else(|| anyhow!("Missing expires_in in refresh response: {response}"))?;

    let expires_at = Utc::now().timestamp() + expires_in;

    let account_id = provider
        .extract_account_id(&response)
        .or_else(|| tokens.account_id.clone());

    let new_tokens = OAuthTokens {
        access_token,
        refresh_token,
        expires_at,
        account_id,
    };

    save_oauth_tokens(client_name, &new_tokens)?;

    Ok(new_tokens)
}

pub async fn prepare_oauth_access_token(
    client: &ReqwestClient,
    provider: &dyn OAuthProvider,
    client_name: &str,
) -> Result<bool> {
    if is_valid_access_token(client_name) {
        return Ok(true);
    }

    let tokens = match load_oauth_tokens(client_name) {
        Some(t) => t,
        None => return Ok(false),
    };

    let tokens = if Utc::now().timestamp() >= tokens.expires_at {
        match provider.flow() {
            OAuthFlow::Pkce => refresh_oauth_token(client, provider, client_name, &tokens).await?,
            OAuthFlow::ClientCredentials => {
                run_client_credentials_flow(provider, client_name).await?;
                load_oauth_tokens(client_name)
                    .ok_or_else(|| anyhow!("Token file missing after client_credentials refresh"))?
            }
        }
    } else {
        tokens
    };

    set_access_token(
        client_name,
        tokens.access_token,
        tokens.expires_at,
        tokens.account_id,
    );

    Ok(true)
}

fn build_token_request(
    client: &ReqwestClient,
    provider: &(impl OAuthProvider + ?Sized),
    params: &[(&str, &str)],
) -> RequestBuilder {
    let mut request = match provider.token_request_format() {
        TokenRequestFormat::Json => {
            let body: serde_json::Map<String, Value> = params
                .iter()
                .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
                .collect();
            if let Some(secret) = provider.client_secret() {
                let mut body = body;
                body.insert(
                    "client_secret".to_string(),
                    Value::String(secret.to_string()),
                );
                client.post(provider.token_url()).json(&body)
            } else {
                client.post(provider.token_url()).json(&body)
            }
        }
        TokenRequestFormat::FormUrlEncoded => {
            let mut form: HashMap<String, String> = params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            if let Some(secret) = provider.client_secret() {
                form.insert("client_secret".to_string(), secret.to_string());
            }
            client.post(provider.token_url()).form(&form)
        }
    };

    for (key, value) in provider.extra_token_headers() {
        request = request.header(key, value);
    }

    request
}

fn listen_for_oauth_callback(redirect_uri: &str) -> Result<(String, String)> {
    let url: Url = redirect_uri.parse()?;
    let host = url.host_str().unwrap_or("127.0.0.1");
    let port = url
        .port()
        .ok_or_else(|| anyhow!("No port in redirect URI"))?;
    let path = url.path();

    println!("Waiting for OAuth callback on {redirect_uri} ...");
    println!(
        "(If the browser shows a 'paste this code' page, ignore it. Coyote captures the callback automatically.)\n"
    );

    let listener = TcpListener::bind(format!("{host}:{port}"))?;

    loop {
        let (mut stream, _) = listener.accept()?;
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
            continue;
        }

        let Some(request_path) = request_line.split_whitespace().nth(1) else {
            continue;
        };

        let Ok(parsed) = format!("http://{host}:{port}{request_path}").parse::<Url>() else {
            continue;
        };

        if !parsed.path().starts_with(path) {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            continue;
        }

        let code = parsed
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.to_string())
            .ok_or_else(|| {
                let error = parsed
                    .query_pairs()
                    .find(|(k, _)| k == "error")
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                anyhow!("OAuth callback returned error: {error}")
            })?;

        let returned_state = parsed
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.to_string())
            .ok_or_else(|| anyhow!("Missing state parameter in OAuth callback"))?;

        let response_body = "<html><body><h2>Authentication successful!</h2><p>You can close this tab and return to your terminal.</p></body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream.write_all(response.as_bytes())?;

        return Ok((code, returned_state));
    }
}

pub fn get_oauth_provider(provider_type: &str) -> Option<Box<dyn OAuthProvider>> {
    match provider_type {
        "claude" => Some(Box::new(super::claude_oauth::ClaudeOAuthProvider)),
        "gemini" => Some(Box::new(super::gemini_oauth::GeminiOAuthProvider)),
        "openai" => Some(Box::new(super::openai_oauth::OpenAIOAuthProvider)),
        _ => None,
    }
}

pub fn get_oauth_provider_for_client(
    client_config: &ClientConfig,
    all_provider_models: &[ProviderModels],
) -> Option<Box<dyn OAuthProvider>> {
    let (client_name, provider_type, auth) = client_config_info(client_config);
    if auth != Some("oauth") {
        return None;
    }

    match client_config {
        ClientConfig::OpenAICompatibleConfig(c) => {
            let base = all_provider_models
                .iter()
                .find(|p| p.provider == client_name)
                .and_then(|p| p.oauth.clone());
            let user_oauth = c.oauth.clone().map(|b| *b);
            let merged = match (base, user_oauth) {
                (None, None) => return None,
                (Some(b), None) => b,
                (None, Some(u)) => u,
                (Some(b), Some(u)) => b.merge(u),
            };
            Some(Box::new(OpenAICompatibleOAuthProvider {
                config: merged,
                client_name: client_name.to_string(),
            }))
        }
        _ => get_oauth_provider(provider_type),
    }
}

pub fn list_oauth_capable_clients(clients: &[ClientConfig]) -> Vec<String> {
    clients
        .iter()
        .filter_map(|client_config| {
            let (name, _, auth) = client_config_info(client_config);
            if auth == Some("oauth") {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

pub(crate) fn client_config_info(
    client_config: &ClientConfig,
) -> (&str, &'static str, Option<&str>) {
    match client_config {
        ClientConfig::ClaudeConfig(c) => (
            c.name.as_deref().unwrap_or("claude"),
            "claude",
            c.auth.as_deref(),
        ),
        ClientConfig::OpenAIConfig(c) => (
            c.name.as_deref().unwrap_or("openai"),
            "openai",
            c.auth.as_deref(),
        ),
        ClientConfig::OpenAICompatibleConfig(c) => (
            c.name.as_deref().unwrap_or("openai-compatible"),
            "openai-compatible",
            c.auth.as_deref(),
        ),
        ClientConfig::GeminiConfig(c) => (
            c.name.as_deref().unwrap_or("gemini"),
            "gemini",
            c.auth.as_deref(),
        ),
        ClientConfig::CohereConfig(c) => (c.name.as_deref().unwrap_or("cohere"), "cohere", None),
        ClientConfig::AzureOpenAIConfig(c) => (
            c.name.as_deref().unwrap_or("azure-openai"),
            "azure-openai",
            None,
        ),
        ClientConfig::VertexAIConfig(c) => {
            (c.name.as_deref().unwrap_or("vertexai"), "vertexai", None)
        }
        ClientConfig::BedrockConfig(c) => (c.name.as_deref().unwrap_or("bedrock"), "bedrock", None),
        ClientConfig::Unknown => ("unknown", "unknown", None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::openai_compatible::OpenAICompatibleConfig;
    use crate::client::{ModelData, ProviderModels};

    fn base_config() -> OAuthConfig {
        OAuthConfig {
            client_id: "base-id".into(),
            token_url: "https://base.example/token".into(),
            flow: OAuthFlow::Pkce,
            client_secret: Some("base-secret".into()),
            authorize_url: Some("https://base.example/authorize".into()),
            redirect_uri: None,
            redirect_port: Some(1234),
            scopes: vec!["a".into(), "b".into()],
            token_request_format: Some(TokenRequestFormat::FormUrlEncoded),
            extra_authorize_params: IndexMap::from([("plan".into(), "base".into())]),
            extra_token_headers: IndexMap::new(),
            extra_request_headers: IndexMap::new(),
            echo_pkce_in_token_exchange: false,
            include_state_in_token_exchange: true,
        }
    }

    fn empty_user_override(client_id: &str, token_url: &str) -> OAuthConfig {
        OAuthConfig {
            client_id: client_id.into(),
            token_url: token_url.into(),
            flow: OAuthFlow::Pkce,
            client_secret: None,
            authorize_url: None,
            redirect_uri: None,
            redirect_port: None,
            scopes: vec![],
            token_request_format: None,
            extra_authorize_params: IndexMap::new(),
            extra_token_headers: IndexMap::new(),
            extra_request_headers: IndexMap::new(),
            echo_pkce_in_token_exchange: false,
            include_state_in_token_exchange: true,
        }
    }

    #[test]
    fn oauth_config_merge_user_wins_per_field() {
        let base = base_config();
        let mut user = empty_user_override("user-id", "https://user.example/token");
        user.client_secret = Some("user-secret".into());
        user.scopes = vec!["c".into()];
        user.extra_authorize_params = IndexMap::from([("plan".into(), "user".into())]);

        let merged = base.merge(user);

        assert_eq!(merged.client_id, "user-id");
        assert_eq!(merged.token_url, "https://user.example/token");
        assert_eq!(merged.client_secret.as_deref(), Some("user-secret"));
        assert_eq!(
            merged.authorize_url.as_deref(),
            Some("https://base.example/authorize")
        );
        assert_eq!(merged.redirect_port, Some(1234));
        assert_eq!(merged.scopes, vec!["c"]);
        assert_eq!(
            merged
                .extra_authorize_params
                .get("plan")
                .map(String::as_str),
            Some("user")
        );
    }

    #[test]
    fn oauth_config_merge_empty_user_keeps_base_optionals() {
        let base = base_config();
        let user = empty_user_override("user-id", "https://user.example/token");

        let merged = base.merge(user);

        assert_eq!(merged.client_id, "user-id");
        assert_eq!(merged.token_url, "https://user.example/token");
        assert_eq!(merged.client_secret.as_deref(), Some("base-secret"));
        assert_eq!(
            merged.authorize_url.as_deref(),
            Some("https://base.example/authorize")
        );
        assert_eq!(merged.redirect_port, Some(1234));
        assert_eq!(merged.scopes, vec!["a", "b"]);
        assert!(matches!(
            merged.token_request_format,
            Some(TokenRequestFormat::FormUrlEncoded)
        ));
        assert_eq!(
            merged
                .extra_authorize_params
                .get("plan")
                .map(String::as_str),
            Some("base")
        );
    }

    #[test]
    fn oauth_config_serde_roundtrip_from_yaml() {
        let yaml = r#"
client_id: xai-client
token_url: https://auth.x.ai/oauth2/token
authorize_url: https://auth.x.ai/oauth2/authorize
scopes:
  - openid
  - profile
  - api:access
redirect_port: 56121
flow: pkce
token_request_format: form_url_encoded
extra_authorize_params:
  plan: generic
  referrer: coyote
echo_pkce_in_token_exchange: true
"#;

        let cfg: OAuthConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(cfg.client_id, "xai-client");
        assert_eq!(cfg.token_url, "https://auth.x.ai/oauth2/token");
        assert_eq!(cfg.scopes.len(), 3);
        assert_eq!(cfg.redirect_port, Some(56121));
        assert!(matches!(cfg.flow, OAuthFlow::Pkce));
        assert!(matches!(
            cfg.token_request_format,
            Some(TokenRequestFormat::FormUrlEncoded)
        ));
        assert!(cfg.echo_pkce_in_token_exchange);
        assert!(cfg.include_state_in_token_exchange);
        assert_eq!(
            cfg.extra_authorize_params.get("plan").map(String::as_str),
            Some("generic")
        );
    }

    #[test]
    fn oauth_flow_defaults_to_pkce_when_missing() {
        let yaml = "client_id: x\ntoken_url: y";

        let cfg: OAuthConfig = serde_yaml::from_str(yaml).unwrap();

        assert!(matches!(cfg.flow, OAuthFlow::Pkce));
    }

    #[test]
    fn oauth_flow_client_credentials_parses() {
        let yaml = "client_id: x\ntoken_url: y\nflow: client_credentials";

        let cfg: OAuthConfig = serde_yaml::from_str(yaml).unwrap();

        assert!(matches!(cfg.flow, OAuthFlow::ClientCredentials));
    }

    fn make_provider_models(provider: &str, oauth: Option<OAuthConfig>) -> ProviderModels {
        ProviderModels {
            provider: provider.into(),
            oauth,
            models: vec![ModelData::new("some-model")],
        }
    }

    fn make_openai_compat_client(
        name: &str,
        auth: Option<&str>,
        oauth: Option<OAuthConfig>,
    ) -> ClientConfig {
        ClientConfig::OpenAICompatibleConfig(OpenAICompatibleConfig {
            name: Some(name.into()),
            api_base: Some("https://api.example/v1".into()),
            api_key: None,
            auth: auth.map(str::to_string),
            oauth: oauth.map(Box::new),
            models: vec![],
            patch: None,
            extra: None,
        })
    }

    #[test]
    fn get_oauth_provider_for_client_merges_defaults_with_user_override() {
        let base = base_config();
        let mut user = empty_user_override("user-id", "https://user.example/token");
        user.echo_pkce_in_token_exchange = true;
        let models = vec![make_provider_models("acme", Some(base))];
        let cc = make_openai_compat_client("acme", Some("oauth"), Some(user));

        let provider = get_oauth_provider_for_client(&cc, &models).unwrap();

        assert_eq!(provider.client_id(), "user-id");
        assert_eq!(provider.token_url(), "https://user.example/token");
        assert!(provider.echo_pkce_in_token_exchange());
        assert_eq!(
            provider.fixed_redirect_uri().as_deref(),
            Some("http://127.0.0.1:1234/callback")
        );
    }

    #[test]
    fn get_oauth_provider_for_client_uses_bundled_defaults_only() {
        let base = base_config();
        let models = vec![make_provider_models("bundled-only", Some(base))];
        let cc = make_openai_compat_client("bundled-only", Some("oauth"), None);

        let provider = get_oauth_provider_for_client(&cc, &models).unwrap();

        assert_eq!(provider.client_id(), "base-id");
        assert_eq!(provider.token_url(), "https://base.example/token");
    }

    #[test]
    fn get_oauth_provider_for_client_uses_inline_only() {
        let user = OAuthConfig {
            client_id: "inline-id".into(),
            token_url: "https://inline.example/token".into(),
            ..empty_user_override("inline-id", "https://inline.example/token")
        };
        let cc = make_openai_compat_client("inline-only", Some("oauth"), Some(user));

        let provider = get_oauth_provider_for_client(&cc, &[]).unwrap();

        assert_eq!(provider.client_id(), "inline-id");
    }

    #[test]
    fn get_oauth_provider_for_client_returns_none_when_no_config_anywhere() {
        let cc = make_openai_compat_client("nothing", Some("oauth"), None);

        assert!(get_oauth_provider_for_client(&cc, &[]).is_none());
    }

    #[test]
    fn get_oauth_provider_for_client_returns_none_when_auth_not_oauth() {
        let base = base_config();
        let models = vec![make_provider_models("api-key-client", Some(base))];

        let cc = make_openai_compat_client("api-key-client", None, None);

        assert!(get_oauth_provider_for_client(&cc, &models).is_none());
    }

    #[test]
    fn openai_compatible_provider_joins_scopes_with_spaces() {
        let mut cfg = base_config();
        cfg.scopes = vec!["one".into(), "two".into(), "three".into()];

        let provider = OpenAICompatibleOAuthProvider {
            config: cfg,
            client_name: "test".into(),
        };

        assert_eq!(provider.scopes(), "one two three");
    }

    #[test]
    fn openai_compatible_provider_prefers_redirect_uri_over_port() {
        let mut cfg = base_config();
        cfg.redirect_uri = Some("https://custom.example/cb".into());
        cfg.redirect_port = Some(9999);

        let provider = OpenAICompatibleOAuthProvider {
            config: cfg,
            client_name: "test".into(),
        };

        assert_eq!(
            provider.fixed_redirect_uri().as_deref(),
            Some("https://custom.example/cb")
        );
    }

    #[test]
    fn openai_compatible_provider_ephemeral_when_no_redirect() {
        let mut cfg = base_config();
        cfg.redirect_uri = None;
        cfg.redirect_port = None;

        let provider = OpenAICompatibleOAuthProvider {
            config: cfg,
            client_name: "test".into(),
        };

        assert!(provider.uses_localhost_redirect());
        assert!(provider.fixed_redirect_uri().is_none());
    }
}
