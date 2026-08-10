use crate::rag::provider::RagProvider;
use crate::rag::{DocumentId, RagData};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use duckdb::Connection;
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
        // Collect into a Result, NOT a filter_map. `.filter_map(|r| r.ok())` here would
        // turn a systematic decode failure (e.g. a schema written by a different duckdb
        // version) into a silently short map, indistinguishable from an empty store.
        let raw: Vec<(u64, Vec<f32>)> = stmt
            .query_map([], |row| {
                let id: u64 = row.get(0)?;
                // The duckdb crate does not implement `FromSql` for `Vec<f32>`, so the
                // column is read as a `Value` and destructured. A FLOAT[N] column yields
                // `Value::Array`, a FLOAT[] column yields `Value::List`; match both so
                // the reader survives a file written under either schema.
                let embedding: Vec<f32> = match row.get::<_, duckdb::types::Value>(1)? {
                    duckdb::types::Value::Array(vals) | duckdb::types::Value::List(vals) => vals
                        .into_iter()
                        .map(|v| match v {
                            duckdb::types::Value::Float(f) => f,
                            duckdb::types::Value::Double(d) => d as f32,
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
    /// exists and `match_bm25` still SUCCEEDS — but returns zero rows for every term.
    /// The index is silently dead. The probe therefore asserts that a KNOWN row comes
    /// back rather than merely that the call did not error.
    fn probe_fts_index(conn: &Connection) -> bool {
        // 1. Structural check — cheap, and short-circuits a never-built index.
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
        // 2. Liveness check — pull a real token out of a real row and confirm the index
        //    scores that same row. A live index returns >= 1; a stale one returns 0
        //    without erroring. An empty `documents` table cannot be probed, and
        //    reporting false is correct: there is nothing to keyword-search, and the
        //    next rebuild_indexes sets the flag directly.
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
        // Validate before building the SQL literal — a NaN would produce malformed SQL.
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
                // which a bare `.filter_map(|r| r.ok())` would silently discard —
                // yielding ZERO results with no error and no log line.
                let distance: f32 = match row.get::<_, duckdb::types::Value>(1)? {
                    duckdb::types::Value::Float(f) => f,
                    duckdb::types::Value::Double(d) => d as f32,
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
        let params: Vec<duckdb::types::Value> = ids
            .iter()
            .map(|id| duckdb::types::Value::UBigInt(id.0 as u64))
            .collect();
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
        // Ordering contract: `WHERE doc_id IN (...)` returns rows in storage order, NOT
        // in the order of `ids`. Since `ids` is the RRF-ranked list, returning storage
        // order would silently discard the ranking. Re-sort by input position.
        let position: std::collections::HashMap<DocumentId, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        rows.sort_by_key(|(id, _)| position.get(id).copied().unwrap_or(usize::MAX));
        Ok(rows)
    }

    async fn rebuild_indexes(&mut self, data: &RagData, _full_rebuild: bool) -> Result<()> {
        // Local on-disk state — a wholesale table reset plus re-INSERT is always
        // correct, so the incremental/full distinction is ignored.
        //
        // 🔴 ANTI-WIPE GUARD. The CREATE OR REPLACE TABLE below writes exactly what
        // `data.vectors` holds. An empty map on a RAG that HAS indexed files, against a
        // store that ALREADY holds vectors, is always a bug — a failed/skipped
        // hydration, or a caller that emptied the live map. Refuse rather than commit
        // the loss. Every legitimate empty-vector rebuild also has an empty `files`
        // (a fresh RAG; the zero-document tests), so this cannot fire on a correct call.
        // The `existing > 0` conjunct is load-bearing, not belt-and-braces: a populated
        // fixture rebuilt against a fresh temp database has count 0 and must proceed.
        if data.vectors.is_empty() && !data.files.is_empty() {
            let existing: i64 = {
                // Scoped: the guard MUST be dropped before `lock_conn()` is taken again
                // below. `Mutex` is not reentrant — holding both self-deadlocks at
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
        // `Connection::transaction()` takes `&mut self`, so this binding must be `mut`.
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
            // The embedding is bound as TEXT and cast in SQL — the duckdb crate's
            // `bind_parameter` has no arm for List/Array, so binding a vector directly
            // fails at runtime with "binding List parameters is not yet supported".
            let mut vstmt = tx.prepare(&format!(
                "INSERT INTO vectors (doc_id, embedding) VALUES (?, CAST(? AS FLOAT[{dim}]))"
            ))?;
            let mut dstmt =
                tx.prepare("INSERT INTO documents (doc_id, page_content) VALUES (?, ?)")?;

            // 🔴 TWO INDEPENDENT LOOPS. `documents` is keyed on `files`, `vectors` on
            // `vectors` — they are DIFFERENT key sets and neither is a subset of the
            // other. Do not merge these into one loop over `&data.vectors` gated on a
            // lookup into `files`: that produces keys(documents) ⊆ keys(vectors), and
            // every id in `files \ vectors` (which `RagData::add`'s zip truncation
            // really does produce) becomes a live, rankable id from BM25 and
            // graph_search that resolves to nothing — no error, no log line.
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
        // leaves the old fts_main_documents schema in place but DEAD — match_bm25 keeps
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
        // fall back to local BM25 — which is also empty, and therefore correct.
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
                // `.filter_map(|r| r.ok())` silently discards — yielding ZERO keyword
                // hits, indistinguishable from "the query matched nothing".
                let score: f32 = match row.get::<_, duckdb::types::Value>(1)? {
                    duckdb::types::Value::Double(d) => d as f32,
                    duckdb::types::Value::Float(f) => f,
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
        // documented on `Rag`'s Clone impl — the pre-clone instance must be discarded.
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
    // Trait methods are only callable with the trait in scope.
    use crate::rag::provider::RagProvider;
    // `RagFile` / `RagDocument` have private fields; struct-literal construction
    // compiles only because this module is a descendant of `crate::rag`. They are
    // deliberately not in the non-test `use` block — unused there, and `--deny warnings`
    // rejects that.
    use crate::rag::{RagDocument, RagFile};

    /// Unique temp path per test. `tempfile` is not a dev-dependency; this mirrors the
    /// house pattern used elsewhere in the tree.
    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(tag: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("coyote-duckdb-{tag}-{unique}.duckdb"));
            Self { path }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            // DuckDB writes a `<path>.wal` sidecar; remove it too or /tmp accumulates
            // one per test run.
            let _ = std::fs::remove_file(self.path.with_extension("duckdb.wal"));
        }
    }

    /// ⚠️ NOTE THE EMPTY `files`. This fixture describes a RAG with ZERO documents.
    /// Since `rebuild_indexes` populates the `documents` table from
    /// `data.iter_documents()` (keyed on `files`), any test built on this helper alone
    /// exercises the `vectors` table and NOTHING ELSE — no `documents` rows, no FTS
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

    /// Two files, one document each — the minimum fixture that produces `documents`
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
        assert!(results.is_empty()); // no vectors in empty data
    }

    /// Rebuilding TWICE must succeed.
    ///
    /// The first rebuild always passes because the tables start empty. The bug this
    /// guards against — `DELETE FROM` plus re-INSERT of the same PKs inside one
    /// transaction — only fires on the SECOND rebuild, with "Duplicate key ... violates
    /// primary key constraint". A single-rebuild test cannot catch it.
    #[tokio::test]
    async fn rebuild_indexes_is_idempotent_across_two_passes() {
        let db = TempDb::new("idempotent");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let mut data = minimal_rag_data();
        data.vectors.insert(DocumentId(0), vec![0.1, 0.2, 0.3]);
        data.vectors.insert(DocumentId(1), vec![0.4, 0.5, 0.6]);

        provider.rebuild_indexes(&data, true).await.unwrap();
        // Second pass over the SAME doc_ids — this is the assertion that matters.
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

    /// Guards the incremental data-loss bug: `sync_documents` hash-skips unchanged
    /// files, so on an incremental pass `data.vectors` holds ONLY the newly embedded
    /// chunks. If the live map is ever emptied, the `CREATE OR REPLACE TABLE` in
    /// rebuild_indexes writes only those, destroying every previously indexed vector.
    /// Hydration via `read_all_vectors()` is what prevents it.
    ///
    /// This simulates the real restart-then-edit sequence: build, drop the in-memory
    /// map, rehydrate from disk, add one vector, rebuild incrementally.
    #[tokio::test]
    async fn rebuild_indexes_preserves_prior_vectors_on_incremental_pass() {
        let db = TempDb::new("incremental");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();

        // Pass 1 — initial full build with two vectors.
        let mut data = minimal_rag_data();
        data.vectors.insert(DocumentId(0), vec![0.1, 0.2, 0.3]);
        data.vectors.insert(DocumentId(1), vec![0.4, 0.5, 0.6]);
        provider.rebuild_indexes(&data, true).await.unwrap();

        // Simulate a restart: the YAML on disk carries NO vectors, so a fresh RagData
        // starts empty. create() hydrates it back from the sidecar.
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

        // Pass 2 — incremental add of ONE new chunk.
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

    /// `has_native_keyword_search()` must be false before any FTS index exists,
    /// otherwise hybrid_search routes into keyword_search and the `?` turns a DuckDB
    /// catalog error into a total query failure.
    ///
    /// 🔴 THE FIXTURE MUST CARRY DOCUMENTS. `rebuild_indexes` builds the FTS index only
    /// when the `documents` table is non-empty, and that table is filled from
    /// `data.iter_documents()` (keyed on `files`), never from `vectors`. With `files`
    /// empty the pragma is skipped and `fts_ready` stores `false`.
    #[tokio::test]
    async fn keyword_search_is_not_advertised_before_first_rebuild() {
        let db = TempDb::new("ftsflag");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        assert!(
            !provider.has_native_keyword_search(),
            "a freshly opened DB has no FTS index yet"
        );

        // 2 files × 1 document → 2 `documents` rows → doc_count > 0 → pragma runs.
        let mut data = populated_rag_data();
        data.vectors.insert(DocumentId(0), vec![0.1, 0.2, 0.3]);
        provider.rebuild_indexes(&data, true).await.unwrap();
        {
            // Pin the precondition explicitly. If this ever reads 0 the assertion below
            // is vacuous and the FTS hazard is unguarded again.
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

    /// The `documents` table is keyed on `files`, NOT on `vectors`.
    ///
    /// `fetch_content` is the sink for BOTH the BM25 and graph_search paths, and both
    /// enumerate `files` via `iter_documents()`. If `rebuild_indexes` only inserts a
    /// documents row for ids that also appear in `data.vectors`, every id in
    /// `files \ vectors` resolves to nothing: healthy BM25 scores, healthy graph hits,
    /// zero results, no error.
    ///
    /// The incremental test CANNOT catch this — its fixture has an empty `files`, so the
    /// documents table is empty in every assertion either way.
    #[tokio::test]
    async fn fetch_content_resolves_documents_without_vectors() {
        let db = TempDb::new("docsnovec");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();

        // Two files, one document each — but a vector for ONLY THE FIRST. This is the
        // `RagData::add` zip-truncation shape.
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

    /// `fetch_content` must return rows in INPUT order, not storage order.
    ///
    /// `YamlProvider` satisfies this contract structurally — it maps over `ids` — so the
    /// existing yaml test proves nothing about DuckDB. DuckDB's `WHERE doc_id IN (...)`
    /// returns storage order and relies on an explicit positional re-sort, which is what
    /// can actually regress. `ids` is the RRF-ranked list, so losing the order silently
    /// discards the ranking while still returning the right documents.
    #[tokio::test]
    async fn duckdb_fetch_content_preserves_input_order() {
        let db = TempDb::new("order");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let data = populated_rag_data();
        provider.rebuild_indexes(&data, true).await.unwrap();

        let id_a = DocumentId::new(0, 0); // inserted first  → storage order 0
        let id_b = DocumentId::new(1, 0); // inserted second → storage order 1

        // REVERSED relative to storage order. Without the positional re-sort this
        // returns ["alpha keyword", "beta keyword"] and the assertion fails.
        let got = provider.fetch_content(&[id_b, id_a]).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, id_b, "input order must win over storage order");
        assert_eq!(got[0].1, "beta keyword");
        assert_eq!(got[1].0, id_a);
        assert_eq!(got[1].1, "alpha keyword");
    }

    /// The anti-wipe guard.
    ///
    /// `rebuild_indexes` does `CREATE OR REPLACE TABLE` and writes exactly what
    /// `data.vectors` holds. An empty map on a RAG that HAS indexed files, against a
    /// store that already holds vectors, is always a bug (failed hydration, or a caller
    /// that emptied the live map) and must be refused rather than committed. The loss
    /// would otherwise be permanent: nothing re-embeds it back.
    #[tokio::test]
    async fn rebuild_indexes_refuses_to_wipe_when_vectors_are_empty_but_files_are_not() {
        let db = TempDb::new("nowipe");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();

        // A healthy store: two files with documents, two vectors.
        let mut data = populated_rag_data();
        data.vectors
            .insert(DocumentId::new(0, 0), vec![0.1, 0.2, 0.3]);
        data.vectors
            .insert(DocumentId::new(1, 0), vec![0.4, 0.5, 0.6]);
        provider.rebuild_indexes(&data, true).await.unwrap();

        // Now the failure state: vectors lost in memory, files intact.
        let broken = populated_rag_data(); // same files, NO vectors
        assert!(
            broken.vectors.is_empty() && !broken.files.is_empty(),
            "precondition"
        );

        let err = provider.rebuild_indexes(&broken, true).await.unwrap_err();
        assert!(
            err.to_string().contains("Refusing to rebuild"),
            "got: {err}"
        );

        // The store must be untouched — this is the whole point.
        let conn = provider.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "the guard must abort BEFORE the CREATE OR REPLACE"
        );
    }

    /// The guard must NOT fire on a legitimately empty RAG — otherwise a fresh
    /// `Rag::init()` cannot complete. Empty `vectors` AND empty `files` is fine.
    #[tokio::test]
    async fn rebuild_indexes_allows_empty_vectors_when_files_are_also_empty() {
        let db = TempDb::new("emptyok");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let data = minimal_rag_data(); // no files, no vectors
        provider
            .rebuild_indexes(&data, true)
            .await
            .expect("a fresh RAG with nothing indexed must rebuild cleanly");
    }

    /// `duplicate()` must clone the Arc, NOT call `open()` again.
    ///
    /// This asserts SHARED state, which is the actual contract. It deliberately does NOT
    /// use `fetch_content(&[])` — that early-returns before ever touching the
    /// connection, so it would pass even against a broken double-opening implementation.
    #[tokio::test]
    async fn duplicate_shares_the_same_connection() {
        let db = TempDb::new("dup");
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let dup = provider.duplicate(&minimal_rag_data());

        // Write through the ORIGINAL...
        {
            let conn = provider.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO documents (doc_id, page_content) VALUES (7, 'shared row')",
                [],
            )
            .unwrap();
        }
        // ...and read it back through the DUPLICATE. Only possible if they share state.
        let via_dup = dup.fetch_content(&[DocumentId(7)]).await.unwrap();
        assert_eq!(
            via_dup.len(),
            1,
            "duplicate() must see writes made via the original"
        );
        assert_eq!(via_dup[0].1, "shared row");
    }

    /// A decode failure must abort hydration rather than yield a thinned map: a short
    /// map is what the next CREATE OR REPLACE commits as the new truth.
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
