use super::ClientConfig;
use super::access_token::{is_valid_access_token, set_access_token};
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

/// Runtime OAuth configuration merged from `models.yaml` provider defaults
/// and user config `clients[i].oauth` overrides.
///
/// Every field except `client_id`, `token_url`, and `flow` is optional so that
/// user config can override individual fields without restating the entire block.
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
    /// Merge a user override into `self` field-by-field. User values win.
    /// Uses `json_patch::merge`-like semantics (see `common.rs:apply_patch`) —
    /// `None` in override means "keep base"; explicit values replace.
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
    fn scopes(&self) -> &str;

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

    let encoded_scopes = urlencoding::encode(provider.scopes());
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
        params.push(("scope", scopes));
    }

    let request = build_token_request(&client, provider, &params);
    let response: Value = request.send().await?.json().await?;

    let access_token = response["access_token"]
        .as_str()
        .ok_or_else(|| {
            anyhow!("Missing access_token in client_credentials response: {response}")
        })?
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
    provider: &impl OAuthProvider,
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
    provider: &impl OAuthProvider,
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
                load_oauth_tokens(client_name).ok_or_else(|| {
                    anyhow!("Token file missing after client_credentials refresh")
                })?
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

    println!("Waiting for OAuth callback on {redirect_uri} ...\n");

    let listener = TcpListener::bind(format!("{host}:{port}"))?;
    let (mut stream, _) = listener.accept()?;

    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let request_path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("Malformed HTTP request from OAuth callback"))?;

    let full_url = format!("http://{host}:{port}{request_path}");
    let parsed: Url = full_url.parse()?;

    if !parsed.path().starts_with(path) {
        bail!("Unexpected callback path: {}", parsed.path());
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

    Ok((code, returned_state))
}

pub fn get_oauth_provider(provider_type: &str) -> Option<Box<dyn OAuthProvider>> {
    match provider_type {
        "claude" => Some(Box::new(super::claude_oauth::ClaudeOAuthProvider)),
        "gemini" => Some(Box::new(super::gemini_oauth::GeminiOAuthProvider)),
        "openai" => Some(Box::new(super::openai_oauth::OpenAIOAuthProvider)),
        _ => None,
    }
}

pub fn resolve_provider_type(client_name: &str, clients: &[ClientConfig]) -> Option<&'static str> {
    for client_config in clients {
        let (config_name, provider_type, auth) = client_config_info(client_config);
        if config_name == client_name {
            if auth == Some("oauth") && get_oauth_provider(provider_type).is_some() {
                return Some(provider_type);
            }
            return None;
        }
    }
    None
}

pub fn list_oauth_capable_clients(clients: &[ClientConfig]) -> Vec<String> {
    clients
        .iter()
        .filter_map(|client_config| {
            let (name, provider_type, auth) = client_config_info(client_config);
            if auth == Some("oauth") && get_oauth_provider(provider_type).is_some() {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn client_config_info(client_config: &ClientConfig) -> (&str, &'static str, Option<&str>) {
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
            None,
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
