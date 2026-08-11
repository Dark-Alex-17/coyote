use crate::rag::provider::RagProvider;
use crate::rag::{DocumentId, RagData};
use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use duckdb::Connection;
use duckdb::types::Value;
use indexmap::IndexMap;
use log::warn;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Serializes `INSTALL` across every thread in this process. DuckDB installs an
/// extension by downloading it to a temp file and then MOVING that file into
/// `~/.duckdb/extensions/...`. Two threads installing the same extension at once
/// both perform that move; on Windows the loser's move targets a file the winner
/// already holds open and fails with "Access is denied", where POSIX would let the
/// replacement through. Guards nothing but the install step, so it is never held
/// across a `DuckDbProvider::conn` guard and cannot invert lock order.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Derive the DuckDB sidecar path from a RAG's YAML path: `docs.yaml` -> `docs.duckdb`.
pub(crate) fn duckdb_path_from_yaml(yaml_path: &Path) -> PathBuf {
    yaml_path.with_extension("duckdb")
}

pub struct DuckDbProvider {
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
    /// Embedding dimension; fixed at open time because the `FLOAT[N]` column depends on it.
    dim: usize,
    /// True once an FTS index has been built on `documents`. Until then
    /// `fts_main_documents.match_bm25` does not exist and any keyword query would
    /// fail with a DuckDB catalog error. Backs `has_native_keyword_search`.
    fts_ready: AtomicBool,
}

impl DuckDbProvider {
    /// Open (or create) the DuckDB file. `dim` is the embedding vector dimension,
    /// supplied by the caller who knows the model.
    pub fn open(db_path: &Path, dim: usize) -> Result<Self> {
        let conn = Connection::open(db_path).with_context(|| {
            format!(
                "Failed to open the DuckDB store at '{}'. If another Coyote process (or \
                 another window) has this RAG open, close it and retry — a duckdb RAG can \
                 only be open in ONE process at a time. Unlike the yaml driver, its data \
                 lives in a single file with an exclusive lock.",
                db_path.display()
            )
        })?;
        // Statement order is load-bearing. `hnsw_enable_experimental_persistence` is
        // registered BY the vss extension, so setting it before `LOAD vss` fails with
        // "Setting with name ... is not in the catalog, but it exists in the vss
        // extension". Omitting it entirely makes CREATE INDEX ... USING HNSW on a
        // file-backed database fail with "HNSW index persistence is not yet supported
        // by default". ensure vss (installing it if missing) -> ensure fts -> SET ->
        // CREATE INDEX.
        Self::ensure_extension(&conn, "vss")?;
        Self::ensure_extension(&conn, "fts")?;
        conn.execute_batch(&format!(
            "SET hnsw_enable_experimental_persistence = true;
             CREATE TABLE IF NOT EXISTS vectors (
                 doc_id UBIGINT PRIMARY KEY,
                 embedding FLOAT[{dim}]
             );
             CREATE INDEX IF NOT EXISTS hnsw_idx
                 ON vectors USING HNSW (embedding)
                 WITH (metric = 'cosine');
             CREATE TABLE IF NOT EXISTS documents (
                 doc_id UBIGINT PRIMARY KEY,
                 page_content TEXT NOT NULL
             );"
        ))
        .context("Failed to initialize DuckDB schema")?;
        // A reopened file may already carry a live FTS index from a previous session,
        // in which case keyword search works immediately.
        let fts_exists = Self::probe_fts_index(&conn);
        Ok(Self {
            path: db_path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
            dim,
            fts_ready: AtomicBool::new(fts_exists),
        })
    }

