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

use super::mcp_factory::McpFactory;
use super::rag_cache::RagCache;
use crate::config::AppConfig;
use crate::mcp::McpServersConfig;
use crate::vault::GlobalVault;

use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub vault: GlobalVault,
    pub mcp_factory: Arc<McpFactory>,
    #[allow(dead_code)]
    pub rag_cache: Arc<RagCache>,
    pub mcp_config: Option<McpServersConfig>,
    pub mcp_log_path: Option<PathBuf>,
}
