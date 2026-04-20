//! Per-process RAG instance cache with weak-reference sharing.
//!
//! `RagCache` lives on [`AppState`](super::AppState) and serves both
//! standalone RAGs (attached via `.rag <name>`) and agent-owned RAGs
//! (loaded from an agent's `documents:` field). The cache keys with
//! [`RagKey`] so that agent RAGs and standalone RAGs occupy distinct
//! namespaces even if they share a name.
//!
//! Entries are held as `Weak<Rag>` so the cache never keeps a RAG
//! alive on its own — once all active scopes drop their `Arc<Rag>`,
//! the cache entry becomes unupgradable and the next `load()` falls
//! through to a fresh disk read.
//!
//! # Phase 1 Step 6.5 scope
//!
//! This file introduces the type scaffolding. Actual cache population
//! (i.e., routing `use_rag`, `use_agent`, and sub-agent spawning
//! through the cache) is deferred to Step 8 when the entry points get
//! rewritten. During the bridge window, `Config.rag` keeps serving
//! today's callers via direct `Rag::load` / `Rag::init` calls and
//! `RagCache` sits on `AppState` as an unused-but-ready service.
//!
//! See `docs/REST-API-ARCHITECTURE.md` section 5 ("RAG Cache") for
//! the full design including concurrent first-load serialization and
//! invalidation semantics.

use crate::rag::Rag;

use anyhow::Result;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RagKey {
    Named(String),
    Agent(String),
}

#[derive(Default)]
pub struct RagCache {
    entries: RwLock<HashMap<RagKey, Weak<Rag>>>,
}

impl RagCache {
    pub fn try_get(&self, key: &RagKey) -> Option<Arc<Rag>> {
        let map = self.entries.read();
        map.get(key).and_then(|weak| weak.upgrade())
    }

    pub fn insert(&self, key: RagKey, rag: &Arc<Rag>) {
        let mut map = self.entries.write();
        map.insert(key, Arc::downgrade(rag));
    }

    pub fn invalidate(&self, key: &RagKey) {
        let mut map = self.entries.write();
        map.remove(key);
    }

    pub async fn load_with<F, Fut>(&self, key: RagKey, loader: F) -> Result<Arc<Rag>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Rag>>,
    {
        if let Some(existing) = self.try_get(&key) {
            return Ok(existing);
        }
        let rag = loader().await?;
        let arc = Arc::new(rag);
        self.insert(key, &arc);
        Ok(arc)
    }
}
