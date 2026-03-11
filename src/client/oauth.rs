use super::ClientConfig;
use super::access_token::{is_valid_access_token, set_access_token};
use crate::config::Config;
use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use inquire::Text;
use reqwest::Client as ReqwestClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use uuid::Uuid;

pub trait OAuthProvider: Send + Sync {
    fn provider_name(&self) -> &str;
    fn client_id(&self) -> &str;
    fn authorize_url(&self) -> &str;
    fn token_url(&self) -> &str;
    fn redirect_uri(&self) -> &str;
    fn scopes(&self) -> &str;

    fn extra_token_headers(&self) -> Vec<(&str, &str)> {
        vec![]
    }

    fn extra_request_headers(&self) -> Vec<(&str, &str)> {
        vec![]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

pub async fn run_oauth_flow(provider: &impl OAuthProvider, client_name: &str) -> Result<()> {
    let random_bytes: [u8; 32] = rand::random::<[u8; 32]>();
    let code_verifier = URL_SAFE_NO_PAD.encode(random_bytes);

    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

    let state = Uuid::new_v4().to_string();

    let encoded_scopes = urlencoding::encode(provider.scopes());
    let encoded_redirect = urlencoding::encode(provider.redirect_uri());

    let authorize_url = format!(
        "{}?code=true&client_id={}&response_type=code&scope={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        provider.authorize_url(),
        provider.client_id(),
        encoded_scopes,
        encoded_redirect,
        code_challenge,
        state
    );

    println!(
        "\nOpen this URL to authenticate with {} (client '{}'):\n",
        provider.provider_name(),
        client_name
    );
    println!("  {authorize_url}\n");

    let _ = open::that(&authorize_url);

    let input = Text::new("Paste the authorization code:").prompt()?;

    let parts: Vec<&str> = input.splitn(2, '#').collect();
    if parts.len() != 2 {
        bail!("Invalid authorization code format. Expected format: <code>#<state>");
    }
    let code = parts[0];
    let returned_state = parts[1];

    if returned_state != state {
        bail!(
            "OAuth state mismatch: expected '{state}', got '{returned_state}'. \
             This may indicate a CSRF attack or a stale authorization attempt."
        );
    }

    let client = ReqwestClient::new();
    let mut request = client.post(provider.token_url()).json(&json!({
        "grant_type": "authorization_code",
        "client_id": provider.client_id(),
        "code": code,
        "code_verifier": code_verifier,
        "redirect_uri": provider.redirect_uri(),
        "state": state,
    }));

    for (key, value) in provider.extra_token_headers() {
        request = request.header(key, value);
    }

    let response: Value = request.send().await?.json().await?;

    let access_token = response["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing access_token in response: {response}"))?
        .to_string();
    let refresh_token = response["refresh_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing refresh_token in response: {response}"))?
        .to_string();
    let expires_in = response["expires_in"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing expires_in in response: {response}"))?;

    let expires_at = Utc::now().timestamp() + expires_in;

    let tokens = OAuthTokens {
        access_token,
        refresh_token,
        expires_at,
    };

    save_oauth_tokens(client_name, &tokens)?;

    println!(
        "Successfully authenticated client '{}' with {} via OAuth. Tokens saved.",
        client_name,
        provider.provider_name()
    );

    Ok(())
}

pub fn load_oauth_tokens(client_name: &str) -> Option<OAuthTokens> {
    let path = Config::token_file(client_name);
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_oauth_tokens(client_name: &str, tokens: &OAuthTokens) -> Result<()> {
    let path = Config::token_file(client_name);
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
    let mut request = client.post(provider.token_url()).json(&json!({
        "grant_type": "refresh_token",
        "client_id": provider.client_id(),
        "refresh_token": tokens.refresh_token,
    }));

    for (key, value) in provider.extra_token_headers() {
        request = request.header(key, value);
    }

    let response: Value = request.send().await?.json().await?;

    let access_token = response["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing access_token in refresh response: {response}"))?
        .to_string();
    let refresh_token = response["refresh_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing refresh_token in refresh response: {response}"))?
        .to_string();
    let expires_in = response["expires_in"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing expires_in in refresh response: {response}"))?;

    let expires_at = Utc::now().timestamp() + expires_in;

    let new_tokens = OAuthTokens {
        access_token,
        refresh_token,
        expires_at,
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
        refresh_oauth_token(client, provider, client_name, &tokens).await?
    } else {
        tokens
    };

    set_access_token(client_name, tokens.access_token.clone(), tokens.expires_at);

    Ok(true)
}

pub fn get_oauth_provider(provider_type: &str) -> Option<impl OAuthProvider> {
    match provider_type {
        "claude" => Some(super::claude_oauth::ClaudeOAuthProvider),
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
        ClientConfig::OpenAIConfig(c) => (c.name.as_deref().unwrap_or("openai"), "openai", None),
        ClientConfig::OpenAICompatibleConfig(c) => (
            c.name.as_deref().unwrap_or("openai-compatible"),
            "openai-compatible",
            None,
        ),
        ClientConfig::GeminiConfig(c) => (c.name.as_deref().unwrap_or("gemini"), "gemini", None),
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
