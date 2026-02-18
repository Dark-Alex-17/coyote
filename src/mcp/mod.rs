use crate::config::Config;
use crate::utils::{AbortSignal, abortable_run_with_spinner};
use crate::vault::interpolate_secrets;
use anyhow::{Context, Result, anyhow};
use bm25::{Document, Language, SearchEngine, SearchEngineBuilder};
use futures_util::future::BoxFuture;
use futures_util::{StreamExt, TryStreamExt, stream};
use indoc::formatdoc;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;

pub const MCP_INVOKE_META_FUNCTION_NAME_PREFIX: &str = "mcp_invoke";
pub const MCP_SEARCH_META_FUNCTION_NAME_PREFIX: &str = "mcp_search";
pub const MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX: &str = "mcp_describe";

type ConnectedServer = RunningService<RoleClient, ()>;

#[derive(Clone, Debug, Default, Serialize)]
pub struct CatalogItem {
    pub name: String,
    pub server: String,
    pub description: String,
}

#[derive(Debug)]
struct ServerCatalog {
    engine: SearchEngine<String>,
    items: HashMap<String, CatalogItem>,
}

impl ServerCatalog {
    pub fn build_bm25(items: &HashMap<String, CatalogItem>) -> SearchEngine<String> {
        let docs = items.values().map(|it| {
            let contents = format!("{}\n{}\nserver:{}", it.name, it.description, it.server);
            Document {
                id: it.name.clone(),
                contents,
            }
        });
        SearchEngineBuilder::<String>::with_documents(Language::English, docs).build()
    }
}

impl Clone for ServerCatalog {
    fn clone(&self) -> Self {
        Self {
            engine: Self::build_bm25(&self.items),
            items: self.items.clone(),
        }
    }
}

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
    servers: HashMap<String, Arc<ConnectedServer>>,
    catalogs: HashMap<String, ServerCatalog>,
}

impl McpRegistry {
    pub async fn init(
        log_path: Option<PathBuf>,
        start_mcp_servers: bool,
        enabled_mcp_servers: Option<String>,
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

        if start_mcp_servers && config.mcp_server_support {
            abortable_run_with_spinner(
                registry.start_select_mcp_servers(enabled_mcp_servers),
                "Loading MCP servers",
                abort_signal,
            )
            .await?;
        }

        Ok(registry)
    }

    pub async fn reinit(
        registry: McpRegistry,
        enabled_mcp_servers: Option<String>,
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
            new_registry.start_select_mcp_servers(enabled_mcp_servers),
            "Loading MCP servers",
            abort_signal,
        )
        .await?;

        Ok(new_registry)
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

        if let Some(servers) = enabled_mcp_servers {
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

            let results: Vec<(String, Arc<_>, ServerCatalog)> = stream::iter(
                server_ids
                    .into_iter()
                    .map(|id| async { self.start_server(id).await }),
            )
            .buffer_unordered(num_cpus::get())
            .try_collect()
            .await?;

            self.servers = results
                .clone()
                .into_iter()
                .map(|(id, server, _)| (id, server))
                .collect();
            self.catalogs = results
                .into_iter()
                .map(|(id, _, catalog)| (id, catalog))
                .collect();
        }

        Ok(())
    }

    async fn start_server(
        &self,
        id: String,
    ) -> Result<(String, Arc<ConnectedServer>, ServerCatalog)> {
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

        let catalog = ServerCatalog {
            engine: ServerCatalog::build_bm25(&items_map),
            items: items_map,
        };

        info!("Started MCP server: {id}");

        Ok((id.to_string(), service, catalog))
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

    pub fn search_tools_server(&self, server: &str, query: &str, top_k: usize) -> Vec<CatalogItem> {
        let Some(catalog) = self.catalogs.get(server) else {
            return vec![];
        };
        let engine = &catalog.engine;
        let raw = engine.search(query, top_k.min(20));

        raw.into_iter()
            .filter_map(|r| catalog.items.get(&r.document.id))
            .take(top_k)
            .cloned()
            .collect()
    }

    pub async fn describe(&self, server_id: &str, tool: &str) -> Result<Value> {
        let server = self
            .servers
            .iter()
            .filter(|(id, _)| &server_id == id)
            .map(|(_, s)| s.clone())
            .next()
            .ok_or(anyhow!("{server_id} MCP server not found in config"))?;

        let tool_schema = server
            .list_tools(None)
            .await?
            .tools
            .into_iter()
            .find(|it| it.name == tool)
            .ok_or(anyhow!(
                "{tool} not found in {server_id} MCP server catalog"
            ))?
            .input_schema;
        Ok(json!({
            "type": "object",
            "properties": {
                "tool": {
                    "type": "string",
                },
                "arguments": tool_schema
            }
        }))
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
            let call_tool_request = CallToolRequestParams {
                name: Cow::Owned(tool.to_owned()),
                arguments: arguments.as_object().cloned(),
                meta: None,
                task: None,
            };

            let result = server.call_tool(call_tool_request).await?;
            Ok(result)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}
