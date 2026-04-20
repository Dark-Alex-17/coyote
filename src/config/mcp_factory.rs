//! Per-process factory for MCP subprocess handles.
//!
//! `McpFactory` lives on [`AppState`](super::AppState) and is the
//! single entrypoint that scopes use to obtain `Arc<ConnectedServer>`
//! handles for MCP tool servers. Multiple scopes requesting the same
//! server can (eventually) share a single subprocess via `Arc`
//! reference counting.
//!
//! # Phase 1 Step 6.5 scope
//!
//! This file introduces the factory scaffolding with a trivial
//! implementation:
//!
//! * `active` — `Mutex<HashMap<McpServerKey, Weak<ConnectedServer>>>`
//!   for future Arc-based sharing across scopes
//! * `acquire` — unimplemented stub for now; will be filled in when
//!   Step 8 rewrites `use_role` / `use_session` / `use_agent` to
//!   actually build `ToolScope`s
//!
//! The full design (idle pool, reaper task, per-server TTL, health
//! checks, graceful shutdown) lands in **Phase 5** per
//! `docs/PHASE-5-IMPLEMENTATION-PLAN.md`. Phase 1 Step 6.5 ships just
//! enough for the type to exist on `AppState` and participate in
//! construction / test round-trips.
//!
//! The key type `McpServerKey` hashes the server name plus its full
//! command/args/env so that two scopes requesting an identically-
//! configured server share an `Arc`, while two scopes requesting
//! differently-configured servers (e.g., different API tokens) get
//! independent subprocesses. This is the sharing-vs-isolation property
//! described in `docs/REST-API-ARCHITECTURE.md` section 5.

use crate::mcp::{ConnectedServer, JsonField, McpServer, spawn_mcp_server};

use anyhow::Result;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct McpServerKey {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl McpServerKey {
    pub fn new(
        name: impl Into<String>,
        command: impl Into<String>,
        args: impl IntoIterator<Item = String>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let mut args: Vec<String> = args.into_iter().collect();
        args.sort();
        let mut env: Vec<(String, String)> = env.into_iter().collect();
        env.sort();
        Self {
            name: name.into(),
            command: command.into(),
            args,
            env,
        }
    }

    pub fn from_spec(name: &str, spec: &McpServer) -> Self {
        let args = spec.args.clone().unwrap_or_default();
        let env: Vec<(String, String)> = spec
            .env
            .as_ref()
            .map(|e| {
                e.iter()
                    .map(|(k, v)| {
                        let v_str = match v {
                            JsonField::Str(s) => s.clone(),
                            JsonField::Bool(b) => b.to_string(),
                            JsonField::Int(i) => i.to_string(),
                        };
                        (k.clone(), v_str)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self::new(name, &spec.command, args, env)
    }
}

#[derive(Default)]
pub struct McpFactory {
    active: Mutex<HashMap<McpServerKey, Weak<ConnectedServer>>>,
}

impl McpFactory {
    pub fn try_get_active(&self, key: &McpServerKey) -> Option<Arc<ConnectedServer>> {
        let map = self.active.lock();
        map.get(key).and_then(|weak| weak.upgrade())
    }

    pub fn insert_active(&self, key: McpServerKey, handle: &Arc<ConnectedServer>) {
        let mut map = self.active.lock();
        map.insert(key, Arc::downgrade(handle));
    }

    pub async fn acquire(
        &self,
        name: &str,
        spec: &McpServer,
        log_path: Option<&Path>,
    ) -> Result<Arc<ConnectedServer>> {
        let key = McpServerKey::from_spec(name, spec);

        if let Some(existing) = self.try_get_active(&key) {
            return Ok(existing);
        }

        let handle = spawn_mcp_server(spec, log_path).await?;
        self.insert_active(key, &handle);
        Ok(handle)
    }
}
