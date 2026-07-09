# Graph RAG Design Spec

## Status: COMPLETE

### Verified From Code (all claims backed by actual file reads)

---

## Goal

Extend the existing two-signal hybrid search (vector HNSW + BM25 → RRF) to a three-signal hybrid
(vector + BM25 + knowledge graph → RRF). The graph captures entity/relationship knowledge extracted
from documents at ingestion time via an LLM call per chunk. At query time, graph traversal expands
context beyond semantic similarity.

---

## Verified Current Architecture

### `Rag` struct (`src/rag/mod.rs:48`)
```rust
pub struct Rag {
    app_config: Arc<AppConfig>,
    name: String,
    path: String,
    embedding_model: Model,
    hnsw: Hnsw<'static, f32, DistCosine>,  // ephemeral, rebuilt on load
    bm25: SearchEngine<DocumentId>,          // ephemeral, rebuilt on load
    data: RagData,                           // serialized to YAML
    last_sources: RwLock<Option<String>>,
}
```

### `RagData` struct (`src/rag/mod.rs:892`)
```rust
pub struct RagData {
    pub embedding_model: String,
    pub chunk_size: usize,
    pub chunk_overlap: usize,
    pub reranker_model: Option<String>,
    pub top_k: usize,
    pub batch_size: Option<usize>,
    pub next_file_id: FileId,
    pub document_paths: Vec<String>,
    pub files: IndexMap<FileId, RagFile>,
    #[serde(with = "serde_vectors")]
    pub vectors: IndexMap<DocumentId, Vec<f32>>,
}
```

### `RagData::new` callers (both need updating):
1. `Rag::init` (`src/rag/mod.rs:219`) — interactive init path
2. `Rag::resolve_init_data` (`src/rag/mod.rs:195`) — config-driven init path

### `Rag::create` (`src/rag/mod.rs:253`) — all init paths converge here:
```rust
pub fn create(app: &AppConfig, name: &str, path: &Path, data: RagData) -> Result<Self> {
    let hnsw = data.build_hnsw();
    let bm25 = data.build_bm25();
    let embedding_model = Model::retrieve_model(app, &data.embedding_model, ModelType::Embedding)?;
    let rag = Rag { app_config: Arc::new(app.clone()), name: name.to_string(),
                    path: path.display().to_string(), data, embedding_model, hnsw, bm25,
                    last_sources: RwLock::new(None) };
    Ok(rag)
}
```

### `hybrid_search` (`src/rag/mod.rs:710`)
```rust
async fn hybrid_search(&self, query: &str, top_k: usize, rerank_model: Option<&str>)
    -> Result<Vec<(DocumentId, String)>>
```
Runs `vector_search` + `keyword_search` in parallel via `tokio::join!`, then either reranks or
applies `reciprocal_rank_fusion(vec![vector_ids, keyword_ids], vec![1.125, 1.0], top_k)`.

### `reciprocal_rank_fusion` (`src/rag/mod.rs:1186`) — standalone fn, already weight-parameterized:
```rust
fn reciprocal_rank_fusion(
    list_of_document_ids: Vec<Vec<DocumentId>>,
    list_of_weights: Vec<f32>,
    top_k: usize,
) -> Vec<DocumentId>
```

### `RagData::del` (`src/rag/mod.rs:953`):
```rust
pub fn del(&mut self, file_ids: Vec<FileId>) {
    for file_id in file_ids {
        if let Some(file) = self.files.swap_remove(&file_id) {
            for (document_index, _) in file.documents.iter().enumerate() {
                let document_id = DocumentId::new(file_id, document_index);
                self.vectors.swap_remove(&document_id);
            }
        }
    }
}
```

### `RagNode` (`src/graph/types.rs:331`):
```rust
pub struct RagNode {
    pub documents: Vec<String>,
    pub query: Option<String>,
    pub top_k: Option<usize>,
    pub embedding_model: Option<String>,
    pub chunk_size: Option<usize>,
    pub chunk_overlap: Option<usize>,
    pub reranker_model: Option<String>,
    pub batch_size: Option<usize>,
    pub state_updates: Option<HashMap<String, String>>,
    pub timeout: Option<u64>,
}
```

### `Client` trait (`src/client/common.rs:40`):
- `async fn chat_completions(&self, input: Input) -> Result<ChatCompletionsOutput>` — needs `Input`
- `async fn chat_completions_inner(&self, client: &ReqwestClient, data: ChatCompletionsData) -> Result<ChatCompletionsOutput>` — accessible on `Box<dyn Client>` via vtable
- `async fn embeddings(&self, data: &EmbeddingsData) -> Result<Vec<Vec<f32>>>`
- `async fn rerank(&self, data: &RerankData) -> Result<RerankOutput>`
- `fn build_client(&self) -> Result<ReqwestClient>`
- `fn model(&self) -> &Model`

