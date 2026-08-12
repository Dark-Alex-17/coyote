use crate::rag::provider::RagProvider;
use crate::rag::{DocumentId, RagData};

use anyhow::Result;
use async_trait::async_trait;
use hnsw_rs::prelude::*;
use indexmap::IndexMap;

pub struct YamlProvider {
    hnsw: Hnsw<'static, f32, DistCosine>,
    content_map: IndexMap<DocumentId, String>,
}

impl YamlProvider {
    pub fn from_data(data: &RagData) -> Self {
        Self {
            hnsw: data.build_hnsw(),
            content_map: Self::build_content_map(data),
        }
    }

    fn build_content_map(data: &RagData) -> IndexMap<DocumentId, String> {
        data.iter_documents()
            .map(|(id, doc)| (id, doc.page_content.clone()))
            .collect()
    }
}

#[async_trait]
impl RagProvider for YamlProvider {
    async fn vector_search(
        &self,
        embedding: &[f32],
        top_k: usize,
        min_score: f32,
    ) -> Result<Vec<(DocumentId, f32)>> {
        let results = self
            .hnsw
            .parallel_search(&[embedding.to_vec()], top_k, 30)
            .into_iter()
            .flat_map(|list| {
                list.into_iter().filter_map(|v| {
                    let score = 1.0 - v.distance;
                    if score > min_score {
                        Some((DocumentId(v.d_id), score))
                    } else {
                        None
                    }
                })
            })
            .collect();

        Ok(results)
    }

    async fn fetch_content(&self, ids: &[DocumentId]) -> Result<Vec<(DocumentId, String)>> {
        Ok(ids
            .iter()
            .filter_map(|id| self.content_map.get(id).map(|text| (*id, text.clone())))
            .collect())
    }

    async fn rebuild_indexes(&mut self, data: &RagData, _full_rebuild: bool) -> Result<()> {
        self.hnsw = data.build_hnsw();

        self.content_map = Self::build_content_map(data);

        Ok(())
    }

    fn duplicate(&self, data: &RagData) -> Box<dyn RagProvider> {
        Box::new(YamlProvider::from_data(data))
    }
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    use crate::rag::{RagDocument, RagFile};

    fn minimal_rag_data() -> RagData {
        RagData {
            embedding_model: "text-embedding-3-small".to_string(),
            chunk_size: 1024,
            chunk_overlap: 50,
            top_k: 5,
            driver: "yaml".to_string(),
            attached: false,
            ..Default::default()
        }
    }

    /// Two files, one chunk each, with vectors, the minimum needed to exercise
    /// `build_content_map` and the `fetch_content` ordering contract.
    /// `DocumentId::new(f, d)` packs (file_index, document_index); `RagData::add`
    /// is the real insertion path but a direct literal is sufficient and avoids
    /// the embedding pipeline.
    fn populated_rag_data() -> RagData {
        let mut data = minimal_rag_data();
        // `files` must be populated: build_content_map iterates data.iter_documents(),
        // which enumerates `files`. Populating `vectors` alone would produce an EMPTY
        // content map, and every assertion below would vacuously pass on a broken impl.
        // The vectors inserted at the end are for the HNSW side only.
        data.files.insert(
            0,
            RagFile {
                hash: "h0".to_string(),
                path: "/tmp/a.md".to_string(),
                documents: vec![RagDocument {
                    page_content: "alpha".to_string(),
                    metadata: Default::default(),
                }],
            },
        );
        data.files.insert(
            1,
            RagFile {
                hash: "h1".to_string(),
                path: "/tmp/b.md".to_string(),
                documents: vec![RagDocument {
                    page_content: "beta".to_string(),
                    metadata: Default::default(),
                }],
            },
        );
        data.vectors
            .insert(DocumentId::new(0, 0), vec![1.0, 0.0, 0.0]);
        data.vectors
            .insert(DocumentId::new(1, 0), vec![0.0, 1.0, 0.0]);
        data
    }

    #[tokio::test]
    async fn yaml_provider_empty_data_returns_nothing() {
        let data = minimal_rag_data();
        let provider = YamlProvider::from_data(&data);

        let results = provider.fetch_content(&[]).await.unwrap();

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn yaml_provider_fetch_content_preserves_input_order() {
        let data = populated_rag_data();
        let provider = YamlProvider::from_data(&data);

        let a = DocumentId::new(0, 0);
        let b = DocumentId::new(1, 0);

        let forward = provider.fetch_content(&[a, b]).await.unwrap();
        assert_eq!(forward.len(), 2, "both documents must resolve");
        assert_eq!(forward[0].1, "alpha");
        assert_eq!(forward[1].1, "beta");

        let reversed = provider.fetch_content(&[b, a]).await.unwrap();

        assert_eq!(
            reversed[0].1, "beta",
            "fetch_content must honor input order"
        );
        assert_eq!(reversed[1].1, "alpha");
    }

    #[tokio::test]
    async fn yaml_provider_fetch_content_skips_missing_ids() {
        let data = populated_rag_data();
        let provider = YamlProvider::from_data(&data);

        let a = DocumentId::new(0, 0);
        let missing = DocumentId::new(99, 0);
        let b = DocumentId::new(1, 0);

        let out = provider.fetch_content(&[a, missing, b]).await.unwrap();
        assert_eq!(out.len(), 2, "missing id is skipped, not an error");
        assert_eq!(out[0].1, "alpha");
        assert_eq!(out[1].1, "beta");
    }

    #[tokio::test]
    async fn yaml_provider_duplicate_returns_equivalent_content() {
        let data = populated_rag_data();
        let provider = YamlProvider::from_data(&data);
        let dup = provider.duplicate(&data);
        let ids = [DocumentId::new(0, 0), DocumentId::new(1, 0)];

        let r1 = provider.fetch_content(&ids).await.unwrap();
        let r2 = dup.fetch_content(&ids).await.unwrap();

        assert_eq!(r1.len(), 2, "fixture must resolve both documents");
        assert_eq!(
            r1, r2,
            "duplicate must resolve the same content as the original"
        );
    }

    #[tokio::test]
    async fn yaml_provider_content_is_keyed_on_files_not_vectors() {
        let mut data = populated_rag_data();
        let orphan = DocumentId::new(9, 0);
        data.vectors.insert(orphan, vec![0.0, 0.0, 1.0]);

        let provider = YamlProvider::from_data(&data);

        let out = provider.fetch_content(&[orphan]).await.unwrap();
        assert!(
            out.is_empty(),
            "an id present only in `vectors` must not resolve to content"
        );

        let real = provider
            .fetch_content(&[DocumentId::new(0, 0), DocumentId::new(1, 0)])
            .await
            .unwrap();
        assert_eq!(real.len(), 2, "file-backed documents must still resolve");
        assert_eq!(real[0].1, "alpha");
        assert_eq!(real[1].1, "beta");
    }
}