    /// Make a DuckDB extension available on `conn`, installing it if this machine does
    /// not have it yet. `LOAD` is attempted first so an extension that is already
    /// installed costs nothing and never touches the network; `INSTALL` is only reached
    /// once, on a machine seeing the extension for the first time.
    ///
    /// `LOAD` is per-connection and so runs on every connection; only `INSTALL` is
    /// serialized, and the second `LOAD` under the lock is what keeps it to one
    /// install. Without that re-check, every thread that queued behind the winner
    /// would still run a redundant `INSTALL` and re-trigger the same file move.
    fn ensure_extension(conn: &Connection, name: &str) -> Result<()> {
        if conn.execute_batch(&format!("LOAD {name};")).is_ok() {
            return Ok(());
        }
        // A poisoned lock means some other thread panicked mid-install; the lock owns
        // no state to corrupt, so recover rather than failing every later open.
        let _install_guard = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Re-check now that we hold the lock: whoever held it before us may already
        // have installed the extension, in which case this `LOAD` finds it on disk.
        if conn.execute_batch(&format!("LOAD {name};")).is_ok() {
            return Ok(());
        }
        conn.execute_batch(&format!("INSTALL {name};"))
            .with_context(|| {
                format!(
                    "Failed to install the DuckDB `{name}` extension. The duckdb RAG driver \
                     needs it, and downloading it needs network access the first time. If this \
                     machine is offline, connect once and retry, or run `INSTALL {name};` \
                     yourself from a DuckDB shell."
                )
            })?;
        conn.execute_batch(&format!("LOAD {name};"))
            .with_context(|| {
                format!("Failed to load the DuckDB `{name}` extension after installing it.")
            })
    }

