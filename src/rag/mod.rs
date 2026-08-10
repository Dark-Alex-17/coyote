use self::splitter::*;

use crate::client::*;
use crate::config::*;
use crate::utils::*;

mod graph;
mod provider;
mod providers;
mod serde_vectors;
mod splitter;

use self::graph::{KnowledgeGraph, extract_entities};
use self::provider::RagProvider;
// `providers::duckdb_path_from_yaml(path)` is called through the module path in
// `create()`, so `providers` itself must stay in scope — do not collapse it into
// the `use` below.
use self::providers::{DuckDbProvider, YamlProvider};

use anyhow::{Context, Result, anyhow, bail};
use bm25::{Language, SearchEngine, SearchEngineBuilder};
use hnsw_rs::prelude::*;
use indexmap::{IndexMap, IndexSet};
use inquire::{Confirm, Select, Text, required, validator::Validation};
use parking_lot::RwLock;
use petgraph::graph::NodeIndex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    cmp::Ordering, collections::HashMap, env, fmt, fmt::Debug, fs, hash::Hash, path::Path,
    sync::Arc, time::Duration,
};
use tokio::time::sleep;

const BM25_SEED_SCORE: f32 = 0.5;

const RAG_TEMPLATE: &str = r#"Answer the query based on the context while respecting the rules. (user query, some textual context and rules, all inside xml tags)

<context>
__CONTEXT__
</context>

<sources>
__SOURCES__
</sources>

<rules>
- If you don't know, just say so.
- If you are not sure, ask for clarification.
- Answer in the same language as the user query.
- If the context appears unreadable or of poor quality, tell the user then answer as best as you can.
- If the answer is not in the context but you think you know the answer, explain that to the user then answer with your own knowledge.
- Answer directly and without using xml tags.
- When using information from the context, cite the relevant source from the <sources> section.
</rules>

<user_query>
__INPUT__
</user_query>"#;

pub struct Rag {
    app_config: Arc<AppConfig>,
    name: String,
    path: String,
    embedding_model: Model,
    // Local BM25: keyword search + graph seeding. Always built from `data.files`
    // regardless of driver, and kept on `Rag` so the sync `graph_search` can use it.
    bm25: SearchEngine<DocumentId>,
    // Vector storage + content retrieval.
    provider: Box<dyn RagProvider>,
    data: RagData,
    last_sources: RwLock<Option<String>>,
    node_to_docs: IndexMap<u32, Vec<DocumentId>>,
}

impl Debug for Rag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rag")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("embedding_model", &self.embedding_model)
            .field("data", &self.data)
            .finish()
    }
}

