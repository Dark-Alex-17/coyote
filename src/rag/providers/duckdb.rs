use crate::rag::provider::RagProvider;
use crate::rag::{DocumentId, RagData};
use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use duckdb::types::Value;
use duckdb::{AccessMode, Config, Connection, OptionalExt};
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
/// replacement through. Guards nothing but the install step.
///
/// Lock order is `DuckDbProvider::conn` -> INSTALL_LOCK, never the reverse:
/// `ensure_writable` reopens the connection, and so may install, while holding the
/// `ConnHandle` guard, whereas nothing ever acquires a `ConnHandle` guard while
/// holding this lock. The cycle that would deadlock cannot form.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Derive the DuckDB sidecar path from a RAG's YAML path: `docs.yaml` -> `docs.duckdb`.
pub(crate) fn duckdb_path_from_yaml(yaml_path: &Path) -> PathBuf {
    yaml_path.with_extension("duckdb")
}

fn parse_float_array_dim(data_type: &str) -> Option<usize> {
    let inner = data_type.strip_prefix("FLOAT[")?.strip_suffix(']')?;
    inner.parse::<usize>().ok().filter(|&n| n > 0)
}

/// The shared connection together with the access mode it was opened with.
///
/// `conn` is an `Option` only so that an upgrade can DROP the read-only connection
/// before asking DuckDB for a read-write one. It is `Some` at every point an outside
/// caller can observe, and is never left `None` on a path that returns `Ok`.
struct ConnHandle {
    conn: Option<Connection>,
    /// True when `conn` was opened READ_WRITE. This lives behind the same mutex as the
    /// connection itself rather than next to it in `DuckDbProvider`, so that a
    /// `duplicate()` clone sharing the `Arc` observes an upgrade performed through any
    /// other handle instead of keeping its own stale copy of the mode.
    writable: bool,
    /// Embedding dimension the `FLOAT[N]` column was opened (or last rebuilt) with.
    /// Shared for the same reason as `writable`: a self-healing rebuild through one
    /// handle updates the width, and every `duplicate()` clone must cast with the new
    /// width instead of erroring on a healthy store with its stale copy.
    dim: usize,
}

impl ConnHandle {
    fn conn(&self) -> Result<&Connection> {
        self.conn.as_ref().ok_or_else(Self::lost)
    }

    fn conn_mut(&mut self) -> Result<&mut Connection> {
        self.conn.as_mut().ok_or_else(Self::lost)
    }

    /// Only reachable when a read-write upgrade failed AND reopening read-only failed
    /// too. Returning an error beats panicking inside a locked scope, which would
    /// poison the mutex for the remaining life of the process.
    fn lost() -> anyhow::Error {
        anyhow!(
            "The DuckDB connection was lost: upgrading it to read-write failed and the \
             store could not be reopened read-only afterwards. Another process is \
             holding the file; retry once it has released it."
        )
    }
}

pub struct DuckDbProvider {
    path: PathBuf,
    conn: Arc<Mutex<ConnHandle>>,
    /// True once an FTS index has been built on `documents`. Until then
    /// `fts_main_documents.match_bm25` does not exist and any keyword query would
    /// fail with a DuckDB catalog error. Backs `has_native_keyword_search`.
    fts_ready: AtomicBool,
}

impl DuckDbProvider {
    /// Open (or create) the DuckDB file. `dim` is the embedding vector dimension,
    /// supplied by the caller who knows the model.
    ///
    /// Opens READ-ONLY whenever the file already carries a complete schema, so that any
    /// number of Coyote processes can query the same RAG at the same time. DuckDB allows
    /// many concurrent readers XOR exactly one writer, so the exclusive read-write handle
    /// is taken only when there is actually something to write: when the store has to be
    /// created or initialized here, or lazily through `ensure_writable` on the rebuild
    /// path.
    pub fn open(db_path: &Path, dim: usize) -> Result<Self> {
        let (conn, writable) = Self::open_for_workload(db_path, dim)?;
        // A reopened file may already carry a live FTS index from a previous session,
        // in which case keyword search works immediately.
        let fts_exists = Self::probe_fts_index(&conn);
        Ok(Self {
            path: db_path.to_path_buf(),
            conn: Arc::new(Mutex::new(ConnHandle {
                conn: Some(conn),
                writable,
                dim,
            })),
            fts_ready: AtomicBool::new(fts_exists),
        })
    }

