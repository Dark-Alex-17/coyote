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