    /// Read all `(doc_id, embedding)` pairs so `create()` can hydrate `data.vectors`
    /// from disk. This is what makes the next incremental sync non-destructive, and is
    /// mandatory rather than an optimization.
    ///
    /// 🔴 THIS PATTERN IS DUCKDB-ONLY. NEVER write the qdrant equivalent. Qdrant Cosine
    /// collections L2-normalize stored vectors on write, so hydrating `data.vectors`
    /// from a Qdrant read fills it with NORMALIZED vectors and the next save() destroys
    /// the originals permanently. The symmetry with this function is exactly why that
    /// bug is easy to introduce.
    ///
    /// ALL-OR-NOTHING. An empty `vectors` table is `Ok(empty)`; ANY decode failure or
    /// non-finite embedding is an `Err`. A partially-hydrated map is worse than no map:
    /// `rebuild_indexes` does `CREATE OR REPLACE TABLE` and writes exactly what it is
    /// given, so a thinned map is committed as the new truth on the next sync.
    pub(crate) fn read_all_vectors(&self) -> Result<IndexMap<DocumentId, Vec<f32>>> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare("SELECT doc_id, embedding FROM vectors")?;
        let raw: Vec<(u64, Vec<f32>)> = stmt
            .query_map([], |row| {
                let id: u64 = row.get(0)?;
                // The duckdb crate does not implement `FromSql` for `Vec<f32>`, so the
                // column is read as a `Value` and destructured. A FLOAT[N] column yields
                // `Value::Array`, a FLOAT[] column yields `Value::List`; match both so
                // the reader survives a file written under either schema.
                let embedding: Vec<f32> = match row.get::<_, Value>(1)? {
                    Value::Array(vals) | Value::List(vals) => vals
                        .into_iter()
                        .map(|v| match v {
                            Value::Float(f) => f,
                            Value::Double(d) => d as f32,
                            _ => f32::NAN,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                Ok((id, embedding))
            })?
            .collect::<duckdb::Result<Vec<_>>>()
            .with_context(|| {
                format!(
                    "Failed to decode the `vectors` table in '{}'. The DuckDB sidecar is \
                     unreadable; delete it and run `.rebuild rag` to re-ingest from source.",
                    self.path.display()
                )
            })?;

        // Validate OUTSIDE the closure so the error can name the offending row. A
        // non-finite embedding is NOT droppable: dropping it thins the map, and the
        // thinned map is what the next CREATE OR REPLACE commits.
        let mut out = IndexMap::with_capacity(raw.len());
        for (id, embedding) in raw {
            if embedding.is_empty() || embedding.iter().any(|f| !f.is_finite()) {
                bail!(
                    "Vector for doc_id {id} in '{}' is empty or contains a non-finite \
                     value. Refusing to hydrate a partial vector map — that would erase \
                     the remaining vectors on the next sync. Delete the sidecar and run \
                     `.rebuild rag` to re-ingest from source.",
                    self.path.display()
                );
            }
            out.insert(DocumentId(id as usize), embedding);
        }

        Ok(out)
    }

    /// Does a LIVE FTS index exist on `documents`? Probed at open time so a reopened
    /// database reports native keyword search accurately.
    ///
    /// A schema-existence check alone is NOT sufficient. After
    /// `CREATE OR REPLACE TABLE documents`, the `fts_main_documents` schema still
    /// exists and `match_bm25` still SUCCEEDS, but returns zero rows for every term.
    /// The index is silently dead. The probe therefore asserts that a KNOWN row comes
    /// back rather than merely that the call did not error.
    fn probe_fts_index(conn: &Connection) -> bool {
        let schema_exists = conn
            .query_row(
                "SELECT COUNT(*) FROM duckdb_schemas() WHERE schema_name = 'fts_main_documents'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        if !schema_exists {
            return false;
        }
        conn.query_row(
            "SELECT COUNT(*) FROM documents d
             WHERE fts_main_documents.match_bm25(
                 d.doc_id,
                 (SELECT string_split(trim(page_content), ' ')[1]
                  FROM documents WHERE length(trim(page_content)) > 0 LIMIT 1)
             ) IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    /// Lock the shared connection, converting mutex poisoning into an `anyhow` error.
    ///
    /// Never `.lock().unwrap()` here: a panic anywhere inside a locked scope poisons the
    /// mutex permanently, and an unwrap would then turn every subsequent RAG query into
    /// a panic for the remaining life of the process.
    fn lock_conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| anyhow!("DuckDB connection mutex was poisoned: {e}"))
    }
}

#[async_trait]
impl RagProvider for DuckDbProvider {
    // NOTE: the MutexGuard is held across the body of these `async fn`s. That is sound
    // ONLY because no `.await` appears inside a locked scope. Adding one would make the
    // generated future non-`Send` and break the `RagProvider: Send + Sync` bound.
    async fn vector_search(
        &self,
        embedding: &[f32],
        top_k: usize,
        min_score: f32,
    ) -> Result<Vec<(DocumentId, f32)>> {
        if embedding.iter().any(|f| !f.is_finite()) {
            bail!("Query embedding contains a non-finite value (NaN or infinity)");
        }
        let vals: String = embedding
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let dim = self.dim;
        let conn = self.lock_conn()?;
        // array_cosine_distance requires a FLOAT[N] ARRAY, not the LIST type FLOAT[].
        // ORDER BY distance ASC is required for the planner to use hnsw_idx; the
        // similarity form (DESC) does NOT trigger the ANN index. Distance is converted
        // back to a similarity score on return.
        let sql = format!(
            "SELECT doc_id, \
             array_cosine_distance(embedding, [{vals}]::FLOAT[{dim}]) AS distance \
             FROM vectors ORDER BY distance ASC LIMIT {top_k}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let results = stmt
            .query_map([], |row| {
                let id: u64 = row.get(0)?;
                // `array_cosine_distance` on a FLOAT[N] column returns FLOAT (f32), NOT
                // DOUBLE. Reading it as f64 raises InvalidColumnType INSIDE the closure,
                // which a bare `.filter_map(|r| r.ok())` would silently discard,
                // yielding ZERO results with no error and no log line.
                let distance: f32 = match row.get::<_, Value>(1)? {
                    Value::Float(f) => f,
                    Value::Double(d) => d as f32,
                    other => {
                        warn!("unexpected distance type from DuckDB: {other:?}");
                        return Err(duckdb::Error::InvalidQuery);
                    }
                };
                Ok((DocumentId(id as usize), 1.0_f32 - distance))
            })?
            // Log-and-drop rather than a bare `.ok()`: a systematic decode failure here
            // is otherwise indistinguishable from "no matches".
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!("vector_search row decode failed: {e}");
                    None
                }
            })
            .filter(|(_, score)| *score > min_score)
            .collect();

        Ok(results)
    }