// CLONING A `Rag` DOES NOT SNAPSHOT ITS BACKING STORE.
//
// `provider.duplicate(&self.data)` is a true snapshot for YamlProvider only.
// DuckDbProvider Arc-clones one shared `Mutex<Connection>` over one file, and
// QdrantProvider addresses the same remote collection. So for those drivers the
// clone and the original are two views of ONE store.
//
// INVARIANT: after calling `rebuild_indexes` on a cloned `Rag`, the pre-clone
// instance MUST be discarded immediately and MUST NOT serve further queries.
// Cloning to READ is always fine; cloning to REBUILD makes the original a
// half-truth (pre-rebuild `data`, post-rebuild store). Note that reassigning
// `RequestContext.rag` drops only one holder of the old `Arc<Rag>` — forked
// request contexts, agents, captured inputs and the RAG cache keep theirs.
impl Clone for Rag {
    fn clone(&self) -> Self {
        Self {
            app_config: self.app_config.clone(),
            name: self.name.clone(),
            path: self.path.clone(),
            embedding_model: self.embedding_model.clone(),
            bm25: self.data.build_bm25(),
            provider: self.provider.duplicate(&self.data),
            node_to_docs: self.data.knowledge_graph.build_node_to_docs(),
            data: self.data.clone(),
            last_sources: RwLock::new(None),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RagInitConfig {
    pub embedding_model: Option<String>,
    pub chunk_size: Option<usize>,
    pub chunk_overlap: Option<usize>,
    pub reranker_model: Option<String>,
    pub top_k: Option<usize>,
    pub batch_size: Option<usize>,
    pub extractor_model: Option<String>,
    pub extractor_prompt: Option<String>,
    pub graph_hops: Option<usize>,
    /// `None` -> "yaml". No serde attribute: this struct derives only
    /// `Debug, Clone, Default` and is built in Rust, never deserialized.
    pub driver: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GraphRagConfig {
    pub extractor_model: Option<String>,
    pub extractor_prompt: Option<String>,
    pub graph_hops: Option<usize>,
}

impl Rag {
    fn create_embeddings_client(&self, model: Model) -> Result<Box<dyn Client>> {
        init_client(&self.app_config, model)
    }

    pub async fn init_with_config(
        app: &AppConfig,
        name: &str,
        save_path: &Path,
        doc_paths: &[String],
        config: &RagInitConfig,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        if doc_paths.is_empty() {
            bail!("Cannot build RAG knowledge base '{name}' with no documents");
        }
        println!("⚙ Initializing RAG...");
        let mut data = Self::resolve_init_data(app, config)?;
        data.driver = config.driver.clone().unwrap_or_else(|| "yaml".to_string());
        let mut rag = Self::create(app, name, save_path, data)?;
        let loaders = app.document_loaders.clone();
        let (spinner, spinner_rx) = Spinner::create("");
        abortable_run_with_spinner_rx(
            rag.sync_documents(doc_paths, true, false, loaders, Some(spinner)),
            spinner_rx,
            abort_signal,
        )
        .await?;
        if rag.save()? {
            println!("✓ Saved RAG to '{}'.", save_path.display());
        }
        Ok(rag)
    }

    fn resolve_init_data(app: &AppConfig, config: &RagInitConfig) -> Result<RagData> {
        let embedding_model_id = config
            .embedding_model
            .clone()
            .or_else(|| app.rag_embedding_model.clone());
        let embedding_model_id = match embedding_model_id {
            Some(value) => {
                println!("Embedding model: {value}");
                value
            }
            None => {
                if !*IS_STDOUT_TERMINAL {
                    bail!(
                        "RAG knowledge base needs an embedding model. Set `embedding_model` \
                         on the rag node, or run the agent interactively once."
                    );
                }
                let models = list_models(app, ModelType::Embedding);
                if models.is_empty() {
                    bail!("No available embedding model");
                }
                select_embedding_model(&models)?
            }
        };
        let embedding_model =
            Model::retrieve_model(app, &embedding_model_id, ModelType::Embedding)?;

        let chunk_size = match config.chunk_size.or(app.rag_chunk_size) {
            Some(value) => {
                println!("Chunk size: {value}");
                value
            }
            None => {
                if !*IS_STDOUT_TERMINAL {
                    bail!(
                        "RAG knowledge base needs a chunk_size. Set `chunk_size` on the \
                         rag node, or run the agent interactively once."
                    );
                }
                set_chunk_size(&embedding_model)?
            }
        };
        let chunk_overlap = match config.chunk_overlap.or(app.rag_chunk_overlap) {
            Some(value) => {
                println!("Chunk overlap: {value}");
                value
            }
            None => {
                if !*IS_STDOUT_TERMINAL {
                    bail!(
                        "RAG knowledge base needs a chunk_overlap. Set `chunk_overlap` on \
                         the rag node, or run the agent interactively once."
                    );
                }
                set_chunk_overlay(chunk_size / 20)?
            }
        };

        let reranker_model = config
            .reranker_model
            .clone()
            .or_else(|| app.rag_reranker_model.clone());
        let top_k = config.top_k.unwrap_or(app.rag_top_k);
        let batch_size = config
            .batch_size
            .or_else(|| embedding_model.max_batch_size());

        Ok(RagData::new(
            embedding_model.id(),
            chunk_size,
            chunk_overlap,
            reranker_model,
            top_k,
            batch_size,
            GraphRagConfig {
                extractor_model: config
                    .extractor_model
                    .clone()
                    .or_else(|| app.rag_extractor_model.clone()),
                extractor_prompt: config
                    .extractor_prompt
                    .clone()
                    .or_else(|| app.rag_extractor_prompt.clone()),
                graph_hops: Some(config.graph_hops.unwrap_or(app.rag_graph_hops)),
            },
        ))
    }

    pub async fn init(
        app: &AppConfig,
        name: &str,
        save_path: &Path,
        doc_paths: &[String],
        abort_signal: AbortSignal,
        prompt_for_driver: bool,
    ) -> Result<Self> {
        if !*IS_STDOUT_TERMINAL {
            bail!("Failed to init rag in non-interactive mode");
        }
        println!("⚙ Initializing RAG...");
        let (embedding_model, chunk_size, chunk_overlap) = Self::create_config(app)?;
        // Only interactive named-RAG creation offers a driver choice. Temp RAGs and
        // agent startup pass `false`; an explicit flag is used rather than inferring
        // from the name because the agent path passes the literal name "rag", which is
        // indistinguishable from a user creating a RAG genuinely named `rag`.
        let driver = if prompt_for_driver {
            let options = vec![
                "yaml   — portable, in-memory HNSW; usable from several Coyote processes at once (default)",
                "duckdb — persistent on-disk store; vectors and content survive restarts; HNSW approximate search. Can only be open in ONE Coyote process at a time, and its driver cannot be changed later without recreating the RAG",
            ];
            let sel = Select::new("RAG storage driver:", options)
                .with_starting_cursor(0)
                .prompt()?;
            if sel.starts_with("duckdb") {
                println!(
                    "Note: a duckdb RAG can only be open in one Coyote process at a time, \
                     and changing its driver later means deleting and recreating the RAG."
                );
                "duckdb"
            } else {
                "yaml"
            }
        } else {
            "yaml"
        };
        let reranker_model = app.rag_reranker_model.clone();
        let top_k = app.rag_top_k;
        let extractor_model = match app.rag_extractor_model.clone() {
            Some(model) => Some(model),
            None => select_extractor_model(app)?,
        };
        let graph_hops = if extractor_model.is_some() {
            set_graph_hops(app.rag_graph_hops)?
        } else {
            app.rag_graph_hops
        };
        let extractor_prompt = app.rag_extractor_prompt.clone();
        let mut data = RagData::new(
            embedding_model.id(),
            chunk_size,
            chunk_overlap,
            reranker_model,
            top_k,
            embedding_model.max_batch_size(),
            GraphRagConfig {
                extractor_model,
                extractor_prompt,
                graph_hops: Some(graph_hops),
            },
        );
        data.driver = driver.to_string();
        let mut rag = Self::create(app, name, save_path, data)?;
        let mut paths = doc_paths.to_vec();
        if paths.is_empty() {
            paths = add_documents()?;
        };
        let loaders = app.document_loaders.clone();
        let (spinner, spinner_rx) = Spinner::create("");
        abortable_run_with_spinner_rx(
            rag.sync_documents(&paths, true, false, loaders, Some(spinner)),
            spinner_rx,
            abort_signal,
        )
        .await?;
        if rag.save()? {
            println!("✓ Saved RAG to '{}'.", save_path.display());
        }
        Ok(rag)
    }

    pub fn load(app: &AppConfig, name: &str, path: &Path) -> Result<Self> {
        let err = || format!("Failed to load rag '{name}' at '{}'", path.display());
        let content = fs::read_to_string(path).with_context(err)?;
        let data: RagData = serde_yaml::from_str(&content).with_context(err)?;
        data.validate().with_context(err)?;
        Self::create(app, name, path, data)
    }

    /// `mut data` — the duckdb arm rehydrates `data.vectors` from the sidecar.
    pub fn create(app: &AppConfig, name: &str, path: &Path, mut data: RagData) -> Result<Self> {
        // Deliberately does NOT call rebuild_indexes: both callers construct the Rag
        // before any documents are added, so rebuilding empty data would be a no-op.
        // Actual population happens later via sync_documents.
        let (provider, bm25): (Box<dyn RagProvider>, _) = match data.driver.as_str() {
            "duckdb" => {
                let db_path = providers::duckdb_path_from_yaml(path);
                let dim = embedding_dim_for_model(&data.embedding_model);
                let duck = DuckDbProvider::open(&db_path, dim)?;
                // HYDRATE — mandatory, not an optimization. The YAML file for a duckdb
                // RAG deliberately omits `vectors`, so `data.vectors` arrives empty from
                // disk. Refilling it from the sidecar is what makes the NEXT incremental
                // sync non-destructive: rebuild_indexes does CREATE OR REPLACE TABLE and
                // writes exactly what data.vectors holds. Skip this and the first
                // `.edit rag-docs` after a restart wipes every previously indexed vector.
                //
                // Guarded on is_empty() so a caller that already has vectors in memory
                // is never overwritten by an empty table.
                //
                // 🔴 `?`, NOT `unwrap_or_default()`. A hydration failure must propagate.
                // Degrading to an empty map here loads a RAG that looks healthy, answers
                // every query with nothing, and then loses the store permanently on the
                // first `.edit rag-docs`. The legitimate "nothing indexed yet" case is
                // already Ok(empty) — open() runs CREATE TABLE IF NOT EXISTS — so `?`
                // costs a new RAG nothing.
                if data.vectors.is_empty() {
                    data.vectors = duck.read_all_vectors()?;
                }
                // data.files is always populated for duckdb, so build_bm25() is the only
                // path; there is no from-DuckDB fallback.
                let bm25 = data.build_bm25();
                (Box::new(duck), bm25)
            }
            "qdrant" => bail!(
                "Qdrant RAGs cannot be constructed via Rag::create(); \
                 use Rag::attach() or Rag::load_async() instead"
            ),
            _ => {
                // "yaml" and any unknown driver — in-memory HNSW.
                let bm25 = data.build_bm25();
                (Box::new(YamlProvider::from_data(&data)), bm25)
            }
        };
        let node_to_docs = data.knowledge_graph.build_node_to_docs();
        let embedding_model =
            Model::retrieve_model(app, &data.embedding_model, ModelType::Embedding)?;
        let rag = Rag {
            app_config: Arc::new(app.clone()),
            name: name.to_string(),
            path: path.display().to_string(),
            data,
            embedding_model,
            bm25,
            provider,
            node_to_docs,
            last_sources: RwLock::new(None),
        };
        Ok(rag)
    }

    pub fn document_paths(&self) -> &[String] {
        &self.data.document_paths
    }

    pub async fn refresh_document_paths(
        &mut self,
        document_paths: &[String],
        refresh: bool,
        force_reingest: bool,
        app: &AppConfig,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        let loaders = app.document_loaders.clone();
        let (spinner, spinner_rx) = Spinner::create("");
        abortable_run_with_spinner_rx(
            self.sync_documents(
                document_paths,
                refresh,
                force_reingest,
                loaders,
                Some(spinner),
            ),
            spinner_rx,
            abort_signal,
        )
        .await?;
        if self.save()? {
            println!("✓ Saved rag to '{}'.", self.path);
        }
        Ok(())
    }

    pub fn create_config(app: &AppConfig) -> Result<(Model, usize, usize)> {
        let embedding_model_id = app.rag_embedding_model.clone();
        let chunk_size = app.rag_chunk_size;
        let chunk_overlap = app.rag_chunk_overlap;
        let embedding_model_id = match embedding_model_id {
            Some(value) => {
                println!("Select embedding model: {value}");
                value
            }
            None => {
                let models = list_models(app, ModelType::Embedding);
                if models.is_empty() {
                    bail!("No available embedding model");
                }
                select_embedding_model(&models)?
            }
        };
        let embedding_model =
            Model::retrieve_model(app, &embedding_model_id, ModelType::Embedding)?;

        let chunk_size = match chunk_size {
            Some(value) => {
                println!("Set chunk size: {value}");
                value
            }
            None => set_chunk_size(&embedding_model)?,
        };
        let chunk_overlap = match chunk_overlap {
            Some(value) => {
                println!("Set chunk overlay: {value}");
                value
            }
            None => {
                let value = chunk_size / 20;
                set_chunk_overlay(value)?
            }
        };

        Ok((embedding_model, chunk_size, chunk_overlap))
    }

    pub fn get_config(&self) -> (Option<String>, usize) {
        (self.data.reranker_model.clone(), self.data.top_k)
    }

    pub fn get_last_sources(&self) -> Option<String> {
        self.last_sources.read().clone()
    }

    pub fn set_last_sources(&self, ids: &[DocumentId]) {
        let mut sources: IndexMap<String, Vec<String>> = IndexMap::new();
        for id in ids {
            let (file_index, _) = id.split();
            if let Some(file) = self.data.files.get(&file_index) {
                sources
                    .entry(file.path.clone())
                    .or_default()
                    .push(format!("{id:?}"));
            }
        }
        let sources = if sources.is_empty() {
            None
        } else {
            Some(
                sources
                    .into_iter()
                    .map(|(path, ids)| format!("{path} ({})", ids.join(",")))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };
        *self.last_sources.write() = sources;
    }

    pub fn set_reranker_model(&mut self, reranker_model: Option<String>) -> Result<()> {
        self.data.reranker_model = reranker_model;
        self.save()?;
        Ok(())
    }

    pub fn set_top_k(&mut self, top_k: usize) -> Result<()> {
        self.data.top_k = top_k;
        self.save()?;
        Ok(())
    }

    pub fn save(&self) -> Result<bool> {
        if self.is_temp() {
            return Ok(false);
        }
        let path = Path::new(&self.path);
        ensure_parent_exists(path)?;

        let content = if self.data.driver == "duckdb" {
            // Embeddings live in the .duckdb sidecar; keep them out of the YAML file.
            // Clone-and-empty rather than mutating self.data — the live map must stay
            // complete for the next incremental sync, and save() takes &self, so any
            // clear-then-restore would leave the object corrupted on an early return.
            let mut on_disk = self.data.clone();
            on_disk.vectors.clear();
            serde_yaml::to_string(&on_disk)
        } else {
            serde_yaml::to_string(&self.data)
        }
        .with_context(|| format!("Failed to serde rag '{}'", self.name))?;
        fs::write(path, content).with_context(|| {
            format!("Failed to save rag '{}' to '{}'", self.name, path.display())
        })?;

        Ok(true)
    }

    pub fn export(&self) -> Result<String> {
        let files: Vec<_> = self
            .data
            .files
            .iter()
            .map(|(_, v)| {
                json!({
                    "path": v.path,
                    "num_chunks": v.documents.len(),
                })
            })
            .collect();
        let data = json!({
            "path": self.path,
            "driver": self.driver(),
            "attached": self.is_attached(),
            "embedding_model": self.embedding_model.id(),
            "chunk_size": self.data.chunk_size,
            "chunk_overlap": self.data.chunk_overlap,
            "reranker_model": self.data.reranker_model,
            "extractor_model": self.data.extractor_model,
            "extractor_prompt": self.data.extractor_prompt,
            "graph_hops": self.data.graph_hops.unwrap_or(1),
            "top_k": self.data.top_k,
            "batch_size": self.data.batch_size,
            "document_paths": self.data.document_paths,
            "files": files,
        });
        let output = serde_yaml::to_string(&data)
            .with_context(|| format!("Unable to show info about rag '{}'", self.name))?;
        Ok(output)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_attached(&self) -> bool {
        self.data.attached
    }

    pub fn driver(&self) -> &str {
        &self.data.driver
    }

    pub fn file_count(&self) -> usize {
        self.data.files.len()
    }

    pub fn is_temp(&self) -> bool {
        self.name == TEMP_RAG_NAME
    }

    pub fn configured_top_k(&self) -> usize {
        self.data.top_k
    }

    pub fn configured_reranker(&self) -> Option<&str> {
        self.data.reranker_model.as_deref()
    }

    pub async fn search(
        &self,
        text: &str,
        top_k: usize,
        rerank_model: Option<&str>,
        abort_signal: AbortSignal,
    ) -> Result<(String, String, Vec<DocumentId>)> {
        let ret = abortable_run_with_spinner(
            self.hybrid_search(text, top_k, rerank_model),
            "Searching",
            abort_signal,
        )
        .await;
        let results = ret?;
        let ids: Vec<_> = results.iter().map(|(id, _)| *id).collect();
        let embeddings = results
            .iter()
            .map(|(id, content)| {
                let source = self.resolve_source(id);
                format!("[Source: {source}]\n{content}")
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let sources = self.format_sources(&ids);
        Ok((embeddings, sources, ids))
    }

    pub async fn search_with_template(
        &self,
        app: &AppConfig,
        text: &str,
        abort_signal: AbortSignal,
    ) -> Result<String> {
        let (reranker_model, top_k) = self.get_config();
        let (embeddings, sources, ids) = self
            .search(text, top_k, reranker_model.as_deref(), abort_signal)
            .await?;
        let rag_template = app.rag_template.as_deref().unwrap_or(RAG_TEMPLATE);
        let text = if embeddings.is_empty() {
            text.to_string()
        } else {
            rag_template
                .replace("__CONTEXT__", &embeddings)
                .replace("__SOURCES__", &sources)
                .replace("__INPUT__", text)
        };
        self.set_last_sources(&ids);
        Ok(text)
    }

    fn resolve_source(&self, id: &DocumentId) -> String {
        let (file_index, _) = id.split();
        self.data
            .files
            .get(&file_index)
            .map(|f| f.path.clone())
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn format_sources(&self, ids: &[DocumentId]) -> String {
        let mut seen = IndexSet::new();
        for id in ids {
            let (file_index, _) = id.split();
            if let Some(file) = self.data.files.get(&file_index) {
                seen.insert(file.path.clone());
            }
        }
        seen.into_iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub async fn sync_documents(
        &mut self,
        paths: &[String],
        refresh: bool,
        force_reingest: bool,
        loaders: HashMap<String, String>,
        spinner: Option<Spinner>,
    ) -> Result<()> {
        debug_assert!(
            !force_reingest || refresh,
            "force_reingest requires refresh"
        );
        let refresh = refresh || force_reingest;
        if let Some(spinner) = &spinner {
            let _ = spinner.set_message(String::new());
        }
        let (document_paths, mut recursive_urls, mut urls, mut protocol_paths, mut local_paths) =
            resolve_paths(&loaders, paths).await?;
        let mut to_deleted: IndexMap<String, Vec<FileId>> = Default::default();
        if refresh {
            for (file_id, file) in &self.data.files {
                to_deleted
                    .entry(file.hash.clone())
                    .or_default()
                    .push(*file_id);
            }
        } else {
            let recursive_urls_cloned = recursive_urls.clone();
            let match_recursive_url = |v: &str| {
                recursive_urls_cloned
                    .iter()
                    .any(|start_url| v.starts_with(start_url))
            };
            recursive_urls = recursive_urls
                .into_iter()
                .filter(|v| !self.data.document_paths.contains(&format!("{v}**")))
                .collect();
            let protocol_paths_cloned = protocol_paths.clone();
            let match_protocol_path =
                |v: &str| protocol_paths_cloned.iter().any(|root| v.starts_with(root));
            protocol_paths = protocol_paths
                .into_iter()
                .filter(|v| !self.data.document_paths.contains(v))
                .collect();
            for (file_id, file) in &self.data.files {
                if is_url(&file.path) {
                    if !urls.swap_remove(&file.path) && !match_recursive_url(&file.path) {
                        to_deleted
                            .entry(file.hash.clone())
                            .or_default()
                            .push(*file_id);
                    }
                } else if is_loader_protocol(&loaders, &file.path) {
                    if !match_protocol_path(&file.path) {
                        to_deleted
                            .entry(file.hash.clone())
                            .or_default()
                            .push(*file_id);
                    }
                } else if !local_paths.swap_remove(&file.path) {
                    to_deleted
                        .entry(file.hash.clone())
                        .or_default()
                        .push(*file_id);
                }
            }
        }

        let mut loaded_documents = vec![];
        let mut has_error = false;
        let mut index = 0;
        let total = recursive_urls.len() + urls.len() + protocol_paths.len() + local_paths.len();
        let handle_error = |error: anyhow::Error, has_error: &mut bool| {
            println!("{}", warning_text(&format!("⚠️ {error}")));
            *has_error = true;
        };
        for start_url in recursive_urls {
            index += 1;
            println!("Load {start_url}** [{index}/{total}]");
            match load_recursive_url(&loaders, &start_url).await {
                Ok(v) => loaded_documents.extend(v),
                Err(err) => handle_error(err, &mut has_error),
            }
        }
        for url in urls {
            index += 1;
            println!("Load {url} [{index}/{total}]");
            match load_url(&loaders, &url).await {
                Ok(v) => loaded_documents.push(v),
                Err(err) => handle_error(err, &mut has_error),
            }
        }
        for protocol_path in protocol_paths {
            index += 1;
            println!("Load {protocol_path} [{index}/{total}]");
            match load_protocol_path(&loaders, &protocol_path) {
                Ok(v) => loaded_documents.extend(v),
                Err(err) => handle_error(err, &mut has_error),
            }
        }
        for local_path in local_paths {
            index += 1;
            println!("Load {local_path} [{index}/{total}]");
            match load_file(&loaders, &local_path).await {
                Ok(v) => loaded_documents.push(v),
                Err(err) => handle_error(err, &mut has_error),
            }
        }

        if has_error {
            let mut aborted = true;
            if *IS_STDOUT_TERMINAL && total > 0 {
                let ans = Confirm::new("Some documents failed to load. Continue?")
                    .with_default(false)
                    .prompt()?;
                aborted = !ans;
            }
            if aborted {
                bail!("Aborted");
            }
        }

        let mut rag_files = vec![];
        for LoadedDocument {
            path,
            contents,
            mut metadata,
        } in loaded_documents
        {
            let hash = sha256(&contents);
            if let Some((i, _)) =
                find_hash_skip(force_reingest, &to_deleted, &self.data.files, &hash, &path)
                && let Some(file_ids) = to_deleted.get_mut(&hash)
            {
                if file_ids.len() == 1 {
                    to_deleted.swap_remove(&hash);
                } else {
                    file_ids.remove(i);
                }
                continue;
            }
            let extension = metadata
                .swap_remove(EXTENSION_METADATA)
                .unwrap_or_else(|| DEFAULT_EXTENSION.into());
            let separator = get_separators(&extension);
            let splitter = RecursiveCharacterTextSplitter::new(
                self.data.chunk_size,
                self.data.chunk_overlap,
                &separator,
            );

            let split_options = SplitterChunkHeaderOptions::default();
            let document = RagDocument::new(contents);
            let split_documents = splitter.split_documents(&[document], &split_options);
            rag_files.push(RagFile {
                hash: hash.clone(),
                path,
                documents: split_documents,
            });
        }

        let mut next_file_id = self.data.next_file_id;
        let mut files = vec![];
        let mut document_ids = vec![];
        let mut embeddings = vec![];
        let mut new_doc_contents: Vec<(DocumentId, String)> = vec![];

        if !rag_files.is_empty() {
            let mut texts = vec![];
            for file in rag_files.into_iter() {
                for (document_index, document) in file.documents.iter().enumerate() {
                    let doc_id = DocumentId::new(next_file_id, document_index);
                    document_ids.push(doc_id);
                    texts.push(document.page_content.clone());
                    if self.data.extractor_model.is_some() {
                        new_doc_contents.push((doc_id, document.page_content.clone()));
                    }
                }
                files.push((next_file_id, file));
                next_file_id += 1;
            }

            let embeddings_data = EmbeddingsData::new(texts, false);
            embeddings = self
                .create_embeddings(embeddings_data, spinner.clone())
                .await?;
        }

        let to_delete_file_ids: Vec<_> = to_deleted.values().flatten().copied().collect();
        self.data.del(to_delete_file_ids);
        self.data.add(next_file_id, files, document_ids, embeddings);
        self.data.document_paths = document_paths.into_iter().collect();

        if self.data.files.is_empty() {
            bail!("No RAG files");
        }

        if !new_doc_contents.is_empty()
            && let Some(extractor_model_id) = self.data.extractor_model.clone()
        {
            match Model::retrieve_model(&self.app_config, &extractor_model_id, ModelType::Chat) {
                Ok(model) => match self.create_embeddings_client(model) {
                    Ok(client) => {
                        let total = new_doc_contents.len();
                        let mut failures = 0usize;
                        for (i, (doc_id, content)) in new_doc_contents.into_iter().enumerate() {
                            progress(
                                &spinner,
                                format!("Extracting entities [{}/{}]", i + 1, total),
                            );
                            match extract_entities(
                                client.as_ref(),
                                &content,
                                self.data.extractor_prompt.as_deref(),
                            )
                            .await
                            {
                                Ok(result) => self.data.knowledge_graph.merge(doc_id, result),
                                Err(e) => {
                                    warn!("Entity extraction failed for doc {doc_id:?}: {e}");
                                    failures += 1;
                                }
                            }
                        }
                        if failures > 0 {
                            progress(
                                &spinner,
                                format!("Entity extraction: {failures}/{total} chunks failed"),
                            );
                        }
                    }
                    Err(e) => warn!("Failed to create extractor client: {e}"),
                },
                Err(e) => warn!("Extractor model not found: {e}"),
            }
        }

        progress(&spinner, "Building store".into());
        // Derived in-memory state is refreshed BEFORE the fallible provider rebuild.
        // `self.data` has already been mutated at this point, so returning early on a
        // provider error while `bm25`/`node_to_docs` still describe the previous corpus
        // would leave this Rag internally inconsistent. Both are pure functions of
        // `self.data` and cannot fail, so doing them first is always safe.
        self.bm25 = self.data.build_bm25();
        self.node_to_docs = self.data.knowledge_graph.build_node_to_docs();
        // `refresh` is true for a full re-index (.rebuild rag / --rebuild-rag /
        // initial build) and false for an incremental .edit rag-docs change.
        // Passing it through is what stops a remote provider from wiping its
        // collection on a one-file add.
        self.provider.rebuild_indexes(&self.data, refresh).await?;

        Ok(())
    }

    async fn hybrid_search(
        &self,
        query: &str,
        top_k: usize,
        rerank_model: Option<&str>,
    ) -> Result<Vec<(DocumentId, String)>> {
        let vector_search_results = self.vector_search(query, top_k, 0.0).await?;
        debug!("vector_search_results: {vector_search_results:?}",);
        let vector_search_ids: Vec<DocumentId> =
            vector_search_results.into_iter().map(|(v, _)| v).collect();

        let keyword_search_results: Vec<(DocumentId, f32)> =
            if self.provider.has_native_keyword_search() {
                // Keyword is ONE of three RRF rankers (vector + keyword + graph);
                // its absence is survivable and produces a slightly worse ranking,
                // whereas a `?` here turns a provider FTS fault into TOTAL query
                // failure — the user gets an error instead of the results the
                // vector and graph rankers already retrieved. Degrade, do not
                // propagate, and do not silently swap in the local BM25 either:
                // that would change the ranking algorithm mid-query.
                match self.provider.keyword_search(query, top_k).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("native keyword search failed, dropping the keyword ranker: {e}");
                        Vec::new()
                    }
                }
            } else {
                self.keyword_search(query, top_k, 0.0)
            };
        debug!("keyword_search_results: {keyword_search_results:?}",);
        let keyword_search_ids: Vec<DocumentId> =
            keyword_search_results.into_iter().map(|(v, _)| v).collect();

        let ids = match rerank_model {
            Some(model_id) => {
                let model = Model::retrieve_model(&self.app_config, model_id, ModelType::Reranker)?;
                let client = self.create_embeddings_client(model)?;
                let ids: IndexSet<DocumentId> = [vector_search_ids, keyword_search_ids]
                    .concat()
                    .into_iter()
                    .collect();
                // `ids` is an `IndexSet` here, not the `Vec` of the RRF branch below,
                // and `&IndexSet<_>` does not coerce to `&[DocumentId]`.
                let ids: Vec<DocumentId> = ids.into_iter().collect();
                let fetched = self.provider.fetch_content(&ids).await?;
                // Build both vectors from the SAME source in the SAME iteration —
                // never zip two independently-built lists. The reranker returns
                // positional indices into `documents`, so any drift between the two
                // resolves reranked hits to the wrong document's text. A partial
                // fetch simply yields a shorter pair, and both shrink together.
                let mut documents_ids = Vec::with_capacity(fetched.len());
                let mut documents = Vec::with_capacity(fetched.len());
                for (id, text) in fetched {
                    documents_ids.push(id);
                    documents.push(text);
                }
                let data = RerankData::new(query.to_string(), documents, top_k);
                let list = client.rerank(&data).await.context("Failed to rerank")?;
                let ids: Vec<_> = list
                    .into_iter()
                    .take(top_k)
                    .filter_map(|item| documents_ids.get(item.index).cloned())
                    .collect();
                debug!("rerank_ids: {ids:?}");
                ids
            }
            None => {
                let ids = if self.data.extractor_model.is_some() {
                    let graph_ids = self.graph_search(query, top_k);
                    debug!("graph_search_ids: {graph_ids:?}");
                    reciprocal_rank_fusion(
                        vec![vector_search_ids, keyword_search_ids, graph_ids],
                        vec![1.125, 1.0, 0.9],
                        top_k,
                    )
                } else {
                    reciprocal_rank_fusion(
                        vec![vector_search_ids, keyword_search_ids],
                        vec![1.125, 1.0],
                        top_k,
                    )
                };
                debug!("rrf_ids: {ids:?}");
                ids
            }
        };
        // `ids` is the ranked list; `fetch_content` preserves that order per the
        // trait's ordering contract, so the result is returned as-is.
        let output = self.provider.fetch_content(&ids).await?;
        Ok(output)
    }

    async fn vector_search(
        &self,
        query: &str,
        top_k: usize,
        min_score: f32,
    ) -> Result<Vec<(DocumentId, f32)>> {
        let splitter = RecursiveCharacterTextSplitter::new(
            self.data.chunk_size,
            self.data.chunk_overlap,
            &DEFAULT_SEPARATORS,
        );
        let texts = splitter.split_text(query);
        let embeddings_data = EmbeddingsData::new(texts, true);
        let query_embeddings = self.create_embeddings(embeddings_data, None).await?;

        let mut results: Vec<(DocumentId, f32)> = vec![];
        for embedding in &query_embeddings {
            let batch = self
                .provider
                .vector_search(embedding, top_k, min_score)
                .await?;
            results.extend(batch);
        }
        Ok(merge_vector_results(results))
    }

    /// Local in-memory BM25 over `data.files` — empty for attached RAGs, which is
    /// correct: they have no local text.
    fn keyword_search(&self, query: &str, top_k: usize, min_score: f32) -> Vec<(DocumentId, f32)> {
        let results = self.bm25.search(query, top_k);
        results
            .into_iter()
            .filter_map(|v| {
                let score = v.score;
                if score > min_score {
                    Some((v.document.id, score))
                } else {
                    None
                }
            })
            .collect()
    }

    fn graph_search(&self, query: &str, top_k: usize) -> Vec<DocumentId> {
        let kg = &self.data.knowledge_graph;
        if kg.entity_index.is_empty() {
            return vec![];
        }

        let query_lower = query.to_lowercase();
        let query_tokens: Vec<&str> = query_lower.split_whitespace().collect();
        let token_count = query_tokens.len().max(1);

        let score_node = |raw: u32| -> f32 {
            let idx = NodeIndex::new(raw as usize);
            if !kg.graph.contains_node(idx) {
                return 0.0;
            }
            let entity = &kg.graph[idx];
            let combined = format!(
                "{} {}",
                entity.name,
                entity.description.as_deref().unwrap_or("")
            )
            .to_lowercase();
            query_tokens
                .iter()
                .filter(|t| combined.contains(*t))
                .count() as f32
                / token_count as f32
        };

        let mut seed_scores: Vec<(u32, f32)> = kg
            .entity_index
            .iter()
            .filter(|(name, _)| {
                let name_str = name.as_str();
                if name_str.contains(' ') {
                    query_lower.contains(name_str)
                } else {
                    // whole-word match: prevents "go" from seeding on every query containing "Django"
                    query_lower
                        .split_whitespace()
                        .any(|token| token.trim_matches(|c: char| !c.is_alphanumeric()) == name_str)
                }
            })
            .map(|(_, &raw)| (raw, score_node(raw).max(BM25_SEED_SCORE)))
            .collect();

        if seed_scores.is_empty() {
            let bm25_results = self.bm25.search(query, top_k * 2);
            'outer: for result in bm25_results {
                if let Some(node_raws) = kg.document_entities.get(&result.document.id.0) {
                    for &raw in node_raws {
                        seed_scores.push((raw, BM25_SEED_SCORE));
                        if seed_scores.len() >= top_k {
                            break 'outer;
                        }
                    }
                }
            }
        }

        if seed_scores.is_empty() {
            return vec![];
        }

        let hops = self.data.graph_hops.unwrap_or(1);
        let mut scored: Vec<(u32, f32)> = kg
            .expand_neighbors_scored(&seed_scores, hops)
            .into_iter()
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));

        let mut result_ids: IndexSet<DocumentId> = IndexSet::new();
        for (raw, _) in scored {
            if let Some(doc_ids) = self.node_to_docs.get(&raw) {
                for &doc_id in doc_ids {
                    result_ids.insert(doc_id);
                    if result_ids.len() >= top_k {
                        break;
                    }
                }
            }
            if result_ids.len() >= top_k {
                break;
            }
        }
        result_ids.into_iter().collect()
    }

    async fn create_embeddings(
        &self,
        data: EmbeddingsData,
        spinner: Option<Spinner>,
    ) -> Result<EmbeddingsOutput> {
        let embedding_client = self.create_embeddings_client(self.embedding_model.clone())?;
        let EmbeddingsData { texts, query } = data;
        let batch_size = self
            .data
            .batch_size
            .or_else(|| self.embedding_model.max_batch_size());
        let batch_size = match self.embedding_model.max_input_tokens() {
            Some(max_input_tokens) => {
                let x = max_input_tokens / self.data.chunk_size;
                match batch_size {
                    Some(y) => x.min(y),
                    None => x,
                }
            }
            None => batch_size.unwrap_or(1),
        };
        let mut output = vec![];
        let batch_chunks = texts.chunks(batch_size.max(1));
        let batch_chunks_len = batch_chunks.len();
        let retry_limit = env::var(get_env_name("embeddings_retry_limit"))
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(2);
        for (index, texts) in batch_chunks.enumerate() {
            progress(
                &spinner,
                format!("Creating embeddings [{}/{batch_chunks_len}]", index + 1),
            );
            let chunk_data = EmbeddingsData {
                texts: texts.to_vec(),
                query,
            };
            let mut retry = 0;
            let chunk_output = loop {
                retry += 1;
                match embedding_client.embeddings(&chunk_data).await {
                    Ok(v) => break v,
                    Err(e) if retry < retry_limit => {
                        debug!("retry {retry} failed: {e}");
                        sleep(Duration::from_secs(2u64.pow(retry - 1))).await;
                        continue;
                    }
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!("Failed to create embedding after {retry_limit} attempts")
                        })?;
                    }
                }
            };
            output.extend(chunk_output);
        }
        Ok(output)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RagData {
    #[serde(default = "RagData::default_driver")]
    pub driver: String,
    #[serde(default)]
    pub attached: bool,

    pub embedding_model: String,
    #[serde(default)]
    pub chunk_size: usize,
    #[serde(default)]
    pub chunk_overlap: usize,
    pub reranker_model: Option<String>,
    #[serde(default)]
    pub top_k: usize,
    pub batch_size: Option<usize>,
    #[serde(default)]
    pub next_file_id: FileId,
    #[serde(default)]
    pub document_paths: Vec<String>,
    #[serde(default)]
    pub files: IndexMap<FileId, RagFile>,
    #[serde(
        default,
        with = "serde_vectors",
        skip_serializing_if = "IndexMap::is_empty"
    )]
    pub vectors: IndexMap<DocumentId, Vec<f32>>,
    #[serde(default)]
    pub extractor_model: Option<String>,
    #[serde(default)]
    pub extractor_prompt: Option<String>,
    #[serde(default)]
    pub graph_hops: Option<usize>,
    #[serde(default)]
    pub knowledge_graph: KnowledgeGraph,
}

impl Debug for RagData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RagData")
            .field("driver", &self.driver)
            .field("attached", &self.attached)
            .field("embedding_model", &self.embedding_model)
            .field("chunk_size", &self.chunk_size)
            .field("chunk_overlap", &self.chunk_overlap)
            .field("reranker_model", &self.reranker_model)
            .field("top_k", &self.top_k)
            .field("batch_size", &self.batch_size)
            .field("next_file_id", &self.next_file_id)
            .field("document_paths", &self.document_paths)
            .field("files", &self.files)
            .field("extractor_model", &self.extractor_model)
            .field("extractor_prompt", &self.extractor_prompt)
            .field("graph_hops", &self.graph_hops)
            .finish()
    }
}

