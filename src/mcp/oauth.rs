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
    #[serde(default)]
    scopes_supported: Vec<String>,
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

    fn scopes(&self) -> String {
        self.scopes.clone()
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
    redirect_host: Option<&str>,
) -> Result<()> {
    let metadata = discover_oauth_metadata(server_url).await?;

    let host = redirect_host.unwrap_or("127.0.0.1");
    let bind_addr = format!("127.0.0.1:{}", callback_port.unwrap_or(0));
    let listener = TcpListener::bind(&bind_addr)?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let redirect_uri = format!("http://{host}:{port}/callback");

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
    let client = Client::new();
    let mut tried: Vec<String> = Vec::new();

    // RFC 9728 @ 5.1: an unauthenticated request should yield a 401 whose
    // WWW-Authenticate challenge advertises the protected resource metadata URL.
    let mut pr_urls = Vec::new();
    if let Some(url) = probe_resource_metadata_url(&client, server_url).await {
        pr_urls.push(url);
    }

    // RFC 9728 @ 3.1: path-aware well-known URL, then root as legacy fallback.
    pr_urls.extend(well_known_urls(server_url, "oauth-protected-resource")?);
    pr_urls.dedup();

    for pr_url in &pr_urls {
        tried.push(pr_url.clone());
        let Ok(resp) = client.get(pr_url).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(pr) = resp.json::<ProtectedResourceMetadata>().await else {
            continue;
        };
        let Some(issuer) = pr.authorization_servers.first() else {
            continue;
        };
        // RFC 8414 @ 3.1: for issuers with a path component the well-known
        // segment is inserted BEFORE the path (with the legacy appended form
        // and root as fallbacks).
        for as_url in well_known_urls(issuer, "oauth-authorization-server")? {
            tried.push(as_url.clone());
            if let Ok(resp) = client.get(&as_url).send().await
                && resp.status().is_success()
                && let Ok(mut meta) = resp.json::<OAuthServerMetadata>().await
            {
                // Some auth servers (e.g. GitHub) omit scopes_supported from
                // their metadata; fall back to the resource's advertised scopes.
                if meta.scopes_supported.is_empty() {
                    meta.scopes_supported = pr.scopes_supported.clone();
                }
                return Ok(meta);
            }
        }
    }

    // Last resort: the MCP server itself may host authorization server metadata.
    for as_url in well_known_urls(server_url, "oauth-authorization-server")? {
        tried.push(as_url.clone());
        if let Ok(resp) = client.get(&as_url).send().await
            && resp.status().is_success()
        {
            return resp
                .json::<OAuthServerMetadata>()
                .await
                .with_context(|| format!("Failed to parse OAuth metadata from {as_url}"));
        }
    }

    Err(anyhow!(
        "Could not discover OAuth metadata for '{server_url}'.\n\
         Tried:\n  {}\n\
         Ensure the server supports MCP OAuth discovery, or consult its documentation.",
        tried.join("\n  ")
    ))
}

/// Probes the MCP server with an unauthenticated request and extracts the
/// `resource_metadata` URL from the 401 `WWW-Authenticate` challenge (RFC 9728 @ 5.1).
async fn probe_resource_metadata_url(client: &Client, server_url: &str) -> Option<String> {
    let resp = client.get(server_url).send().await.ok()?;
    let header = resp.headers().get(reqwest::header::WWW_AUTHENTICATE)?;

    parse_resource_metadata(header.to_str().ok()?)
}

/// Extracts the `resource_metadata` parameter value from a `WWW-Authenticate`
/// challenge, e.g. `Bearer error="...", resource_metadata="https://..."`.
fn parse_resource_metadata(challenge: &str) -> Option<String> {
    let (_, rest) = challenge.split_once("resource_metadata=")?;
    let rest = rest.trim_start();
    let value = if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next()?
    } else {
        rest.split([',', ' ']).next()?
    };

    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Builds candidate well-known metadata URLs for `url`, ordered by spec preference:
/// 1. Path-aware (RFC 8414 @ 3.1 / RFC 9728 @ 3.1): `{origin}/.well-known/{suffix}{path}`
/// 2. Legacy appended form: `{url}/.well-known/{suffix}`
/// 3. Root: `{origin}/.well-known/{suffix}`
///
/// URLs without a path component yield only the root form.
fn well_known_urls(url: &str, suffix: &str) -> Result<Vec<String>> {
    let parsed = Url::parse(url).with_context(|| format!("Invalid URL: {url}"))?;
    let origin = extract_base_url(url)?;
    let path = parsed.path().trim_end_matches('/');

    let mut urls = Vec::new();
    if !path.is_empty() && path != "/" {
        urls.push(format!("{origin}/.well-known/{suffix}{path}"));
        urls.push(format!("{origin}{path}/.well-known/{suffix}"));
    }
    urls.push(format!("{origin}/.well-known/{suffix}"));

    Ok(urls)
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
    fn well_known_urls_path_aware_first_for_url_with_path() {
        let urls = well_known_urls(
            "https://api.githubcopilot.com/mcp",
            "oauth-protected-resource",
        )
        .unwrap();

        assert_eq!(
            urls,
            vec![
                "https://api.githubcopilot.com/.well-known/oauth-protected-resource/mcp",
                "https://api.githubcopilot.com/mcp/.well-known/oauth-protected-resource",
                "https://api.githubcopilot.com/.well-known/oauth-protected-resource",
            ]
        );
    }

    #[test]
    fn well_known_urls_inserts_before_issuer_path() {
        let urls = well_known_urls(
            "https://github.com/login/oauth",
            "oauth-authorization-server",
        )
        .unwrap();

        assert_eq!(
            urls[0],
            "https://github.com/.well-known/oauth-authorization-server/login/oauth"
        );
    }

    #[test]
    fn well_known_urls_root_only_for_url_without_path() {
        let urls = well_known_urls("https://mcp.notion.com", "oauth-authorization-server").unwrap();

        assert_eq!(
            urls,
            vec!["https://mcp.notion.com/.well-known/oauth-authorization-server"]
        );
    }

    #[test]
    fn well_known_urls_ignores_trailing_slash() {
        let urls = well_known_urls(
            "https://api.githubcopilot.com/mcp/",
            "oauth-protected-resource",
        )
        .unwrap();

        assert_eq!(
            urls[0],
            "https://api.githubcopilot.com/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn parse_resource_metadata_extracts_quoted_url() {
        let challenge = r#"Bearer error="invalid_request", error_description="No access token was provided in this request", resource_metadata="https://api.githubcopilot.com/.well-known/oauth-protected-resource/mcp""#;

        let url = parse_resource_metadata(challenge);

        assert_eq!(
            url,
            Some(
                "https://api.githubcopilot.com/.well-known/oauth-protected-resource/mcp"
                    .to_string()
            )
        );
    }

    #[test]
    fn parse_resource_metadata_extracts_unquoted_url() {
        let challenge = "Bearer resource_metadata=https://example.com/.well-known/oauth-protected-resource/mcp, error=\"invalid_token\"";

        let url = parse_resource_metadata(challenge);

        assert_eq!(
            url,
            Some("https://example.com/.well-known/oauth-protected-resource/mcp".to_string())
        );
    }

    #[test]
    fn parse_resource_metadata_returns_none_when_absent() {
        assert_eq!(
            parse_resource_metadata(r#"Bearer error="invalid_token""#),
            None
        );
        assert_eq!(
            parse_resource_metadata(r#"Bearer resource_metadata="""#),
            None
        );
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
