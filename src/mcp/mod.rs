mod sse_transport;

use crate::config::AppConfig;
use crate::config::paths;
use crate::utils::{AbortSignal, abortable_run_with_spinner};
use crate::vault::Vault;
use crate::vault::interpolate_secrets;
use anyhow::{Context, Result, anyhow};
use futures_util::{StreamExt, TryStreamExt, stream};
use http::{HeaderName, HeaderValue};
use indoc::formatdoc;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde::{Deserialize, Serialize};
use sse_transport::LegacySseTransport;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;

pub const MCP_INVOKE_META_FUNCTION_NAME_PREFIX: &str = "mcp_invoke";
pub const MCP_SEARCH_META_FUNCTION_NAME_PREFIX: &str = "mcp_search";
pub const MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX: &str = "mcp_describe";

pub type ConnectedServer = RunningService<RoleClient, ()>;

#[derive(Clone, Debug, Default, Serialize)]
pub struct CatalogItem {
    pub name: String,
    pub server: String,
    pub description: String,
}

#[derive(Debug)]
struct ServerCatalog {
    items: HashMap<String, CatalogItem>,
}

impl Clone for ServerCatalog {
    fn clone(&self) -> Self {
        Self {
            items: self.items.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct McpServersConfig {
    #[serde(rename = "mcpServers")]
    pub mcp_servers: HashMap<String, McpServer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpServer {
    #[serde(rename = "type")]
    pub transport_type: McpTransportType,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, JsonField>>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
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
            if self.url.is_some() || self.headers.is_some() {
                return Err(anyhow!(
                    "MCP server '{name}' has type \"stdio\" but also specifies remote fields \
                     (url/headers). Remove the remote fields or change the type to \"http\" or \"sse\"."
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub(crate) enum McpTransportType {
    Stdio,
    Http,
    Sse,
}

#[derive(Debug, Clone, Deserialize)]
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
    catalogs: HashMap<String, ServerCatalog>,
}

impl McpRegistry {
    pub async fn init(
        log_path: Option<PathBuf>,
        start_mcp_servers: bool,
        enabled_mcp_servers: Option<String>,
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

        let (parsed_content, missing_secrets) = interpolate_secrets(&content, vault);

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

        registry.config = Some(mcp_servers_config);

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
        enabled_mcp_servers: Option<String>,
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

        let results: Vec<(String, Arc<_>, ServerCatalog)> = stream::iter(
            ids_to_start
                .into_iter()
                .map(|id| async { self.start_server(id).await }),
        )
        .buffer_unordered(num_cpus::get())
        .try_collect()
        .await?;

        for (id, server, catalog) in results {
            self.servers.insert(id.clone(), server);
            self.catalogs.insert(id, catalog);
        }

        Ok(())
    }

    async fn start_server(
        &self,
        id: String,
    ) -> Result<(String, Arc<ConnectedServer>, ServerCatalog)> {
        let spec = self
            .config
            .as_ref()
            .and_then(|c| c.mcp_servers.get(&id))
            .with_context(|| format!("MCP server not found in config: {id}"))?;

        let service = spawn_mcp_server(spec, self.log_path.as_deref()).await?;

        let tools = service.list_tools(None).await?;
        debug!("Available tools for MCP server {id}: {tools:?}");

        let mut items_vec = Vec::new();
        for t in tools.tools {
            let name = t.name.to_string();
            let description = t.description.unwrap_or_default().to_string();
            items_vec.push(CatalogItem {
                name,
                server: id.clone(),
                description,
            });
        }

        let mut items_map = HashMap::new();
        items_vec.into_iter().for_each(|it| {
            items_map.insert(it.name.clone(), it);
        });

        let catalog = ServerCatalog { items: items_map };

        info!("Started MCP server: {id}");

        Ok((id.to_string(), service, catalog))
    }

    fn resolve_server_ids(&self, enabled_mcp_servers: Option<String>) -> Vec<String> {
        if let Some(config) = &self.config
            && let Some(servers) = enabled_mcp_servers
        {
            if servers == "all" {
                config.mcp_servers.keys().cloned().collect()
            } else {
                let enabled_servers: HashSet<String> =
                    servers.split(',').map(|s| s.trim().to_string()).collect();
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

pub(crate) async fn spawn_mcp_server(
    spec: &McpServer,
    log_path: Option<&Path>,
) -> Result<Arc<ConnectedServer>> {
    match spec.transport_type {
        McpTransportType::Http => {
            let url = spec.url.as_deref().expect("validated: http spec has url");
            spawn_http_mcp_server(url, spec.headers.as_ref()).await
        }
        McpTransportType::Sse => {
            let url = spec.url.as_deref().expect("validated: sse spec has url");
            spawn_sse_mcp_server(url, spec.headers.as_ref()).await
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

async fn spawn_http_mcp_server(
    url: &str,
    headers: Option<&HashMap<String, String>>,
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

async fn spawn_sse_mcp_server(
    url: &str,
    headers: Option<&HashMap<String, String>>,
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
            .open(log_path)?;
        let (transport, _) = TokioChildProcess::builder(cmd).stderr(log_file).spawn()?;
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