impl RagData {
    pub fn new(
        embedding_model: String,
        chunk_size: usize,
        chunk_overlap: usize,
        reranker_model: Option<String>,
        top_k: usize,
        batch_size: Option<usize>,
        graph: GraphRagConfig,
    ) -> Self {
        Self {
            driver: "yaml".to_string(),
            attached: false,
            embedding_model,
            chunk_size,
            chunk_overlap,
            reranker_model,
            top_k,
            batch_size,
            next_file_id: 0,
            document_paths: Default::default(),
            files: Default::default(),
            vectors: Default::default(),
            extractor_model: graph.extractor_model,
            extractor_prompt: graph.extractor_prompt,
            graph_hops: graph.graph_hops,
            knowledge_graph: KnowledgeGraph::default(),
        }
    }

    fn default_driver() -> String {
        "yaml".to_string()
    }

    pub fn validate(&self) -> Result<()> {
        if self.top_k == 0 {
            bail!(
                "top_k must be >= 1 (got 0). A top_k of 0 makes every query return \
                 no results with no error. Set `top_k:` in the RAG YAML."
            );
        }
        if !self.attached {
            if self.chunk_size == 0 {
                bail!(
                    "chunk_size must be >= 1 (got 0) for a non-attached RAG. A \
                     chunk_size of 0 panics with a divide-by-zero while sizing \
                     embedding batches. Set `chunk_size:` in the RAG YAML."
                );
            }
            if self.chunk_overlap >= self.chunk_size {
                bail!(
                    "chunk_overlap ({}) must be strictly less than chunk_size ({}).",
                    self.chunk_overlap,
                    self.chunk_size
                );
            }
        }
        match (self.driver.as_str(), self.attached) {
            ("yaml", false) => Ok(()),
            ("duckdb", false) => Ok(()),
            ("qdrant", true) => Ok(()),
            ("qdrant", false) => Ok(()),
            ("yaml", true) => bail!(
                "driver 'yaml' cannot be attached (attached: true). \
                 Attached RAGs require an external driver (qdrant)."
            ),
            ("duckdb", true) => bail!(
                "driver 'duckdb' cannot be attached (attached: true). \
                 DuckDB is a local-only driver; use 'qdrant' for external collections."
            ),
            (other, _) => {
                bail!("Unknown RAG driver '{other}'. Valid drivers: yaml, duckdb, qdrant.")
            }
        }
    }

