//! Per-scope tool runtime: resolved functions + live MCP handles +
//! call tracker.
//!
//! `ToolScope` is the unit of tool availability for a single request.
//! Every active `RoleLike` (role, session, agent) conceptually owns one.
//! The contents are:
//!
//! * `functions` — the `Functions` declarations visible to the LLM for
//!   this scope (global tools + role/session/agent filters applied)
//! * `mcp_runtime` — live MCP subprocess handles for the servers this
//!   scope has enabled, keyed by server name
//! * `tool_tracker` — per-scope tool call history for auto-continuation
//!   and looping detection
//!
//! # Phase 1 Step 6.5 scope
//!
//! This file introduces the type scaffolding. Scope transitions
//! (`use_role`, `use_session`, `use_agent`, `exit_*`) that actually
//! build and swap `ToolScope` instances are deferred to Step 8 when
//! the entry points (`main.rs`, `repl/mod.rs`) get rewritten to thread
//! `RequestContext` through the pipeline. During the bridge window,
//! `Config.functions` / `Config.mcp_registry` keep serving today's
//! callers and `ToolScope` sits alongside them on `RequestContext` as
//! an unused (but compiling and tested) parallel structure.
//!
//! The fields mirror the plan in `docs/REST-API-ARCHITECTURE.md`
//! section 5 and `docs/PHASE-1-IMPLEMENTATION-PLAN.md` Step 6.5.

use crate::function::{Functions, ToolCallTracker};
use crate::mcp::{CatalogItem, ConnectedServer, McpRegistry};

use anyhow::{Context, Result, anyhow};
use bm25::{Document, Language, SearchEngineBuilder};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

pub struct ToolScope {
    pub functions: Functions,
    pub mcp_runtime: McpRuntime,
    pub tool_tracker: ToolCallTracker,
}

impl Default for ToolScope {
    fn default() -> Self {
        Self {
            functions: Functions::default(),
            mcp_runtime: McpRuntime::default(),
            tool_tracker: ToolCallTracker::default(),
        }
    }
}

#[derive(Default)]
pub struct McpRuntime {
    pub servers: HashMap<String, Arc<ConnectedServer>>,
}

impl McpRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn insert(&mut self, name: String, handle: Arc<ConnectedServer>) {
        self.servers.insert(name, handle);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<ConnectedServer>> {
        self.servers.get(name)
    }

    pub fn server_names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    pub fn sync_from_registry(&mut self, registry: &McpRegistry) {
        self.servers.clear();
        for (name, handle) in registry.running_servers() {
            self.servers.insert(name.clone(), Arc::clone(handle));
        }
    }

    async fn catalog_items(&self, server: &str) -> Result<HashMap<String, CatalogItem>> {
        let server_handle = self
            .get(server)
            .cloned()
            .with_context(|| format!("{server} MCP server not found in runtime"))?;
        let tools = server_handle.list_tools(None).await?;
        let mut items = HashMap::new();

        for tool in tools.tools {
            let item = CatalogItem {
                name: tool.name.to_string(),
                server: server.to_string(),
                description: tool.description.unwrap_or_default().to_string(),
            };
            items.insert(item.name.clone(), item);
        }

        Ok(items)
    }

    pub async fn search(
        &self,
        server: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<CatalogItem>> {
        let items = self.catalog_items(server).await?;
        let docs = items.values().map(|item| Document {
            id: item.name.clone(),
            contents: format!(
                "{}\n{}\nserver:{}",
                item.name, item.description, item.server
            ),
        });
        let engine = SearchEngineBuilder::<String>::with_documents(Language::English, docs).build();

        Ok(engine
            .search(query, top_k.min(20))
            .into_iter()
            .filter_map(|result| items.get(&result.document.id))
            .take(top_k)
            .cloned()
            .collect())
    }

    pub async fn describe(&self, server: &str, tool: &str) -> Result<Value> {
        let server_handle = self
            .get(server)
            .cloned()
            .with_context(|| format!("{server} MCP server not found in runtime"))?;

        let tool_schema = server_handle
            .list_tools(None)
            .await?
            .tools
            .into_iter()
            .find(|item| item.name == tool)
            .ok_or_else(|| anyhow!("{tool} not found in {server} MCP server catalog"))?
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

    pub async fn invoke(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<CallToolResult> {
        let server_handle = self
            .get(server)
            .cloned()
            .with_context(|| format!("Invoked MCP server does not exist: {server}"))?;

        let request = CallToolRequestParams {
            name: Cow::Owned(tool.to_owned()),
            arguments: arguments.as_object().cloned(),
            meta: None,
            task: None,
        };

        server_handle.call_tool(request).await.map_err(Into::into)
    }
}
