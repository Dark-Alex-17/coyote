use super::ClientConfig;
use super::access_token::{is_valid_access_token, set_access_token};
use crate::config::Config;
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
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

pub enum TokenRequestFormat {
    Json,
    FormUrlEncoded,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

pub async fn run_oauth_flow(provider: &dyn OAuthProvider, client_name: &str) -> Result<()> {
    let random_bytes: [u8; 32] = rand::random::<[u8; 32]>();
    let code_verifier = URL_SAFE_NO_PAD.encode(random_bytes);

    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

    let state = Uuid::new_v4().to_string();

    let redirect_uri = if provider.uses_localhost_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let uri = format!("http://127.0.0.1:{port}/callback");
        drop(listener);
        uri
    } else {
        provider.redirect_uri().to_string()
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

    let (code, returned_state) = if provider.uses_localhost_redirect() {
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
    let request = build_token_request(
        &client,
        provider,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", provider.client_id()),
            ("code", &code),
            ("code_verifier", &code_verifier),
            ("redirect_uri", &redirect_uri),
            ("state", &state),
        ],
    );

    let response: Value = request.send().await?.json().await?;

    let access_token = response["access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing access_token in response: {response}"))?
        .to_string();
    let refresh_token = response["refresh_token"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing refresh_token in response: {response}"))?
        .to_string();
    let expires_in = response["expires_in"]
        .as_i64()
        .ok_or_else(|| anyhow!("Missing expires_in in response: {response}"))?;

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
    provider: &impl OAuthProvider,
    client_name: &str,
    tokens: &OAuthTokens,
) -> Result<OAuthTokens> {
    let request = build_token_request(
        client,
        provider,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", provider.client_id()),
            ("refresh_token", &tokens.refresh_token),
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
        .unwrap_or_else(|| tokens.refresh_token.clone());
    let expires_in = response["expires_in"]
        .as_i64()
        .ok_or_else(|| anyhow!("Missing expires_in in refresh response: {response}"))?;

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
