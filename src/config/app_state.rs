//! Shared global services for a running Loki process.
//!
//! `AppState` holds the services that are genuinely process-wide and
//! immutable during request handling: the frozen [`AppConfig`], the
//! credential [`Vault`](GlobalVault), the [`McpFactory`](super::mcp_factory::McpFactory)
//! for MCP subprocess sharing, and the [`RagCache`](super::rag_cache::RagCache)
//! for shared RAG instances. It is intended to be wrapped in `Arc`
//! and shared across every [`RequestContext`] that a frontend (CLI,
//! REPL, API) creates.
//!
//! This struct deliberately does **not** hold a live `McpRegistry`.
//! MCP server processes are scoped to whichever `RoleLike`
//! (role/session/agent) is currently active, because each scope may
//! demand a different enabled server set. Live MCP processes are
//! owned by per-scope
//! [`ToolScope`](super::tool_scope::ToolScope)s on the
//! [`RequestContext`] and acquired through `McpFactory`.
//!
//! # Phase 1 scope
//!
//! This is Phase 1 of the REST API refactor:
//!
//! * **Step 0** introduced this struct alongside the existing
//!   [`Config`](super::Config)
//! * **Step 6.5** added the `mcp_factory` and `rag_cache` fields
//!
//! Neither field is wired into the runtime yet — they exist as
//! additive scaffolding that Step 8+ will connect when the entry
//! points migrate. See `docs/PHASE-1-IMPLEMENTATION-PLAN.md`.

use super::mcp_factory::{McpFactory, McpServerKey};
use super::rag_cache::RagCache;
use crate::config::AppConfig;
use crate::function::Functions;
use crate::mcp::{McpRegistry, McpServersConfig};
use crate::utils::AbortSignal;
use crate::vault::{GlobalVault, Vault};

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub vault: GlobalVault,
    pub mcp_factory: Arc<McpFactory>,
    pub rag_cache: Arc<RagCache>,
    pub mcp_config: Option<McpServersConfig>,
    pub mcp_log_path: Option<PathBuf>,
    pub mcp_registry: Option<Arc<McpRegistry>>,
    pub functions: Functions,
}

impl AppState {
    pub async fn init(
        config: Arc<AppConfig>,
        log_path: Option<PathBuf>,
        start_mcp_servers: bool,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let vault = Arc::new(Vault::init(&config));

        let mcp_registry = McpRegistry::init(
            log_path,
            start_mcp_servers,
            config.enabled_mcp_servers.clone(),
            abort_signal,
            &config,
            &vault,
        )
        .await?;

        let mcp_config = mcp_registry.mcp_config().cloned();
        let mcp_log_path = mcp_registry.log_path().cloned();

        let mcp_factory = Arc::new(McpFactory::default());
        if let Some(mcp_servers_config) = &mcp_config {
            for (id, handle) in mcp_registry.running_servers() {
                if let Some(spec) = mcp_servers_config.mcp_servers.get(id) {
                    let key = McpServerKey::from_spec(id, spec);
                    mcp_factory.insert_active(key, handle);
                }
            }
        }

        let mut functions = Functions::init(config.visible_tools.as_ref().unwrap_or(&Vec::new()))?;
        if !mcp_registry.is_empty() && config.mcp_server_support {
            functions.append_mcp_meta_functions(mcp_registry.list_started_servers());
        }

        let mcp_registry = if mcp_registry.is_empty() {
            None
        } else {
            Some(Arc::new(mcp_registry))
        };

        Ok(Self {
            config,
            vault,
            mcp_factory,
            rag_cache: Arc::new(RagCache::default()),
            mcp_config,
            mcp_log_path,
            mcp_registry,
            functions,
        })
    }
}
