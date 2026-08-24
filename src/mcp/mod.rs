mod auth_client;
pub(crate) mod manage;
pub(crate) mod oauth;
pub(crate) mod render;
mod sse_transport;

use crate::config::AppConfig;
use crate::config::paths;
use crate::utils::{AbortSignal, abortable_run_with_spinner};
use crate::vault::Vault;
use crate::vault::interpolate_secrets;
use anyhow::Error;
use anyhow::{Context, Result, anyhow};
use auth_client::McpOAuthClient;
use futures_util::{StreamExt, TryStreamExt, stream};
use http::{HeaderName, HeaderValue};
use indexmap::IndexMap;
use indoc::formatdoc;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde::{Deserialize, Serialize};
use sse_transport::LegacySseTransport;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fmt::Display;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;

pub const MCP_INVOKE_META_FUNCTION_NAME_PREFIX: &str = "mcp_invoke";
pub const MCP_SEARCH_META_FUNCTION_NAME_PREFIX: &str = "mcp_search";
pub const MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX: &str = "mcp_describe";
pub const MCP_READ_META_FUNCTION_NAME_PREFIX: &str = "mcp_read";
pub const MCP_PROMPT_META_FUNCTION_NAME_PREFIX: &str = "mcp_prompt";

pub const MCP_META_FUNCTION_PREFIXES: [&str; 5] = [
    MCP_INVOKE_META_FUNCTION_NAME_PREFIX,
    MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
    MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX,
    MCP_READ_META_FUNCTION_NAME_PREFIX,
    MCP_PROMPT_META_FUNCTION_NAME_PREFIX,
];

pub fn is_mcp_meta_function(name: &str) -> bool {
    MCP_META_FUNCTION_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

pub fn mcp_meta_function_names(server: &str) -> Vec<String> {
    MCP_META_FUNCTION_PREFIXES
        .iter()
        .map(|prefix| format!("{prefix}_{server}"))
        .collect()
}

pub type ConnectedServer = RunningService<RoleClient, ()>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogItemKind {
    #[default]
    Tool,
    Resource,
    ResourceTemplate,
    Prompt,
}

impl CatalogItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Resource => "resource",
            Self::ResourceTemplate => "resource_template",
            Self::Prompt => "prompt",
        }
    }
}