    /// Every (DocumentId, &RagDocument) in the corpus, in `files` order.
    ///
    /// This — NOT `vectors` — is the authoritative document id space. BM25, the
    /// knowledge graph and content lookup all key off it; `vectors` is a subset,
    /// since `add`'s zip truncates whenever fewer embeddings come back than
    /// document ids were sent.
    pub fn iter_documents(&self) -> impl Iterator<Item = (DocumentId, &RagDocument)> {
        self.files.iter().flat_map(|(file_index, file)| {
            file.documents
                .iter()
                .enumerate()
                .map(move |(document_index, document)| {
                    (DocumentId::new(*file_index, document_index), document)
                })
        })
    }

    pub fn del(&mut self, file_ids: Vec<FileId>) {
        let mut graph_doc_ids = vec![];
        for file_id in file_ids {
            if let Some(file) = self.files.swap_remove(&file_id) {
                for (document_index, _) in file.documents.iter().enumerate() {
                    let document_id = DocumentId::new(file_id, document_index);
                    self.vectors.swap_remove(&document_id);
                    graph_doc_ids.push(document_id);
                }
            }
        }
        self.knowledge_graph.remove_documents(&graph_doc_ids);
    }

    pub fn add(
        &mut self,
        next_file_id: FileId,
        files: Vec<(FileId, RagFile)>,
        document_ids: Vec<DocumentId>,
        embeddings: EmbeddingsOutput,
    ) {
        self.next_file_id = next_file_id;
        self.files.extend(files);
        self.vectors
            .extend(document_ids.into_iter().zip(embeddings));
    }

