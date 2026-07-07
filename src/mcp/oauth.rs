use crate::client::oauth::{OAuthProvider, TokenRequestFormat, load_oauth_tokens, run_oauth_flow};
use crate::config::paths;
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use inquire::Text;
use log::warn;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::TcpListener;
use url::Url;

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    scopes_supported: Vec<String>,
    registration_endpoint: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct McpRegistration {
    client_id: String,
}

struct McpOAuthProvider {
    client_id: String,
    authorize_url: String,
    token_url: String,
    scopes: String,
    fixed_redirect: String,
}

impl OAuthProvider for McpOAuthProvider {
    fn provider_name(&self) -> &str {
        "MCP"
    }

    fn client_id(&self) -> &str {
        &self.client_id
    }

    fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    fn token_url(&self) -> &str {
        &self.token_url
    }

    fn redirect_uri(&self) -> &str {
        ""
    }

    fn scopes(&self) -> &str {
        &self.scopes
    }

    fn token_request_format(&self) -> TokenRequestFormat {
        TokenRequestFormat::FormUrlEncoded
    }

    fn uses_localhost_redirect(&self) -> bool {
        false
    }

    fn fixed_redirect_uri(&self) -> Option<String> {
        Some(self.fixed_redirect.clone())
    }
}

pub async fn run_mcp_oauth_flow(
    server_name: &str,
    server_url: &str,
    configured_client_id: Option<&str>,
    callback_port: Option<u16>,
) -> Result<()> {
    let metadata = discover_oauth_metadata(server_url).await?;

    let bind_addr = format!("127.0.0.1:{}", callback_port.unwrap_or(0));
    let listener = TcpListener::bind(&bind_addr)?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let client_id = if let Some(id) = configured_client_id {
        id.to_string()
    } else if let Some(cached) = load_registered_client_id(server_name) {
        cached
    } else if let Some(reg_endpoint) = &metadata.registration_endpoint {
        match register_client(reg_endpoint, &redirect_uri).await {
            Ok(id) => {
                let _ = save_registered_client_id(server_name, &id);
                id
            }
            Err(e) => {
                warn!("Dynamic client registration failed: {e}. Falling back to manual entry.");
                Text::new("Enter the OAuth client ID for this MCP server:")
                    .prompt()
                    .context("Failed to read client ID")?
            }
        }
    } else {
        Text::new("Enter the OAuth client ID for this MCP server:")
            .prompt()
            .context("Failed to read client ID")?
    };

    let provider = McpOAuthProvider {
        client_id,
        authorize_url: metadata.authorization_endpoint,
        token_url: metadata.token_endpoint,
        scopes: metadata.scopes_supported.join(" "),
        fixed_redirect: redirect_uri,
    };

    run_oauth_flow(&provider, &mcp_token_key(server_name)).await
}

pub fn load_valid_mcp_token(server_name: &str) -> Option<String> {
    let tokens = load_oauth_tokens(&mcp_token_key(server_name))?;
    if Utc::now().timestamp() < tokens.expires_at {
        Some(tokens.access_token)
    } else {
        None
    }
}

fn mcp_token_key(server_name: &str) -> String {
    format!("mcp_{server_name}")
}

fn load_registered_client_id(server_name: &str) -> Option<String> {
    let path = paths::oauth_tokens_path().join(format!("mcp_{server_name}_registration.json"));
    let content = fs::read_to_string(path).ok()?;
    let reg: McpRegistration = serde_json::from_str(&content).ok()?;

    Some(reg.client_id)
}

fn save_registered_client_id(server_name: &str, client_id: &str) -> Result<()> {
    let dir = paths::oauth_tokens_path();
    fs::create_dir_all(&dir)?;

    let path = dir.join(format!("mcp_{server_name}_registration.json"));
    let reg = McpRegistration {
        client_id: client_id.to_string(),
    };

    fs::write(path, serde_json::to_string_pretty(&reg)?)?;

    Ok(())
}

