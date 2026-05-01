//! Shared global services for a running Loki process.
//!
//! `AppState` holds the services that are genuinely process-wide and
//! immutable during request handling: the frozen [`AppConfig`], the
//! credential [`Vault`](GlobalVault), the [`McpFactory`](super::mcp_factory::McpFactory)
//! for MCP subprocess sharing, the [`RagCache`](super::rag_cache::RagCache)
//! for shared RAG instances, the global MCP registry, and the base
//! [`Functions`] declarations seeded into per-request `ToolScope`s. It
//! is wrapped in `Arc` and shared across every [`RequestContext`] that
//! a frontend (CLI, REPL, API) creates.
//!
//! Built via [`AppState::init`] from an `Arc<AppConfig>` plus
//! startup context (log path, MCP-start flag, abort signal). The
//! `init` call is the single place that wires the vault, MCP
//! registry, and global functions together.

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
    #[cfg(test)]
    pub fn test_default() -> Self {
        Self {
            config: Arc::new(AppConfig::default()),
            vault: Arc::new(Vault::default()),
            mcp_factory: Arc::new(McpFactory::default()),
            rag_cache: Arc::new(RagCache::default()),
            mcp_config: None,
            mcp_log_path: None,
            mcp_registry: None,
            functions: Functions::default(),
        }
    }

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