    pub fn build_hnsw(&self) -> Hnsw<'static, f32, DistCosine> {
        let hnsw = Hnsw::new(32, self.vectors.len(), 16, 200, DistCosine {});
        let list: Vec<_> = self.vectors.iter().map(|(k, v)| (v, k.0)).collect();
        hnsw.parallel_insert(&list);
        hnsw
    }

    pub fn build_bm25(&self) -> SearchEngine<DocumentId> {
        // Shares `iter_documents` with the providers' content maps so the BM25 key
        // space and the content key space are identical by construction.
        let documents: Vec<_> = self
            .iter_documents()
            .map(|(id, doc)| bm25::Document::new(id, &doc.page_content))
            .collect();
        SearchEngineBuilder::<DocumentId>::with_documents(Language::English, documents)
            .k1(1.5)
            .b(0.75)
            .build()
    }
}

impl Default for RagData {
    fn default() -> Self {
        RagData::new(
            String::new(),
            0,
            0,
            None,
            5,
            None,
            GraphRagConfig::default(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagFile {
    hash: String,
    path: String,
    documents: Vec<RagDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagDocument {
    pub page_content: String,
    pub metadata: DocumentMetadata,
}

impl RagDocument {
    pub fn new<S: Into<String>>(page_content: S) -> Self {
        RagDocument {
            page_content: page_content.into(),
            metadata: IndexMap::new(),
        }
    }
}

impl Default for RagDocument {
    fn default() -> Self {
        RagDocument {
            page_content: "".to_string(),
            metadata: IndexMap::new(),
        }
    }
}

pub type FileId = usize;

#[derive(Clone, Copy, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct DocumentId(usize);

impl Debug for DocumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (file_index, document_index) = self.split();
        f.write_fmt(format_args!("{file_index}-{document_index}"))
    }
}

impl DocumentId {
    pub fn new(file_index: usize, document_index: usize) -> Self {
        let value = (file_index << (usize::BITS / 2)) | document_index;
        Self(value)
    }

    pub fn split(self) -> (usize, usize) {
        let value = self.0;
        let low_mask = (1 << (usize::BITS / 2)) - 1;
        let low = value & low_mask;
        let high = value >> (usize::BITS / 2);
        (high, low)
    }
}

fn select_embedding_model(models: &[&Model]) -> Result<String> {
    let max_width = models.iter().map(|v| v.id().len()).max().unwrap_or(0);
    let models: Vec<_> = models
        .iter()
        .map(|v| SelectOption::new(v.id(), v.description(), max_width))
        .collect();
    let result = Select::new("Select embedding model:", models)
        .with_formatter(&|opt| opt.value.value.clone())
        .prompt()?;
    Ok(result.value)
}

const EXTRACTOR_SKIP: &str = "Skip";

fn select_extractor_model(app: &AppConfig) -> Result<Option<String>> {
    let models = list_models(app, ModelType::Chat);
    if models.is_empty() {
        return Ok(None);
    }
    let pad = models
        .iter()
        .map(|v| v.id().len())
        .max()
        .unwrap_or(0)
        .max(EXTRACTOR_SKIP.len());
    let mut options = vec![SelectOption::new(
        EXTRACTOR_SKIP.to_string(),
        "vector + full text search only (no graph)".to_string(),
        pad,
    )];
    options.extend(
        models
            .iter()
            .map(|v| SelectOption::new(v.id(), v.description(), pad)),
    );
    let result = Select::new("Extractor model for graph-based RAG (optional):", options)
        .with_formatter(&|opt| opt.value.value.clone())
        .prompt()?;
    Ok(if result.value == EXTRACTOR_SKIP {
        None
    } else {
        Some(result.value)
    })
}

#[derive(Debug)]
struct SelectOption {
    pub value: String,
    pub display: String,
}

impl SelectOption {
    pub fn new(value: String, description: String, pad: usize) -> Self {
        let display = if description.is_empty() {
            format!("{value:<pad$}")
        } else {
            format!("{value:<pad$} ({description})")
        };
        Self { value, display }
    }
}

impl fmt::Display for SelectOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display)
    }
}