    async fn fetch_content(&self, ids: &[DocumentId]) -> Result<Vec<(DocumentId, String)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.lock_conn()?;
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql =
            format!("SELECT doc_id, page_content FROM documents WHERE doc_id IN ({placeholders})");
        let params: Vec<Value> = ids.iter().map(|id| Value::UBigInt(id.0 as u64)).collect();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows: Vec<(DocumentId, String)> = stmt
            .query_map(duckdb::params_from_iter(params.iter()), |row| {
                let id: u64 = row.get(0)?;
                let text: String = row.get(1)?;
                Ok((DocumentId(id as usize), text))
            })?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!("fetch_content row decode failed: {e}");
                    None
                }
            })
            .collect();
        let position: HashMap<DocumentId, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();

        rows.sort_by_key(|(id, _)| position.get(id).copied().unwrap_or(usize::MAX));

        Ok(rows)
    }

    async fn rebuild_indexes(&mut self, data: &RagData, _full_rebuild: bool) -> Result<()> {
        // Local on-disk state. A wholesale table reset plus re-INSERT is always
        // correct, so the incremental/full distinction is ignored.
        //
        // 🔴 ANTI-WIPE GUARD. The CREATE OR REPLACE TABLE below writes exactly what
        // `data.vectors` holds. An empty map on a RAG that HAS indexed files, against a
        // store that ALREADY holds vectors, is always a bug; a failed/skipped
        // hydration, or a caller that emptied the live map. Refuse rather than commit
        // the loss. Every legitimate empty-vector rebuild also has an empty `files`
        // (a fresh RAG; the zero-document tests), so this cannot fire on a correct call.
        // The `existing > 0` conjunct is load-bearing, not belt-and-braces: a populated
        // fixture rebuilt against a fresh temp database has count 0 and must proceed.
        if data.vectors.is_empty() && !data.files.is_empty() {
            let existing: i64 = {
                // Scoped: the guard MUST be dropped before `lock_conn()` is taken again
                // below. `Mutex` is not reentrant; holding both self-deadlocks at
                // runtime, with no compile error.
                let conn = self.lock_conn()?;
                conn.query_row("SELECT count(*) FROM vectors", [], |r| r.get(0))
                    .context("Failed to count existing vectors before rebuild")?
            };
            if existing > 0 {
                bail!(
                    "Refusing to rebuild the DuckDB store at '{}': the in-memory vector \
                     map is empty, but this RAG has {} indexed file(s) and the store \
                     already holds {existing} vector(s). Rebuilding would erase them. \
                     Vector hydration failed, or `data.vectors` was cleared on a live \
                     Rag. Nothing was written; the store is intact.",
                    self.path.display(),
                    data.files.len()
                );
            }
        }
        // Validate BEFORE opening the transaction so a bad embedding aborts before any
        // write, not mid-write with the guard held.
        for (doc_id, embedding) in &data.vectors {
            if embedding.iter().any(|f| !f.is_finite()) {
                bail!(
                    "Embedding for document {} contains a non-finite value",
                    doc_id.0
                );
            }
        }
        let dim = self.dim;
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction()
            .context("Failed to begin DuckDB transaction")?;

        // Use `CREATE OR REPLACE TABLE`, not `DELETE FROM`. Deleting rows and
        // re-inserting the SAME primary keys inside ONE transaction violates DuckDB's PK
        // constraint: the index holds deleted keys until commit, so the second rebuild
        // fails with a duplicate-key error. `CREATE OR REPLACE` drops the table and its
        // indexes atomically, leaving no stale keys. It also drops the HNSW index, which
        // is recreated after commit.
        tx.execute_batch(&format!(
            "CREATE OR REPLACE TABLE vectors (
                 doc_id UBIGINT PRIMARY KEY,
                 embedding FLOAT[{dim}]
             );
             CREATE OR REPLACE TABLE documents (
                 doc_id UBIGINT PRIMARY KEY,
                 page_content TEXT NOT NULL
             );"
        ))
        .context("Failed to reset DuckDB tables")?;

        {
            // Plain INSERT: tables are empty after CREATE OR REPLACE, so no PK conflict.
            // The embedding is bound as TEXT and cast in SQL. The duckdb crate's
            // `bind_parameter` has no arm for List/Array, so binding a vector directly
            // fails at runtime with "binding List parameters is not yet supported".
            let mut vstmt = tx.prepare(&format!(
                "INSERT INTO vectors (doc_id, embedding) VALUES (?, CAST(? AS FLOAT[{dim}]))"
            ))?;
            let mut dstmt =
                tx.prepare("INSERT INTO documents (doc_id, page_content) VALUES (?, ?)")?;

            // 🔴 TWO INDEPENDENT LOOPS. `documents` is keyed on `files`, `vectors` on
            // `vectors`. They are DIFFERENT key sets and neither is a subset of the
            // other. Do not merge these into one loop over `&data.vectors` gated on a
            // lookup into `files`: that produces keys(documents) ⊆ keys(vectors), and
            // every id in `files \ vectors` (which `RagData::add`'s zip truncation
            // really does produce) becomes a live, rankable id from BM25 and
            // graph_search that resolves to nothing; no error, no log line.
            for (id, doc) in data.iter_documents() {
                dstmt.execute(duckdb::params![id.0 as u64, doc.page_content.as_str()])?;
            }

            for (doc_id, embedding) in &data.vectors {
                // Serialize as a DuckDB array literal: "[0.1,0.2,...]". Finiteness was
                // validated above, so `to_string()` cannot emit NaN/inf here.
                let embedding_text = {
                    let mut s = String::with_capacity(embedding.len() * 12 + 2);
                    s.push('[');
                    for (i, f) in embedding.iter().enumerate() {
                        if i > 0 {
                            s.push(',');
                        }
                        s.push_str(&f.to_string());
                    }
                    s.push(']');
                    s
                };
                vstmt.execute(duckdb::params![doc_id.0 as u64, embedding_text.as_str()])?;
            }
        } // drop prepared statements before commit

        tx.commit().context("Failed to commit DuckDB transaction")?;

        // Recreate the HNSW index, dropped by CREATE OR REPLACE TABLE above. When
        // CREATE INDEX USING HNSW fails, the preceding COMMIT still returns Ok, so the
        // failure lands here; swallowing it would leave the database populated but
        // UNINDEXED, with every vector_search silently falling back to a full scan.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS hnsw_idx \
             ON vectors USING HNSW (embedding) WITH (metric = 'cosine');",
        )
        .context("Failed to recreate HNSW index")?;

        // Rebuild the FTS index (must be outside the transaction). This rebuild is
        // MANDATORY on every pass, not an optimization: CREATE OR REPLACE TABLE above
        // leaves the old fts_main_documents schema in place but DEAD; match_bm25 keeps
        // succeeding while returning zero rows for every term.
        // GUARDED on a non-empty table: FTS cannot index an empty table, and
        // rebuild_indexes is legitimately called with zero documents (a fresh RAG, and
        // three of the tests below).
        let doc_count: i64 = conn
            .query_row("SELECT count(*) FROM documents", [], |r| r.get(0))
            .context("Failed to count documents before FTS rebuild")?;

        if doc_count > 0 {
            conn.execute_batch(
                "DROP FUNCTION IF EXISTS fts_main_documents_match_bm25;
                 PRAGMA create_fts_index('documents', 'doc_id', 'page_content', overwrite=1);",
            )
            .context("Failed to rebuild DuckDB FTS index")?;
        }

        // Live only if an index was actually built. With zero documents there is no FTS
        // index, so `has_native_keyword_search()` must stay false and hybrid_search must
        // fall back to local BM25, which is also empty, and therefore correct.
        self.fts_ready.store(doc_count > 0, Ordering::Relaxed);

        Ok(())
    }

    async fn keyword_search(&self, query: &str, top_k: usize) -> Result<Vec<(DocumentId, f32)>> {
        let conn = self.lock_conn()?;
        // match_bm25 returns NULL for non-matching rows; WHERE filters them out.
        let mut stmt = conn.prepare(
            "SELECT doc_id, fts_main_documents.match_bm25(doc_id, ?) AS score
             FROM documents
             WHERE score IS NOT NULL
             ORDER BY score DESC
             LIMIT ?",
        )?;
        let results = stmt
            .query_map(duckdb::params![query, top_k as u64], |row| {
                let id: u64 = row.get(0)?;
                // Same hazard as vector_search's distance column: guessing the width
                // wrong raises InvalidColumnType INSIDE the closure, which a bare
                // `.filter_map(|r| r.ok())` silently discards, yielding ZERO keyword
                // hits, indistinguishable from "the query matched nothing".
                let score: f32 = match row.get::<_, Value>(1)? {
                    Value::Double(d) => d as f32,
                    Value::Float(f) => f,
                    other => {
                        warn!("unexpected match_bm25 score type from DuckDB: {other:?}");
                        return Err(duckdb::Error::InvalidQuery);
                    }
                };
                Ok((DocumentId(id as usize), score))
            })?
            .filter_map(|r| match r {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!("keyword_search row decode failed: {e}");
                    None
                }
            })
            .collect();
        Ok(results)
    }

    fn has_native_keyword_search(&self) -> bool {
        // NOT an unconditional `true`. The FTS schema only exists after
        // rebuild_indexes has run the pragma (or after reopening a database where a
        // previous session did). `create()` deliberately does NOT call rebuild_indexes,
        // so a freshly created RAG reaches this point with no FTS index at all.
        // Returning true there would route every query into keyword_search, whose `?`
        // would propagate a DuckDB catalog error and fail the WHOLE search.
        self.fts_ready.load(Ordering::Relaxed)
    }

    fn duplicate(&self, _data: &RagData) -> Box<dyn RagProvider> {
        // Do NOT call DuckDbProvider::open() here. `Connection::open()` instantiates a
        // NEW DuckDB *database* handle on the same file; the first handle still holds
        // the file lock, so the second open fails with a locking error. Cloning the Arc
        // shares the already-open connection, serialized by the Mutex.
        //
        // This means DuckDbProvider clones are NOT independent snapshots: state lives on
        // disk and a rebuild through one handle is immediately visible to the other.
        // That is unavoidable for any on-disk store and is handled by the discipline
        // documented on `Rag`'s Clone impl, the pre-clone instance must be discarded.
        Box::new(DuckDbProvider {
            path: self.path.clone(),
            conn: Arc::clone(&self.conn),
            dim: self.dim,
            fts_ready: AtomicBool::new(self.fts_ready.load(Ordering::Relaxed)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag::provider::RagProvider;
    use crate::rag::{RagDocument, RagFile};
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, fs};

    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(tag: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("coyote-duckdb-{tag}-{unique}.duckdb"));
            Self { path }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(self.path.with_extension("duckdb.wal"));
        }
    }

    /// ⚠️ NOTE THE EMPTY `files`. This fixture describes a RAG with ZERO documents.
    /// Since `rebuild_indexes` populates the `documents` table from
    /// `data.iter_documents()` (keyed on `files`), any test built on this helper alone
    /// exercises the `vectors` table and NOTHING ELSE. No `documents` rows, no FTS
    /// index. That is correct for the rebuild tests below, and is exactly why the
    /// FTS-flag test and the `fetch_content` tests use `populated_rag_data()` instead.
    /// Do not "simplify" them back onto this helper.
    fn minimal_rag_data() -> RagData {
        RagData {
            embedding_model: "text-embedding-3-small".to_string(),
            chunk_size: 1024,
            chunk_overlap: 50,
            top_k: 5,
            driver: "duckdb".to_string(),
            attached: false,
            ..Default::default()
        }
    }

    /// Two files, one document each; the minimum fixture that produces `documents`
    /// rows.
    fn populated_rag_data() -> RagData {
        let mut data = minimal_rag_data();
        debug_assert_eq!(DocumentId::new(0, 0), DocumentId(0));
        data.files.insert(
            0,
            RagFile {
                hash: "h0".to_string(),
                path: "/tmp/a.md".to_string(),
                documents: vec![RagDocument {
                    page_content: "alpha keyword".to_string(),
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
                    page_content: "beta keyword".to_string(),
                    metadata: Default::default(),
                }],
            },
        );
        data
    }

    #[tokio::test]
    async fn open_creates_schema() {
        let db = TempDb::new("schema");
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let conn = provider.conn.lock().unwrap();

        let v: i64 = conn
            .query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0))
            .unwrap();
        let d: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();

        assert_eq!(v, 0);
        assert_eq!(d, 0);
    }

    #[tokio::test]
    async fn vector_search_returns_top_result() {
        let db = TempDb::new("vsearch");
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();
        {
            let conn = provider.conn.lock().unwrap();
            // The ::FLOAT[3] cast is REQUIRED: a bare [0.1, 0.2, 0.3] literal infers
            // DOUBLE[], which does not match the FLOAT[N] ARRAY column type.
            conn.execute(
                "INSERT INTO vectors (doc_id, embedding) VALUES (0, [0.1, 0.2, 0.3]::FLOAT[3])",
                [],
            )
            .unwrap();
        }

        let results = provider
            .vector_search(&[0.1, 0.2, 0.3], 5, 0.0)
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.0, 0);
        assert!(results[0].1 > 0.99);
    }

    #[tokio::test]
    async fn fetch_content_returns_stored_text() {
        let db = TempDb::new("fetch");
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();
        {
            let conn = provider.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO documents (doc_id, page_content) VALUES (42, 'hello world')",
                [],
            )
            .unwrap();
        }

        let results = provider.fetch_content(&[DocumentId(42)]).await.unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "hello world");
    }

    #[tokio::test]
    async fn rebuild_indexes_then_vector_search() {
        let db = TempDb::new("rebuild");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let data = minimal_rag_data();
        provider.rebuild_indexes(&data, true).await.unwrap();

        let results = provider
            .vector_search(&[0.1, 0.2, 0.3], 5, 0.0)
            .await
            .unwrap();

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn rebuild_indexes_is_idempotent_across_two_passes() {
        let db = TempDb::new("idempotent");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let mut data = minimal_rag_data();
        data.vectors.insert(DocumentId(0), vec![0.1, 0.2, 0.3]);
        data.vectors.insert(DocumentId(1), vec![0.4, 0.5, 0.6]);

        provider.rebuild_indexes(&data, true).await.unwrap();

        provider
            .rebuild_indexes(&data, true)
            .await
            .expect("second rebuild must not violate the primary key constraint");

        let conn = provider.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "rebuild must replace rows, not duplicate them");
    }

    #[tokio::test]
    async fn rebuild_indexes_preserves_prior_vectors_on_incremental_pass() {
        let db = TempDb::new("incremental");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();

        let mut data = minimal_rag_data();
        data.vectors.insert(DocumentId(0), vec![0.1, 0.2, 0.3]);
        data.vectors.insert(DocumentId(1), vec![0.4, 0.5, 0.6]);
        provider.rebuild_indexes(&data, true).await.unwrap();

        let mut reloaded = minimal_rag_data();
        assert!(
            reloaded.vectors.is_empty(),
            "YAML-loaded data starts with no vectors"
        );
        reloaded.vectors = provider.read_all_vectors().unwrap();
        assert_eq!(
            reloaded.vectors.len(),
            2,
            "hydration must restore both vectors"
        );

        reloaded.vectors.insert(DocumentId(2), vec![0.7, 0.8, 0.9]);
        provider.rebuild_indexes(&reloaded, false).await.unwrap();

        let conn = provider.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0))
            .unwrap();
        // Without hydration this is 1 — the two originals silently vanish.
        assert_eq!(
            count, 3,
            "an incremental pass must not destroy previously indexed vectors"
        );
    }

    #[tokio::test]
    async fn keyword_search_is_not_advertised_before_first_rebuild() {
        let db = TempDb::new("ftsflag");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        assert!(
            !provider.has_native_keyword_search(),
            "a freshly opened DB has no FTS index yet"
        );

        let mut data = populated_rag_data();
        data.vectors.insert(DocumentId(0), vec![0.1, 0.2, 0.3]);
        provider.rebuild_indexes(&data, true).await.unwrap();
        {
            let conn = provider.conn.lock().unwrap();
            let docs: i64 = conn
                .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
                .unwrap();
            assert_eq!(docs, 2, "the fixture must produce documents rows");
        }
        assert!(
            provider.has_native_keyword_search(),
            "rebuild_indexes creates the FTS index and must flip the flag"
        );
    }

    #[tokio::test]
    async fn fetch_content_resolves_documents_without_vectors() {
        let db = TempDb::new("docsnovec");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();

        let mut data = populated_rag_data();
        let id_a = DocumentId::new(0, 0);
        let id_b = DocumentId::new(1, 0);
        data.vectors.insert(id_a, vec![0.1, 0.2, 0.3]);
        assert!(
            !data.vectors.contains_key(&id_b),
            "precondition: b has no vector"
        );

        provider.rebuild_indexes(&data, true).await.unwrap();

        let got = provider.fetch_content(&[id_a, id_b]).await.unwrap();
        assert_eq!(
            got.len(),
            2,
            "fetch_content must resolve BOTH documents; the one without a vector is \
             still reachable by keyword and graph search"
        );
        assert_eq!(got[0].1, "alpha keyword");
        assert_eq!(got[1].1, "beta keyword");
    }

    #[tokio::test]
    async fn duckdb_fetch_content_preserves_input_order() {
        let db = TempDb::new("order");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let data = populated_rag_data();
        provider.rebuild_indexes(&data, true).await.unwrap();

        let id_a = DocumentId::new(0, 0);
        let id_b = DocumentId::new(1, 0);

        let got = provider.fetch_content(&[id_b, id_a]).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, id_b, "input order must win over storage order");
        assert_eq!(got[0].1, "beta keyword");
        assert_eq!(got[1].0, id_a);
        assert_eq!(got[1].1, "alpha keyword");
    }

    #[tokio::test]
    async fn rebuild_indexes_refuses_to_wipe_when_vectors_are_empty_but_files_are_not() {
        let db = TempDb::new("nowipe");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();

        let mut data = populated_rag_data();
        data.vectors
            .insert(DocumentId::new(0, 0), vec![0.1, 0.2, 0.3]);
        data.vectors
            .insert(DocumentId::new(1, 0), vec![0.4, 0.5, 0.6]);
        provider.rebuild_indexes(&data, true).await.unwrap();

        let broken = populated_rag_data();
        assert!(
            broken.vectors.is_empty() && !broken.files.is_empty(),
            "precondition"
        );

        let err = provider.rebuild_indexes(&broken, true).await.unwrap_err();
        assert!(
            err.to_string().contains("Refusing to rebuild"),
            "got: {err}"
        );

        let conn = provider.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "the guard must abort BEFORE the CREATE OR REPLACE"
        );
    }

    #[tokio::test]
    async fn rebuild_indexes_allows_empty_vectors_when_files_are_also_empty() {
        let db = TempDb::new("emptyok");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let data = minimal_rag_data();
        provider
            .rebuild_indexes(&data, true)
            .await
            .expect("a fresh RAG with nothing indexed must rebuild cleanly");
    }

    #[tokio::test]
    async fn duplicate_shares_the_same_connection() {
        let db = TempDb::new("dup");
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let dup = provider.duplicate(&minimal_rag_data());

        {
            let conn = provider.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO documents (doc_id, page_content) VALUES (7, 'shared row')",
                [],
            )
            .unwrap();
        }

        let via_dup = dup.fetch_content(&[DocumentId(7)]).await.unwrap();

        assert_eq!(
            via_dup.len(),
            1,
            "duplicate() must see writes made via the original"
        );
        assert_eq!(via_dup[0].1, "shared row");
    }

    #[tokio::test]
    async fn read_all_vectors_rejects_non_finite_embeddings() {
        let db = TempDb::new("nonfinite");
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();
        {
            let conn = provider.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO vectors (doc_id, embedding) VALUES (0, [0.1, 0.2, 0.3]::FLOAT[3])",
                [],
            )
            .unwrap();
            conn.execute(
                // Each element is cast individually: a bare ['nan', 0.2, 0.3] literal
                // mixes text and numerics, so DuckDB infers DECIMAL and the conversion
                // fails before the row is ever stored.
                "INSERT INTO vectors (doc_id, embedding) VALUES \
                 (1, [CAST('nan' AS FLOAT), CAST(0.2 AS FLOAT), CAST(0.3 AS FLOAT)]::FLOAT[3])",
                [],
            )
            .unwrap();
        }
        let err = provider.read_all_vectors().unwrap_err();
        assert!(
            err.to_string().contains("non-finite"),
            "hydration must abort rather than silently drop the row; got: {err}"
        );
    }

    #[test]
    fn duckdb_path_from_yaml_swaps_extension() {
        let p = duckdb_path_from_yaml(Path::new("/tmp/rags/docs.yaml"));

        assert_eq!(p, PathBuf::from("/tmp/rags/docs.duckdb"));
    }
}