async fn register_client(endpoint: &str, redirect_uri: &str) -> Result<String> {
    let body = serde_json::json!({
        "client_name": "Coyote",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none"
    });

    let response: serde_json::Value = Client::new()
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .context("Failed to reach registration endpoint")?
        .json()
        .await
        .context("Failed to parse registration response")?;

    response["client_id"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing client_id in registration response: {response}"))
        .map(|s| s.to_string())
}

async fn discover_oauth_metadata(server_url: &str) -> Result<OAuthServerMetadata> {
    let base = extract_base_url(server_url)?;
    let client = Client::new();

    // RFC 9728: try protected resource metadata first; it points to the auth server
    let pr_url = format!("{base}/.well-known/oauth-protected-resource");
    if let Ok(resp) = client.get(&pr_url).send().await
        && resp.status().is_success()
        && let Ok(pr) = resp.json::<ProtectedResourceMetadata>().await
        && let Some(auth_server) = pr.authorization_servers.first()
    {
        let as_url = format!("{auth_server}/.well-known/oauth-authorization-server");
        if let Ok(resp) = client.get(&as_url).send().await
            && resp.status().is_success()
            && let Ok(meta) = resp.json::<OAuthServerMetadata>().await
        {
            return Ok(meta);
        }
    }

    let as_url = format!("{base}/.well-known/oauth-authorization-server");
    let resp = client
        .get(&as_url)
        .send()
        .await
        .with_context(|| format!("Failed to reach {as_url}"))?;

    if resp.status().is_success() {
        return resp
            .json::<OAuthServerMetadata>()
            .await
            .with_context(|| format!("Failed to parse OAuth metadata from {as_url}"));
    }

    Err(anyhow!(
        "Could not discover OAuth metadata for '{server_url}'.\n\
         Tried:\n  {pr_url}\n  {as_url}\n\
         Ensure the server supports MCP OAuth discovery, or consult its documentation."
    ))
}

fn extract_base_url(url: &str) -> Result<String> {
    let parsed = Url::parse(url).with_context(|| format!("Invalid URL: {url}"))?;
    let scheme = parsed.scheme();
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("No host in URL: {url}"))?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();

    Ok(format!("{scheme}://{host}{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::get_env_name;
    use serial_test::serial;
    use std::{
        env, fs,
        time::{self, SystemTime},
    };

    fn with_temp_cache<F: FnOnce()>(f: F) {
        let unique = SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-mcp-oauth-test-{unique}"));
        fs::create_dir_all(&root).unwrap();
        let env_key = get_env_name("cache_dir");
        let prev = env::var_os(&env_key);
        unsafe {
            env::set_var(&env_key, &root);
        }
        f();
        unsafe {
            match prev {
                Some(v) => env::set_var(&env_key, v),
                None => env::remove_var(&env_key),
            }
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_base_url_strips_path_and_query() {
        let result = extract_base_url("https://mcp.notion.com/mcp?foo=bar").unwrap();

        assert_eq!(result, "https://mcp.notion.com");
    }

    #[test]
    fn extract_base_url_preserves_explicit_port() {
        let result = extract_base_url("http://localhost:8080/mcp").unwrap();

        assert_eq!(result, "http://localhost:8080");
    }

    #[test]
    fn extract_base_url_standard_port_omitted() {
        let result = extract_base_url("https://example.com/mcp/v1").unwrap();

        assert_eq!(result, "https://example.com");
    }

    #[test]
    fn extract_base_url_rejects_invalid_url() {
        assert!(extract_base_url("not-a-url").is_err());
    }

    #[test]
    #[serial]
    fn registered_client_id_roundtrip() {
        with_temp_cache(|| {
            save_registered_client_id("notion", "client-xyz-123").unwrap();

            let loaded = load_registered_client_id("notion");

            assert_eq!(loaded, Some("client-xyz-123".to_string()));
        });
    }

    #[test]
    #[serial]
    fn load_registered_client_id_returns_none_for_missing() {
        with_temp_cache(|| {
            let loaded = load_registered_client_id("no-such-server");

            assert!(loaded.is_none());
        });
    }

    #[test]
    #[serial]
    fn registered_client_id_second_save_overwrites_first() {
        with_temp_cache(|| {
            save_registered_client_id("github", "first-id").unwrap();
            save_registered_client_id("github", "second-id").unwrap();

            let loaded = load_registered_client_id("github");

            assert_eq!(loaded, Some("second-id".to_string()));
        });
    }
}