fn set_chunk_size(model: &Model) -> Result<usize> {
    let default_value = model.default_chunk_size().to_string();
    let help_message = model
        .max_tokens_per_chunk()
        .map(|v| format!("The model's max_tokens is {v}"));

    let mut text = Text::new("Set chunk size:")
        .with_default(&default_value)
        .with_validator(move |text: &str| {
            let out = match text.parse::<usize>() {
                Ok(_) => Validation::Valid,
                Err(_) => Validation::Invalid("Must be a integer".into()),
            };
            Ok(out)
        });
    if let Some(help_message) = &help_message {
        text = text.with_help_message(help_message);
    }
    let value = text.prompt()?;
    value.parse().map_err(|_| anyhow!("Invalid chunk_size"))
}

fn set_graph_hops(default_value: usize) -> Result<usize> {
    let value = Text::new("Set graph expansion hops:")
        .with_default(&default_value.to_string())
        .with_help_message("Number of hops to expand from matched entities (0 = seed nodes only, 1 = direct neighbors, 2 = neighbors of neighbors)")
        .with_validator(move |text: &str| {
            let out = match text.parse::<usize>() {
                Ok(_) => Validation::Valid,
                _ => Validation::Invalid("Must be a non-negative integer".into()),
            };
            Ok(out)
        })
        .prompt()?;
    value.parse().map_err(|_| anyhow!("Invalid graph_hops"))
}

fn set_chunk_overlay(default_value: usize) -> Result<usize> {
    let value = Text::new("Set chunk overlay:")
        .with_default(&default_value.to_string())
        .with_validator(move |text: &str| {
            let out = match text.parse::<usize>() {
                Ok(_) => Validation::Valid,
                Err(_) => Validation::Invalid("Must be a integer".into()),
            };
            Ok(out)
        })
        .prompt()?;
    value.parse().map_err(|_| anyhow!("Invalid chunk_overlay"))
}

fn add_documents() -> Result<Vec<String>> {
    let text = Text::new("Add documents:")
        .with_validator(required!("This field is required"))
        .with_help_message("e.g. file;dir/;dir/**/*.{md,mdx};loader:resource;url;website/**")
        .prompt()?;
    let paths = text
        .split(';')
        .filter_map(|v| {
            let v = v.trim().to_string();
            if v.is_empty() { None } else { Some(v) }
        })
        .collect();
    Ok(paths)
}

async fn resolve_paths<T: AsRef<str>>(
    loaders: &HashMap<String, String>,
    paths: &[T],
) -> Result<(
    IndexSet<String>,
    IndexSet<String>,
    IndexSet<String>,
    IndexSet<String>,
    IndexSet<String>,
)> {
    let mut document_paths = IndexSet::new();
    let mut recursive_urls = IndexSet::new();
    let mut urls = IndexSet::new();
    let mut protocol_paths = IndexSet::new();
    let mut absolute_paths = vec![];
    for path in paths {
        let path = path.as_ref().trim();
        if is_url(path) {
            if let Some(start_url) = path.strip_suffix("**") {
                recursive_urls.insert(start_url.to_string());
            } else {
                urls.insert(path.to_string());
            }
            document_paths.insert(path.to_string());
        } else if is_loader_protocol(loaders, path) {
            protocol_paths.insert(path.to_string());
            document_paths.insert(path.to_string());
        } else {
            let resolved_path = resolve_home_dir(path);
            let absolute_path = to_absolute_path(&resolved_path)
                .with_context(|| format!("Invalid path '{path}'"))?;
            absolute_paths.push(resolved_path);
            document_paths.insert(absolute_path);
        }
    }
    let local_paths = expand_glob_paths(&absolute_paths, false).await?;
    Ok((
        document_paths,
        recursive_urls,
        urls,
        protocol_paths,
        local_paths,
    ))
}

fn progress(spinner: &Option<Spinner>, message: String) {
    if let Some(spinner) = spinner {
        let _ = spinner.set_message(message);
    }
}

/// Decide whether a just-loaded document may skip re-chunking and re-embedding.
///
/// Returns the position of the matching `FileId` within `to_deleted[hash]`, together with
/// that `FileId`. The caller needs the position to un-mark the file for deletion. `None`
/// means "ingest this document": either a full re-ingest was requested, or no
/// already-indexed file has both this content hash and this path.
fn find_hash_skip(
    force_reingest: bool,
    to_deleted: &IndexMap<String, Vec<FileId>>,
    files: &IndexMap<FileId, RagFile>,
    hash: &str,
    path: &str,
) -> Option<(usize, FileId)> {
    if force_reingest {
        return None;
    }
    let file_ids = to_deleted.get(hash)?;
    file_ids
        .iter()
        .enumerate()
        .find(|(_, v)| files[*v].path == path)
        .map(|(i, v)| (i, *v))
}

/// Global score sort + dedup keeping the best score per document.
///
/// NO overall cap: each `provider.vector_search` call already returns <= top_k,
/// so the pool is bounded by `top_k * query_chunks`, and `reciprocal_rank_fusion`
/// truncates to `top_k` itself. Capping here would let whichever query chunk has
/// the strongest absolute scores crowd out every other chunk's hits.
///
/// Free function (not a method) so it is unit-testable without an embeddings client.
fn merge_vector_results(mut results: Vec<(DocumentId, f32)>) -> Vec<(DocumentId, f32)> {
    debug_assert!(
        results.iter().all(|(_, score)| score.is_finite()),
        "provider returned a non-finite score; NaN silently degrades sort order"
    );
    results.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut seen = IndexSet::new();
    results
        .into_iter()
        .filter(|(id, _)| seen.insert(*id))
        .collect()
}

fn reciprocal_rank_fusion(
    list_of_document_ids: Vec<Vec<DocumentId>>,
    list_of_weights: Vec<f32>,
    top_k: usize,
) -> Vec<DocumentId> {
    let rrf_k = top_k * 2;
    let mut map: IndexMap<DocumentId, f32> = IndexMap::new();
    for (document_ids, weight) in list_of_document_ids.into_iter().zip(list_of_weights) {
        for (index, &item) in document_ids.iter().enumerate() {
            *map.entry(item).or_default() += (1.0 / ((rrf_k + index + 1) as f32)) * weight;
        }
    }
    let mut sorted_items: Vec<(DocumentId, f32)> = map.into_iter().collect();
    sorted_items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));

    sorted_items
        .into_iter()
        .take(top_k)
        .map(|(v, _)| v)
        .collect()
}

