use crate::config::Config;
use crate::utils::{abortable_run_with_spinner, AbortSignal};
use crate::vault::interpolate_secrets;
use anyhow::{anyhow, Context, Result};
use futures_util::future::BoxFuture;
use futures_util::{stream, StreamExt, TryStreamExt};
use indoc::formatdoc;
use rmcp::model::{CallToolRequestParam, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;

pub const MCP_INVOKE_META_FUNCTION_NAME_PREFIX: &str = "mcp_invoke";
pub const MCP_LIST_META_FUNCTION_NAME_PREFIX: &str = "mcp_list";

type ConnectedServer = RunningService<RoleClient, ()>;

#[derive(Debug, Clone, Deserialize)]
struct McpServersConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServer>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpServer {
    command: String,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, JsonField>>,
    cwd: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum JsonField {
    Str(String),
    Bool(bool),
    Int(i64),
}

#[derive(Debug, Clone, Default)]
pub struct McpRegistry {
    log_path: Option<PathBuf>,
    config: Option<McpServersConfig>,
    servers: HashMap<String, Arc<RunningService<RoleClient, ()>>>,
}

impl McpRegistry {
    pub async fn init(
        log_path: Option<PathBuf>,
        start_mcp_servers: bool,
        use_mcp_servers: Option<String>,
        abort_signal: AbortSignal,
        config: &Config,
    ) -> Result<Self> {
        let mut registry = Self {
            log_path,
            ..Default::default()
        };
        if !Config::mcp_config_file().try_exists().with_context(|| {
            format!(
                "Failed to check MCP config file at {}",
                Config::mcp_config_file().display()
            )
        })? {
            debug!(
                "MCP config file does not exist at {}, skipping MCP initialization",
                Config::mcp_config_file().display()
            );
            return Ok(registry);
        }
        let err = || {
            format!(
                "Failed to load MCP config file at {}",
                Config::mcp_config_file().display()
            )
        };
        let content = tokio::fs::read_to_string(Config::mcp_config_file())
            .await
            .with_context(err)?;

        if content.trim().is_empty() {
            debug!("MCP config file is empty, skipping MCP initialization");
            return Ok(registry);
        }

        let (parsed_content, missing_secrets) = interpolate_secrets(&content, &config.vault);

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
        registry.config = Some(mcp_servers_config);

        if start_mcp_servers && config.mcp_servers {
            abortable_run_with_spinner(
                registry.start_select_mcp_servers(use_mcp_servers),
                "Loading MCP servers",
                abort_signal,
            )
            .await?;
        }

        Ok(registry)
    }

    pub async fn reinit(
        registry: McpRegistry,
        use_mcp_servers: Option<String>,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        debug!("Reinitializing MCP registry");
        debug!("Stopping all MCP servers");
        let mut new_registry = abortable_run_with_spinner(
            registry.stop_all_servers(),
            "Stopping MCP servers",
            abort_signal.clone(),
        )
        .await?;

        abortable_run_with_spinner(
            new_registry.start_select_mcp_servers(use_mcp_servers),
            "Loading MCP servers",
            abort_signal,
        )
        .await?;

        Ok(new_registry)
    }

    async fn start_select_mcp_servers(&mut self, use_mcp_servers: Option<String>) -> Result<()> {
        if self.config.is_none() {
            debug!("MCP config is not present; assuming MCP servers are disabled globally. Skipping MCP initialization");
            return Ok(());
        }

        if let Some(servers) = use_mcp_servers {
            debug!("Starting selected MCP servers: {:?}", servers);
            let config = self
                .config
                .as_ref()
                .with_context(|| "MCP Config not defined. Cannot start servers")?;
            let mcp_servers = config.mcp_servers.clone();

            let enabled_servers: HashSet<String> =
                servers.split(',').map(|s| s.trim().to_string()).collect();
            let server_ids: Vec<String> = if servers == "all" {
                mcp_servers.into_keys().collect()
            } else {
                mcp_servers
                    .into_keys()
                    .filter(|id| enabled_servers.contains(id))
                    .collect()
            };

            let results: Vec<(String, Arc<_>)> = stream::iter(
                server_ids
                    .into_iter()
                    .map(|id| async { self.start_server(id).await }),
            )
            .buffer_unordered(num_cpus::get())
            .try_collect()
            .await?;

            self.servers = results.into_iter().collect();
        }

        Ok(())
    }

    async fn start_server(&self, id: String) -> Result<(String, Arc<ConnectedServer>)> {
        let server = self
            .config
            .as_ref()
            .and_then(|c| c.mcp_servers.get(&id))
            .with_context(|| format!("MCP server not found in config: {id}"))?;
        let mut cmd = Command::new(&server.command);
        if let Some(args) = &server.args {
            cmd.args(args);
        }
        if let Some(env) = &server.env {
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
        if let Some(cwd) = &server.cwd {
            cmd.current_dir(cwd);
        }

        let transport = if let Some(log_path) = self.log_path.as_ref() {
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
                .with_context(|| format!("Failed to start MCP server: {}", &server.command))?,
        );
        debug!(
            "Available tools for MCP server {id}: {:?}",
            service.list_tools(None).await?
        );

        info!("Started MCP server: {id}");

        Ok((id.to_string(), service))
    }

    pub async fn stop_all_servers(mut self) -> Result<Self> {
        for (id, server) in self.servers {
            Arc::try_unwrap(server)
                .map_err(|_| anyhow!("Failed to unwrap Arc for MCP server: {id}"))?
                .cancel()
                .await
                .with_context(|| format!("Failed to stop MCP server: {id}"))?;
            info!("Stopped MCP server: {id}");
        }

        self.servers = HashMap::new();

        Ok(self)
    }

    pub fn list_started_servers(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    pub fn list_configured_servers(&self) -> Vec<String> {
        if let Some(config) = &self.config {
            config.mcp_servers.keys().cloned().collect()
        } else {
            vec![]
        }
    }

    pub fn catalog(&self) -> BoxFuture<'static, Result<Value>> {
        let servers: Vec<(String, Arc<ConnectedServer>)> = self
            .servers
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect();

        Box::pin(async move {
            let mut out = Vec::with_capacity(servers.len());
            for (id, server) in servers {
                let tools = server.list_tools(None).await?;
                let resources = server.list_resources(None).await.unwrap_or_default();
                // TODO implement prompt sampling for MCP servers
                // let prompts = server.service.list_prompts(None).await.unwrap_or_default();
                out.push(json!({
                  "server": id,
                  "tools": tools,
                  "resources": resources,
                }));
            }
            Ok(Value::Array(out))
        })
    }

    pub fn invoke(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> BoxFuture<'static, Result<CallToolResult>> {
        let server = self
            .servers
            .get(server)
            .cloned()
            .with_context(|| format!("Invoked MCP server does not exist: {server}"));

        let tool = tool.to_owned();
        Box::pin(async move {
            let server = server?;
            let call_tool_request = CallToolRequestParam {
                name: Cow::Owned(tool.to_owned()),
                arguments: arguments.as_object().cloned(),
            };

            let result = server.call_tool(call_tool_request).await?;
            Ok(result)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}