**Key finding**: `Input` cannot be constructed without `RequestContext` (which `Rag` doesn't have).
Instead, `extract_entities` uses `chat_completions_inner` directly with manually built
`ChatCompletionsData`. This is accessible via `Box<dyn Client>`.

### `Message` (`src/client/message.rs:22`):
```rust
pub fn new(role: MessageRole, content: MessageContent) -> Self
```
`MessageRole::User`, `MessageContent::Text(String)` — both confirmed.

### `AppConfig` RAG fields (`src/config/app_config.rs:71`):
```rust
pub rag_embedding_model: Option<String>,
pub rag_reranker_model: Option<String>,
pub rag_top_k: usize,        // default: 5
pub rag_chunk_size: Option<usize>,
pub rag_chunk_overlap: Option<usize>,
pub rag_template: Option<String>,
```

### `patch_messages` — confirmed exported from `crate::client::*` (used in `input.rs:5`)

### `init_client(app_config, model)` — works for any `ModelType`, including `Chat`

### `ModelType` variants: `Chat`, `Embedding`, `Reranker` (confirmed in `model.rs`)

### petgraph serde: `NodeIndex` serializes as inner `u32`; `StableGraph` preserves index positions
through roundtrip. `IndexMap<DocumentId, Vec<NodeIndex>>` safe for YAML (DocumentId is newtype over
usize, serializes as integer key).

---

## New Dependency

```toml
petgraph = { version = "0.7", features = ["serde-1"] }
```

---

## New File: `src/rag/graph.rs`

All graph types and extraction logic. Module declared in `mod.rs` as `mod graph; use self::graph::*;`.

### Types:
- `Entity { name: String, entity_type: String, description: Option<String> }`
- `Relationship { relation_type: String, weight: f32 }`
- `ExtractionResult { entities: Vec<ExtractedEntity>, relationships: Vec<ExtractedRelationship> }`
- `ExtractedEntity { name: String, r#type: String, description: Option<String> }`
- `ExtractedRelationship { from: String, to: String, r#type: String, weight: Option<f32> }`
- `KnowledgeGraph { graph: StableGraph<Entity, Relationship>, entity_index: IndexMap<String, NodeIndex>, document_entities: IndexMap<DocumentId, Vec<NodeIndex>> }`

### Key methods on `KnowledgeGraph`:
- `merge(doc_id: DocumentId, result: ExtractionResult)` — merges extraction into graph
- `remove_documents(ids: &[DocumentId])` — removes entities exclusive to deleted documents
- `build_node_to_docs(&self) -> IndexMap<NodeIndex, Vec<DocumentId>>` — ephemeral reverse map

### `extract_entities(client: &dyn Client, chunk: &str) -> Result<ExtractionResult>`:
- Builds `ChatCompletionsData` manually (no `Input` needed)
- Calls `patch_messages` then `client.chat_completions_inner(&reqwest_client, data).await`
- Strips markdown code fences from response before JSON parse
- Temperature: `Some(0.0)` for deterministic extraction

### Extraction prompt: structured JSON output requesting entities + relationships

---

## Changes to `src/rag/mod.rs`

### `Rag` struct — add one ephemeral field:
```rust
node_to_docs: IndexMap<NodeIndex, Vec<DocumentId>>,  // ephemeral, rebuilt on load
```

### `Rag::create` — build node_to_docs before moving data:
```rust
let node_to_docs = data.knowledge_graph.build_node_to_docs();
// then add to struct literal
```

### `Rag` Clone impl — add:
```rust
node_to_docs: self.data.knowledge_graph.build_node_to_docs(),
```

### `RagData` struct — three new fields (all `#[serde(default)]` for backward compat):
```rust
#[serde(default)]
pub graph_enabled: bool,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub extractor_model: Option<String>,
#[serde(default)]
pub knowledge_graph: KnowledgeGraph,
```

### `RagData::new` — two new params: `graph_enabled: bool, extractor_model: Option<String>`

### `RagData::del` — collect doc_ids during existing loop, call `remove_documents` at end:
```rust
let mut doc_ids_to_remove = vec![];
for file_id in file_ids {
    if let Some(file) = self.files.swap_remove(&file_id) {
        for (document_index, _) in file.documents.iter().enumerate() {
            let document_id = DocumentId::new(file_id, document_index);
            self.vectors.swap_remove(&document_id);
            doc_ids_to_remove.push(document_id);
        }
    }
}
self.knowledge_graph.remove_documents(&doc_ids_to_remove);
```

### `Rag::init` (line 219) — add two params to `RagData::new`:
```rust
app.rag_graph_enabled,
app.rag_extractor_model.clone(),
```

### `resolve_init_data` — resolve from config+app, pass to `RagData::new`:
```rust
let graph_enabled = config.graph_enabled.unwrap_or(app.rag_graph_enabled);
let extractor_model = config.extractor_model.clone().or_else(|| app.rag_extractor_model.clone());
```

### `sync_documents` — entity extraction block after `rag_files` built, before embedding:
```rust
if self.data.graph_enabled {
    if let Some(extractor_model_id) = self.data.extractor_model.clone() {
        let model = Model::retrieve_model(&self.app_config, &extractor_model_id, ModelType::Chat)?;
        let client = self.create_embeddings_client(model)?;
        let total_chunks: usize = rag_files.iter().map(|f| f.documents.len()).sum();
        let mut chunk_num = 0;
        let file_offset = next_file_id;
        for (batch_file_idx, rag_file) in rag_files.iter().enumerate() {
            let file_id = file_offset + batch_file_idx;
            for (doc_idx, doc) in rag_file.documents.iter().enumerate() {
                chunk_num += 1;
                progress(&spinner, format!("Extracting entities [{chunk_num}/{total_chunks}]"));
                let doc_id = DocumentId::new(file_id, doc_idx);
                match extract_entities(client.as_ref(), &doc.page_content).await {
                    Ok(result) => self.data.knowledge_graph.merge(doc_id, result),
                    Err(e) => debug!("Entity extraction failed for {doc_id:?}: {e}"),
                }
            }
        }
    }
}
```

### After line 705 (after hnsw/bm25 rebuild in sync_documents):
```rust
self.node_to_docs = self.data.knowledge_graph.build_node_to_docs();
```

### `hybrid_search` — add third signal:
```rust
let graph_search_ids: Vec<DocumentId> = if self.data.graph_enabled
    && !self.data.knowledge_graph.entity_index.is_empty()
{
    self.graph_search(query, &keyword_search_ids, top_k)
} else {
    vec![]
};
// RRF: extend to 3-way when graph has results, fall back to 2-way otherwise
```

### New `graph_search` method (sync):
```rust
fn graph_search(&self, query: &str, bm25_anchor_ids: &[DocumentId], top_k: usize) -> Vec<DocumentId>
```
Phase 1: entity names from query via substring match in `entity_index`.
Phase 2: fallback — entities from top BM25 document chunks.
Phase 3: expand 1-hop neighbors in `StableGraph`.
Phase 4: score docs by entity overlap ratio, return top_k.

### `RagInitConfig` — two new fields:
```rust
pub graph_enabled: Option<bool>,
pub extractor_model: Option<String>,
```

---

## Changes to `src/config/app_config.rs`

New fields alongside existing `rag_*` block:
```rust
pub rag_graph_enabled: bool,            // default: false
pub rag_extractor_model: Option<String>, // default: None
```
Defaults, env var overrides, and propagation all follow the same pattern as existing `rag_*` fields.

---

## Changes to `src/graph/types.rs` — `RagNode`

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub graph_enabled: Option<bool>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub extractor_model: Option<String>,
```

---

## Changes to `src/config/agent.rs`

Pass new fields through to `RagInitConfig`:
```rust
graph_enabled: rag_node.graph_enabled,
extractor_model: rag_node.extractor_model.clone(),
```

---

## Backward Compatibility

- All new `RagData` fields have `#[serde(default)]` — old YAML files load without migration
- `graph_enabled` defaults `false` — existing RAG instances unchanged
- `graph_search_ids` empty → 2-way RRF runs (identical to current behavior)
- `node_to_docs` rebuild on `create()` is O(n) over empty map for old instances

---

## V1 Scope Exclusions

- LLM entity extraction from query at search time (V1 uses substring match + BM25 anchoring)
- Multi-hop traversal (field reserved, 1-hop only in V1)
- Entity embeddings / fuzzy entity lookup
- Bincode for large-corpus graph storage
- Gleaning / multi-pass extraction

---

## Implementation Progress

- [x] Cargo.toml — petgraph dependency
- [x] src/rag/graph.rs — new file
- [x] src/rag/mod.rs — mod/use, Rag struct, create, clone
- [x] src/rag/mod.rs — RagData fields, new, del
- [x] src/rag/mod.rs — Rag::init, resolve_init_data
- [x] src/rag/mod.rs — sync_documents extraction block
- [x] src/rag/mod.rs — hybrid_search + graph_search
- [x] src/rag/mod.rs — RagInitConfig fields
- [x] src/config/app_config.rs — new fields
- [x] src/config/mod.rs — propagation
- [x] src/graph/types.rs — RagNode fields
- [x] src/config/agent.rs — propagation
- [x] cargo check — clean (0 warnings, 1065 tests passing)