/// Map an embedding model id to its vector dimension.
///
/// The DuckDB `FLOAT[N]` column type and its HNSW index are fixed at schema-creation
/// time, so this value must be decided before the first insert. An unrecognized model
/// falls back to 1536; if that is wrong, DuckDB raises a dimension-mismatch error on
/// the first insert rather than silently corrupting the schema, and the recovery is to
/// delete the sidecar and re-ingest from source.
fn embedding_dim_for_model(model_id: &str) -> usize {
    match model_id {
        m if m.contains("3-large") => 3072,
        m if m.contains("3-small") || m.contains("ada-002") => 1536,
        m if m.contains("nomic-embed-text") || m.contains("all-minilm") => 768,
        m if m.contains("jina-embeddings-v2") => 1024,
        _ => 1536,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_dim_for_model_maps_known_models() {
        assert_eq!(embedding_dim_for_model("text-embedding-3-large"), 3072);
        assert_eq!(embedding_dim_for_model("text-embedding-3-small"), 1536);
        assert_eq!(embedding_dim_for_model("text-embedding-ada-002"), 1536);
        assert_eq!(embedding_dim_for_model("nomic-embed-text"), 768);
        assert_eq!(embedding_dim_for_model("all-minilm"), 768);
        assert_eq!(embedding_dim_for_model("jina-embeddings-v2-base-en"), 1024);
        // Unknown models fall back to the OpenAI-compatible default.
        assert_eq!(embedding_dim_for_model("some-unknown-model"), 1536);
    }

    #[test]
    fn document_id_round_trip() {
        let id = DocumentId::new(5, 17);
        let (file, doc) = id.split();
        assert_eq!(file, 5);
        assert_eq!(doc, 17);
    }

    #[test]
    fn document_id_zero_zero() {
        let id = DocumentId::new(0, 0);
        let (file, doc) = id.split();
        assert_eq!(file, 0);
        assert_eq!(doc, 0);
    }

    #[test]
    fn document_id_large_values() {
        let id = DocumentId::new(1000, 9999);
        let (file, doc) = id.split();
        assert_eq!(file, 1000);
        assert_eq!(doc, 9999);
    }

    #[test]
    fn document_id_debug_format() {
        let id = DocumentId::new(3, 7);
        let formatted = format!("{id:?}");
        assert_eq!(formatted, "3-7");
    }

    #[test]
    fn document_id_equality() {
        let a = DocumentId::new(1, 2);
        let b = DocumentId::new(1, 2);
        assert_eq!(a, b);
    }

    #[test]
    fn document_id_inequality() {
        let a = DocumentId::new(1, 2);
        let b = DocumentId::new(1, 3);
        assert_ne!(a, b);
    }

    #[test]
    fn document_id_ordering() {
        let a = DocumentId::new(0, 1);
        let b = DocumentId::new(1, 0);
        assert!(a < b);
    }

    #[test]
    fn rag_document_new() {
        let doc = RagDocument::new("hello world");
        assert_eq!(doc.page_content, "hello world");
        assert!(doc.metadata.is_empty());
    }

    #[test]
    fn rag_document_default() {
        let doc = RagDocument::default();
        assert_eq!(doc.page_content, "");
        assert!(doc.metadata.is_empty());
    }

    #[test]
    fn rag_data_new_defaults() {
        let data = RagData::new(
            "model".into(),
            1000,
            20,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        assert_eq!(data.embedding_model, "model");
        assert_eq!(data.chunk_size, 1000);
        assert_eq!(data.chunk_overlap, 20);
        assert_eq!(data.top_k, 5);
        assert!(data.reranker_model.is_none());
        assert!(data.files.is_empty());
        assert!(data.vectors.is_empty());
        assert!(data.document_paths.is_empty());
        assert_eq!(data.next_file_id, 0);
    }

    #[test]
    fn rag_data_iter_documents_yields_all_documents_in_file_order() {
        let mut data = RagData::new(
            "m".into(),
            100,
            10,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        let file = RagFile {
            hash: "abc".into(),
            path: "test.txt".into(),
            documents: vec![RagDocument::new("first"), RagDocument::new("second")],
        };
        data.files.insert(0, file);

        let documents: Vec<_> = data
            .iter_documents()
            .map(|(id, doc)| (id, doc.page_content.as_str()))
            .collect();
        assert_eq!(
            documents,
            vec![
                (DocumentId::new(0, 0), "first"),
                (DocumentId::new(0, 1), "second"),
            ]
        );
    }

    #[test]
    fn rag_data_iter_documents_is_empty_without_files() {
        let data = RagData::new(
            "m".into(),
            100,
            10,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        assert_eq!(data.iter_documents().count(), 0);
    }

    /// The document id space is `files`, never `vectors`: `add`'s zip truncates
    /// silently, so a vector may exist for an id no file provides. Content lookup
    /// and BM25 both key off this iterator and must agree.
    #[test]
    fn rag_data_iter_documents_ignores_vector_only_ids() {
        let mut data = RagData::new(
            "m".into(),
            100,
            10,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        let file = RagFile {
            hash: "abc".into(),
            path: "test.txt".into(),
            documents: vec![RagDocument::new("only one")],
        };
        data.files.insert(0, file);
        data.vectors.insert(DocumentId::new(0, 5), vec![1.0]);

        let ids: Vec<_> = data.iter_documents().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![DocumentId::new(0, 0)]);
    }

    #[test]
    fn rag_data_del_removes_files_and_vectors() {
        let mut data = RagData::new(
            "m".into(),
            100,
            10,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        let file = RagFile {
            hash: "abc".into(),
            path: "test.txt".into(),
            documents: vec![RagDocument::new("doc")],
        };
        data.files.insert(0, file);
        let doc_id = DocumentId::new(0, 0);
        data.vectors.insert(doc_id, vec![0.1, 0.2, 0.3]);

        assert!(data.files.contains_key(&0));
        assert!(data.vectors.contains_key(&doc_id));

        data.del(vec![0]);

        assert!(!data.files.contains_key(&0));
        assert!(!data.vectors.contains_key(&doc_id));
    }

    #[test]
    fn rag_data_del_nonexistent_is_noop() {
        let mut data = RagData::new(
            "m".into(),
            100,
            10,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        data.del(vec![99]);
        assert!(data.files.is_empty());
    }

    #[test]
    fn rag_data_add_inserts_files_and_vectors() {
        let mut data = RagData::new(
            "m".into(),
            100,
            10,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        let file = RagFile {
            hash: "xyz".into(),
            path: "new.txt".into(),
            documents: vec![RagDocument::new("content")],
        };
        let doc_id = DocumentId::new(0, 0);
        let embeddings = vec![vec![0.5, 0.6, 0.7]];

        data.add(1, vec![(0, file)], vec![doc_id], embeddings);

        assert_eq!(data.next_file_id, 1);
        assert!(data.files.contains_key(&0));
        assert!(data.vectors.contains_key(&doc_id));
        assert_eq!(data.vectors[&doc_id], vec![0.5, 0.6, 0.7]);
    }

    #[test]
    fn rag_template_contains_placeholders() {
        assert!(RAG_TEMPLATE.contains("__CONTEXT__"));
        assert!(RAG_TEMPLATE.contains("__SOURCES__"));
        assert!(RAG_TEMPLATE.contains("__INPUT__"));
    }

    #[test]
    fn get_separators_returns_language_specific() {
        let rs_seps = get_separators("rs");
        assert!(rs_seps.iter().any(|s| s.contains("fn ")));

        let py_seps = get_separators("py");
        assert!(py_seps.iter().any(|s| s.contains("def ")));

        let md_seps = get_separators("md");
        assert!(md_seps.iter().any(|s| s.contains("# ")));
    }

    #[test]
    fn get_separators_unknown_returns_defaults() {
        let seps = get_separators("xyz");
        assert_eq!(seps, DEFAULT_SEPARATORS.to_vec());
    }

    #[test]
    fn get_separators_all_known_extensions() {
        let known = [
            "c", "cc", "cpp", "go", "java", "js", "mjs", "cjs", "php", "proto", "py", "rst", "rb",
            "rs", "scala", "swift", "md", "mkd", "tex", "htm", "html", "sol",
        ];
        for ext in known {
            let seps = get_separators(ext);
            assert_ne!(
                seps,
                DEFAULT_SEPARATORS.to_vec(),
                "Extension '{ext}' should have language-specific separators"
            );
        }
    }

    #[test]
    fn rag_data_build_bm25_empty() {
        let data = RagData::new(
            "m".into(),
            100,
            10,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        let engine = data.build_bm25();
        let results = engine.search("anything", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn rag_data_build_bm25_finds_documents() {
        let mut data = RagData::new(
            "m".into(),
            100,
            10,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        let file = RagFile {
            hash: "h".into(),
            path: "test.txt".into(),
            documents: vec![
                RagDocument::new("rust programming language"),
                RagDocument::new("python scripting language"),
            ],
        };
        data.files.insert(0, file);

        let engine = data.build_bm25();
        let results = engine.search("rust", 5);
        assert!(!results.is_empty());
        let top = &results[0];
        let (file_idx, doc_idx) = top.document.id.split();
        assert_eq!(file_idx, 0);
        assert_eq!(doc_idx, 0);
    }

    #[test]
    fn rag_data_del_removes_graph_entities() {
        use super::graph::{ExtractedEntity, ExtractionResult};
        let mut data = RagData::new(
            "m".into(),
            100,
            10,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        let file = RagFile {
            hash: "abc".into(),
            path: "test.txt".into(),
            documents: vec![RagDocument::new("Python is great")],
        };
        data.files.insert(0, file);
        let doc_id = DocumentId::new(0, 0);
        data.knowledge_graph.merge(
            doc_id,
            ExtractionResult {
                entities: vec![ExtractedEntity {
                    name: "Python".to_string(),
                    entity_type: "TECHNOLOGY".to_string(),
                    description: None,
                }],
                relationships: vec![],
            },
        );
        assert!(
            data.knowledge_graph.entity_index.contains_key("python"),
            "entity should exist before del"
        );
        data.del(vec![0]);
        assert!(
            !data.knowledge_graph.entity_index.contains_key("python"),
            "entity should be removed after del"
        );
    }

    #[test]
    fn reciprocal_rank_fusion_empty_lists() {
        let result = super::reciprocal_rank_fusion(vec![], vec![], 5);
        assert!(result.is_empty(), "empty input should produce empty output");
    }

    #[test]
    fn reciprocal_rank_fusion_deduplicates_across_signals() {
        let doc_a = DocumentId::new(0, 0);
        let doc_b = DocumentId::new(0, 1);
        let result = super::reciprocal_rank_fusion(
            vec![vec![doc_a, doc_b], vec![doc_a, doc_b]],
            vec![1.0, 1.0],
            5,
        );
        let unique: std::collections::HashSet<_> = result.iter().collect();
        assert_eq!(
            unique.len(),
            result.len(),
            "each document should appear at most once"
        );
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn reciprocal_rank_fusion_respects_top_k() {
        let docs: Vec<DocumentId> = (0..10).map(|i| DocumentId::new(0, i)).collect();
        let result = super::reciprocal_rank_fusion(vec![docs], vec![1.0], 3);
        assert_eq!(result.len(), 3, "result should be capped at top_k=3");
    }

    #[test]
    fn reciprocal_rank_fusion_weights_affect_ranking() {
        let doc_a = DocumentId::new(0, 0);
        let doc_b = DocumentId::new(0, 1);
        let result = super::reciprocal_rank_fusion(
            vec![vec![doc_a, doc_b], vec![doc_b, doc_a]],
            vec![10.0, 1.0],
            2,
        );
        assert_eq!(
            result[0], doc_a,
            "higher-weight signal's top doc should rank first"
        );
    }

    #[test]
    fn merge_vector_results_empty_input() {
        let result = super::merge_vector_results(vec![]);
        assert!(result.is_empty(), "empty input should produce empty output");
    }

    #[test]
    fn merge_vector_results_keeps_best_score_per_document() {
        let doc = DocumentId::new(0, 0);
        let result = super::merge_vector_results(vec![(doc, 0.2), (doc, 0.9)]);
        assert_eq!(result.len(), 1, "a document must not be double-counted");
        assert_eq!(result[0].0, doc);
        assert_eq!(
            result[0].1, 0.9,
            "dedup must keep the highest score, not the first seen"
        );
    }

    #[test]
    fn merge_vector_results_sorts_globally_by_descending_score() {
        let doc_a = DocumentId::new(0, 0);
        let doc_b = DocumentId::new(1, 0);
        let doc_c = DocumentId::new(2, 0);
        // Interleaved as two per-chunk hit lists would arrive: concatenating them
        // would yield a, c, b — only a global sort produces c, a, b.
        let result = super::merge_vector_results(vec![(doc_a, 0.5), (doc_c, 0.9), (doc_b, 0.1)]);
        let ids: Vec<DocumentId> = result.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![doc_c, doc_a, doc_b]);
    }

    #[test]
    fn merge_vector_results_does_not_truncate() {
        let input: Vec<(DocumentId, f32)> = (0..10)
            .map(|i| (DocumentId::new(i, 0), i as f32 / 10.0))
            .collect();
        let result = super::merge_vector_results(input);
        assert_eq!(
            result.len(),
            10,
            "merging must not cap the pool; truncation belongs to reciprocal_rank_fusion"
        );
    }

    fn hash_skip_fixture() -> (IndexMap<FileId, RagFile>, IndexMap<String, Vec<FileId>>) {
        let mut files: IndexMap<FileId, RagFile> = Default::default();
        files.insert(
            7,
            RagFile {
                hash: "abc".into(),
                path: "test.txt".into(),
                documents: vec![RagDocument::new("unchanged")],
            },
        );
        let mut to_deleted: IndexMap<String, Vec<FileId>> = Default::default();
        to_deleted.insert("abc".into(), vec![7]);
        (files, to_deleted)
    }

    #[test]
    fn force_reingest_re_embeds_hash_identical_files() {
        let (files, to_deleted) = hash_skip_fixture();
        assert_eq!(
            find_hash_skip(true, &to_deleted, &files, "abc", "test.txt"),
            None,
            "a forced re-ingest must not skip an unchanged file"
        );
    }

    #[test]
    fn refresh_without_force_still_hash_skips() {
        let (files, to_deleted) = hash_skip_fixture();
        assert_eq!(
            find_hash_skip(false, &to_deleted, &files, "abc", "test.txt"),
            Some((0, 7)),
            "an unchanged file should be skipped and un-marked for deletion"
        );
    }

    #[test]
    fn find_hash_skip_returns_none_on_path_change() {
        let (files, to_deleted) = hash_skip_fixture();
        assert_eq!(
            find_hash_skip(false, &to_deleted, &files, "abc", "moved.txt"),
            None
        );
        assert_eq!(
            find_hash_skip(true, &to_deleted, &files, "abc", "moved.txt"),
            None
        );
    }

    #[test]
    fn ragdata_new_has_yaml_driver_and_not_attached() {
        let data = RagData::new(
            "text-embedding-3-small".to_string(),
            1024,
            50,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        assert_eq!(data.driver, "yaml");
        assert!(!data.attached);
    }

    #[test]
    fn ragdata_deserializes_without_driver_field() {
        let yaml = "
embedding_model: text-embedding-3-small
chunk_size: 1024
chunk_overlap: 50
top_k: 5
next_file_id: 0
document_paths: []
files: {}
vectors: {}
";
        let data: RagData = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(data.driver, "yaml");
        assert!(!data.attached);
    }

    #[test]
    fn ragdata_round_trips_driver_and_attached() {
        let mut data = RagData::new(
            "text-embedding-3-small".to_string(),
            1024,
            50,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        data.driver = "qdrant".to_string();
        data.attached = true;

        let yaml = serde_yaml::to_string(&data).unwrap();
        let restored: RagData = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(restored.driver, "qdrant");
        assert!(restored.attached);
    }

    #[test]
    fn ragdata_validate_rejects_yaml_attached() {
        let mut data = RagData::new(
            "m".into(),
            1024,
            50,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        data.attached = true;
        let err = data.validate().unwrap_err().to_string();
        assert!(err.contains("cannot be attached"), "got: {err}");
    }

    #[test]
    fn ragdata_validate_accepts_qdrant_attached() {
        let mut data = RagData::new(
            "m".into(),
            1024,
            50,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        data.driver = "qdrant".to_string();
        data.attached = true;
        assert!(data.validate().is_ok());
    }

    #[test]
    fn ragdata_validate_rejects_zero_top_k_from_a_truncated_yaml() {
        let yaml = "
embedding_model: text-embedding-3-small
chunk_size: 1024
chunk_overlap: 50
";
        let data: RagData = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(data.top_k, 0, "a missing top_k must default to 0");
        let err = data.validate().unwrap_err().to_string();
        assert!(err.contains("top_k must be >= 1"), "got: {err}");
    }

    #[test]
    fn ragdata_validate_rejects_zero_chunk_size_when_not_attached() {
        let yaml = "
embedding_model: text-embedding-3-small
top_k: 5
";
        let data: RagData = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(data.chunk_size, 0);
        let err = data.validate().unwrap_err().to_string();
        assert!(err.contains("chunk_size must be >= 1"), "got: {err}");
    }

    #[test]
    fn ragdata_validate_allows_zero_chunk_size_when_attached() {
        let yaml = "
driver: qdrant
attached: true
embedding_model: text-embedding-3-small
top_k: 5
";
        let data: RagData = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(data.chunk_size, 0);
        assert!(data.validate().is_ok());
    }

    #[test]
    fn ragdata_validate_rejects_overlap_not_less_than_chunk_size() {
        let data = RagData::new(
            "m".into(),
            100,
            100,
            None,
            5,
            None,
            GraphRagConfig::default(),
        );
        let err = data.validate().unwrap_err().to_string();
        assert!(err.contains("chunk_overlap"), "got: {err}");
    }
}