impl Display for CatalogItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CatalogItem {
    pub kind: CatalogItemKind,
    pub name: String,
    pub server: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct McpServersConfig {
    #[serde(rename = "mcpServers")]
    pub mcp_servers: IndexMap<String, McpServer>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct McpOAuthConfig {
    #[serde(rename = "clientId", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(rename = "clientSecret", skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(rename = "callbackPort", skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    #[serde(rename = "redirectHost", skip_serializing_if = "Option::is_none")]
    pub redirect_host: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpServer {
    #[serde(rename = "type")]
    pub transport_type: McpTransportType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<IndexMap<String, JsonField>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthConfig>,
}

impl McpServer {
    pub fn is_remote(&self) -> bool {
        matches!(
            self.transport_type,
            McpTransportType::Http | McpTransportType::Sse
        )
    }

    pub fn validate(&self, name: &str) -> Result<()> {
        if self.is_remote() {
            let type_label = match self.transport_type {
                McpTransportType::Http => "http",
                McpTransportType::Sse => "sse",
                _ => unreachable!(),
            };
            if self.url.is_none() {
                return Err(anyhow!(
                    "MCP server '{name}' has type \"{type_label}\" but is missing a \"url\" field"
                ));
            }
            if self.command.is_some() || self.args.is_some() || self.cwd.is_some() {
                return Err(anyhow!(
                    "MCP server '{name}' has type \"{type_label}\" but also specifies stdio fields \
                     (command/args/cwd). Remove the stdio fields or change the type to \"stdio\"."
                ));
            }
        } else {
            if self.command.is_none() {
                return Err(anyhow!(
                    "MCP server '{name}' is missing a \"command\" field (required for stdio transport)"
                ));
            }
            if self.url.is_some() || self.headers.is_some() || self.oauth.is_some() {
                return Err(anyhow!(
                    "MCP server '{name}' has type \"stdio\" but also specifies remote fields \
                     (url/headers/oauth). Remove the remote fields or change the type to \"http\" or \"sse\"."
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub(crate) enum McpTransportType {
    Stdio,
    Http,
    Sse,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum JsonField {
    Str(String),
    Bool(bool),
    Int(i64),
}

#[derive(Debug, Clone, Default)]
pub struct McpRegistry {
    log_path: Option<PathBuf>,
    config: Option<McpServersConfig>,
    servers: HashMap<String, Arc<ConnectedServer>>,
}

impl McpRegistry {
    pub async fn init(
        log_path: Option<PathBuf>,
        start_mcp_servers: bool,
        enabled_mcp_servers: Option<Vec<String>>,
        abort_signal: AbortSignal,
        app_config: &AppConfig,
        vault: &Vault,
    ) -> Result<Self> {
        let mut registry = Self {
            log_path,
            ..Default::default()
        };
        if !paths::mcp_config_file().try_exists().with_context(|| {
            format!(
                "Failed to check MCP config file at {}",
                paths::mcp_config_file().display()
            )
        })? {
            debug!(
                "MCP config file does not exist at {}, skipping MCP initialization",
                paths::mcp_config_file().display()
            );
            return Ok(registry);
        }
        let err = || {
            format!(
                "Failed to load MCP config file at {}",
                paths::mcp_config_file().display()
            )
        };
        let content = tokio::fs::read_to_string(paths::mcp_config_file())
            .await
            .with_context(err)?;

        if content.trim().is_empty() {
            debug!("MCP config file is empty, skipping MCP initialization");
            return Ok(registry);
        }

        let (parsed_content, missing_secrets) = interpolate_secrets(&content, vault)?;

        if !missing_secrets.is_empty() {
            return Err(anyhow!(formatdoc!(
                "
								MCP config file references secrets that are missing from the vault: {:?}
								Please add these secrets to the vault and try again.",
                missing_secrets
            )));
        }

        let mcp_servers_config: McpServersConfig =
            serde_json::from_str(&parsed_content).with_context(err)?;

        for (name, spec) in &mcp_servers_config.mcp_servers {
            spec.validate(name)?;
        }

        let mut merged = mcp_servers_config;
        if !app_config.no_workspace_mcp
            && let Some(ws_path) = paths::workspace_mcp_config_file()
        {
            match tokio::fs::read_to_string(&ws_path).await {
                Ok(ws_content) if !ws_content.trim().is_empty() => {
                    match interpolate_secrets(&ws_content, vault) {
                        Ok((parsed, missing)) if missing.is_empty() => {
                            match serde_json::from_str::<McpServersConfig>(&parsed) {
                                Ok(ws_config) => {
                                    let mut loaded = Vec::new();
                                    for (name, spec) in ws_config.mcp_servers {
                                        match spec.validate(&name) {
                                            Ok(_) => {
                                                loaded.push(name.clone());
                                                merged.mcp_servers.insert(name, spec);
                                            }
                                            Err(e) => warn!(
                                                "Invalid workspace MCP server '{name}': {e}. Skipping."
                                            ),
                                        }
                                    }
                                    if !loaded.is_empty() {
                                        eprintln!(
                                            "Loading workspace MCP servers: {}",
                                            loaded.join(", ")
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to parse workspace MCP config: {e}. Skipping.")
                                }
                            }
                        }
                        Ok((_, missing)) => warn!(
                            "Workspace MCP config references missing vault secrets: {missing:?}. Skipping."
                        ),
                        Err(e) => {
                            warn!("Failed to process workspace MCP config: {e}. Skipping.")
                        }
                    }
                }
                _ => {}
            }
        }
        registry.config = Some(merged);

        if start_mcp_servers && app_config.mcp_server_support {
            abortable_run_with_spinner(
                registry.start_select_mcp_servers(enabled_mcp_servers),
                "Loading MCP servers",
                abort_signal,
            )
            .await?;
        }

        Ok(registry)
    }

    async fn start_select_mcp_servers(
        &mut self,
        enabled_mcp_servers: Option<Vec<String>>,
    ) -> Result<()> {
        if self.config.is_none() {
            debug!(
                "MCP config is not present; assuming MCP servers are disabled globally. Skipping MCP initialization"
            );
            return Ok(());
        }

        let desired_ids = self.resolve_server_ids(enabled_mcp_servers);
        let ids_to_start: Vec<String> = desired_ids
            .into_iter()
            .filter(|id| !self.servers.contains_key(id))
            .collect();

        if ids_to_start.is_empty() {
            return Ok(());
        }

        debug!("Starting selected MCP servers: {:?}", ids_to_start);

        let results: Vec<Option<(String, Arc<ConnectedServer>)>> = stream::iter(
            ids_to_start
                .into_iter()
                .map(|id| async { self.start_server(id).await }),
        )
        .buffer_unordered(num_cpus::get())
        .try_collect()
        .await?;

        for (id, server) in results.into_iter().flatten() {
            self.servers.insert(id, server);
        }

        Ok(())
    }

    async fn start_server(&self, id: String) -> Result<Option<(String, Arc<ConnectedServer>)>> {
        let spec = self
            .config
            .as_ref()
            .and_then(|c| c.mcp_servers.get(&id))
            .with_context(|| format!("MCP server not found in config: {id}"))?;

        let (auth, auth_reason) = resolve_http_auth(&id, spec).await;

        let service = match spawn_mcp_server(spec, self.log_path.as_deref(), auth).await {
            Ok(s) => s,
            Err(e) if is_auth_required_error(&e) => {
                warn!(
                    "{}",
                    McpAuthRequired {
                        server: id,
                        reason: auth_reason,
                    }
                );
                return Ok(None);
            }
            Err(e) => return Err(e),
        };

        info!("Started MCP server: {id}");

        Ok(Some((id, service)))
    }

    fn resolve_server_ids(&self, enabled_mcp_servers: Option<Vec<String>>) -> Vec<String> {
        if let Some(config) = &self.config
            && let Some(servers) = enabled_mcp_servers
        {
            if servers.iter().any(|s| s.trim() == "all") {
                config.mcp_servers.keys().cloned().collect()
            } else {
                let enabled_servers: HashSet<String> =
                    servers.into_iter().map(|s| s.trim().to_string()).collect();
                config
                    .mcp_servers
                    .keys()
                    .filter(|id| enabled_servers.contains(*id))
                    .cloned()
                    .collect()
            }
        } else {
            vec![]
        }
    }

    pub fn running_servers(&self) -> &HashMap<String, Arc<ConnectedServer>> {
        &self.servers
    }

    pub fn list_started_servers(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn mcp_config(&self) -> Option<&McpServersConfig> {
        self.config.as_ref()
    }

    pub fn log_path(&self) -> Option<&PathBuf> {
        self.log_path.as_ref()
    }
}

/// How a remote MCP server authenticates outgoing requests.
pub(crate) enum HttpAuth {
    /// Only the static headers from the server spec; no OAuth token.
    StaticOnly,
    /// OAuth-managed: HTTP transports inject a fresh bearer token per request
    /// via [`McpOAuthClient`] (ignoring the carried token); SSE transports
    /// send the carried token as a static header.
    Managed { server: String, token: String },
}

impl fmt::Debug for HttpAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaticOnly => f.write_str("StaticOnly"),
            Self::Managed { server, token: _ } => f
                .debug_struct("Managed")
                .field("server", server)
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

impl HttpAuth {
    pub(crate) fn from_token_status(status: &oauth::McpTokenStatus, server: &str) -> Self {
        match status {
            oauth::McpTokenStatus::Token(token) => Self::Managed {
                server: server.to_string(),
                token: token.clone(),
            },
            oauth::McpTokenStatus::NotAuthenticated | oauth::McpTokenStatus::RefreshFailed => {
                Self::StaticOnly
            }
        }
    }
}

pub(crate) async fn resolve_http_auth(name: &str, spec: &McpServer) -> (HttpAuth, McpAuthReason) {
    let token_status = if spec.is_remote() {
        oauth::load_or_refresh_mcp_token(name).await
    } else {
        oauth::McpTokenStatus::NotAuthenticated
    };
    (
        HttpAuth::from_token_status(&token_status, name),
        McpAuthReason::from_token_status(&token_status),
    )
}

pub(crate) async fn spawn_mcp_server(
    spec: &McpServer,
    log_path: Option<&Path>,
    auth: HttpAuth,
) -> Result<Arc<ConnectedServer>> {
    match spec.transport_type {
        McpTransportType::Http => {
            let url = spec.url.as_deref().expect("validated: http spec has url");
            match auth {
                HttpAuth::Managed { server, token: _ } => {
                    spawn_oauth_http_mcp_server(url, &server, spec.headers.as_ref()).await
                }
                HttpAuth::StaticOnly => spawn_http_mcp_server(url, spec.headers.as_ref()).await,
            }
        }
        McpTransportType::Sse => {
            let url = spec.url.as_deref().expect("validated: sse spec has url");
            let bearer_token = match auth {
                HttpAuth::Managed { server: _, token } => Some(token),
                HttpAuth::StaticOnly => None,
            };
            let headers = merge_bearer_token(spec.headers.as_ref(), bearer_token);
            spawn_sse_mcp_server(url, headers.as_ref()).await
        }
        McpTransportType::Stdio => {
            let command = spec
                .command
                .as_deref()
                .expect("validated: stdio spec has command");
            spawn_stdio_mcp_server(command, spec, log_path).await
        }
    }
}

fn merge_bearer_token(
    headers: Option<&IndexMap<String, String>>,
    bearer_token: Option<String>,
) -> Option<IndexMap<String, String>> {
    match (headers, bearer_token) {
        (None, None) => None,
        (Some(h), None) => Some(h.clone()),
        (None, Some(token)) => {
            let mut m = IndexMap::new();
            m.insert("Authorization".to_string(), format!("Bearer {token}"));
            Some(m)
        }
        (Some(h), Some(token)) => {
            let mut m = h.clone();
            m.retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
            m.insert("Authorization".to_string(), format!("Bearer {token}"));
            Some(m)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpAuthReason {
    NotAuthenticated,
    RefreshFailed,
    TokenRejected,
}

impl McpAuthReason {
    pub(crate) fn from_token_status(status: &oauth::McpTokenStatus) -> Self {
        match status {
            oauth::McpTokenStatus::Token(_) => Self::TokenRejected,
            oauth::McpTokenStatus::NotAuthenticated => Self::NotAuthenticated,
            oauth::McpTokenStatus::RefreshFailed => Self::RefreshFailed,
        }
    }
}

#[derive(Debug)]
pub(crate) struct McpAuthRequired {
    pub server: String,
    pub reason: McpAuthReason,
}

impl Display for McpAuthRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let server = &self.server;
        match self.reason {
            McpAuthReason::NotAuthenticated => write!(
                f,
                "MCP server '{server}' requires OAuth authentication and was not started \
                 (no stored credentials). Run `.mcp auth {server}` (or `coyote --auth-mcp \
                 {server}`) to authenticate and attach it."
            ),
            McpAuthReason::RefreshFailed => write!(
                f,
                "MCP server '{server}' was not started: stored OAuth token has expired and \
                 automatic refresh failed. Run `.mcp auth {server}` (or `coyote --auth-mcp \
                 {server}`) to re-authenticate and attach it."
            ),
            McpAuthReason::TokenRejected => write!(
                f,
                "MCP server '{server}' was not started: the server rejected the stored OAuth \
                 token. Run `.mcp auth {server}` (or `coyote --auth-mcp {server}`) to \
                 re-authenticate and attach it."
            ),
        }
    }
}

pub(crate) fn is_auth_required_error(e: &Error) -> bool {
    e.downcast_ref::<McpAuthRequired>().is_some()
        || e.chain()
            .any(|cause| cause.to_string().contains("Auth required"))
}

async fn spawn_http_mcp_server(
    url: &str,
    headers: Option<&IndexMap<String, String>>,
) -> Result<Arc<ConnectedServer>> {
    let transport = if let Some(hdrs) = headers
        && !hdrs.is_empty()
    {
        let mut custom = HashMap::new();
        for (k, v) in hdrs {
            let name = k
                .parse::<HeaderName>()
                .with_context(|| format!("Invalid header name: {k}"))?;
            let value = v
                .parse::<HeaderValue>()
                .with_context(|| format!("Invalid header value for {k}"))?;
            custom.insert(name, value);
        }
        let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom);
        StreamableHttpClientTransport::from_config(config)
    } else {
        StreamableHttpClientTransport::from_uri(url)
    };
    let service = Arc::new(
        ().serve(transport)
            .await
            .with_context(|| format!("Failed to connect to HTTP MCP server: {url}"))?,
    );
    Ok(service)
}

/// Builds the custom-header map for an OAuth-managed HTTP transport, dropping
/// any static `Authorization` entry case-insensitively: [`McpOAuthClient`]
/// owns that header, and a stale configured value must not collide with the
/// per-request token.
fn oauth_custom_headers(
    headers: Option<&IndexMap<String, String>>,
) -> Result<HashMap<HeaderName, HeaderValue>> {
    let mut custom = HashMap::new();
    let Some(hdrs) = headers else {
        return Ok(custom);
    };

    for (k, v) in hdrs {
        if k.eq_ignore_ascii_case("authorization") {
            continue;
        }
        let name = k
            .parse::<HeaderName>()
            .with_context(|| format!("Invalid header name: {k}"))?;
        let value = v
            .parse::<HeaderValue>()
            .with_context(|| format!("Invalid header value for {k}"))?;
        custom.insert(name, value);
    }

    Ok(custom)
}

async fn spawn_oauth_http_mcp_server(
    url: &str,
    server: &str,
    headers: Option<&IndexMap<String, String>>,
) -> Result<Arc<ConnectedServer>> {
    // Mirror rmcp's default_http_client, which `with_client` bypasses:
    // idle pooling off avoids a documented TCP delayed-ACK stall, and
    // redirects off keeps custom headers from being replayed to a redirect
    // target.
    let inner = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("Failed to build HTTP client for OAuth-managed MCP transport")?;
    let client = McpOAuthClient::new(inner, server);
    // `auth_header` stays None so the wrapper injects a fresh token per
    // request; `reinit_on_expired_session` defaults to true in rmcp 3.1.2
    // but is pinned explicitly because transparent session re-init is
    // load-bearing for long-lived sessions.
    let config = StreamableHttpClientTransportConfig::with_uri(url)
        .custom_headers(oauth_custom_headers(headers)?)
        .reinit_on_expired_session(true);
    let transport = StreamableHttpClientTransport::with_client(client, config);
    let service = Arc::new(
        ().serve(transport)
            .await
            .with_context(|| format!("Failed to connect to HTTP MCP server: {url}"))?,
    );

    Ok(service)
}

async fn spawn_sse_mcp_server(
    url: &str,
    headers: Option<&IndexMap<String, String>>,
) -> Result<Arc<ConnectedServer>> {
    let sse = LegacySseTransport::connect(url, headers)
        .await
        .with_context(|| format!("Failed to connect to SSE MCP server: {url}"))?;
    let (sink, stream) = sse.into_parts();
    let service = Arc::new(
        ().serve((sink, stream))
            .await
            .with_context(|| format!("Failed to initialize SSE MCP server: {url}"))?,
    );
    Ok(service)
}

async fn spawn_stdio_mcp_server(
    command: &str,
    spec: &McpServer,
    log_path: Option<&Path>,
) -> Result<Arc<ConnectedServer>> {
    let mut cmd = Command::new(command);
    if let Some(args) = &spec.args {
        cmd.args(args);
    }
    if let Some(env) = &spec.env {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| match v {
                JsonField::Str(s) => (k.clone(), s.clone()),
                JsonField::Bool(b) => (k.clone(), b.to_string()),
                JsonField::Int(i) => (k.clone(), i.to_string()),
            })
            .collect();
        cmd.envs(env);
    }
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }

    let transport = if let Some(log_path) = log_path {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());

        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("Failed to open MCP log file at '{}'", log_path.display()))?;
        let (transport, _) = TokioChildProcess::builder(cmd)
            .stderr(log_file)
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server: {command}"))?;
        transport
    } else {
        TokioChildProcess::new(cmd)?
    };

    let service = Arc::new(
        ().serve(transport)
            .await
            .with_context(|| format!("Failed to start MCP server: {command}"))?,
    );
    Ok(service)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_server(command: &str) -> McpServer {
        McpServer {
            transport_type: McpTransportType::Stdio,
            command: Some(command.to_string()),
            args: None,
            env: None,
            cwd: None,
            url: None,
            headers: None,
            oauth: None,
        }
    }

    fn http_server(url: &str) -> McpServer {
        McpServer {
            transport_type: McpTransportType::Http,
            command: None,
            args: None,
            env: None,
            cwd: None,
            url: Some(url.to_string()),
            headers: None,
            oauth: None,
        }
    }

    fn sse_server(url: &str) -> McpServer {
        McpServer {
            transport_type: McpTransportType::Sse,
            command: None,
            args: None,
            env: None,
            cwd: None,
            url: Some(url.to_string()),
            headers: None,
            oauth: None,
        }
    }

    fn make_registry_with_config(server_names: &[&str]) -> McpRegistry {
        let mut mcp_servers = IndexMap::new();
        for name in server_names {
            mcp_servers.insert(name.to_string(), stdio_server("echo"));
        }
        McpRegistry {
            config: Some(McpServersConfig { mcp_servers }),
            ..Default::default()
        }
    }

    #[test]
    fn validate_stdio_with_command_succeeds() {
        let spec = stdio_server("npx");

        assert!(spec.validate("test").is_ok());
    }

    #[test]
    fn validate_stdio_missing_command_fails() {
        let spec = McpServer {
            transport_type: McpTransportType::Stdio,
            command: None,
            args: None,
            env: None,
            cwd: None,
            url: None,
            headers: None,
            oauth: None,
        };

        let err = spec.validate("test").unwrap_err();

        assert!(err.to_string().contains("missing a \"command\" field"));
    }

    #[test]
    fn validate_stdio_with_url_fails() {
        let spec = McpServer {
            transport_type: McpTransportType::Stdio,
            command: Some("cmd".into()),
            args: None,
            env: None,
            cwd: None,
            url: Some("http://localhost".into()),
            headers: None,
            oauth: None,
        };

        let err = spec.validate("test").unwrap_err();

        assert!(err.to_string().contains("remote fields"));
    }

    #[test]
    fn validate_stdio_with_headers_fails() {
        let mut headers = IndexMap::new();
        headers.insert("Auth".into(), "Bearer tok".into());
        let spec = McpServer {
            transport_type: McpTransportType::Stdio,
            command: Some("cmd".into()),
            args: None,
            env: None,
            cwd: None,
            url: None,
            headers: Some(headers),
            oauth: None,
        };

        let err = spec.validate("test").unwrap_err();

        assert!(err.to_string().contains("remote fields"));
    }

    #[test]
    fn validate_http_with_url_succeeds() {
        let spec = http_server("http://localhost:8080");

        assert!(spec.validate("test").is_ok());
    }

    #[test]
    fn validate_http_missing_url_fails() {
        let spec = McpServer {
            transport_type: McpTransportType::Http,
            command: None,
            args: None,
            env: None,
            cwd: None,
            url: None,
            headers: None,
            oauth: None,
        };

        let err = spec.validate("test").unwrap_err();

        assert!(err.to_string().contains("missing a \"url\" field"));
    }

    #[test]
    fn validate_http_with_command_fails() {
        let spec = McpServer {
            transport_type: McpTransportType::Http,
            command: Some("npx".into()),
            args: None,
            env: None,
            cwd: None,
            url: Some("http://localhost".into()),
            headers: None,
            oauth: None,
        };

        let err = spec.validate("test").unwrap_err();

        assert!(err.to_string().contains("stdio fields"));
    }

    #[test]
    fn validate_http_with_args_fails() {
        let spec = McpServer {
            transport_type: McpTransportType::Http,
            command: None,
            args: Some(vec!["--flag".into()]),
            env: None,
            cwd: None,
            url: Some("http://localhost".into()),
            headers: None,
            oauth: None,
        };

        let err = spec.validate("test").unwrap_err();

        assert!(err.to_string().contains("stdio fields"));
    }

    #[test]
    fn validate_http_with_cwd_fails() {
        let spec = McpServer {
            transport_type: McpTransportType::Http,
            command: None,
            args: None,
            env: None,
            cwd: Some("/tmp".into()),
            url: Some("http://localhost".into()),
            headers: None,
            oauth: None,
        };

        let err = spec.validate("test").unwrap_err();

        assert!(err.to_string().contains("stdio fields"));
    }

    #[test]
    fn validate_sse_with_url_succeeds() {
        let spec = sse_server("http://sse.example.com");

        assert!(spec.validate("test").is_ok());
    }

    #[test]
    fn validate_sse_missing_url_fails() {
        let spec = McpServer {
            transport_type: McpTransportType::Sse,
            command: None,
            args: None,
            env: None,
            cwd: None,
            url: None,
            headers: None,
            oauth: None,
        };

        let err = spec.validate("test").unwrap_err();

        assert!(err.to_string().contains("missing a \"url\" field"));
    }

    #[test]
    fn is_remote_true_for_http_and_sse() {
        assert!(http_server("http://x").is_remote());
        assert!(sse_server("http://x").is_remote());
    }

    #[test]
    fn is_remote_false_for_stdio() {
        assert!(!stdio_server("cmd").is_remote());
    }

    #[test]
    fn deserialize_stdio_server_from_json() {
        let json = r#"{
            "mcpServers": {
                "my-server": {
                    "type": "stdio",
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server"]
                }
            }
        }"#;

        let config: McpServersConfig = serde_json::from_str(json).unwrap();

        assert!(config.mcp_servers.contains_key("my-server"));

        let spec = &config.mcp_servers["my-server"];

        assert_eq!(spec.transport_type, McpTransportType::Stdio);
        assert_eq!(spec.command.as_deref(), Some("npx"));
        assert_eq!(
            spec.args.as_ref().unwrap(),
            &["-y", "@modelcontextprotocol/server"]
        );
    }

    #[test]
    fn deserialize_http_server_from_json() {
        let json = r#"{
            "mcpServers": {
                "remote": {
                    "type": "http",
                    "url": "http://localhost:8080/mcp",
                    "headers": {"Authorization": "Bearer tok"}
                }
            }
        }"#;
        let config: McpServersConfig = serde_json::from_str(json).unwrap();

        let spec = &config.mcp_servers["remote"];

        assert_eq!(spec.transport_type, McpTransportType::Http);
        assert_eq!(spec.url.as_deref(), Some("http://localhost:8080/mcp"));
        assert_eq!(
            spec.headers.as_ref().unwrap()["Authorization"],
            "Bearer tok"
        );
    }

    #[test]
    fn deserialize_env_with_mixed_types() {
        let json = r#"{
            "mcpServers": {
                "s": {
                    "type": "stdio",
                    "command": "cmd",
                    "env": {
                        "STR_VAR": "hello",
                        "BOOL_VAR": true,
                        "INT_VAR": 42
                    }
                }
            }
        }"#;
        let config: McpServersConfig = serde_json::from_str(json).unwrap();

        let env = config.mcp_servers["s"].env.as_ref().unwrap();

        assert!(matches!(env["STR_VAR"], JsonField::Str(ref s) if s == "hello"));
        assert!(matches!(env["BOOL_VAR"], JsonField::Bool(true)));
        assert!(matches!(env["INT_VAR"], JsonField::Int(42)));
    }

    #[test]
    fn deserialize_multiple_servers() {
        let json = r#"{
            "mcpServers": {
                "github": { "type": "stdio", "command": "gh-mcp" },
                "remote-api": { "type": "http", "url": "http://api.example.com" }
            }
        }"#;

        let config: McpServersConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.mcp_servers.len(), 2);
        assert!(config.mcp_servers.contains_key("github"));
        assert!(config.mcp_servers.contains_key("remote-api"));
    }

    #[test]
    fn deserialize_empty_servers_map() {
        let json = r#"{ "mcpServers": {} }"#;

        let config: McpServersConfig = serde_json::from_str(json).unwrap();

        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn deserialize_server_with_cwd() {
        let json = r#"{
            "mcpServers": {
                "s": {
                    "type": "stdio",
                    "command": "cmd",
                    "cwd": "/tmp/work"
                }
            }
        }"#;

        let config: McpServersConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.mcp_servers["s"].cwd.as_deref(), Some("/tmp/work"));
    }

    #[test]
    fn resolve_all_returns_all_configured_servers() {
        let registry = make_registry_with_config(&["github", "slack", "jira"]);

        let mut ids = registry.resolve_server_ids(Some(vec!["all".to_string()]));
        ids.sort();

        assert_eq!(ids, vec!["github", "jira", "slack"]);
    }

    #[test]
    fn resolve_comma_separated_returns_matching_servers() {
        let registry = make_registry_with_config(&["github", "slack", "jira"]);

        let mut ids =
            registry.resolve_server_ids(Some(vec!["github".to_string(), "jira".to_string()]));
        ids.sort();

        assert_eq!(ids, vec!["github", "jira"]);
    }

    #[test]
    fn resolve_single_server_name() {
        let registry = make_registry_with_config(&["github", "slack"]);

        let ids = registry.resolve_server_ids(Some(vec!["slack".to_string()]));

        assert_eq!(ids, vec!["slack"]);
    }

    #[test]
    fn resolve_none_returns_empty() {
        let registry = make_registry_with_config(&["github"]);

        let ids = registry.resolve_server_ids(None);

        assert!(ids.is_empty());
    }

    #[test]
    fn resolve_no_config_returns_empty() {
        let registry = McpRegistry::default();

        let ids = registry.resolve_server_ids(Some(vec!["all".to_string()]));

        assert!(ids.is_empty());
    }

    #[test]
    fn resolve_nonexistent_server_filtered_out() {
        let registry = make_registry_with_config(&["github"]);

        let ids = registry
            .resolve_server_ids(Some(vec!["github".to_string(), "nonexistent".to_string()]));

        assert_eq!(ids, vec!["github"]);
    }

    #[test]
    fn resolve_all_nonexistent_returns_empty() {
        let registry = make_registry_with_config(&["github"]);

        let ids = registry.resolve_server_ids(Some(vec!["foo".to_string(), "bar".to_string()]));

        assert!(ids.is_empty());
    }

    #[test]
    fn resolve_trims_whitespace() {
        let registry = make_registry_with_config(&["github", "slack"]);

        let mut ids = registry.resolve_server_ids(Some(vec![
            "  github  ".to_string(),
            "  slack  ".to_string(),
        ]));
        ids.sort();

        assert_eq!(ids, vec!["github", "slack"]);
    }

    #[test]
    fn registry_default_is_empty() {
        let registry = McpRegistry::default();

        assert!(registry.is_empty());
        assert!(registry.list_started_servers().is_empty());
        assert!(registry.mcp_config().is_none());
        assert!(registry.log_path().is_none());
    }

    #[test]
    fn registry_with_config_reports_config() {
        let registry = make_registry_with_config(&["github"]);

        assert!(registry.mcp_config().is_some());
        assert!(
            registry
                .mcp_config()
                .unwrap()
                .mcp_servers
                .contains_key("github")
        );
    }

    #[test]
    fn meta_function_prefixes_are_correct() {
        assert_eq!(MCP_INVOKE_META_FUNCTION_NAME_PREFIX, "mcp_invoke");
        assert_eq!(MCP_SEARCH_META_FUNCTION_NAME_PREFIX, "mcp_search");
        assert_eq!(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX, "mcp_describe");
        assert_eq!(MCP_READ_META_FUNCTION_NAME_PREFIX, "mcp_read");
        assert_eq!(MCP_PROMPT_META_FUNCTION_NAME_PREFIX, "mcp_prompt");
    }

    #[test]
    fn is_mcp_meta_function_classifies_names() {
        assert!(is_mcp_meta_function("mcp_invoke_github"));
        assert!(is_mcp_meta_function("mcp_search_github"));
        assert!(is_mcp_meta_function("mcp_describe_github"));
        assert!(is_mcp_meta_function("mcp_read_github"));
        assert!(is_mcp_meta_function("mcp_prompt_github"));
        assert!(!is_mcp_meta_function("mcp_gateway_tool"));
        assert!(!is_mcp_meta_function("fs_read"));
        assert!(!is_mcp_meta_function(""));
        assert!(!is_mcp_meta_function("mcp_"));
    }

    #[test]
    fn meta_function_prefixes_are_not_prefixes_of_each_other() {
        for (i, a) in MCP_META_FUNCTION_PREFIXES.iter().enumerate() {
            for (j, b) in MCP_META_FUNCTION_PREFIXES.iter().enumerate() {
                if i != j {
                    assert!(!b.starts_with(a), "{a} is a prefix of {b}");
                }
            }
        }
    }

    #[test]
    fn is_mcp_meta_function_preserves_lax_prefix_matching() {
        assert!(is_mcp_meta_function("mcp_invoker_x"));
    }

    #[test]
    fn mcp_meta_function_names_returns_all_prefixes_in_order() {
        assert_eq!(
            mcp_meta_function_names("github"),
            vec![
                "mcp_invoke_github",
                "mcp_search_github",
                "mcp_describe_github",
                "mcp_read_github",
                "mcp_prompt_github",
            ]
        );
    }

    #[test]
    fn merge_bearer_token_both_none_returns_none() {
        assert!(merge_bearer_token(None, None).is_none());
    }

    #[test]
    fn merge_bearer_token_headers_only_passes_through() {
        let mut h = IndexMap::new();
        h.insert("X-Key".to_string(), "val".to_string());

        let result = merge_bearer_token(Some(&h), None).unwrap();

        assert_eq!(result["X-Key"], "val");
        assert!(!result.contains_key("Authorization"));
    }

    #[test]
    fn merge_bearer_token_token_only_injects_bearer() {
        let result = merge_bearer_token(None, Some("tok123".to_string())).unwrap();

        assert_eq!(result["Authorization"], "Bearer tok123");
    }

    #[test]
    fn merge_bearer_token_both_merges_and_overrides_authorization() {
        let mut h = IndexMap::new();
        h.insert("Authorization".to_string(), "old".to_string());
        h.insert("X-Custom".to_string(), "keep".to_string());

        let result = merge_bearer_token(Some(&h), Some("newtoken".to_string())).unwrap();

        assert_eq!(result["Authorization"], "Bearer newtoken");
        assert_eq!(result["X-Custom"], "keep");
    }

    #[test]
    fn merge_bearer_token_replaces_authorization_case_insensitively() {
        let mut h = IndexMap::new();
        h.insert("authorization".to_string(), "Bearer stale-1".to_string());
        h.insert("AUTHORIZATION".to_string(), "Bearer stale-2".to_string());
        h.insert("X-Custom".to_string(), "keep".to_string());

        let result = merge_bearer_token(Some(&h), Some("newtoken".to_string())).unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result["Authorization"], "Bearer newtoken");
        assert_eq!(result["X-Custom"], "keep");
        assert!(!result.contains_key("authorization"));
        assert!(!result.contains_key("AUTHORIZATION"));
    }

    #[test]
    fn http_auth_from_token_status_maps_token_to_managed() {
        assert!(matches!(
            HttpAuth::from_token_status(&oauth::McpTokenStatus::Token("tok".into()), "srv"),
            HttpAuth::Managed { server, token } if server == "srv" && token == "tok"
        ));
        assert!(matches!(
            HttpAuth::from_token_status(&oauth::McpTokenStatus::NotAuthenticated, "srv"),
            HttpAuth::StaticOnly
        ));
        assert!(matches!(
            HttpAuth::from_token_status(&oauth::McpTokenStatus::RefreshFailed, "srv"),
            HttpAuth::StaticOnly
        ));
    }

    #[test]
    fn http_auth_debug_redacts_token() {
        let auth = HttpAuth::Managed {
            server: "srv".into(),
            token: "live-secret".into(),
        };

        let debug = format!("{auth:?}");

        assert!(debug.contains("srv"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("live-secret"));
    }

    #[test]
    fn oauth_custom_headers_strips_authorization_case_insensitively() {
        let mut h = IndexMap::new();
        h.insert("Authorization".to_string(), "Bearer stale-1".to_string());
        h.insert("authorization".to_string(), "Bearer stale-2".to_string());
        h.insert("AUTHORIZATION".to_string(), "Bearer stale-3".to_string());
        h.insert("X-Custom".to_string(), "keep".to_string());

        let custom = oauth_custom_headers(Some(&h)).unwrap();

        assert_eq!(custom.len(), 1);
        assert_eq!(custom[&HeaderName::from_static("x-custom")], "keep");
    }

    #[test]
    fn oauth_custom_headers_none_is_empty() {
        assert!(oauth_custom_headers(None).unwrap().is_empty());
    }

    #[test]
    fn oauth_custom_headers_rejects_invalid_header_name() {
        let mut h = IndexMap::new();
        h.insert("bad header".to_string(), "v".to_string());

        assert!(oauth_custom_headers(Some(&h)).is_err());
    }

    #[test]
    fn oauth_custom_headers_keeps_non_authorization_headers() {
        let mut h = IndexMap::new();
        h.insert("X-Api-Key".to_string(), "k".to_string());
        h.insert("X-Trace".to_string(), "t".to_string());

        let custom = oauth_custom_headers(Some(&h)).unwrap();

        assert_eq!(custom.len(), 2);
        assert_eq!(custom[&HeaderName::from_static("x-api-key")], "k");
        assert_eq!(custom[&HeaderName::from_static("x-trace")], "t");
    }

    #[test]
    fn is_auth_required_error_matches_rmcp_message() {
        let e = anyhow!("Auth required, when send initialize request");

        assert!(is_auth_required_error(&e));
    }

    #[test]
    fn is_auth_required_error_does_not_match_unrelated() {
        let e = anyhow!("Connection refused");

        assert!(!is_auth_required_error(&e));
    }

    #[test]
    fn is_auth_required_error_survives_context_wrapping() {
        let e = anyhow!("Auth required, when send initialize request").context(
            "MCP server 'github' requires OAuth authentication. \
             Run `coyote --auth-mcp github` or `.mcp auth github` in the REPL to authenticate.",
        );

        assert!(is_auth_required_error(&e));
    }

    #[test]
    fn auth_reason_maps_token_status() {
        assert_eq!(
            McpAuthReason::from_token_status(&oauth::McpTokenStatus::Token("tok".into())),
            McpAuthReason::TokenRejected
        );
        assert_eq!(
            McpAuthReason::from_token_status(&oauth::McpTokenStatus::NotAuthenticated),
            McpAuthReason::NotAuthenticated
        );
        assert_eq!(
            McpAuthReason::from_token_status(&oauth::McpTokenStatus::RefreshFailed),
            McpAuthReason::RefreshFailed
        );
    }

    #[test]
    fn mcp_auth_required_context_downcasts_with_reason() {
        let e = anyhow!("Auth required, when send initialize request").context(McpAuthRequired {
            server: "github".into(),
            reason: McpAuthReason::RefreshFailed,
        });

        assert!(is_auth_required_error(&e));
        let ctx = e.downcast_ref::<McpAuthRequired>().unwrap();
        assert_eq!(ctx.server, "github");
        assert_eq!(ctx.reason, McpAuthReason::RefreshFailed);
    }

    #[test]
    fn mcp_auth_required_display_is_reason_specific() {
        let msg = |reason| {
            McpAuthRequired {
                server: "github".into(),
                reason,
            }
            .to_string()
        };

        assert!(msg(McpAuthReason::NotAuthenticated).contains("no stored credentials"));
        assert!(msg(McpAuthReason::RefreshFailed).contains("expired and automatic refresh failed"));
        assert!(msg(McpAuthReason::TokenRejected).contains("rejected the stored OAuth token"));
    }
}