    /// Pick the weakest access mode that can serve this store, returning the connection
    /// and whether it came back writable.
    fn open_for_workload(db_path: &Path, dim: usize) -> Result<(Connection, bool)> {
        if db_path.exists()
            && let Ok(conn) = Self::open_read_only(db_path)
            && Self::store_is_initialized(&conn)
        {
            return Ok((conn, false));
        }
        // Three cases land here: the file does not exist yet, it could not be opened
        // read-only (another process holds it read-write), or it carries no usable
        // schema. All of them need a read-write handle, and the read-write attempt is
        // also what produces the actionable lock error for the middle case.
        let conn = Self::open_read_write(db_path, dim)?;
        Ok((conn, true))
    }

    /// Is this file already a fully initialized Coyote store?
    ///
    /// This gate decides whether a read-only open is viable, so it must be exact: every
    /// statement in `init_schema` is rejected outright on a read-only handle, INCLUDING
    /// `CREATE TABLE IF NOT EXISTS` against a table that already exists, which DuckDB
    /// refuses rather than treating as a no-op. Anything missing therefore forces a
    /// read-write open. The HNSW index is part of the check because a store whose tables
    /// survived but whose index did not would otherwise be opened read-only and silently
    /// serve every `vector_search` from a full scan.
    fn store_is_initialized(conn: &Connection) -> bool {
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM duckdb_tables() \
                 WHERE table_name IN ('vectors', 'documents')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if tables < 2 {
            return false;
        }
        conn.query_row(
            "SELECT count(*) FROM duckdb_indexes() WHERE index_name = 'hnsw_idx'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    /// Open the store read-only. Many processes may hold such a handle at once.
    fn open_read_only(db_path: &Path) -> Result<Connection> {
        let config = Config::default()
            .access_mode(AccessMode::ReadOnly)
            .context("Failed to build a read-only DuckDB configuration")?;
        let conn = Connection::open_with_flags(db_path, config).with_context(|| {
            format!(
                "Failed to open the DuckDB store at '{}' read-only",
                db_path.display()
            )
        })?;
        Self::establish_session(&conn)?;
        Ok(conn)
    }

    pub fn introspect_dim(db_path: &Path) -> Result<Option<usize>> {
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Self::open_read_only(db_path).with_context(|| {
            format!(
                "Cannot inspect the existing RAG store at '{}'",
                db_path.display()
            )
        })?;
        let ty: Option<String> = conn
            .query_row(
                "SELECT data_type FROM duckdb_columns() \
                 WHERE table_name = 'vectors' AND column_name = 'embedding'",
                [],
                |r| r.get(0),
            )
            .optional()
            .with_context(|| {
                format!(
                    "Failed to introspect the embedding dimension of the DuckDB store \
                     at '{}'",
                    db_path.display()
                )
            })?;

        Ok(ty.and_then(|t| parse_float_array_dim(&t)))
    }

    /// Open the store read-write and make sure its schema exists. Exactly one process
    /// may hold such a handle, and no reader from another process may hold it meanwhile.
    fn open_read_write(db_path: &Path, dim: usize) -> Result<Connection> {
        let config = Config::default()
            .access_mode(AccessMode::ReadWrite)
            .context("Failed to build a read-write DuckDB configuration")?;
        let conn = Connection::open_with_flags(db_path, config).with_context(|| {
            format!(
                "Failed to open the DuckDB store at '{}' for writing. Another Coyote \
                 process (or another window) has this RAG open: a duckdb RAG supports MANY \
                 concurrent READERS, but only ONE writer at a time, and a writer excludes \
                 readers in other processes. Close that process, or wait for its sync to \
                 finish, and retry.",
                db_path.display()
            )
        })?;
        Self::establish_session(&conn)?;
        Self::init_schema(&conn, dim)?;
        Ok(conn)
    }

    /// Install the per-connection session state that every connection needs, whatever
    /// its access mode.
    ///
    /// Extension `LOAD`s and `SET` are per-CONNECTION, not per-database: a connection
    /// opened later — an upgrade, in particular — starts with none of this and must run
    /// it again. None of these statements write to the database, so they all succeed on
    /// a read-only handle.
    ///
    /// Statement order is load-bearing. `hnsw_enable_experimental_persistence` is
    /// registered BY the vss extension, so setting it before `LOAD vss` fails with
    /// "Setting with name ... is not in the catalog, but it exists in the vss
    /// extension". ensure vss (installing it if missing) -> ensure fts -> SET.
    fn establish_session(conn: &Connection) -> Result<()> {
        Self::ensure_extension(conn, "vss")?;
        Self::ensure_extension(conn, "fts")?;
        conn.execute_batch("SET hnsw_enable_experimental_persistence = true;")
            .context("Failed to enable DuckDB HNSW index persistence")
    }

    /// Create the tables and the vector index. Every statement here WRITES, so this only
    /// ever runs on a read-write connection.
    ///
    /// Must be preceded by `establish_session`: without the `SET` it performs, a
    /// CREATE INDEX ... USING HNSW on a file-backed database fails with "HNSW index
    /// persistence is not yet supported by default".
    fn init_schema(conn: &Connection, dim: usize) -> Result<()> {
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS vectors (
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
        .context("Failed to initialize DuckDB schema")
    }

    /// Guarantee the shared connection is read-write, upgrading it in place if it is not.
    /// EVERY write path must call this before touching the store.
    ///
    /// The upgrade replaces the `Connection` INSIDE the shared `Arc<Mutex<..>>`, so
    /// `duplicate()` clones, which share that `Arc`, see it too. The read-only connection
    /// is dropped before the read-write open because DuckDB tracks the file lock per
    /// database instance and the old handle still holds one.
    ///
    /// On failure the store is reopened read-only so that queries keep working, and the
    /// error is propagated so the caller aborts instead of writing. A failed upgrade must
    /// leave the provider degraded, never bricked, and never silently read-only-with-a-
    /// caller-that-thinks-it-wrote.
    fn ensure_writable(&self) -> Result<()> {
        let mut handle = self.lock_conn()?;
        if handle.writable {
            return Ok(());
        }
        drop(handle.conn.take());
        match Self::open_read_write(&self.path, handle.dim) {
            Ok(conn) => {
                handle.conn = Some(conn);
                handle.writable = true;
                Ok(())
            }
            Err(e) => {
                handle.conn = Self::open_read_only(&self.path).ok();
                Err(e.context(format!(
                    "Cannot write to the DuckDB RAG at '{}': it is open read-only and could \
                     not be upgraded to read-write, because another Coyote process has this \
                     RAG open. NOTHING WAS WRITTEN. Close the other process, or wait for it \
                     to finish, and retry.",
                    self.path.display()
                )))
            }
        }
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
        let handle = self.lock_conn()?;
        let mut stmt = handle
            .conn()?
            .prepare("SELECT doc_id, embedding FROM vectors")?;
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
    fn lock_conn(&self) -> Result<MutexGuard<'_, ConnHandle>> {
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
        let handle = self.lock_conn()?;
        let dim = handle.dim;
        if embedding.len() != dim {
            let rows: i64 = handle
                .conn()?
                .query_row("SELECT count(*) FROM vectors", [], |r| r.get(0))
                .context("Failed to count vectors before a dimension-mismatch query")?;
            if rows == 0 {
                // A never-synced store answers "nothing", not a cast error.
                return Ok(Vec::new());
            }

            bail!(
                "RAG store at '{}' was built with {dim}-dim embeddings, but the \
                 embedding model now returns {}-dim vectors. The embedding model \
                 changed since ingestion. Re-embed the documents, or delete the \
                 sidecar file and re-ingest.",
                self.path.display(),
                embedding.len()
            );
        }

        let vals: String = embedding
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        // array_cosine_distance requires a FLOAT[N] ARRAY, not the LIST type FLOAT[].
        // ORDER BY distance ASC is required for the planner to use hnsw_idx; the
        // similarity form (DESC) does NOT trigger the ANN index. Distance is converted
        // back to a similarity score on return.
        let sql = format!(
            "SELECT doc_id, \
             array_cosine_distance(embedding, [{vals}]::FLOAT[{dim}]) AS distance \
             FROM vectors ORDER BY distance ASC LIMIT {top_k}"
        );
        let mut stmt = handle.conn()?.prepare(&sql)?;
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
        let handle = self.lock_conn()?;
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql =
            format!("SELECT doc_id, page_content FROM documents WHERE doc_id IN ({placeholders})");
        let params: Vec<Value> = ids.iter().map(|id| Value::UBigInt(id.0 as u64)).collect();
        let mut stmt = handle.conn()?.prepare(&sql)?;
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
                let handle = self.lock_conn()?;
                handle
                    .conn()?
                    .query_row("SELECT count(*) FROM vectors", [], |r| r.get(0))
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
        let dim = match data.vectors.first() {
            None => self.lock_conn()?.dim,
            Some((_, first)) => {
                let dim = first.len();
                if let Some((doc_id, other)) = data.vectors.iter().find(|(_, e)| e.len() != dim) {
                    let matching = data.vectors.values().filter(|e| e.len() == dim).count();

                    bail!(
                        "Refusing to rebuild the RAG store at '{}': the rebuild batch \
                         mixes {dim}-dim and {}-dim vectors ({matching} vs {} vectors; \
                         first mismatch: document {}). Re-embed the documents, or \
                         delete the sidecar file and re-ingest.",
                        self.path.display(),
                        other.len(),
                        data.vectors.len() - matching,
                        doc_id.0
                    );
                }

                dim
            }
        };
        // THE write path. Everything above this line only reads, so the upgrade happens
        // here, after both guards have had their say: a rebuild that is going to be
        // refused must not first take the exclusive lock away from other processes.
        //
        // This is also the point that makes a silently-dropped write impossible. If the
        // upgrade fails, `?` aborts the rebuild before a single statement is issued and
        // the caller gets the error. Nothing below can run on a read-only connection.
        self.ensure_writable()?;
        let mut handle = self.lock_conn()?;
        let conn = handle.conn_mut()?;
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

        handle.dim = dim;

        Ok(())
    }

    async fn keyword_search(&self, query: &str, top_k: usize) -> Result<Vec<(DocumentId, f32)>> {
        let handle = self.lock_conn()?;
        // match_bm25 returns NULL for non-matching rows; WHERE filters them out.
        let mut stmt = handle.conn()?.prepare(
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
        //
        // Sharing the Arc also shares the ACCESS MODE, which lives inside the ConnHandle
        // rather than beside it: when one handle upgrades itself to read-write, every
        // clone is upgraded with it and none is left holding a stale "read-only" belief.
        // The embedding dimension lives there too, so a self-healing rebuild through
        // one handle updates the width every clone casts with.
        Box::new(DuckDbProvider {
            path: self.path.clone(),
            conn: Arc::clone(&self.conn),
            fts_ready: AtomicBool::new(self.fts_ready.load(Ordering::Relaxed)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag::provider::RagProvider;
    use crate::rag::{RagDocument, RagFile};
    use std::process::Command;
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
        let handle = provider.conn.lock().unwrap();
        let conn = handle.conn().unwrap();

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
            let handle = provider.conn.lock().unwrap();
            let conn = handle.conn().unwrap();
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
            let handle = provider.conn.lock().unwrap();
            let conn = handle.conn().unwrap();
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

        let handle = provider.conn.lock().unwrap();
        let conn = handle.conn().unwrap();
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

        let handle = provider.conn.lock().unwrap();
        let conn = handle.conn().unwrap();
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
            let handle = provider.conn.lock().unwrap();
            let conn = handle.conn().unwrap();
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

        let handle = provider.conn.lock().unwrap();
        let conn = handle.conn().unwrap();
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

    #[test]
    fn parse_float_array_dim_handles_arrays_lists_and_scalars() {
        assert_eq!(parse_float_array_dim("FLOAT[768]"), Some(768));
        assert_eq!(parse_float_array_dim("FLOAT[]"), None);
        assert_eq!(parse_float_array_dim("VARCHAR"), None);
    }

    #[test]
    fn introspect_dim_round_trips_the_open_dim() {
        let db = TempDb::new("introspect");

        assert_eq!(
            DuckDbProvider::introspect_dim(&db.path).unwrap(),
            None,
            "a file that does not exist has no dim"
        );
        {
            let _provider = DuckDbProvider::open(&db.path, 5).unwrap();
        }
        assert_eq!(DuckDbProvider::introspect_dim(&db.path).unwrap(), Some(5));
    }

    #[test]
    fn introspect_dim_propagates_an_unopenable_existing_file() {
        let db = TempDb::new("introspectgarbage");
        fs::write(&db.path, b"not a duckdb database").unwrap();

        let err = DuckDbProvider::introspect_dim(&db.path).unwrap_err();

        assert!(
            format!("{err:#}").contains(&format!(
                "Cannot inspect the existing RAG store at '{}'",
                db.path.display()
            )),
            "an existing-but-unopenable file must be an error naming the path; got: {err:#}"
        );
    }

    #[tokio::test]
    async fn rebuild_indexes_self_heals_dim_from_the_vectors_it_writes() {
        let db = TempDb::new("selfheal");
        let mut provider = DuckDbProvider::open(&db.path, 5).unwrap();
        let mut data = minimal_rag_data();
        data.vectors.insert(DocumentId(0), vec![0.1, 0.2, 0.3]);

        provider.rebuild_indexes(&data, true).await.unwrap();

        let results = provider
            .vector_search(&[0.1, 0.2, 0.3], 5, 0.0)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);

        drop(provider);
        assert_eq!(DuckDbProvider::introspect_dim(&db.path).unwrap(), Some(3));
    }

    #[tokio::test]
    async fn a_self_healed_dim_is_visible_through_duplicate_clones() {
        let db = TempDb::new("dimdup");
        let mut provider = DuckDbProvider::open(&db.path, 5).unwrap();
        let dup = provider.duplicate(&minimal_rag_data());

        let mut data = minimal_rag_data();
        data.vectors.insert(DocumentId(0), vec![0.1, 0.2, 0.3]);
        provider.rebuild_indexes(&data, true).await.unwrap();

        let results = dup.vector_search(&[0.1, 0.2, 0.3], 5, 0.0).await.unwrap();
        assert_eq!(
            results.len(),
            1,
            "a duplicate() clone must observe the dim written by a rebuild through \
             the original"
        );
    }

    #[tokio::test]
    async fn rebuild_indexes_rejects_mixed_dim_vectors() {
        let db = TempDb::new("mixeddim");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let mut data = minimal_rag_data();
        data.vectors.insert(DocumentId(0), vec![0.1, 0.2, 0.3]);
        data.vectors.insert(DocumentId(1), vec![0.1, 0.2, 0.3, 0.4]);

        let err = provider.rebuild_indexes(&data, true).await.unwrap_err();

        assert!(
            err.to_string().contains("rebuild batch mixes"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn vector_search_dim_mismatch_on_empty_store_returns_nothing() {
        let db = TempDb::new("dimempty");
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();

        let results = provider.vector_search(&[0.1, 0.2], 5, 0.0).await.unwrap();

        assert!(
            results.is_empty(),
            "a never-synced store must answer 'nothing', not a cast error"
        );
    }

    #[tokio::test]
    async fn vector_search_dim_mismatch_on_populated_store_errors() {
        let db = TempDb::new("dimfull");
        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let mut data = minimal_rag_data();
        data.vectors.insert(DocumentId(0), vec![0.1, 0.2, 0.3]);
        provider.rebuild_indexes(&data, true).await.unwrap();

        let err = provider
            .vector_search(&[0.1, 0.2], 5, 0.0)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("was built with"), "got: {err}");
    }

    #[tokio::test]
    async fn duplicate_shares_the_same_connection() {
        let db = TempDb::new("dup");
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();
        let dup = provider.duplicate(&minimal_rag_data());

        {
            let handle = provider.conn.lock().unwrap();
            let conn = handle.conn().unwrap();
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
            let handle = provider.conn.lock().unwrap();
            let conn = handle.conn().unwrap();
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

    /// Was the shared connection opened read-write? Reads the flag that lives inside the
    /// shared handle, which is the same one `ensure_writable` flips.
    fn is_writable(provider: &DuckDbProvider) -> bool {
        provider.conn.lock().unwrap().writable
    }

    /// Build a fully initialized store, then let the read-write handle go so the file is
    /// unlocked for the next opener.
    async fn seed_store(path: &Path) {
        let mut provider = DuckDbProvider::open(path, 3).unwrap();
        assert!(is_writable(&provider), "a fresh file must open read-write");
        let mut data = populated_rag_data();
        data.vectors
            .insert(DocumentId::new(0, 0), vec![0.1, 0.2, 0.3]);
        provider.rebuild_indexes(&data, true).await.unwrap();
    }

    #[tokio::test]
    async fn a_fresh_file_is_opened_read_write() {
        let db = TempDb::new("freshrw");
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();

        assert!(
            is_writable(&provider),
            "the schema has to be created, which writes, so a missing file must open \
             read-write"
        );
    }

    #[tokio::test]
    async fn an_initialized_store_is_reopened_read_only() {
        let db = TempDb::new("reopenro");
        seed_store(&db.path).await;

        let provider = DuckDbProvider::open(&db.path, 3).unwrap();

        assert!(
            !is_writable(&provider),
            "a store that needs no schema work must open read-only, so that other Coyote \
             processes can query it at the same time"
        );
    }

    #[tokio::test]
    async fn a_file_without_the_coyote_schema_is_opened_read_write() {
        let db = TempDb::new("noschema");
        {
            // A valid DuckDB file that is not one of ours. Opening it read-only would
            // strand it forever: the init batch is refused on a read-only handle.
            let conn = Connection::open(&db.path).unwrap();
            conn.execute_batch("CREATE TABLE unrelated (x INTEGER);")
                .unwrap();
        }

        let provider = DuckDbProvider::open(&db.path, 3).unwrap();

        assert!(
            is_writable(&provider),
            "a schema-less file must open read-write"
        );
        let handle = provider.conn.lock().unwrap();
        let tables: i64 = handle
            .conn()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM duckdb_tables() \
                 WHERE table_name IN ('vectors', 'documents')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2, "the schema must have been created");
    }

    #[tokio::test]
    async fn a_read_only_store_still_serves_vector_and_keyword_search() {
        let db = TempDb::new("rosearch");
        seed_store(&db.path).await;

        let provider = DuckDbProvider::open(&db.path, 3).unwrap();
        assert!(!is_writable(&provider), "precondition: opened read-only");

        let hits = provider
            .vector_search(&[0.1, 0.2, 0.3], 5, 0.0)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the persisted HNSW index must be queryable read-only"
        );
        assert!(hits[0].1 > 0.99);

        assert!(
            provider.has_native_keyword_search(),
            "the FTS index built by the previous session must still be detected on a \
             read-only handle"
        );
        let kw = provider.keyword_search("alpha", 5).await.unwrap();
        assert_eq!(kw.len(), 1, "keyword search must work read-only");

        let docs = provider
            .fetch_content(&[DocumentId::new(0, 0)])
            .await
            .unwrap();
        assert_eq!(docs[0].1, "alpha keyword");
    }

    #[tokio::test]
    async fn a_read_only_handle_refuses_a_direct_write() {
        let db = TempDb::new("rorefuse");
        seed_store(&db.path).await;
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();
        assert!(!is_writable(&provider), "precondition: opened read-only");

        let handle = provider.conn.lock().unwrap();
        let err = handle
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO documents (doc_id, page_content) VALUES (99, 'nope')",
                [],
            )
            .unwrap_err();

        // The backstop, not the primary defence: `rebuild_indexes` upgrades first and
        // never reaches a write on a read-only handle. It matters anyway because DuckDB
        // lets `transaction()` open and `commit()` return Ok on a read-only connection,
        // so a write that slipped through would look like it had succeeded.
        assert!(
            err.to_string().contains("read-only mode"),
            "a read-only handle must refuse writes loudly; got: {err}"
        );
    }

    #[tokio::test]
    async fn rebuild_indexes_upgrades_a_read_only_connection() {
        let db = TempDb::new("upgrade");
        seed_store(&db.path).await;

        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        assert!(!is_writable(&provider), "precondition: opened read-only");

        let mut data = populated_rag_data();
        data.vectors = provider.read_all_vectors().unwrap();
        data.vectors
            .insert(DocumentId::new(1, 0), vec![0.4, 0.5, 0.6]);
        provider.rebuild_indexes(&data, false).await.unwrap();

        assert!(
            is_writable(&provider),
            "the write path must have upgraded the connection in place"
        );

        // The upgraded connection is a NEW connection, so the per-connection session
        // state has to have been re-established on it. Without the re-run `SET`, the
        // CREATE INDEX ... USING HNSW inside rebuild_indexes would already have failed.
        let persisted: String = {
            let handle = provider.conn.lock().unwrap();
            handle
                .conn()
                .unwrap()
                .query_row(
                    "SELECT CAST(current_setting('hnsw_enable_experimental_persistence') \
                     AS VARCHAR)",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(
            persisted, "true",
            "the upgraded connection must re-run the SET; session state does not carry \
             over from the dropped read-only connection"
        );

        drop(provider);
        let reopened = DuckDbProvider::open(&db.path, 3).unwrap();
        let all = reopened.read_all_vectors().unwrap();
        assert_eq!(all.len(), 2, "the upgraded write must have reached disk");
    }

    #[tokio::test]
    async fn an_upgrade_is_visible_through_duplicate_clones() {
        let db = TempDb::new("upgradedup");
        seed_store(&db.path).await;

        let mut provider = DuckDbProvider::open(&db.path, 3).unwrap();
        assert!(!is_writable(&provider), "precondition: opened read-only");
        let dup = provider.duplicate(&minimal_rag_data());

        let mut data = populated_rag_data();
        data.vectors = provider.read_all_vectors().unwrap();
        provider.rebuild_indexes(&data, false).await.unwrap();

        // `duplicate()` shares the Arc, and the access mode lives inside it, so the clone
        // must observe the upgrade rather than keep believing it is read-only.
        let via_dup = dup.fetch_content(&[DocumentId::new(0, 0)]).await.unwrap();
        assert_eq!(
            via_dup.len(),
            1,
            "the clone must still read after an upgrade"
        );
        assert!(
            is_writable(&provider),
            "the shared handle must report writable to every clone"
        );
    }

    /// Two REAL OS processes reading one store at the same time.
    ///
    /// Ignored by default because it re-executes the test binary as a child process,
    /// which is heavier and more environment-dependent than the rest of the suite. Run it
    /// with:
    ///   cargo test --all -- --ignored duckdb_store_is_shared_across_processes
    ///
    /// It cannot be written as an ordinary in-process test: DuckDB keeps ONE database
    /// instance per process, so a second open in the same process bypasses the file lock
    /// entirely (a read-write open succeeds even while this process holds a read-only
    /// one). Only separate processes exercise the lock this feature exists to avoid.
    #[tokio::test]
    #[ignore = "spawns a second OS process; run explicitly with --ignored"]
    async fn duckdb_store_is_shared_across_processes() {
        const CHILD_DB: &str = "COYOTE_DUCKDB_MULTIPROC_DB";
        const CHILD_EXPECT: &str = "COYOTE_DUCKDB_MULTIPROC_EXPECT";
        const TEST_NAME: &str =
            "rag::providers::duckdb::tests::duckdb_store_is_shared_across_processes";

        if let Ok(path) = env::var(CHILD_DB) {
            let expect = env::var(CHILD_EXPECT).unwrap_or_default();
            let opened = DuckDbProvider::open(Path::new(&path), 3);
            match expect.as_str() {
                "readable" => {
                    let provider = opened.expect(
                        "a second process must be able to open a store that another \
                         process holds READ-ONLY",
                    );
                    assert!(!is_writable(&provider), "the child must land read-only");
                    let hits = provider
                        .vector_search(&[0.1, 0.2, 0.3], 5, 0.0)
                        .await
                        .unwrap();
                    assert_eq!(hits.len(), 1, "the child must read the seeded vector");
                }
                "blocked" => {
                    let err = opened.err().expect(
                        "a second process must NOT be able to open a store that another \
                         process holds READ-WRITE",
                    );
                    let msg = format!("{err:#}");
                    assert!(
                        msg.contains("concurrent READERS") && msg.contains("only ONE writer"),
                        "the lock error must explain the reader/writer rule; got: {msg}"
                    );
                }
                other => panic!("unknown child expectation {other:?}"),
            }
            return;
        }

        let db = TempDb::new("multiproc");
        seed_store(&db.path).await;

        let run_child = |expect: &str| {
            Command::new(env::current_exe().unwrap())
                .args(["--exact", "--ignored", "--nocapture", TEST_NAME])
                .env(CHILD_DB, &db.path)
                .env(CHILD_EXPECT, expect)
                .output()
                .expect("failed to spawn the child test process")
        };

        // Phase 1: this process holds a READ-ONLY handle. The child must get one too.
        let provider = DuckDbProvider::open(&db.path, 3).unwrap();
        assert!(!is_writable(&provider), "precondition: parent is read-only");
        let out = run_child("readable");
        assert!(
            out.status.success(),
            "child could not share the read-only store:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let hits = provider
            .vector_search(&[0.1, 0.2, 0.3], 5, 0.0)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the parent must still read after the child ran"
        );

        // Phase 2: upgrade this process to READ-WRITE. The child must now be refused,
        // with the message that explains why.
        provider.ensure_writable().unwrap();
        let out = run_child("blocked");
        assert!(
            out.status.success(),
            "a writer must exclude other processes, with an actionable error:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn duckdb_path_from_yaml_swaps_extension() {
        let p = duckdb_path_from_yaml(Path::new("/tmp/rags/docs.yaml"));

        assert_eq!(p, PathBuf::from("/tmp/rags/docs.duckdb"));
    }
}
