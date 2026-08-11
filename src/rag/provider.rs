use super::{DocumentId, RagData};
use anyhow::Result;
use async_trait::async_trait;

/// Abstracts where RAG vector data is stored and queried.
///
/// The Rag orchestrator owns: embeddings, chunking, BM25 keyword search, graph RAG,
/// entity extraction, RRF merging. Providers own: vector storage and content retrieval.
#[async_trait]
pub trait RagProvider: Send + Sync {
    /// Vector similarity search. Returns (DocumentId, score) sorted by score desc.
    /// `embedding` is a single query vector from Coyote's embedding model.
    async fn vector_search(
        &self,
        embedding: &[f32],
        top_k: usize,
        min_score: f32,
    ) -> Result<Vec<(DocumentId, f32)>>;

    /// Resolve document IDs to their page content.
    ///
    /// **Ordering contract:** implementations MUST return results in the same
    /// relative order as the input `ids` slice. `hybrid_search` passes an
    /// RRF-ranked list and feeds the result straight to the LLM. A provider
    /// that returns rows in storage order (e.g. Qdrant `get_points`, DuckDB
    /// `WHERE id IN (...)`) would silently discard the ranking. Implementations
    /// that query an unordered backend must re-sort by input position before
    /// returning.
    ///
    /// Returns only IDs that were found; callers must handle partial returns
    /// (a missing ID is skipped, not an error).
    /// YamlProvider: reads from an in-memory content map built from data.files.
    /// DuckDbProvider: queries the documents table by id.
    /// QdrantProvider: fetches payload from the remote collection.
    async fn fetch_content(&self, ids: &[DocumentId]) -> Result<Vec<(DocumentId, String)>>;

    /// Rebuild internal indexes from freshly updated RagData.
    /// Called once at the end of every sync_documents pass.
    ///
    /// `full_rebuild` mirrors `sync_documents`' `refresh` parameter:
    ///   - `true`: a full re-index (`.rebuild rag`, `--rebuild-rag`, initial build).
    ///     Destructive strategies (wipe-then-reindex) are permitted.
    ///   - `false`: an incremental change (`.edit rag-docs` adding/removing a file).
    ///     Implementations MUST NOT wipe existing state; upsert only.
    ///
    /// The parameter is part of the signature from the outset so it is fixed
    /// while there is exactly one implementor. Yaml/DuckDb ignore it,
    /// rebuilding their local state wholesale is fast and always correct.
    /// Only a remote provider is destructive enough to care.
    async fn rebuild_indexes(&mut self, data: &RagData, full_rebuild: bool) -> Result<()>;

    /// Keyword / full-text search. Returns (DocumentId, BM25-style score) sorted desc.
    ///
    /// Default impl returns `Ok(vec![])`. Callers fall back to `Rag.bm25` (local in-memory
    /// BM25 built from `data.files`).
    ///
    /// Callers check `has_native_keyword_search()` before deciding which path to take:
    ///   - true  → call this method; skip `Rag.bm25`
    ///   - false → call `Rag.keyword_search()` which uses `Rag.bm25` (sync, infallible)
    async fn keyword_search(&self, query: &str, top_k: usize) -> Result<Vec<(DocumentId, f32)>> {
        let _ = (query, top_k);

        Ok(vec![])
    }

    /// Returns true if this provider implements a native keyword-search index.
    /// When false, `Rag.hybrid_search` uses the local `Rag.bm25` field instead.
    fn has_native_keyword_search(&self) -> bool {
        false
    }

    /// Deep-clone the provider with fresh indexes derived from `data`.
    /// Required because Box<dyn RagProvider> is not Clone.
    /// Called by Rag's Clone impl (which clones before mutating in rebuild_rag/edit_rag_docs).
    fn duplicate(&self, data: &RagData) -> Box<dyn RagProvider>;
}
