use crate::client::oauth::{
    OAuthProvider, OAuthTokens, TokenRequestFormat, load_oauth_tokens, refresh_oauth_token,
    run_oauth_flow, token_response_keys,
};
use crate::config::paths;
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use inquire::Text;
use log::{debug, warn};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::net::TcpListener;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync;
use url::Url;

const REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH_FAILURE_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    resource: Option<String>,
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
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    token_url: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

struct DiscoveredOAuth {
    metadata: OAuthServerMetadata,
    resource: Option<String>,
}

struct McpOAuthProvider {
    client_id: String,
    authorize_url: String,
    token_url: String,
    scopes: String,
    fixed_redirect: String,
    resource: String,
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

    fn extra_authorize_params(&self) -> Vec<(&str, &str)> {
        vec![("resource", self.resource.as_str())]
    }

    fn extra_token_params(&self) -> Vec<(&str, &str)> {
        vec![("resource", self.resource.as_str())]
    }
}

pub async fn run_mcp_oauth_flow(
    server_name: &str,
    server_url: &str,
    configured_client_id: Option<&str>,
    callback_port: Option<u16>,
    redirect_host: Option<&str>,
) -> Result<()> {
    let discovered = discover_oauth_metadata(server_url).await?;
    let metadata = discovered.metadata;
    let resource = resolve_resource(discovered.resource, server_url)?;

    let host = redirect_host.unwrap_or("127.0.0.1");

    // Reuse a cached dynamic registration together with the exact redirect
    // URI it was registered with (AWS et al. match redirect URIs exactly).
    // Only when no client_id is configured explicitly.
    let cached_reuse: Option<(String, String)> = if configured_client_id.is_none() {
        load_registration(server_name).and_then(|reg| {
            let redirect = reg.redirect_uri?;
            let port = cached_redirect_port(&redirect, host, callback_port)?;
            // The registered port must still be free for our callback listener.
            TcpListener::bind(format!("127.0.0.1:{port}")).ok()?;
            Some((reg.client_id, redirect))
        })
    } else {
        None
    };

    let (client_id, redirect_uri) = if let Some((client_id, redirect_uri)) = cached_reuse {
        // Re-save so registrations cached before token_url/resource were
        // persisted gain them, enabling token refresh next time.
        if let Err(e) = save_registration(
            server_name,
            &client_id,
            &redirect_uri,
            &metadata.token_endpoint,
            &resource,
        ) {
            debug!("Failed to update cached MCP registration for '{server_name}': {e}");
        }
        (client_id, redirect_uri)
    } else {
        let bind_addr = format!("127.0.0.1:{}", callback_port.unwrap_or(0));
        let listener = TcpListener::bind(&bind_addr)?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let redirect_uri = format!("http://{host}:{port}/callback");

        let client_id = if let Some(id) = configured_client_id {
            id.to_string()
        } else if let Some(reg_endpoint) = &metadata.registration_endpoint {
            match register_client(reg_endpoint, &redirect_uri).await {
                Ok(id) => id,
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
        // Persist regardless of how the client_id was obtained (DCR, config,
        // or manual entry) so refresh_mcp_token can run the refresh_token
        // grant later without interactive re-auth.
        if let Err(e) = save_registration(
            server_name,
            &client_id,
            &redirect_uri,
            &metadata.token_endpoint,
            &resource,
        ) {
            debug!("Failed to cache MCP registration for '{server_name}': {e}");
        }
        (client_id, redirect_uri)
    };

    let provider = McpOAuthProvider {
        client_id,
        authorize_url: metadata.authorization_endpoint,
        token_url: metadata.token_endpoint,
        scopes: metadata.scopes_supported.join(" "),
        fixed_redirect: redirect_uri,
        resource,
    };

    run_oauth_flow(&provider, &mcp_token_key(server_name)).await
}

pub async fn load_or_refresh_mcp_token(server_name: &str) -> Option<String> {
    let key = mcp_token_key(server_name);
    let tokens = load_oauth_tokens(&key)?;
    if Utc::now().timestamp() < tokens.expires_at {
        return Some(tokens.access_token);
    }

    if in_refresh_failure_backoff(server_name) {
        debug!("Skipping token refresh for MCP server '{server_name}': recent attempt failed");
        return None;
    }

    let lock = refresh_lock(server_name);
    let _guard = lock.lock().await;

    // A concurrent caller may have refreshed while we waited for the lock.
    let tokens = load_oauth_tokens(&key)?;
    if Utc::now().timestamp() < tokens.expires_at {
        return Some(tokens.access_token);
    }

    if in_refresh_failure_backoff(server_name) {
        debug!("Skipping token refresh for MCP server '{server_name}': recent attempt failed");
        return None;
    }

    match refresh_mcp_token(server_name, &key, &tokens).await {
        Ok(access_token) => Some(access_token),
        Err(e) => {
            note_refresh_failure(server_name);
            warn!(
                "Failed to refresh OAuth token for MCP server '{server_name}'. \
                 Run `.mcp auth {server_name}` to re-authenticate."
            );
            debug!(
                "Token refresh error for MCP server '{server_name}': {}",
                redact_refresh_error(&e)
            );
            None
        }
    }
}

async fn refresh_mcp_token(server_name: &str, key: &str, tokens: &OAuthTokens) -> Result<String> {
    if tokens.refresh_token.is_none() {
        return Err(anyhow!("no refresh token stored"));
    }

    let reg =
        load_registration(server_name).ok_or_else(|| anyhow!("no cached client registration"))?;
    let token_url = reg.token_url.ok_or_else(|| {
        anyhow!("cached registration has no token URL (saved by an older version)")
    })?;
    let resource = reg.resource.ok_or_else(|| {
        anyhow!("cached registration has no resource (saved by an older version)")
    })?;

    let provider = McpOAuthProvider {
        client_id: reg.client_id,
        authorize_url: String::new(),
        token_url,
        scopes: String::new(),
        fixed_redirect: String::new(),
        resource,
    };

    let client = Client::builder().timeout(REFRESH_HTTP_TIMEOUT).build()?;
    let refreshed = refresh_oauth_token(&client, &provider, key, tokens).await?;
    Ok(refreshed.access_token)
}

fn refresh_lock(server_name: &str) -> Arc<sync::Mutex<()>> {
    static LOCKS: OnceLock<parking_lot::Mutex<HashMap<String, Arc<sync::Mutex<()>>>>> =
        OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .entry(server_name.to_string())
        .or_default()
        .clone()
}

fn refresh_failures() -> &'static parking_lot::Mutex<HashMap<String, Instant>> {
    static FAILURES: OnceLock<parking_lot::Mutex<HashMap<String, Instant>>> = OnceLock::new();
    FAILURES.get_or_init(Default::default)
}

fn note_refresh_failure(server_name: &str) {
    refresh_failures()
        .lock()
        .insert(server_name.to_string(), Instant::now());
}

fn in_refresh_failure_backoff(server_name: &str) -> bool {
    refresh_failures()
        .lock()
        .get(server_name)
        .is_some_and(|failed_at| failed_at.elapsed() < REFRESH_FAILURE_BACKOFF)
}

/// Refresh errors may embed the token endpoint's JSON response, which can
/// contain live tokens; strip everything from the first `{` before logging.
fn redact_refresh_error(e: &anyhow::Error) -> String {
    let msg = e.to_string();
    match msg.find('{') {
        Some(idx) => format!("{}<response body redacted>", &msg[..idx]),
        None => msg,
    }
}

fn mcp_token_key(server_name: &str) -> String {
    format!("mcp_{server_name}")
}

fn load_registration(server_name: &str) -> Option<McpRegistration> {
    let path = paths::oauth_tokens_dir().join(format!("mcp_{server_name}_registration.json"));
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_registration(
    server_name: &str,
    client_id: &str,
    redirect_uri: &str,
    token_url: &str,
    resource: &str,
) -> Result<()> {
    let dir = paths::oauth_tokens_dir();
    fs::create_dir_all(&dir)?;

    let path = dir.join(format!("mcp_{server_name}_registration.json"));
    let reg = McpRegistration {
        client_id: client_id.to_string(),
        redirect_uri: Some(redirect_uri.to_string()),
        token_url: Some(token_url.to_string()),
        resource: Some(resource.to_string()),
    };

    fs::write(path, serde_json::to_string_pretty(&reg)?)?;

    Ok(())
}

/// Returns the port of a cached registered redirect URI if it is still
/// compatible with the current configuration: same redirect host, and, when
/// a callback port is pinned in config, the same port. Servers like AWS
/// match redirect URIs exactly, so a cached registration is only reusable
/// with the identical redirect URI it was registered with.
fn cached_redirect_port(
    cached_redirect: &str,
    host: &str,
    pinned_port: Option<u16>,
) -> Option<u16> {
    let url = Url::parse(cached_redirect).ok()?;
    if url.host_str() != Some(host) {
        return None;
    }
    let port = url.port()?;
    if pinned_port.is_some_and(|p| p != port) {
        return None;
    }
    Some(port)
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
        .ok_or_else(|| {
            anyhow!(
                "Missing client_id in registration response (keys: {})",
                token_response_keys(&response)
            )
        })
        .map(|s| s.to_string())
}

/// Derives the canonical resource URI for an MCP server per RFC 8707 @ 2 and
/// the MCP spec: the configured server URL with query and fragment stripped.
fn canonical_resource(server_url: &str) -> Result<String> {
    let mut url =
        Url::parse(server_url).with_context(|| format!("Invalid MCP server URL: {server_url}"))?;
    url.set_query(None);
    url.set_fragment(None);

    let s = url.to_string();
    Ok(match url.path() {
        "/" => s.trim_end_matches('/').to_string(),
        _ => s,
    })
}

/// Resolves the RFC 8707 resource indicator: prefers the value advertised in
/// the protected resource metadata, but only after validating it identifies
/// the server we are connecting to (RFC 9728 @ 3.3); same scheme/host/port
/// as the configured server URL. Falls back to the canonical server URL on
/// mismatch, empty value, or absence.
fn resolve_resource(advertised: Option<String>, server_url: &str) -> Result<String> {
    let canonical = canonical_resource(server_url)?;
    let Some(advertised) = advertised.filter(|r| !r.is_empty()) else {
        return Ok(canonical);
    };
    match (Url::parse(&advertised), Url::parse(server_url)) {
        (Ok(a), Ok(s)) if a.origin() == s.origin() => Ok(advertised),
        _ => {
            warn!(
                "Ignoring protected resource metadata resource '{advertised}': \
                 it does not match the MCP server origin. Using '{canonical}' instead."
            );
            Ok(canonical)
        }
    }
}

async fn discover_oauth_metadata(server_url: &str) -> Result<DiscoveredOAuth> {
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
                return Ok(DiscoveredOAuth {
                    metadata: meta,
                    resource: pr.resource.clone(),
                });
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
                .with_context(|| format!("Failed to parse OAuth metadata from {as_url}"))
                .map(|metadata| DiscoveredOAuth {
                    metadata,
                    resource: None,
                });
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
        env,
        ffi::OsString,
        fs,
        path::PathBuf,
        time::{self, SystemTime},
    };

    fn with_temp_cache<F: FnOnce()>(f: F) {
        struct Restore {
            key: String,
            prev: Option<OsString>,
            root: PathBuf,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match self.prev.take() {
                        Some(v) => env::set_var(&self.key, v),
                        None => env::remove_var(&self.key),
                    }
                }
                let _ = fs::remove_dir_all(&self.root);
            }
        }

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
        let _restore = Restore {
            key: env_key,
            prev,
            root,
        };
        f();
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
    fn canonical_resource_strips_query() {
        let result = canonical_resource("https://aws-mcp.us-east-1.api.aws/mcp?oauth=initialize");

        assert_eq!(result.unwrap(), "https://aws-mcp.us-east-1.api.aws/mcp");
    }

    #[test]
    fn canonical_resource_strips_fragment() {
        let result = canonical_resource("https://example.com/mcp#section");

        assert_eq!(result.unwrap(), "https://example.com/mcp");
    }

    #[test]
    fn canonical_resource_preserves_path_and_port() {
        let result = canonical_resource("http://localhost:8080/mcp/v1?x=1");

        assert_eq!(result.unwrap(), "http://localhost:8080/mcp/v1");
    }

    #[test]
    fn canonical_resource_rejects_invalid_url() {
        assert!(canonical_resource("not-a-url").is_err());
    }

    #[test]
    fn canonical_resource_bare_host_has_no_trailing_slash() {
        let result = canonical_resource("https://mcp.example.com");

        assert_eq!(result.unwrap(), "https://mcp.example.com");
    }

    #[test]
    fn resolve_resource_prefers_matching_advertised() {
        let result = resolve_resource(
            Some("https://aws-mcp.us-east-1.api.aws/mcp".into()),
            "https://aws-mcp.us-east-1.api.aws/mcp?oauth=initialize",
        );

        assert_eq!(result.unwrap(), "https://aws-mcp.us-east-1.api.aws/mcp");
    }

    #[test]
    fn resolve_resource_rejects_cross_origin_advertised() {
        let result = resolve_resource(
            Some("https://evil.example.com/mcp".into()),
            "https://aws-mcp.us-east-1.api.aws/mcp",
        );

        assert_eq!(result.unwrap(), "https://aws-mcp.us-east-1.api.aws/mcp");
    }

    #[test]
    fn resolve_resource_empty_falls_back_to_canonical() {
        let result = resolve_resource(Some(String::new()), "https://example.com/mcp");

        assert_eq!(result.unwrap(), "https://example.com/mcp");
    }

    #[test]
    fn resolve_resource_none_falls_back_to_canonical() {
        let result = resolve_resource(None, "https://example.com/mcp");

        assert_eq!(result.unwrap(), "https://example.com/mcp");
    }

    #[test]
    fn protected_resource_metadata_deserializes_resource_field() {
        let json = r#"{"resource":"https://aws-mcp.us-east-1.api.aws/mcp","authorization_servers":["https://us-east-1.oauth.signin.aws/"]}"#;

        let pr: ProtectedResourceMetadata = serde_json::from_str(json).unwrap();

        assert_eq!(
            pr.resource.as_deref(),
            Some("https://aws-mcp.us-east-1.api.aws/mcp")
        );
        assert_eq!(
            pr.authorization_servers,
            vec!["https://us-east-1.oauth.signin.aws/"]
        );
    }

    #[test]
    fn mcp_provider_sends_resource_in_authorize_and_token_params() {
        let provider = McpOAuthProvider {
            client_id: "client-123".into(),
            authorize_url: "https://as.example/authorize".into(),
            token_url: "https://as.example/token".into(),
            scopes: String::new(),
            fixed_redirect: "http://127.0.0.1:9000/callback".into(),
            resource: "https://aws-mcp.us-east-1.api.aws/mcp".into(),
        };

        assert_eq!(
            provider.extra_authorize_params(),
            vec![("resource", "https://aws-mcp.us-east-1.api.aws/mcp")]
        );
        assert_eq!(
            provider.extra_token_params(),
            vec![("resource", "https://aws-mcp.us-east-1.api.aws/mcp")]
        );
    }

    #[test]
    #[serial]
    fn registered_client_id_roundtrip() {
        with_temp_cache(|| {
            save_registration(
                "notion",
                "client-xyz-123",
                "http://127.0.0.1:49152/callback",
                "https://as.example/token",
                "https://mcp.example/mcp",
            )
            .unwrap();

            let loaded = load_registration("notion").unwrap();

            assert_eq!(loaded.client_id, "client-xyz-123");
            assert_eq!(
                loaded.token_url.as_deref(),
                Some("https://as.example/token")
            );
            assert_eq!(loaded.resource.as_deref(), Some("https://mcp.example/mcp"));
        });
    }

    #[test]
    #[serial]
    fn load_registration_returns_none_for_missing() {
        with_temp_cache(|| {
            let loaded = load_registration("no-such-server");

            assert!(loaded.is_none());
        });
    }

    #[test]
    #[serial]
    fn registration_second_save_overwrites_first() {
        with_temp_cache(|| {
            save_registration(
                "github",
                "first-id",
                "http://127.0.0.1:49152/callback",
                "https://as.example/token",
                "https://mcp.example/mcp",
            )
            .unwrap();
            save_registration(
                "github",
                "second-id",
                "http://127.0.0.1:49153/callback",
                "https://as.example/token",
                "https://mcp.example/mcp",
            )
            .unwrap();

            let loaded = load_registration("github").unwrap();

            assert_eq!(loaded.client_id, "second-id");
            assert_eq!(
                loaded.redirect_uri.as_deref(),
                Some("http://127.0.0.1:49153/callback")
            );
        });
    }

    #[test]
    #[serial]
    fn old_format_registration_still_loads() {
        with_temp_cache(|| {
            let dir = paths::oauth_tokens_dir();
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("mcp_legacy_registration.json"),
                r#"{"client_id":"legacy-id"}"#,
            )
            .unwrap();

            let loaded = load_registration("legacy").unwrap();

            assert_eq!(loaded.client_id, "legacy-id");
            assert_eq!(loaded.redirect_uri, None);
            assert_eq!(loaded.token_url, None);
            assert_eq!(loaded.resource, None);
        });
    }

    #[test]
    #[serial]
    fn save_registration_persists_redirect_uri() {
        with_temp_cache(|| {
            save_registration(
                "aws",
                "client-abc",
                "http://127.0.0.1:49152/callback",
                "https://as.example/token",
                "https://mcp.example/mcp",
            )
            .unwrap();

            let loaded = load_registration("aws").unwrap();

            assert_eq!(loaded.client_id, "client-abc");
            assert_eq!(
                loaded.redirect_uri.as_deref(),
                Some("http://127.0.0.1:49152/callback")
            );
        });
    }

    #[test]
    fn mcp_registration_deserializes_without_new_fields_and_roundtrips() {
        let old: McpRegistration = serde_json::from_str(r#"{"client_id":"legacy-id"}"#).unwrap();

        assert_eq!(old.client_id, "legacy-id");
        assert_eq!(old.token_url, None);
        assert_eq!(old.resource, None);

        let full = McpRegistration {
            client_id: "client-abc".into(),
            redirect_uri: Some("http://127.0.0.1:49152/callback".into()),
            token_url: Some("https://as.example/token".into()),
            resource: Some("https://mcp.example/mcp".into()),
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: McpRegistration = serde_json::from_str(&json).unwrap();

        assert_eq!(back.token_url.as_deref(), Some("https://as.example/token"));
        assert_eq!(back.resource.as_deref(), Some("https://mcp.example/mcp"));
    }

    #[test]
    #[serial]
    fn expired_token_with_old_format_registration_returns_none() {
        with_temp_cache(|| {
            let dir = paths::oauth_tokens_dir();
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                paths::token_file("mcp_legacyref"),
                r#"{"access_token":"stale","refresh_token":"refresh-abc","expires_at":0}"#,
            )
            .unwrap();
            fs::write(
                dir.join("mcp_legacyref_registration.json"),
                r#"{"client_id":"legacy-id"}"#,
            )
            .unwrap();

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let token = rt.block_on(load_or_refresh_mcp_token("legacyref"));

            assert_eq!(token, None);
        });
    }

    #[test]
    fn refresh_failure_backoff_memoizes_per_server() {
        assert!(!in_refresh_failure_backoff("backoff-test-server"));

        note_refresh_failure("backoff-test-server");

        assert!(in_refresh_failure_backoff("backoff-test-server"));
        assert!(!in_refresh_failure_backoff("backoff-other-server"));
    }

    #[test]
    fn redact_refresh_error_strips_response_body() {
        let with_body = anyhow!(
            "Missing access_token in refresh response: {}",
            r#"{"access_token":"live-secret"}"#
        );
        let without_body = anyhow!("no refresh token stored");

        assert_eq!(
            redact_refresh_error(&with_body),
            "Missing access_token in refresh response: <response body redacted>"
        );
        assert_eq!(
            redact_refresh_error(&without_body),
            "no refresh token stored"
        );
    }

    #[test]
    fn cached_redirect_port_matches() {
        let port = cached_redirect_port("http://127.0.0.1:49152/callback", "127.0.0.1", None);

        assert_eq!(port, Some(49152));
    }

    #[test]
    fn cached_redirect_port_rejects_host_mismatch() {
        let port = cached_redirect_port("http://127.0.0.1:49152/callback", "localhost", None);

        assert_eq!(port, None);
    }

    #[test]
    fn cached_redirect_port_respects_pinned_port() {
        assert_eq!(
            cached_redirect_port("http://127.0.0.1:49152/callback", "127.0.0.1", Some(50000)),
            None
        );
        assert_eq!(
            cached_redirect_port("http://127.0.0.1:49152/callback", "127.0.0.1", Some(49152)),
            Some(49152)
        );
    }
}
