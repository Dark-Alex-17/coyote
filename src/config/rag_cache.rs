use crate::rag::Rag;

use anyhow::Result;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RagKey {
    Named(String),
    Agent(String),
    GraphNode { agent: String, node: String },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::hooks::test_sink;
    use crate::rag::RagData;
    use anyhow::anyhow;
    use serial_test::serial;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A cache hit must never reach the loader: the loader closure is where
    /// a real RAG build (and its `rag.sync.*` hook dispatch) happens, so
    /// returning the cached entry is what keeps warm-cache lookups silent.
    #[tokio::test]
    #[serial]
    async fn cache_hit_returns_the_cached_rag_without_running_the_loader() {
        let _sink = test_sink::install();
        test_sink::drain();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("coyote-rag-cache-hit-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kb.yaml");
        let data = RagData {
            driver: "yaml".to_string(),
            // The only embedding model resolvable in tests; see
            // `seed_model_registries` in the request_context tests.
            embedding_model: "test-seeded:test-embedder".to_string(),
            chunk_size: 1000,
            chunk_overlap: 100,
            top_k: 5,
            ..Default::default()
        };
        std::fs::write(&path, serde_yaml::to_string(&data).unwrap()).unwrap();
        let rag = Arc::new(Rag::load(&AppConfig::default(), "kb", &path).await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);

        let cache = RagCache::default();
        let key = RagKey::Named("kb".to_string());
        cache.insert(key.clone(), &rag);

        let loaded = cache
            .load_with(key, || async {
                Err(anyhow!("a cache hit must not rebuild"))
            })
            .await
            .unwrap();

        assert!(Arc::ptr_eq(&loaded, &rag));
        assert!(
            test_sink::drain().is_empty(),
            "a cache hit must dispatch no hooks"
        );
    }
}
