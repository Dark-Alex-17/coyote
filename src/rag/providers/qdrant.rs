use crate::rag::provider::RagProvider;
use crate::rag::{DocumentId, FileId, RagData};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use log::{debug, warn};
use parking_lot::RwLock;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Method, Response, StatusCode};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use url::{Host, Url};

/// Marks a `DocumentId` that stands in for a point id Coyote cannot carry
/// directly. Qdrant accepts UUID strings as point ids, and that is what
/// LangChain writes by default.
///
/// `DocumentId` packs `(file_index, document_index)` into one `usize` with the
/// file index in the high half, so this bit is only reachable at a file index of
/// 2^31. Nothing local gets near that, and an attached RAG builds no local index
/// at all — `data.files` and `data.vectors` stay empty and every
/// `DocumentId::split` caller early-returns on `data.attached`. Along the
/// attached path the id is an opaque key carried through RRF, which is what
/// makes a synthetic one safe here and nowhere else.
const SYNTHETIC_ID_TAG: usize = 1 << (usize::BITS - 1);

/// Two-way map between a raw Qdrant point id and the `DocumentId` the retrieval
/// pipeline sees.
///
/// Only ids that cannot survive the round trip are interned. A plain `u64` that
/// fits below the tag keeps mapping to itself, so integer-keyed collections
/// behave exactly as they did before this map existed.
#[derive(Default)]
struct PointIdInterner {
    handles: HashMap<String, DocumentId>,
    raw: HashMap<DocumentId, Value>,
    next: usize,
}

impl PointIdInterner {
    /// The `DocumentId` for a raw point id, minting a handle if one is needed.
    ///
    /// `None` only for a missing id, which is a malformed response.
    fn document_id(&mut self, raw: &Value) -> Option<DocumentId> {
        if raw.is_null() {
            return None;
        }
        // The pre-existing integer path, unchanged. `try_from` rather than `as`
        // so a value too wide for the target's `usize` is interned instead of
        // silently truncated into a different point.
        if let Some(n) = raw.as_u64()
            && let Ok(n) = usize::try_from(n)
            && n & SYNTHETIC_ID_TAG == 0
        {
            return Some(DocumentId(n));
        }
        Some(self.intern(raw))
    }

    fn intern(&mut self, raw: &Value) -> DocumentId {
        // Keyed on the JSON rendering, so the string "1" and the integer 1 are
        // not conflated into one point.
        let key = raw.to_string();
        if let Some(handle) = self.handles.get(&key) {
            return *handle;
        }
        let handle = DocumentId(SYNTHETIC_ID_TAG | self.next);
        self.next += 1;
        self.handles.insert(key, handle);
        self.raw.insert(handle, raw.clone());
        handle
    }

    /// The original id for a handle, or `None` when the id was never interned —
    /// i.e. it is a plain integer that is already its own id.
    fn raw_id(&self, handle: DocumentId) -> Option<&Value> {
        self.raw.get(&handle)
    }

    /// Builds the `ids` array for an outbound `/points` fetch. Every entry is the
    /// id Qdrant issued, integer or string; a synthetic handle must never leave
    /// this process.
    fn outbound_ids(&self, ids: &[DocumentId]) -> Vec<Value> {
        ids.iter()
            .map(|id| match self.raw_id(*id) {
                Some(raw) => raw.clone(),
                None => Value::from(id.0 as u64),
            })
            .collect()
    }
}

fn parse_search_hits(
    interner: &mut PointIdInterner,
    body: &Value,
    min_score: f32,
) -> Result<Vec<(DocumentId, f32)>> {
    let hits = body["result"]
        .as_array()
        .context("Unexpected /points/search response shape")?;

    Ok(hits
        .iter()
        .filter_map(|pt| {
            let score = pt["score"].as_f64()? as f32;
            Some((interner.document_id(&pt["id"])?, score))
        })
        .filter(|(_, score)| min_score <= 0.0 || *score > min_score)
        .collect())
}

fn parse_points(interner: &mut PointIdInterner, body: &Value) -> Result<Vec<(DocumentId, String)>> {
    let points = body["result"]
        .as_array()
        .context("Unexpected /points response shape")?;

    Ok(points
        .iter()
        .filter_map(|pt| {
            let text = pt["payload"]["page_content"].as_str()?.to_string();
            Some((interner.document_id(&pt["id"])?, text))
        })
        .collect())
}

/// Render Qdrant's error envelope into a human-readable message.
///
/// `body` is the raw response text. Two shapes have to be tolerated:
///   * application-level errors carry `{"status": {"error": "..."}, "time": 0.0}`,
///     while successful responses carry a bare string `{"status": "ok", ...}` — so
///     `status` is string-or-object and a struct with `status: String` fails to
///     parse every error body;
///   * routing-level 404s (a wrong HTTP verb) return an EMPTY body with no JSON at
///     all, which without the length check surfaces as "EOF while parsing a value"
///     instead of the actual 404.
fn format_error_body(status: StatusCode, body: &str) -> String {
    if body.is_empty() {
        return format!("HTTP {status} (empty body — check the HTTP verb and path)");
    }
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["status"]["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {status}: {body}"))
}

/// Read the vector dimension out of a parsed `GET /collections/{name}` response.
fn vector_dimension_from_collection(body: &Value) -> Result<u64> {
    let params = &body["result"]["config"]["params"];
    params["vectors"]["size"]
        .as_u64()
        .or_else(|| {
            params["vectors"]
                .as_object()
                .and_then(|m| m.values().next())
                .and_then(|v| v["size"].as_u64())
        })
        .context("Could not determine vector dimension from collection config")
}

/// True if a parsed `GET /collections/{name}` response describes a NAMED
/// (multi-vector) collection.
///
/// `vector_search` posts an unnamed vector, which a named-vector collection
/// rejects with HTTP 400 on every query, so attaching one yields a RAG that is
/// silently 100% broken. A named collection holding a SINGLE vector is
/// structurally a map, identical in kind to the multi-named case, and rejects
/// the same way; testing for a numeric `size` directly under `vectors` catches
/// it, whereas counting keys (`len() > 1`) would wrongly accept it.
fn is_multi_vector_config(body: &Value) -> bool {
    body["result"]["config"]["params"]["vectors"]["size"]
        .as_u64()
        .is_none()
}

/// The `exists` flag out of a `GET /collections/{c}/exists` body.
///
/// Anything that is not a JSON bool at `result.exists` is an error rather than a
/// `false`. `new_owned` treats a successful probe as proof the host really is a
/// Qdrant, so a proxy that answers 200 with something else must not read as
/// "the collection is absent" and leave a provider pointed at nothing.
fn collection_exists_from_body(body: &Value) -> Result<bool> {
    body["result"]["exists"]
        .as_bool()
        .context("Unexpected collection-exists response shape")
}

/// The decision layer for a Coyote-owned collection: which files to write back,
/// which remote points to drop, and when to refuse outright.
///
/// It is deliberately free of HTTP so that everything able to delete remote data
/// is exercised by plain maps in the tests below. `rebuild_indexes` supplies the
/// scrolled remote state and carries out the plan this returns.
mod owned {
    use crate::rag::{DocumentId, FileId};

    use anyhow::{Result, bail};
    use std::collections::{BTreeMap, BTreeSet};

    /// One past the highest file index the wire point-id format can carry.
    ///
    /// Staying below it keeps every `(file_index << 32) | chunk_index` under
    /// `SYNTHETIC_ID_TAG`, so an owned point always takes the interner's integer
    /// fast path and can never alias a synthetic handle.
    ///
    /// This is the *wire* bound and is deliberately stricter than, and separate
    /// from, the *packing* bound `DocumentId::try_new` enforces: a file index in
    /// `[2^31, 2^32)` packs perfectly well locally and only collides remotely.
    const WIRE_FILE_INDEX_LIMIT: usize = 1 << 31;

    /// The point id an owned collection stores a document under.
    ///
    /// The shift is a fixed 32 and must stay that way. `DocumentId` packs with
    /// `usize::BITS / 2`, which is width-dependent; the wire format is not. The
    /// two coincide on a 64-bit host, which is what lets the read path parse
    /// owned ids with no special casing at all.
    ///
    /// Infallible by design: the file index is bounded once per file by
    /// `check_wire_range` before this runs for any of that file's chunks, and a
    /// chunk-index check here would be vacuous — `split` masks, so an
    /// overflowing chunk index has already corrupted the file bits by the time
    /// it returns.
    pub fn wire_point_id(id: DocumentId) -> u64 {
        let (file_index, chunk_index) = id.split();
        ((file_index as u64) << 32) | (chunk_index as u64)
    }

    /// The file index a wire point id encodes.
    pub fn wire_file_index(id: u64) -> u64 {
        id >> 32
    }

    /// Bound a file index once per file, before any of its points are minted.
    pub fn check_wire_range(file_index: usize) -> Result<()> {
        if file_index >= WIRE_FILE_INDEX_LIMIT {
            bail!(
                "RAG file index {file_index} is too large to store in Qdrant (limit 2^31). \
                 This RAG has churned through more files than the remote point-id format \
                 supports; recreate it to reset the counter."
            );
        }
        Ok(())
    }

    /// A scrolled remote point, carrying every payload field a guard reads.
    ///
    /// Trimming this list disables guards silently rather than failing to
    /// compile: `writer_id` backs the foreign-install warning, `embedding_model`
    /// the model-mixing refusal, `path` the bulk-deletion threshold.
    ///
    /// An absent payload field arrives as an empty string — absent and empty are
    /// equally unusable for all of these — except `file_id`, which has no such
    /// sentinel and so is `None`.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct RemotePoint {
        pub file_id: Option<u64>,
        pub file_hash: String,
        pub coyote_rag_id: String,
        pub writer_id: String,
        pub embedding_model: String,
        pub path: String,
    }

    /// One file's desired remote state, derived from `data.files`.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct DesiredFile {
        pub hash: String,
        pub path: String,
        /// Every wire point id the file's chunks map to, embeddable or not.
        pub ids: BTreeSet<u64>,
    }

    /// The state the guards compare the remote against that is not per-file.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct ReconcileCtx {
        pub collection: String,
        pub host: String,
        /// This RAG's ownership marker, read from `driver_config` and never from
        /// a vault-resolved copy.
        pub rag_id: String,
        /// This Coyote install's identity.
        pub writer_id: String,
        pub embedding_model: String,
        /// `data.vectors.is_empty()`.
        pub vectors_empty: bool,
        /// Set when this sync is creating the collection rather than finding it.
        pub creating_collection: bool,
    }

    /// A file to write back, with the exact points to write.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct UpsertFile {
        pub file_id: FileId,
        /// All of the file's achievable points, never a delta against what the
        /// remote already holds. The delta is empty in the re-minted-FileId case,
        /// so writing it would leave the old hash in place, make the retention
        /// rule keep stale points forever, and re-select the file every sync.
        ///
        /// Empty is a legitimate outcome — a file whose chunks have no embeddings
        /// yet and no remote points is selected on every sync. Callers must not
        /// turn that into an empty request or count it as work.
        pub point_ids: Vec<u64>,
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct Reconcile {
        pub upsert_files: Vec<UpsertFile>,
        pub delete_ids: Vec<u64>,
        /// Non-fatal and meant for `warn!`: a foreign writer id, points written
        /// before Coyote recorded a model, and the shortfall of desired points
        /// that have no embedding yet. None of these may become an `Err` — the
        /// foreign writer warns by design and a shortfall is ordinary.
        pub warnings: Vec<String>,
    }

    /// **Owned-collection safety invariant.** For a coyote-owned Qdrant
    /// collection, `rebuild_indexes` performs exactly two kinds of remote
    /// mutation: (1) full-replace upserts of points whose IDs are in the desired
    /// set, and (2) exact-ID deletions of points observed in the pre-write
    /// scroll that fail the retention rule below — that is, whose ID is absent
    /// from the desired set, **or** whose ID is currently unachievable *and*
    /// whose payload `file_hash` disagrees with the local hash for its file. The
    /// desired set is derived exclusively from `data.files` — every
    /// `(file_id, chunk_index)` reachable via `iter_documents` — never from
    /// `data.vectors`, never from remote state, never from vector-map coverage.
    /// The collection itself is never dropped, recreated, or filter-wiped. Every
    /// write pass is gated by preconditions: the RAG is not attached; every
    /// scanned remote point carries this RAG's ownership marker; the desired set
    /// is non-empty whenever the remote is non-empty; and the corpus has not
    /// shrunk past the bulk-deletion threshold. Because `sync_documents` always
    /// presents the complete corpus, and a hash-skipped file retains its FileId
    /// and chunk list verbatim, every point of an unchanged file is in the
    /// desired set *by construction* — unreachable by the delete pass regardless
    /// of hashes, vectors, or remote contents. **Convergence corollary:** from
    /// any remote state (interrupted sync, reassigned FileIds, stale points), one
    /// successful sync makes the remote exactly equal to the desired set
    /// restricted to embeddable points.
    ///
    /// **ID-set membership alone is NOT the safety property.** Saying that
    /// deletion removes exactly the points "absent from the desired set" is
    /// necessary but not sufficient: a FileId re-minted onto a different file
    /// after an interrupted sync leaves stale points *inside* the desired ID
    /// range, carrying the previous file's content. Retention requires per-point
    /// hash agreement as well. Do not simplify the retention rule back down to a
    /// set difference.
    ///
    /// Every guard is judged here, before the caller writes anything. A guard
    /// that tripped after the upsert pass would leave the collection mutated
    /// with the YAML unsaved, so every retry would re-upsert and re-bail forever.
    pub fn reconcile(
        desired: &BTreeMap<FileId, DesiredFile>,
        achievable: &BTreeSet<u64>,
        remote: &BTreeMap<u64, RemotePoint>,
        ctx: &ReconcileCtx,
        full_rebuild: bool,
    ) -> Result<Reconcile> {
        let mut warnings = Vec::new();

        // Ownership is judged across the whole scroll before anything else: a
        // foreign collection must report that it is foreign, not that one of its
        // points happens to lack a `path`.
        if remote
            .values()
            .any(|point| point.coyote_rag_id.is_empty() || point.coyote_rag_id != ctx.rag_id)
        {
            bail!(
                "Refusing to sync: collection '{}' on {} contains points that do not belong to \
                 this RAG (expected marker {}). Another Coyote RAG is probably writing to the \
                 same collection. Point this RAG at a different collection, or delete the \
                 collection in Qdrant if the other data is disposable. This is not recoverable \
                 from inside Coyote — it will not adopt or overwrite points it did not write.",
                ctx.collection,
                ctx.host,
                ctx.rag_id
            );
        }

        // Same marker, different install. This warns and never bails: using one
        // owned RAG from two machines serially is legitimate, and only concurrent
        // use is mutual deletion.
        let foreign_writer = remote
            .values()
            .map(|point| point.writer_id.as_str())
            .find(|writer| !writer.is_empty() && *writer != ctx.writer_id);
        if let Some(other) = foreign_writer {
            warnings.push(format!(
                "Warning: collection '{}' was last written by a different Coyote install \
                 ({other}). If two machines sync this RAG concurrently they will delete each \
                 other's documents. Syncing a `driver: qdrant` RAG's YAML across machines is \
                 only safe if you never sync from both at once.",
                ctx.collection
            ));
        }

        let mut scrolled: Vec<(u64, FileId)> = Vec::with_capacity(remote.len());
        let mut remote_paths: BTreeSet<&str> = BTreeSet::new();
        let mut unmarked_model = 0usize;

        for (&id, point) in remote {
            let Some(payload_file_id) = point.file_id else {
                bail!(missing_field(ctx, id, "file_id"));
            };
            if point.path.is_empty() {
                bail!(missing_field(ctx, id, "path"));
            }
            if point.file_hash.is_empty() {
                bail!(missing_field(ctx, id, "file_hash"));
            }

            // The id and the payload encode the file independently, so a
            // disagreement is a corrupt or foreign write that carries the right
            // ownership marker and so slipped past the check above.
            let encoded_file_id = wire_file_index(id);
            if encoded_file_id != payload_file_id {
                bail!(
                    "Refusing to sync: point {id} in collection '{}' encodes file {encoded_file_id} \
                     in its id but claims file_id {payload_file_id} in its payload. Coyote will \
                     not guess which document it belongs to; delete the collection in Qdrant to \
                     rebuild it from the local copy.",
                    ctx.collection
                );
            }
            let Ok(file_id) = FileId::try_from(payload_file_id) else {
                bail!(
                    "Refusing to sync: point {id} in collection '{}' claims file_id \
                     {payload_file_id}, which is wider than this build can represent.",
                    ctx.collection
                );
            };

            // A missing model marker only means the point predates Coyote
            // recording one. A differing one is the corruption case: two vector
            // spaces in one collection make similarity scores meaningless, and no
            // dimension check sees it when the models share a width.
            if point.embedding_model.is_empty() {
                unmarked_model += 1;
            } else if point.embedding_model != ctx.embedding_model {
                bail!(
                    "Refusing to sync: collection '{}' holds vectors from embedding model '{}' \
                     but this RAG is configured for '{}'. Mixing embedding models in one \
                     collection makes similarity scores meaningless. Run `.rebuild rag` to \
                     re-embed the whole corpus under '{}'.",
                    ctx.collection,
                    point.embedding_model,
                    ctx.embedding_model,
                    ctx.embedding_model
                );
            }

            remote_paths.insert(point.path.as_str());
            scrolled.push((id, file_id));
        }

        if unmarked_model > 0 {
            warnings.push(format!(
                "Collection '{}' holds {unmarked_model} points with no embedding_model marker, so \
                 they cannot be checked against '{}'. They were written before Coyote recorded \
                 one.",
                ctx.collection, ctx.embedding_model
            ));
        }

        // The scroll, not the collection's `points_count`, is the authoritative
        // non-empty signal: `points_count` is documented as approximate, and a
        // short scroll is fail-safe here for the same reason it is in the delete
        // pass below.
        if desired.is_empty() && !remote.is_empty() {
            bail!(
                "Refusing to sync: this RAG has no documents but collection '{}' holds {} points. \
                 If you intend to discard the remote data, delete the collection in Qdrant \
                 directly.",
                ctx.collection,
                remote.len()
            );
        }

        // On the create arm the "remote is populated" conjunct is false by
        // definition, so it is dropped there. Keeping it would let a truncated
        // YAML create the collection, find nothing achievable, write zero points
        // and report success: a silently empty RAG.
        if ctx.vectors_empty
            && !desired.is_empty()
            && (ctx.creating_collection || !remote.is_empty())
        {
            bail!(
                "Refusing to sync: RAG '{}' lists {} documents but holds no vectors. This usually \
                 means the RAG's YAML was truncated or hand-edited. Restore it from backup, or \
                 run `.rebuild rag` to re-embed from source.",
                ctx.collection,
                desired.len()
            );
        }

        // Distinct paths on both sides, not points and not file ids. Points are
        // useless on the full-rebuild arm, where every FileId is legitimately
        // retired and re-minted, and file ids are not stable across an
        // *interrupted* turnover — the remote holds both generations while local
        // rolled back, so counting them would refuse to recover from exactly the
        // state this reconcile exists to fix. Paths survive FileId turnover.
        // Distinct locally too: overlapping source roots can produce two FileIds
        // sharing one path, which would inflate the local side.
        let local_paths: BTreeSet<&str> = desired.values().map(|file| file.path.as_str()).collect();
        // `saturating_sub` is load-bearing, not stylistic: both sides are `usize`
        // and any growing corpus makes the local side larger, where a plain `-`
        // panics in debug and wraps to ~1.8e19 in release — clearing every
        // threshold and refusing every ordinary sync.
        let dropped = remote_paths.len().saturating_sub(local_paths.len());
        // A net count, deliberately: `|remote \ local|` would trip on a legitimate
        // mass rename.
        if !remote_paths.is_empty() && dropped > usize::max(10, remote_paths.len() / 4) {
            let examples: Vec<&str> = remote_paths
                .difference(&local_paths)
                .take(10)
                .copied()
                .collect();
            bail!(
                "Refusing to sync: this would delete {dropped} of {} documents from collection \
                 '{}'. That usually means some source paths did not resolve (an unmounted drive, \
                 an interrupted cloud sync, a changed ignore rule) rather than a real deletion. \
                 Confirm the source paths resolve and retry. If the removal is intended, re-run \
                 after removing the documents in a smaller batch, or delete the collection in \
                 Qdrant. Documents that would go, up to ten: {}",
                remote_paths.len(),
                ctx.collection,
                examples.join(", ")
            );
        }

        let mut remote_by_file: BTreeMap<FileId, BTreeSet<u64>> = BTreeMap::new();
        for &(id, file_id) in &scrolled {
            remote_by_file.entry(file_id).or_default().insert(id);
        }

        let mut upsert_files = Vec::new();
        let mut shortfall = 0usize;

        for (&file_id, file) in desired {
            check_wire_range(file_id)?;

            let achievable_ids: BTreeSet<u64> =
                file.ids.intersection(achievable).copied().collect();
            shortfall += file.ids.len() - achievable_ids.len();

            let remote_ids = remote_by_file.get(&file_id);
            let hash_differs = remote_ids
                .is_some_and(|ids| ids.iter().any(|id| remote[id].file_hash != file.hash));
            // A subset test against what is *achievable*, never set inequality and
            // never against the full desired set. Comparing the desired set would
            // re-select a file with a vectorless chunk forever, since that point
            // can never exist remotely; testing inequality would re-select forever
            // whenever the remote holds more points than this sync can produce.
            // The only question that matters is whether anything writable is
            // missing remotely.
            let missing_remotely = !remote_ids.is_some_and(|ids| achievable_ids.is_subset(ids));

            if full_rebuild || remote_ids.is_none() || hash_differs || missing_remotely {
                upsert_files.push(UpsertFile {
                    file_id,
                    point_ids: achievable_ids.into_iter().collect(),
                });
            }
        }

        if shortfall > 0 {
            warnings.push(format!(
                "{shortfall} document chunks have no embedding yet and were not written to \
                 collection '{}'. A later sync writes them once their embeddings succeed.",
                ctx.collection
            ));
        }

        // Built ONLY from scrolled points — never from a filter, never from the
        // desired set. That is what makes a short scroll fail-safe: an under-scan
        // leaves points alive instead of deleting ones it never saw. Do not
        // "optimize" this into a filter-delete.
        let mut delete_ids = Vec::new();
        for &(id, file_id) in &scrolled {
            let keep = desired.get(&file_id).is_some_and(|file| {
                // The hash conjunct is load-bearing: a FileId re-minted onto a
                // different file after an interrupted sync leaves the previous
                // file's points inside the desired id range, so membership alone
                // would keep content that local state no longer describes. The
                // achievable disjunct is load-bearing in the other direction:
                // without it this tests the pre-write scrolled hash and deletes
                // the very points the upsert pass just wrote.
                file.ids.contains(&id)
                    && (achievable.contains(&id) || remote[&id].file_hash == file.hash)
            });
            if !keep {
                delete_ids.push(id);
            }
        }

        Ok(Reconcile {
            upsert_files,
            delete_ids,
            warnings,
        })
    }

    fn missing_field(ctx: &ReconcileCtx, id: u64, field: &str) -> String {
        format!(
            "Refusing to sync: point {id} in collection '{}' has no `{field}` in its payload. \
             Coyote will not guess which document it belongs to; delete the collection in Qdrant \
             to rebuild it from the local copy.",
            ctx.collection
        )
    }
}

/// How many points one scroll page asks for.
const SCROLL_PAGE_SIZE: u64 = 1000;

/// The payload fields the scroll must fetch.
///
/// Trimming this list neither fails to compile nor fails at runtime — it
/// silently disables whichever guard reads the missing field. Without
/// `writer_id` the foreign-install warning never fires, without
/// `embedding_model` the model-mixing refusal never fires, without `path` the
/// bulk-deletion threshold never fires. They just stop happening.
const SCROLL_PAYLOAD_FIELDS: [&str; 6] = [
    "file_id",
    "file_hash",
    "coyote_rag_id",
    "writer_id",
    "embedding_model",
    "path",
];

/// The largest request body Coyote sends, half the server's 32 MiB hard limit.
/// The margin absorbs the `{"points": [...]}` envelope and the separators
/// between points, which the per-point accounting does not measure.
const UPSERT_BYTE_CAP: usize = 16 * 1024 * 1024;

/// How many ids one delete request carries.
const DELETE_ID_CHUNK: usize = 1000;

/// Payload schema version stamped on every point Coyote writes.
const PAYLOAD_SCHEMA_VERSION: u64 = 1;

/// The upsert verb and endpoint.
///
/// `PUT`, not `POST`: `POST /collections/{c}/points` is the get-by-id route
/// `fetch_content` uses. The two verbs share a path and have disjoint required
/// fields, so mixing them up is a 400 rather than a silent no-op.
///
/// `wait` is a query parameter and only works as one. In the body it is ignored:
/// the server returns 200 with `status: "acknowledged"`, meaning the write is
/// queued with no channel to report a later failure.
fn upsert_request(base_url: &str, collection: &str) -> (Method, String) {
    (
        Method::PUT,
        format!("{base_url}/collections/{collection}/points?wait=true"),
    )
}

/// The delete verb and endpoint. `DELETE` on this path is a routing-level 404
/// with an empty body; the operation is a `POST`.
fn delete_request(base_url: &str, collection: &str) -> (Method, String) {
    (
        Method::POST,
        format!("{base_url}/collections/{collection}/points/delete?wait=true"),
    )
}

/// Rejects a write the server has not applied yet.
///
/// `acknowledged` means the request was queued and returned before the WAL
/// applied it, so nothing will ever report a failure that happens afterwards.
/// The sync's success gates the YAML save, and that only holds together if
/// reported success means durable.
fn check_write_completed(body: &Value, operation: &str) -> Result<()> {
    match body["result"]["status"].as_str() {
        Some("completed") => Ok(()),
        Some(status) => bail!(
            "Qdrant reported the {operation} as '{status}' rather than 'completed'. The write \
             is not durable yet, so Coyote cannot treat this sync as successful."
        ),
        None => bail!("Unexpected {operation} response shape: {body}"),
    }
}

/// One scroll page's request body. `offset` is the previous page's
/// `next_page_offset`, absent on the first page.
fn scroll_request(offset: Option<&Value>) -> Value {
    let mut body = serde_json::json!({
        "limit": SCROLL_PAGE_SIZE,
        "with_vector": false,
        "with_payload": { "include": SCROLL_PAYLOAD_FIELDS },
    });
    if let Some(offset) = offset {
        body["offset"] = offset.clone();
    }

    body
}

/// Maps a scrolled point's payload onto the fields the reconcile judges.
///
/// Absent and empty are equally unusable for the string fields, so both arrive
/// as an empty string. `file_id` has no such sentinel and stays `None`.
fn remote_point(payload: &Value) -> owned::RemotePoint {
    let text = |field: &str| payload[field].as_str().unwrap_or_default().to_string();

    owned::RemotePoint {
        file_id: payload["file_id"].as_u64(),
        file_hash: text("file_hash"),
        coyote_rag_id: text("coyote_rag_id"),
        writer_id: text("writer_id"),
        embedding_model: text("embedding_model"),
        path: text("path"),
    }
}

/// One point's payload.
///
/// `file_id` and `chunk_index` go out as JSON integers: the `file_id` payload
/// index is typed `integer` and a string does not match it. `page_content` is
/// the name the read path reads.
fn point_payload(
    file_id: FileId,
    chunk_index: usize,
    file_hash: &str,
    path: &str,
    page_content: &str,
    ctx: &owned::ReconcileCtx,
) -> Value {
    // Bumping this version is not enough on its own to migrate anything. An
    // unchanged file hash-matches and is never rewritten, so its points keep
    // whatever schema they were written under. Migrating them means adding
    // `schema_version` to the scroll's include list and forcing a version
    // mismatch into the upsert set.
    serde_json::json!({
        "file_id": file_id as u64,
        "chunk_index": chunk_index as u64,
        "file_hash": file_hash,
        "page_content": page_content,
        "path": path,
        "coyote_rag_id": ctx.rag_id,
        "writer_id": ctx.writer_id,
        "embedding_model": ctx.embedding_model,
        "schema_version": PAYLOAD_SCHEMA_VERSION,
    })
}

/// One point as the upsert body carries it.
fn upsert_point(id: u64, vector: &[f32], payload: Value) -> Value {
    serde_json::json!({ "id": id, "vector": vector, "payload": payload })
}

/// A point waiting for a batch, with the serialized length that decides which
/// batch it lands in.
struct PendingPoint {
    body: Value,
    bytes: usize,
    /// Names the chunk if it turns out to be too large to send at all.
    origin: String,
}

fn pending_point(body: Value, origin: String) -> Result<PendingPoint> {
    let bytes = serde_json::to_vec(&body)
        .context("Failed to serialize a Qdrant point")?
        .len();

    Ok(PendingPoint {
        body,
        bytes,
        origin,
    })
}

/// Groups points into request bodies that stay under `cap`.
///
/// Lengths are measured, never derived from the vector dimension: chunk text
/// dominates the body and varies per point. A point that cannot fit in a request
/// of its own is an error here rather than a 400 from the server.
fn batch_points(points: Vec<PendingPoint>, cap: usize) -> Result<Vec<Vec<Value>>> {
    let mut batches: Vec<Vec<Value>> = Vec::new();
    let mut batch: Vec<Value> = Vec::new();
    let mut bytes = 0usize;

    for point in points {
        if point.bytes > cap {
            bail!(
                "Refusing to sync: {} serializes to {} bytes, more than the {cap} bytes Coyote \
                 sends to Qdrant in one request. Lower the RAG's chunk size and re-ingest the \
                 document.",
                point.origin,
                point.bytes
            );
        }
        if !batch.is_empty() && bytes + point.bytes > cap {
            batches.push(std::mem::take(&mut batch));
            bytes = 0;
        }
        bytes += point.bytes;
        batch.push(point.body);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }

    Ok(batches)
}

/// Splits the delete list into per-request id chunks.
fn delete_id_batches(ids: &[u64]) -> impl Iterator<Item = &[u64]> {
    ids.chunks(DELETE_ID_CHUNK)
}

/// Rejects an upsert that cannot carry every point the reconcile selected.
///
/// The upsert loop walks the file's local chunks and skips any selected id it
/// finds no chunk for, so a selected id outside `local.documents` is dropped in
/// silence: the sync reports success, the file's hash matches from then on, and
/// the point is neither retried nor deleted on any later sync. Nothing recovers
/// from that, so a mismatch stops the sync before it writes anything for this
/// file.
fn check_selection_complete(path: &str, pending: usize, selected: usize) -> Result<()> {
    if pending != selected {
        bail!(
            "Refusing to sync: the reconcile selected {selected} points for '{path}' but only \
             {pending} of them exist in the local copy. Writing those would report a success \
             that silently drops the rest, after which the file's hash matches and they are \
             never written again."
        );
    }

    Ok(())
}

/// Rejects a collection the read and write paths cannot both use.
///
/// `local_dim` is the length of any local vector. `None` means the RAG holds no
/// vectors to measure against, so the size check is skipped rather than guessed —
/// there is nothing to upsert in that case either. The static model-to-dimension
/// map is not an alternative: it answers 1536 for every model it does not know,
/// which would bail on collections that are perfectly fine.
fn check_collection_config(
    body: &Value,
    collection: &str,
    host: &str,
    local_dim: Option<usize>,
) -> Result<()> {
    if is_multi_vector_config(body) {
        bail!(
            "Collection '{collection}' on {host} stores named vectors, but Coyote reads and \
             writes a single unnamed one, which a named-vector collection rejects on every \
             request."
        );
    }

    match body["result"]["config"]["params"]["vectors"]["distance"].as_str() {
        Some("Cosine") => {}
        distance => bail!(
            "Collection '{collection}' on {host} uses {} distance, but Coyote scores owned \
             collections with Cosine: the read path filters on the raw similarity, which only \
             Cosine reports on the scale it expects.",
            distance.unwrap_or("an unreadable")
        ),
    }

    if let Some(local_dim) = local_dim {
        let remote_dim = vector_dimension_from_collection(body)?;
        if remote_dim != local_dim as u64 {
            bail!(
                "Collection '{collection}' on {host} stores {remote_dim}-dimensional vectors but \
                 this RAG's are {local_dim}-dimensional, so the embedding model changed after the \
                 collection was created. Delete the collection in Qdrant and run `.rebuild rag`."
            );
        }
    }

    Ok(())
}

/// What the creation wizard should do with the collection name it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionAction {
    Create,
    Adopt,
}

/// Decides whether a collection name is usable for a new Coyote-owned RAG.
///
/// `existing` is the `GET /collections/{c}` body, or `None` when the collection
/// is absent. Kept apart from the requests so all four outcomes are decided in
/// one place, over one value, and so the refusals can be exercised without a
/// server.
///
/// Adopting is deliberately restricted to an empty collection. The first sync
/// reconciles the collection against the local corpus and deletes what it does
/// not recognise, so adopting a populated one would destroy data this RAG never
/// wrote. An empty one is the retry case — the process died between creating the
/// collection and saving the YAML — and reclaiming it is what makes that
/// recoverable.
fn plan_owned_collection(
    existing: Option<&Value>,
    collection: &str,
    host: &str,
    dim: usize,
    embedding_model: &str,
) -> Result<CollectionAction> {
    let Some(body) = existing else {
        return Ok(CollectionAction::Create);
    };

    // Never defaulted to zero: reading "no answer" as "empty" would adopt — and
    // then delete the contents of — a collection whose size could not be read.
    let points = body["result"]["points_count"].as_u64().with_context(|| {
        format!(
            "Cannot tell how many points collection '{collection}' on {host} holds, and Coyote \
             only adopts an empty collection. Check the collection in Qdrant, or choose a \
             different name."
        )
    })?;
    if points > 0 {
        bail!(
            "Collection '{collection}' on {host} already contains {points} point(s) that were \
             not written by this RAG. Adopting it would overwrite and delete that data on the \
             next sync. Choose a different collection name, or delete the collection in Qdrant \
             first."
        );
    }

    check_collection_config(body, collection, host, Some(dim)).with_context(|| {
        format!(
            "Collection '{collection}' on {host} cannot be used with embedding model \
             '{embedding_model}', which produces {dim}-dimensional vectors. Choose a different \
             collection name, or delete the existing collection in Qdrant."
        )
    })?;

    Ok(CollectionAction::Adopt)
}

/// Chunk text for every document in `data`, keyed by `DocumentId`.
///
/// Built from `iter_documents`, the same iterator `build_bm25` walks, so the
/// keyword index and this map share a key space by construction. `data.vectors`
/// is deliberately not the source: a chunk whose embedding failed has no vector
/// and therefore no remote point, and that is exactly the chunk this map has to
/// answer for.
fn build_local_content(data: &RagData) -> HashMap<DocumentId, String> {
    data.iter_documents()
        .map(|(id, doc)| (id, doc.page_content.clone()))
        .collect()
}

/// Splits `ids` against the local map: one slot per input position, `None` where
/// the map has no answer, plus the ids that still need the remote.
fn split_local_hits(
    local: &HashMap<DocumentId, String>,
    ids: &[DocumentId],
) -> (Vec<Option<String>>, Vec<DocumentId>) {
    let mut hits = Vec::with_capacity(ids.len());
    let mut misses = Vec::new();
    for id in ids {
        match local.get(id) {
            Some(text) => hits.push(Some(text.clone())),
            None => {
                hits.push(None);
                misses.push(*id);
            }
        }
    }
    (hits, misses)
}

/// Emits by input position, preferring the local answer and falling back to the
/// remote batch, which arrives in whatever order Qdrant chose. An id neither
/// source resolved is skipped rather than reported, per the trait.
fn merge_in_input_order(
    ids: &[DocumentId],
    local_hits: Vec<Option<String>>,
    remote: Vec<(DocumentId, String)>,
) -> Vec<(DocumentId, String)> {
    let mut remote: HashMap<DocumentId, String> = remote.into_iter().collect();
    ids.iter()
        .zip(local_hits)
        .filter_map(|(id, local)| local.or_else(|| remote.remove(id)).map(|text| (*id, text)))
        .collect()
}

/// Client for a Qdrant collection, attached or Coyote-owned.
///
/// An attached collection is read-only: Coyote does not own the documents, so
/// `rebuild_indexes` refuses rather than pretending to. An owned collection is
/// reconciled against the local corpus on every sync.
///
/// Anything cached here beyond `client`, `base_url`, `collection` and
/// `point_ids` owes an explicit answer to one question: what happens to it on
/// `duplicate` and on `rebuild_indexes`? For `local_content` the answer is that
/// both rebuild it from the `RagData` they are handed and neither carries the
/// old map across, because a carried map describes a corpus that has since moved
/// on and `fetch_content` would serve that stale text with no error to show for
/// it.
pub struct QdrantProvider {
    client: Client,
    base_url: String,
    collection: String,
    point_ids: Arc<RwLock<PointIdInterner>>,
    /// Chunk text keyed by the same document ids BM25 and `data.vectors` use. A
    /// hit here is served without a request, which also covers chunks that have
    /// no vector and hence no remote point at all.
    local_content: HashMap<DocumentId, String>,
    /// Whether the collection belongs to something else. Fixed by which
    /// constructor ran and never mutated afterwards; it decides whether a read
    /// path that finds the collection gone may offer to rebuild it, which is
    /// only true for a collection Coyote wrote.
    attached: bool,
}

impl QdrantProvider {
    fn skips_proxy(base_url: &str) -> bool {
        let Ok(url) = Url::parse(base_url) else {
            return false;
        };
        match url.host() {
            Some(Host::Domain(name)) => {
                name == "localhost" || name.ends_with(".localhost") || name.ends_with(".local")
            }
            Some(Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
            // No stable is_unique_local, so fc00::/7 is matched directly.
            Some(Host::Ipv6(ip)) => ip.is_loopback() || ip.segments()[0] & 0xfe00 == 0xfc00,
            None => false,
        }
    }

    fn make_client(base_url: &str, api_key: Option<&str>) -> Result<Client> {
        let mut headers = HeaderMap::new();
        if let Some(key) = api_key {
            let mut value =
                HeaderValue::from_str(key).context("api-key header value is not valid ASCII")?;
            value.set_sensitive(true);
            headers.insert("api-key", value);
        }
        let mut builder = Client::builder().default_headers(headers);
        if Self::skips_proxy(base_url) {
            builder = builder.no_proxy();
        }
        builder.build().context("Failed to build reqwest client")
    }

    pub(crate) fn normalize_base_url(host: &str) -> String {
        if host.starts_with("http://") || host.starts_with("https://") {
            host.to_string()
        } else {
            format!("http://{host}")
        }
    }

    async fn error_message(resp: Response) -> String {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        format_error_body(status, &body)
    }

    /// Shared `GET /collections/{name}` fetch. Both the dimension and the
    /// multi-vector probe discriminate on this same response.
    async fn fetch_collection(
        host: &str,
        collection: &str,
        api_key: Option<&str>,
    ) -> Result<Value> {
        let base_url = Self::normalize_base_url(host);
        let client = Self::make_client(&base_url, api_key)?;
        let resp = client
            .get(format!("{base_url}/collections/{collection}"))
            .send()
            .await
            .with_context(|| format!("Failed to connect to {host}"))?;
        if !resp.status().is_success() {
            bail!(
                "Failed to read collection '{collection}': {}",
                Self::error_message(resp).await
            );
        }

        Ok(resp.json().await?)
    }

    /// Builds a provider for a Coyote-owned collection, which is allowed not to
    /// exist yet.
    ///
    /// Existence is `rebuild_indexes`' business, not the constructor's. Several
    /// guards tell the user to delete the collection in Qdrant and rebuild from
    /// the local copy; a constructor that refused an absent collection turned
    /// that advice into a dead end, because the RAG has to load before
    /// `.rebuild rag` can run at all. The only way out was then a full paid
    /// re-embed.
    ///
    /// The probe is `/collections/{c}/exists` and its answer is deliberately
    /// discarded — what is wanted from it is that it answered at all. It splits
    /// "the server is fine, the collection is gone" (HTTP 200 with
    /// `exists: false`) from everything that must still fail here: an
    /// unreachable host or DNS failure, a rejected api key, a 5xx, and a
    /// mistyped base url behind a proxy that answers every path, which this
    /// endpoint catches because only a Qdrant returns the shape it expects.
    /// Treating a 404 on the plain collection fetch as "absent" would have
    /// tolerated that last case and built a provider aimed at nothing.
    pub async fn new_owned(host: &str, collection: &str, api_key: Option<&str>) -> Result<Self> {
        let base_url = Self::normalize_base_url(host);
        let client = Self::make_client(&base_url, api_key)?;
        if !Self::collection_exists(&client, &base_url, collection).await? {
            debug!(
                "Qdrant collection '{collection}' does not exist on {base_url}; the next full \
                 rebuild recreates it and repopulates it from the local copy."
            );
        }

        Ok(Self::build(client, base_url, collection, false))
    }

    /// Builds a provider for a collection Coyote does not own, which must
    /// already exist.
    ///
    /// An absent one here is a genuine misconfiguration — a typo, or a
    /// collection somebody else deleted — and there is nothing Coyote could
    /// recreate from, so it fails at construction.
    pub async fn new_attached(host: &str, collection: &str, api_key: Option<&str>) -> Result<Self> {
        let base_url = Self::normalize_base_url(host);
        let client = Self::make_client(&base_url, api_key)?;
        let resp = client
            .get(format!("{base_url}/collections/{collection}"))
            .send()
            .await
            .with_context(|| format!("Failed to connect to {host}"))?;
        if !resp.status().is_success() {
            bail!(
                "Collection '{collection}' not accessible at {host}: {}",
                Self::error_message(resp).await
            );
        }

        Ok(Self::build(client, base_url, collection, true))
    }

    fn build(client: Client, base_url: String, collection: &str, attached: bool) -> Self {
        Self {
            client,
            base_url,
            collection: collection.to_string(),
            point_ids: Arc::default(),
            local_content: HashMap::new(),
            attached,
        }
    }

    /// Caches every chunk's text so `fetch_content` can answer without a request.
    ///
    /// Only meaningful for a Coyote-owned collection: `data.files` is empty for an
    /// attached one, so the caller skips this rather than build an empty map.
    pub fn with_local_content(mut self, data: &RagData) -> Self {
        self.local_content = build_local_content(data);
        self
    }

    pub async fn list_collections(host: &str, api_key: Option<&str>) -> Result<Vec<String>> {
        let base_url = Self::normalize_base_url(host);
        let client = Self::make_client(&base_url, api_key)?;
        let resp = client
            .get(format!("{base_url}/collections"))
            .send()
            .await
            .with_context(|| format!("Failed to connect to {host}"))?;
        if !resp.status().is_success() {
            bail!(
                "Failed to list collections: {}",
                Self::error_message(resp).await
            );
        }

        let body: Value = resp.json().await?;
        let names = body["result"]["collections"]
            .as_array()
            .context("Unexpected /collections response shape")?
            .iter()
            .filter_map(|v| v["name"].as_str().map(str::to_string))
            .collect();

        Ok(names)
    }

    pub async fn get_vector_dimension(
        host: &str,
        collection: &str,
        api_key: Option<&str>,
    ) -> Result<u64> {
        let body = Self::fetch_collection(host, collection, api_key).await?;

        vector_dimension_from_collection(&body)
    }

    pub async fn is_multi_vector(
        host: &str,
        collection: &str,
        api_key: Option<&str>,
    ) -> Result<bool> {
        let body = Self::fetch_collection(host, collection, api_key).await?;

        Ok(is_multi_vector_config(&body))
    }

    pub async fn sample_point_id(
        host: &str,
        collection: &str,
        api_key: Option<&str>,
    ) -> Result<Option<String>> {
        let base_url = Self::normalize_base_url(host);
        let client = Self::make_client(&base_url, api_key)?;
        let url = format!("{base_url}/collections/{collection}/points/scroll");
        let body = serde_json::json!({ "limit": 1, "with_payload": false });

        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {host}"))?;

        if !resp.status().is_success() {
            bail!(
                "Failed to sample a point from '{collection}': {}",
                Self::error_message(resp).await
            );
        }

        let data: Value = resp.json().await?;
        let id_val = data["result"]["points"]
            .as_array()
            .and_then(|pts| pts.first())
            .map(|pt| pt["id"].to_string());

        Ok(id_val)
    }

    /// `GET /collections/{c}/exists`.
    ///
    /// This and `create_collection` take a client instead of `&self` because the
    /// creation wizard calls them before there is any provider to call them on.
    /// The client comes from `make_client` and already carries the api key.
    pub(crate) async fn collection_exists(
        client: &Client,
        base_url: &str,
        collection: &str,
    ) -> Result<bool> {
        let resp = client
            .get(format!("{base_url}/collections/{collection}/exists"))
            .send()
            .await
            .with_context(|| format!("Failed to connect to {base_url}"))?;
        if !resp.status().is_success() {
            bail!(
                "Failed to check whether collection '{collection}' exists: {}",
                Self::error_message(resp).await
            );
        }

        let body: Value = resp.json().await.with_context(|| {
            format!("Unreadable collection-exists response for '{collection}' on {base_url}")
        })?;

        collection_exists_from_body(&body)
    }

    /// Creates the collection, then the `file_id` payload index the schema
    /// requires.
    ///
    /// One unnamed vector with Cosine distance and server defaults for everything
    /// else. Cosine matches the DuckDB driver and the read path's client-side
    /// `> min_score` filtering. It also L2-normalizes on write — write
    /// `[3, 0, 0, 0]` and read back `[1, 0, 0, 0]` — which is exactly why vectors
    /// are never hydrated back from Qdrant: the local YAML copy is the only
    /// authoritative one.
    ///
    /// The index is created here rather than by the caller so the two cannot
    /// drift apart.
    pub(crate) async fn create_collection(
        client: &Client,
        base_url: &str,
        collection: &str,
        dim: usize,
    ) -> Result<()> {
        let body = serde_json::json!({ "vectors": { "size": dim, "distance": "Cosine" } });
        let resp = client
            .put(format!("{base_url}/collections/{collection}"))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {base_url}"))?;
        if !resp.status().is_success() {
            bail!(
                "Failed to create collection '{collection}': {}",
                Self::error_message(resp).await
            );
        }

        Self::ensure_file_id_index(client, base_url, collection).await
    }

    /// Asserts the `file_id` payload index, typed `integer`.
    ///
    /// Not for queries: nothing in this file sends a `filter` — `vector_search`
    /// posts only `vector`, `limit` and `with_payload`, and deletes go by
    /// explicit id. What the index carries is the field's declared type, and the
    /// type is what is load-bearing here: the scroll reads `payload["file_id"]`
    /// with `as_u64()`, which answers `None` for a JSON string and trips the
    /// mandatory-field bail in the reconcile.
    ///
    /// Safe to repeat, which is why every full rebuild re-asserts it rather than
    /// leaving it to collection creation alone: a create that failed between the
    /// collection and its index would otherwise leave the field untyped forever,
    /// with no path back. Observed against Qdrant 1.19.0, re-asserting an
    /// identical index answers HTTP 200 `status: "completed"` in tens of
    /// milliseconds, and asserting `integer` over an existing `keyword` index
    /// also answers 200 and replaces it, leaving `payload_schema.file_id` typed
    /// `integer`.
    ///
    /// `wait=true` because index creation is a write like any other, and the
    /// index has to exist before the first point lands.
    async fn ensure_file_id_index(client: &Client, base_url: &str, collection: &str) -> Result<()> {
        let body = serde_json::json!({ "field_name": "file_id", "field_schema": "integer" });
        let resp = client
            .put(format!(
                "{base_url}/collections/{collection}/index?wait=true"
            ))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to connect to {base_url}"))?;
        if !resp.status().is_success() {
            bail!(
                "Failed to create the file_id payload index on '{collection}': {}",
                Self::error_message(resp).await
            );
        }
        let body: Value = resp.json().await.with_context(|| {
            format!("Unreadable index-creation response for '{collection}' on {base_url}")
        })?;

        check_write_completed(&body, "file_id payload index creation")
    }

    /// Creates the collection a new Coyote-owned RAG will write to, or adopts an
    /// existing empty one.
    ///
    /// Static, and not a method, because the creation wizard calls this before
    /// it builds a provider.
    ///
    /// The collection is created eagerly, at wizard time, rather than on the
    /// first sync: a wrong host, an unusable name or a dimension clash has to
    /// fail before the user pays for an embedding pass over their whole corpus.
    pub(crate) async fn ensure_owned_collection(
        host: &str,
        collection: &str,
        api_key: Option<&str>,
        dim: usize,
        embedding_model: &str,
    ) -> Result<CollectionAction> {
        let base_url = Self::normalize_base_url(host);
        let client = Self::make_client(&base_url, api_key)?;

        let existing = if Self::collection_exists(&client, &base_url, collection).await? {
            Some(Self::fetch_collection(host, collection, api_key).await?)
        } else {
            None
        };

        let action =
            plan_owned_collection(existing.as_ref(), collection, host, dim, embedding_model)?;
        if action == CollectionAction::Create {
            Self::create_collection(&client, &base_url, collection, dim).await?;
        }

        Ok(action)
    }

    /// Checks the existing collection against what both paths assume before
    /// anything is written to it, so a mismatch is one clear error rather than a
    /// mid-batch 400.
    async fn preflight(&self, local_dim: Option<usize>) -> Result<()> {
        let resp = self
            .client
            .get(format!("{}/collections/{}", self.base_url, self.collection))
            .send()
            .await
            .with_context(|| format!("Failed to connect to {}", self.base_url))?;
        if !resp.status().is_success() {
            bail!(
                "Failed to read collection '{}': {}",
                self.collection,
                Self::error_message(resp).await
            );
        }
        let body: Value = resp.json().await.with_context(|| {
            format!(
                "Unreadable collection config for '{}' on {}",
                self.collection, self.base_url
            )
        })?;

        check_collection_config(&body, &self.collection, &self.base_url, local_dim)
    }

    /// Pages the whole collection into the map the reconcile judges.
    ///
    /// Collect only: nothing is decided here. The scan also has to finish before
    /// the first write. Qdrant's scroll is a cursor over an ordered id space, not
    /// a snapshot, so a write interleaved with it can move a point past the
    /// cursor and out of the scan entirely — and a point the scan never saw is a
    /// point the reconcile cannot account for.
    async fn scroll_all(&self) -> Result<BTreeMap<u64, owned::RemotePoint>> {
        let url = format!(
            "{}/collections/{}/points/scroll",
            self.base_url, self.collection
        );
        let mut remote = BTreeMap::new();
        let mut offset: Option<Value> = None;

        loop {
            let resp = self
                .client
                .post(&url)
                .json(&scroll_request(offset.as_ref()))
                .send()
                .await
                .with_context(|| format!("Failed to connect to {}", self.base_url))?;
            if !resp.status().is_success() {
                bail!(
                    "Failed to scroll collection '{}': {}",
                    self.collection,
                    Self::error_message(resp).await
                );
            }
            let body: Value = resp.json().await.with_context(|| {
                format!(
                    "Unreadable scroll response for collection '{}' on {}",
                    self.collection, self.base_url
                )
            })?;
            let points = body["result"]["points"]
                .as_array()
                .context("Unexpected /points/scroll response shape")?;
            let page_is_empty = points.is_empty();

            for point in points {
                let Some(id) = point["id"].as_u64() else {
                    bail!(
                        "Refusing to sync: collection '{}' on {} holds a point with a \
                         non-integer id ({}). Coyote-owned collections use integer ids \
                         exclusively, so this collection holds data Coyote did not write. \
                         Point this RAG at a different collection, or delete the collection in \
                         Qdrant if the other data is disposable.",
                        self.collection,
                        self.base_url,
                        point["id"]
                    );
                };
                remote.insert(id, remote_point(&point["payload"]));
            }

            // Null on the last page, and only there.
            match body["result"]["next_page_offset"].clone() {
                Value::Null => break,
                cursor => {
                    // A server that hands back the offset it was given, or that
                    // pages nothing while still promising more, would spin this
                    // loop forever over a map that only grows.
                    if page_is_empty || offset.as_ref() == Some(&cursor) {
                        bail!(
                            "Failed to scroll collection '{}' on {}: the scroll made no \
                             progress at offset {cursor}, so paging it again would never \
                             finish.",
                            self.collection,
                            self.base_url
                        );
                    }
                    offset = Some(cursor);
                }
            }
        }

        Ok(remote)
    }

    async fn upsert_points(&self, points: Vec<Value>) -> Result<()> {
        let count = points.len();
        let (method, url) = upsert_request(&self.base_url, &self.collection);
        let resp = self
            .client
            .request(method, url)
            .json(&serde_json::json!({ "points": points }))
            .send()
            .await
            .with_context(|| format!("Failed to connect to {}", self.base_url))?;
        if !resp.status().is_success() {
            bail!(
                "Failed to write {count} points to collection '{}': {}",
                self.collection,
                Self::error_message(resp).await
            );
        }
        let body: Value = resp.json().await.with_context(|| {
            format!(
                "Unreadable upsert response for collection '{}' on {}",
                self.collection, self.base_url
            )
        })?;

        check_write_completed(&body, "upsert")
    }

    async fn delete_points(&self, ids: &[u64]) -> Result<()> {
        let (method, url) = delete_request(&self.base_url, &self.collection);
        let resp = self
            .client
            .request(method, url)
            .json(&serde_json::json!({ "points": ids }))
            .send()
            .await
            .with_context(|| format!("Failed to connect to {}", self.base_url))?;
        if !resp.status().is_success() {
            bail!(
                "Failed to delete {} points from collection '{}': {}",
                ids.len(),
                self.collection,
                Self::error_message(resp).await
            );
        }
        let body: Value = resp.json().await.with_context(|| {
            format!(
                "Unreadable delete response for collection '{}' on {}",
                self.collection, self.base_url
            )
        })?;

        check_write_completed(&body, "delete")
    }
}

#[async_trait]
impl RagProvider for QdrantProvider {
    async fn vector_search(
        &self,
        embedding: &[f32],
        top_k: usize,
        min_score: f32,
    ) -> Result<Vec<(DocumentId, f32)>> {
        let url = format!(
            "{}/collections/{}/points/search",
            self.base_url, self.collection
        );
        // `score_threshold` is deliberately NOT sent. It is metric-aware: on Cosine
        // collections 0.0 means "no floor" as expected, but Euclid collections score
        // by negative distance, where 0.0 filters everything out. The attach wizard
        // does not pin the distance metric, so filter locally instead; i.e. where a
        // 0.0 floor is correctly treated as "no floor" (see `parse_search_hits`).
        let body = serde_json::json!({
            "vector": embedding,
            "limit": top_k,
            "with_payload": false,
        });
        let resp = self.client.post(&url).json(&body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let detail = Self::error_message(resp).await;
            // An owned collection that has gone missing is recoverable from the
            // local copy at no embedding cost, so say which command does it
            // rather than surfacing a bare 404. An attached one is somebody
            // else's to restore.
            if status == StatusCode::NOT_FOUND && !self.attached {
                bail!(
                    "Collection '{}' does not exist on {}. Run `.rebuild rag` to recreate it and \
                     repopulate it from the local copy — no re-embedding is needed. ({detail})",
                    self.collection,
                    self.base_url
                );
            }
            bail!("Qdrant search on '{}' failed: {detail}", self.collection);
        }
        let data: Value = resp.json().await?;
        // The interner is what lets a UUID-keyed collection work: a string id gets
        // a synthetic handle here and the original is replayed by `fetch_content`.
        let mut interner = self.point_ids.write();

        parse_search_hits(&mut interner, &data, min_score)
    }

    async fn fetch_content(&self, ids: &[DocumentId]) -> Result<Vec<(DocumentId, String)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let (local_hits, misses) = split_local_hits(&self.local_content, ids);
        // Nothing left to ask Qdrant for. On an owned collection, whose whole
        // corpus is in the map, that is the ordinary case and the query path costs
        // no request at all.
        if misses.is_empty() {
            return Ok(merge_in_input_order(ids, local_hits, vec![]));
        }

        let url = format!("{}/collections/{}/points", self.base_url, self.collection);
        // Qdrant is asked for the ids it issued, never for a synthetic handle.
        let id_list = self.point_ids.read().outbound_ids(&misses);
        let body = serde_json::json!({
            "ids": id_list,
            "with_payload": true,
        });

        let resp = self.client.post(&url).json(&body).send().await?;

        if !resp.status().is_success() {
            bail!(
                "Qdrant point fetch on '{}' failed: {}",
                self.collection,
                Self::error_message(resp).await
            );
        }
        let data: Value = resp.json().await?;
        let remote = {
            let mut interner = self.point_ids.write();
            parse_points(&mut interner, &data)?
        };
        // `/points` does not guarantee response order matches request order, and the
        // caller's RRF ranking is carried by that order, so the two sources are
        // re-laid over the input positions rather than concatenated.
        Ok(merge_in_input_order(ids, local_hits, remote))
    }

    async fn rebuild_indexes(&mut self, data: &RagData, full_rebuild: bool) -> Result<()> {
        // First, and never relaxed. A silent `Ok(())` would make `.rebuild rag`
        // and `.edit rag-docs` look like they worked while writing nothing to the
        // remote, leaving the user believing the collection was updated.
        if data.attached {
            bail!(
                "This RAG is attached to an external Qdrant collection. Coyote does not own \
                 its documents and cannot rebuild it. Manage the collection directly, or \
                 create a Coyote-owned RAG with `.rag <name>`."
            );
        }

        // Refused at run time, on the one entry point that writes. It must not
        // become a `compile_error!`: attaching to a collection something else
        // writes is supported on 32-bit, and a compile-time refusal would take
        // the whole crate down with the write path. `DocumentId` packs 16/16 bits
        // on a 32-bit target, so every wire id from file index 1 upwards exceeds
        // `u32::MAX`, fails the read path's `usize::try_from` and gets interned as
        // a synthetic handle — while BM25 and the vectors map keep the 16/16-packed
        // id. One document, two ids, two search legs that never agree.
        #[cfg(target_pointer_width = "32")]
        bail!(
            "Coyote cannot write to a Qdrant collection on a 32-bit build: a remote point id \
             does not fit in a 32-bit document id, so the same document would answer to two \
             different ids on the vector and keyword search legs. Attaching to a collection \
             something else writes still works."
        );

        // Refreshed here, and only here: after the guards above and before any
        // HTTP below, so a failed remote write cannot leave the map describing a
        // state the remote never reached. `data` is already the advanced
        // in-memory corpus by the time this runs, so from now on `fetch_content`
        // answers for the new chunks whether or not the write that follows
        // succeeds.
        self.local_content = build_local_content(data);

        // Read straight off `&RagData`, whose `driver_config` is deliberately left
        // un-interpolated; a vault-resolved copy is not this RAG's identity. Never
        // mint a replacement either — a fresh marker orphans every point already
        // in the collection behind the ownership guard.
        let Some(rag_id) = data
            .driver_config
            .get("coyote_rag_id")
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
        else {
            bail!(
                "Refusing to sync: this RAG has no `coyote_rag_id` in its driver_config, which \
                 is how Coyote recognises the points it wrote to collection '{}'. Minting a new \
                 one would orphan every point already there. Restore the RAG's YAML from backup.",
                self.collection
            );
        };
        let writer_id = crate::config::paths::install_id()
            .context("Failed to read this install's identity, which stamps every point written")?;
        // The one authoritative dimension available here: the local vectors
        // themselves. `None` means there is nothing to upsert anyway. One length
        // is enough because a RAG embeds its whole corpus with a single model —
        // a changed model is a `.rebuild rag`, and a mixed collection is refused
        // by the reconcile's embedding-model guard.
        let local_dim = data.vectors.values().next().map(Vec::len);

        let creating_collection =
            !Self::collection_exists(&self.client, &self.base_url, &self.collection).await?;
        if creating_collection {
            if !full_rebuild {
                bail!(
                    "Collection '{}' no longer exists on {}. Run `.rebuild rag` to recreate and \
                     repopulate it from the local copy.",
                    self.collection,
                    self.base_url
                );
            }
            // Recreating destroys nothing: the collection is gone, and the local
            // YAML holds every vector. Without this arm every guard that says
            // "delete the collection in Qdrant" is a dead end.
            let Some(dim) = local_dim else {
                bail!(
                    "Collection '{}' does not exist on {} and this RAG holds no vectors, so \
                     Coyote cannot tell what vector size to create it with. This usually means \
                     the RAG's YAML was truncated or hand-edited; restore it from backup.",
                    self.collection,
                    self.base_url
                );
            };
            Self::create_collection(&self.client, &self.base_url, &self.collection, dim).await?;
        } else {
            self.preflight(local_dim).await?;
        }

        let ctx = owned::ReconcileCtx {
            collection: self.collection.clone(),
            host: self.base_url.clone(),
            rag_id: rag_id.to_string(),
            writer_id,
            embedding_model: data.embedding_model.clone(),
            vectors_empty: data.vectors.is_empty(),
            creating_collection,
        };

        let mut desired: BTreeMap<FileId, owned::DesiredFile> = BTreeMap::new();
        let mut achievable: BTreeSet<u64> = BTreeSet::new();
        for (&file_id, file) in &data.files {
            // Once per file: the bound is on the file index, so checking it per
            // point would ask the same question once per chunk.
            owned::check_wire_range(file_id)?;
            let mut ids = BTreeSet::new();
            for chunk_index in 0..file.documents.len() {
                let document_id = DocumentId::new(file_id, chunk_index);
                let point_id = owned::wire_point_id(document_id);
                ids.insert(point_id);
                // A chunk whose embedding failed has no vector, so it is desired
                // but not achievable this sync.
                if data.vectors.contains_key(&document_id) {
                    achievable.insert(point_id);
                }
            }
            desired.insert(
                file_id,
                owned::DesiredFile {
                    hash: file.hash.clone(),
                    path: file.path.clone(),
                    ids,
                },
            );
        }

        let remote = self.scroll_all().await?;
        // Every guard fires in here, before anything is written. An `Err` leaves
        // the collection untouched.
        let plan = owned::reconcile(&desired, &achievable, &remote, &ctx, full_rebuild)?;
        for warning in &plan.warnings {
            warn!("{warning}");
        }

        // The one place a pre-existing collection's payload index gets repaired.
        // It sits below the reconcile because that is where the ownership guard
        // is decided, and an index is a write: asserting it any earlier would
        // stamp a schema onto a foreign collection before Coyote had established
        // it may touch it at all. The create arm already asserted it, so this
        // only covers collections that were found rather than made.
        if full_rebuild && !creating_collection {
            Self::ensure_file_id_index(&self.client, &self.base_url, &self.collection).await?;
        }

        // Upserts first, deletes last. Both orders converge after a crash, so
        // this is about the window in between: upsert-first leaves stale
        // duplicates, delete-first leaves documents missing, and for a retrieval
        // system stale-but-present beats silently absent.
        for file in &plan.upsert_files {
            let local = data
                .files
                .get(&file.file_id)
                .context("Reconcile selected a file the local set does not hold")?;
            let selected: BTreeSet<u64> = file.point_ids.iter().copied().collect();
            let mut pending = Vec::with_capacity(file.point_ids.len());

            for (chunk_index, document) in local.documents.iter().enumerate() {
                let document_id = DocumentId::new(file.file_id, chunk_index);
                let point_id = owned::wire_point_id(document_id);
                if !selected.contains(&point_id) {
                    continue;
                }
                let vector = data
                    .vectors
                    .get(&document_id)
                    .context("Reconcile selected a point that has no local vector")?;
                let payload = point_payload(
                    file.file_id,
                    chunk_index,
                    &local.hash,
                    &local.path,
                    &document.page_content,
                    &ctx,
                );
                pending.push(pending_point(
                    upsert_point(point_id, vector, payload),
                    format!("chunk {chunk_index} of '{}'", local.path),
                )?);
            }

            // Every selected point, or none of them: a selected id with no local
            // chunk behind it would otherwise be skipped in silence.
            check_selection_complete(&local.path, pending.len(), selected.len())?;

            // A file with nothing achievable yields no batches and so costs no
            // request, which is what keeps the reconcile free to report it every
            // sync.
            for batch in batch_points(pending, UPSERT_BYTE_CAP)? {
                self.upsert_points(batch).await?;
            }
        }

        for ids in delete_id_batches(&plan.delete_ids) {
            self.delete_points(ids).await?;
        }

        Ok(())
    }

    fn duplicate(&self, data: &RagData) -> Box<dyn RagProvider> {
        // Cloning the client shares the connection pool and the injected api-key
        // header. Sharing is correct: both handles address the same remote
        // collection, and neither of them writes to it.
        //
        // The point-id map is shared for the same reason, and because it MUST be:
        // `Rag::clone()` hands the clone `DocumentId`s that the original minted,
        // so a fresh map would resolve them to nothing and `fetch_content` would
        // ask Qdrant for a synthetic handle — zero results, no error. Resetting it
        // would also re-mint handles for ids the original still holds.
        //
        // The content map goes the other way: rebuilt from `data`, never carried
        // from `self`. `Rag::clone` calls this with pre-mutation data and only then
        // mutates and rebuilds, so a carried map would outlive the corpus it
        // described and be served as if current. Attached data holds no files,
        // which makes this an empty map and leaves the clone on the remote path.
        //
        // `attached` comes from `data` for the same reason: the clone describes
        // the corpus it was handed, and that is what decides whether a missing
        // collection is Coyote's to rebuild.
        Box::new(Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            collection: self.collection.clone(),
            point_ids: Arc::clone(&self.point_ids),
            local_content: build_local_content(data),
            attached: data.attached,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::owned::*;
    use super::*;
    use crate::rag::FileId;
    use crate::rag::{RagDocument, RagFile};
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    #[test]
    fn error_message_reads_the_object_status_envelope() {
        let body =
            r#"{"status": {"error": "Wrong input: Not existing vector name error:"}, "time": 0.0}"#;
        let msg = format_error_body(StatusCode::BAD_REQUEST, body);
        assert!(msg.contains("Not existing vector name"), "got: {msg}");
        assert!(
            !msg.contains("EOF"),
            "must not fall through to a parse error"
        );
    }

    #[test]
    fn error_message_survives_the_string_status_and_the_empty_body() {
        let ok = format_error_body(StatusCode::OK, r#"{"status": "ok", "time": 0.0}"#);
        assert!(
            ok.contains("200"),
            "no `status.error` present → fall back to status+body: {ok}"
        );

        let empty = format_error_body(StatusCode::NOT_FOUND, "");

        assert!(empty.contains("empty body"), "got: {empty}");
        assert!(
            empty.contains("verb"),
            "the message must point at the likely cause: {empty}"
        );
    }

    #[test]
    fn vector_dimension_handles_both_collection_shapes() {
        let unnamed = serde_json::json!({
            "result": {"config": {"params": {"vectors": {"size": 1536, "distance": "Cosine"}}}}
        });
        assert_eq!(vector_dimension_from_collection(&unnamed).unwrap(), 1536);

        let named = serde_json::json!({
            "result": {"config": {"params": {"vectors": {"text": {"size": 768, "distance": "Cosine"}}}}}
        });
        assert_eq!(vector_dimension_from_collection(&named).unwrap(), 768);

        let junk = serde_json::json!({"result": {"config": {"params": {}}}});
        assert!(vector_dimension_from_collection(&junk).is_err());
    }

    #[test]
    fn is_multi_vector_rejects_the_named_single_collection() {
        let unnamed = serde_json::json!({
            "result": {"config": {"params": {"vectors": {"size": 1536, "distance": "Cosine"}}}}
        });
        assert!(!is_multi_vector_config(&unnamed));

        let named_single = serde_json::json!({
            "result": {"config": {"params": {"vectors": {"text": {"size": 1536}}}}}
        });
        assert!(
            is_multi_vector_config(&named_single),
            "named-single must be rejected too"
        );

        let named_multi = serde_json::json!({
            "result": {"config": {"params": {"vectors": {"text": {"size": 1536}, "image": {"size": 512}}}}}
        });
        assert!(is_multi_vector_config(&named_multi));
    }

    #[test]
    fn normalize_base_url_only_adds_a_scheme_when_missing() {
        assert_eq!(
            QdrantProvider::normalize_base_url("qdrant.example.com:6333"),
            "http://qdrant.example.com:6333"
        );
        assert_eq!(
            QdrantProvider::normalize_base_url("https://xyz.cloud.qdrant.io"),
            "https://xyz.cloud.qdrant.io"
        );
        assert_eq!(
            QdrantProvider::normalize_base_url("http://localhost:6333"),
            "http://localhost:6333"
        );
    }

    #[test]
    fn collection_exists_reads_only_a_real_bool() {
        assert!(
            !collection_exists_from_body(&serde_json::json!({"result": {"exists": false}}))
                .unwrap()
        );
        assert!(
            collection_exists_from_body(&serde_json::json!({"result": {"exists": true}})).unwrap()
        );

        // Everything below is a 200 that is not a Qdrant answering. Reading any
        // of them as `false` would let `new_owned` build a provider aimed at a
        // host that never had the collection, and the failure would only surface
        // at the first rebuild.
        collection_exists_from_body(&serde_json::json!({}))
            .expect_err("an empty body has no `exists` to read");
        collection_exists_from_body(&Value::from("<html><body>hi</body></html>"))
            .expect_err("a catch-all proxy's HTML page is not an exists probe");
        collection_exists_from_body(&serde_json::json!({"result": {"exists": "no"}}))
            .expect_err("a string is not a bool, however much it reads like one");
    }

    /// A server on an ephemeral port that answers every request with the same
    /// status and body, and records each request line it was asked for.
    ///
    /// The request head is read to its blank line before the reply is written:
    /// a server that answers and closes without draining leaves the client
    /// seeing a connection reset instead of the status code under test, which
    /// would make every assertion below pass for the wrong reason.
    fn canned_server(status: &str, body: &str) -> (String, Arc<RwLock<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(RwLock::new(Vec::new()));

        let seen = Arc::clone(&requests);
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Ok(peer) = stream.try_clone() else {
                    continue;
                };
                let mut reader = BufReader::new(peer);

                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                seen.write().push(line.trim_end().to_string());
                // Drain the headers. `<= 2` catches both the blank CRLF that
                // ends the head and an EOF part-way through it.
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) <= 2 {
                        break;
                    }
                }

                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        (base_url, requests)
    }

    /// A server on an ephemeral port that picks its reply per request. Routes are
    /// matched in order as substrings of the request line and the first hit wins,
    /// so callers list the specific paths before the prefixes they share. Every
    /// request line is recorded in the order it arrived.
    ///
    /// Two things it does that `canned_server` does not, both needed once the
    /// requests carry bodies. The body is drained as well as the head: closing a
    /// socket with an unread request body still queued on it sends an RST, which
    /// destroys the response the client has not finished reading. And the reply
    /// closes the connection, so a rebuild's run of requests cannot pick up a
    /// pooled socket this fixture has already dropped.
    ///
    /// An unmatched request is answered with a 500 so a missing route surfaces as
    /// a rebuild that failed at a named step rather than as a parse error.
    fn scripted_server(routes: Vec<(&'static str, String)>) -> (String, Arc<RwLock<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(RwLock::new(Vec::new()));

        let seen = Arc::clone(&requests);
        let respond = |status: &str, body: &str| {
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let responses: Vec<(&'static str, String)> = routes
            .into_iter()
            .map(|(needle, body)| (needle, respond("200 OK", &body)))
            .collect();
        let unmatched = respond(
            "500 Internal Server Error",
            r#"{"status":{"error":"no route"}}"#,
        );

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Ok(peer) = stream.try_clone() else {
                    continue;
                };
                let mut reader = BufReader::new(peer);

                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let request_line = line.trim_end().to_string();
                seen.write().push(request_line.clone());

                // Drain the headers, keeping the body length on the way past.
                // `<= 2` catches both the blank CRLF that ends the head and an
                // EOF part-way through it.
                let mut body_len = 0usize;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) <= 2 {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        body_len = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; body_len];
                let _ = reader.read_exact(&mut body);

                let response = responses
                    .iter()
                    .find(|(needle, _)| request_line.contains(needle))
                    .map_or(&unmatched, |(_, response)| response);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        (base_url, requests)
    }

    #[tokio::test]
    async fn new_owned_accepts_an_absent_collection_and_probes_the_exists_endpoint() {
        let (host, requests) = canned_server("200 OK", r#"{"result":{"exists":false}}"#);

        QdrantProvider::new_owned(&host, "docs", None)
            .await
            .expect("an absent collection is the cold-start case the next full rebuild fixes");

        let seen = requests.read();
        assert!(
            seen.iter()
                .any(|line| line.contains("/collections/docs/exists")),
            "the probe has to be the exists endpoint — a plain collection fetch cannot tell \
             an absent collection from an unreachable host: {seen:?}"
        );
    }

    #[tokio::test]
    async fn new_attached_refuses_a_collection_that_is_not_there() {
        let (host, _requests) = canned_server("404 Not Found", r#"{"status":{"error":"gone"}}"#);

        let Err(err) = QdrantProvider::new_attached(&host, "docs", None).await else {
            panic!("Coyote owns no copy of an attached collection and cannot recreate it");
        };

        assert!(err.to_string().contains("not accessible"), "got: {err}");
    }

    #[tokio::test]
    async fn both_constructors_fail_fast_on_a_rejected_api_key() {
        // The 403 that a wrong key produces must never be read as "absent" by
        // the owned arm: that would defer a credentials problem to the first
        // rebuild, which then recreates the collection instead of reporting it.
        let (host, _requests) = canned_server("403 Forbidden", r#"{"status":{"error":"denied"}}"#);

        assert!(
            QdrantProvider::new_owned(&host, "docs", Some("wrong"))
                .await
                .is_err(),
            "a rejected key is not an absent collection"
        );
        assert!(
            QdrantProvider::new_attached(&host, "docs", Some("wrong"))
                .await
                .is_err(),
            "a rejected key is not an accessible collection"
        );
    }

    #[tokio::test]
    async fn new_owned_rejects_a_200_that_is_not_a_qdrant() {
        // A mistyped base url behind a proxy that answers every path with its
        // own page. The status is fine; only the body gives it away.
        let (host, _requests) = canned_server("200 OK", "<html><body>Not Qdrant</body></html>");

        assert!(
            QdrantProvider::new_owned(&host, "docs", None)
                .await
                .is_err(),
            "a provider must not be built against a host that is not a Qdrant"
        );
    }

    /// Both constructors have to fail when nothing is listening at all. This was
    /// the original bug: an unreachable host read as "the collection is absent",
    /// so setup succeeded and the RAG only broke later, at query time.
    #[tokio::test]
    async fn both_constructors_reject_an_unreachable_host() {
        const CLOSED: &str = "http://127.0.0.1:1";

        assert!(
            QdrantProvider::new_owned(CLOSED, "docs", None)
                .await
                .is_err(),
            "a connect failure is not an absent collection"
        );
        assert!(
            QdrantProvider::new_attached(CLOSED, "docs", None)
                .await
                .is_err(),
            "a connect failure is not an accessible collection"
        );
    }

    #[tokio::test]
    async fn rebuild_indexes_refuses_an_attached_rag_and_an_unmarked_owned_one() {
        let mut provider = QdrantProvider {
            client: Client::new(),
            // Both refusals land before the first request, so an unroutable port
            // is a check that neither of them reaches the network.
            base_url: "http://127.0.0.1:1".to_string(),
            collection: "c".to_string(),
            point_ids: Arc::default(),
            local_content: HashMap::new(),
            attached: false,
        };

        let attached = RagData {
            driver: "qdrant".to_string(),
            attached: true,
            ..Default::default()
        };
        let err = provider
            .rebuild_indexes(&attached, true)
            .await
            .expect_err("an attached qdrant RAG must never report a successful rebuild");
        assert!(err.to_string().contains("cannot rebuild"), "got: {err}");

        // An owned RAG whose YAML lost its ownership marker. Minting a fresh one
        // would orphan every point already in the collection behind the ownership
        // guard, so this stops before it can write anything.
        let owned = RagData {
            driver: "qdrant".to_string(),
            attached: false,
            ..Default::default()
        };
        let err = provider
            .rebuild_indexes(&owned, true)
            .await
            .expect_err("an owned RAG with no coyote_rag_id must not be synced");
        assert!(err.to_string().contains("coyote_rag_id"), "got: {err}");
    }

    #[tokio::test]
    async fn fetch_content_short_circuits_on_an_empty_id_list() {
        let provider = QdrantProvider {
            client: Client::new(),
            base_url: "http://127.0.0.1:1".to_string(),
            collection: "c".to_string(),
            point_ids: Arc::default(),
            local_content: HashMap::new(),
            attached: false,
        };

        assert!(provider.fetch_content(&[]).await.unwrap().is_empty());
    }

    #[test]
    fn local_and_private_hosts_skip_the_proxy() {
        for host in [
            "http://localhost:6333",
            "http://127.0.0.1:6333",
            "http://192.168.0.56:6333",
            "http://10.1.2.3:6333",
            "http://172.16.4.5:6333",
            "http://qdrant.local:6333",
            "http://[::1]:6333",
        ] {
            assert!(
                QdrantProvider::skips_proxy(host),
                "{host} should not be proxied"
            );
        }
    }

    #[test]
    fn public_hosts_still_honour_the_environment() {
        for host in [
            "https://qdrant.example.com",
            "http://8.8.8.8:6333",
            "https://xyz.eu-central.aws.cloud.qdrant.io:6333",
            "http://172.32.0.1:6333",
        ] {
            assert!(
                !QdrantProvider::skips_proxy(host),
                "{host} must keep the environment's proxy"
            );
        }
    }

    /// Euclid collections score by NEGATIVE distance, so the 0.0 the caller
    /// passes must mean "no floor". Filtering on it drops every hit — the exact
    /// bug that keeps Qdrant's own `score_threshold` off the wire.
    #[test]
    fn a_zero_floor_keeps_negative_euclid_scores() {
        let mut interner = PointIdInterner::default();
        let search = serde_json::json!({
            "result": [
                {"id": 1, "score": -0.12},
                {"id": 2, "score": -8.5},
            ]
        });

        let hits = parse_search_hits(&mut interner, &search, 0.0).unwrap();

        assert_eq!(hits.len(), 2, "a 0.0 floor must not drop negative scores");
    }

    #[test]
    fn a_positive_floor_still_filters() {
        let mut interner = PointIdInterner::default();
        let search = serde_json::json!({
            "result": [
                {"id": 1, "score": 0.9},
                {"id": 2, "score": 0.2},
            ]
        });

        let hits = parse_search_hits(&mut interner, &search, 0.5).unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, DocumentId(1));
    }

    /// A UUID-keyed collection has to survive the whole `vector_search` →
    /// `fetch_content` round trip, and the fetch must ask Qdrant for the ORIGINAL
    /// string id. Parsing ids with `as_u64()` used to drop these hits inside a
    /// `filter_map`, i.e. zero results and no error.
    #[test]
    fn uuid_point_ids_round_trip_and_are_requested_verbatim() {
        let mut interner = PointIdInterner::default();
        let first_uuid = "3f1b0c2e-1111-4000-8000-000000000001";
        let second_uuid = "3f1b0c2e-2222-4000-8000-000000000002";

        let search = serde_json::json!({
            "result": [
                {"id": first_uuid, "score": 0.91},
                {"id": second_uuid, "score": 0.42},
            ]
        });
        let hits = parse_search_hits(&mut interner, &search, 0.0).unwrap();
        assert_eq!(hits.len(), 2, "string ids must not be silently dropped");

        let ids: Vec<DocumentId> = hits.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            interner.outbound_ids(&ids),
            vec![Value::from(first_uuid), Value::from(second_uuid)],
            "the fetch must send the ids Qdrant issued, not the handles"
        );

        // Qdrant may answer /points in any order; the handles still map back and
        // the caller's RRF ranking is recoverable.
        let points = serde_json::json!({
            "result": [
                {"id": second_uuid, "payload": {"page_content": "second"}},
                {"id": first_uuid, "payload": {"page_content": "first"}},
            ]
        });
        let mut rows = parse_points(&mut interner, &points).unwrap();
        let position: HashMap<DocumentId, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        rows.sort_by_key(|(id, _)| position.get(id).copied().unwrap_or(usize::MAX));
        assert_eq!(
            rows,
            vec![
                (ids[0], "first".to_string()),
                (ids[1], "second".to_string())
            ]
        );
    }

    /// Integer-keyed collections must be untouched by the interner: the id maps to
    /// itself on the way in and goes back out as the same integer.
    #[test]
    fn integer_point_ids_are_passed_through_untouched() {
        let mut interner = PointIdInterner::default();
        let search = serde_json::json!({
            "result": [{"id": 7, "score": 0.9}, {"id": 0, "score": 0.5}]
        });

        let hits = parse_search_hits(&mut interner, &search, 0.0).unwrap();
        assert_eq!(
            hits,
            vec![(DocumentId(7), 0.9_f32), (DocumentId(0), 0.5_f32)]
        );

        let ids: Vec<DocumentId> = hits.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            interner.outbound_ids(&ids),
            vec![Value::from(7_u64), Value::from(0_u64)],
            "integer ids must not be regressed into synthetic handles"
        );
        assert!(
            interner.raw_id(DocumentId(7)).is_none(),
            "a plain integer id is its own id and needs no map entry"
        );
    }

    /// Synthetic handles are stable per point id and live in a range no packed
    /// `DocumentId` can reach.
    #[test]
    fn synthetic_handles_are_stable_and_never_collide_with_packed_ids() {
        let mut interner = PointIdInterner::default();
        let uuid = Value::from("9d2f0a11-3333-4000-8000-00000000000a");

        let handle = interner.document_id(&uuid).unwrap();
        assert_eq!(
            interner.document_id(&uuid).unwrap(),
            handle,
            "the same point id must keep the same handle across queries"
        );
        assert_ne!(
            interner.document_id(&Value::from("other")).unwrap(),
            handle,
            "distinct point ids must not share a handle"
        );
        assert_ne!(handle.0 & SYNTHETIC_ID_TAG, 0, "a handle carries the tag");

        // A packed (file_index, document_index) never sets the tag bit: it is the
        // top bit of the file index, which would take 2^31 indexed files.
        for (file_index, document_index) in [(0, 0), (1, 0), (0, 4242), (1_000_000, 999)] {
            assert_eq!(
                DocumentId::new(file_index, document_index).0 & SYNTHETIC_ID_TAG,
                0,
                "packed ({file_index}, {document_index}) must stay out of the handle range"
            );
        }

        // The one integer id that WOULD land on the tag is interned instead of
        // being handed back as itself, so it cannot alias a handle.
        let collides = Value::from(SYNTHETIC_ID_TAG as u64);
        let interned = interner.document_id(&collides).unwrap();
        assert_eq!(interner.raw_id(interned), Some(&collides));
        assert_eq!(
            interner.outbound_ids(&[interned]),
            vec![collides],
            "the original integer must still be what Qdrant is asked for"
        );
    }

    /// `duplicate()` shares the map rather than resetting it: `Rag::clone()` hands
    /// the clone `DocumentId`s the original minted, and a fresh map would turn
    /// those into requests for a synthetic handle — zero results, no error.
    #[test]
    fn duplicate_shares_the_point_id_map() {
        let provider = QdrantProvider {
            client: Client::new(),
            base_url: "http://127.0.0.1:1".to_string(),
            collection: "c".to_string(),
            point_ids: Arc::default(),
            local_content: HashMap::new(),
            attached: false,
        };
        let uuid = Value::from("c0ffee00-4444-4000-8000-000000000007");
        let handle = provider.point_ids.write().document_id(&uuid).unwrap();

        let dup = provider.duplicate(&RagData {
            driver: "qdrant".to_string(),
            attached: true,
            ..Default::default()
        });
        // Downcasting is not available through `dyn RagProvider`, so go via the
        // shared Arc: the clone must observe the original's interning.
        assert_eq!(Arc::strong_count(&provider.point_ids), 2);
        assert_eq!(provider.point_ids.read().raw_id(handle), Some(&uuid));
        drop(dup);
    }

    /// An owned corpus of two files whose chunks all carry text, but where
    /// `(0, 1)` has NO vector — no embedding means no remote point, so only the
    /// local map can ever answer for it. Populating `vectors` alone would leave
    /// `build_local_content` empty and every assertion below vacuously true, so
    /// `files` is the part that matters here.
    fn owned_rag_data() -> RagData {
        let mut data = RagData {
            embedding_model: MODEL.to_string(),
            driver: "qdrant".to_string(),
            attached: false,
            ..Default::default()
        };
        data.files.insert(
            0,
            RagFile {
                hash: "h0".to_string(),
                path: "/tmp/a.md".to_string(),
                documents: vec![
                    RagDocument {
                        page_content: "alpha".to_string(),
                        metadata: Default::default(),
                    },
                    RagDocument {
                        page_content: "vectorless".to_string(),
                        metadata: Default::default(),
                    },
                ],
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
        data.vectors.insert(DocumentId::new(0, 0), vec![1.0, 0.0]);
        data.vectors.insert(DocumentId::new(1, 0), vec![0.0, 1.0]);
        data
    }

    /// A provider pointed at a closed port: any request it makes is an `Err`, so
    /// an `Ok` result is itself evidence the network was never touched.
    fn unroutable_provider(local_content: HashMap<DocumentId, String>) -> QdrantProvider {
        QdrantProvider {
            client: Client::new(),
            base_url: "http://127.0.0.1:1".to_string(),
            collection: "c".to_string(),
            point_ids: Arc::default(),
            local_content,
            attached: false,
        }
    }

    /// `rebuild_indexes` refreshes the content map before it touches the network,
    /// so a chunk with no vector — and therefore no remote point — still resolves.
    ///
    /// The fixture deliberately carries no `coyote_rag_id`, so the rebuild stops
    /// at the ownership guard. That guard sits above `install_id()` (which would
    /// read the real user config dir) and above the first request, so the fetch
    /// below can only find anything if the refresh ran above it: move the refresh
    /// under the guard, or under the HTTP, and the map is empty and this fails.
    /// The unroutable `base_url` is the other half of the proof — a request would
    /// turn the fetch into an `Err`.
    #[tokio::test]
    async fn rebuild_indexes_refreshes_local_content_before_touching_the_network() {
        let mut provider = unroutable_provider(HashMap::new());
        let data = owned_rag_data();
        let vectorless = DocumentId::new(0, 1);
        assert!(
            !data.vectors.contains_key(&vectorless),
            "the chunk under test must have no vector, or it proves nothing"
        );

        let err = provider
            .rebuild_indexes(&data, true)
            .await
            .expect_err("an owned RAG with no coyote_rag_id must not be synced");
        assert!(
            err.to_string().contains("coyote_rag_id"),
            "the rebuild must stop at the ownership guard, above install_id and any \
             request: {err}"
        );

        let out = provider
            .fetch_content(&[vectorless])
            .await
            .expect("the refreshed map must answer without a request");
        assert_eq!(
            out,
            vec![(vectorless, "vectorless".to_string())],
            "the new file's vectorless chunk must resolve to its own text"
        );
    }

    #[tokio::test]
    async fn duplicate_rebuilds_the_content_map_from_the_passed_data() {
        let provider = unroutable_provider(HashMap::new());
        let data = owned_rag_data();
        let vectorless = DocumentId::new(0, 1);

        let dup = provider.duplicate(&data);

        let out = dup
            .fetch_content(&[vectorless])
            .await
            .expect("the clone must answer from the map it built, not from the remote");
        assert_eq!(
            out,
            vec![(vectorless, "vectorless".to_string())],
            "an empty original map must not stop the clone from serving the data it was given"
        );
    }

    #[tokio::test]
    async fn duplicate_does_not_carry_the_originals_content_map() {
        let old_id = DocumentId::new(0, 1);
        let provider = unroutable_provider(build_local_content(&owned_rag_data()));
        let other = RagData {
            driver: "qdrant".to_string(),
            ..Default::default()
        };

        let dup = provider.duplicate(&other);

        assert!(
            dup.fetch_content(&[old_id]).await.is_err(),
            "an id the new data does not hold must fall through to the remote, which is \
             what proves the original's map was not carried across"
        );
    }

    #[test]
    fn merge_in_input_order_emits_by_input_position_across_both_sources() {
        let a = DocumentId::new(0, 0);
        let b = DocumentId::new(0, 1);
        let c = DocumentId::new(1, 0);
        let unresolved = DocumentId::new(9, 0);
        let ids = [a, b, c, unresolved];
        let local_hits = vec![Some("alpha".to_string()), None, None, None];
        // Qdrant answered in its own order, and for neither `a` nor `unresolved`.
        let remote = vec![(c, "gamma".to_string()), (b, "beta".to_string())];

        let out = merge_in_input_order(&ids, local_hits, remote);

        assert_eq!(
            out,
            vec![
                (a, "alpha".to_string()),
                (b, "beta".to_string()),
                (c, "gamma".to_string()),
            ],
            "the output follows the input ids rather than the remote's order, and the id \
             neither source resolved is skipped, not reported"
        );
    }

    /// The half of the split that `merge_in_input_order` trusts blindly: it zips
    /// `hits` against `ids`, so a slot that goes missing rather than being `None`
    /// shifts every later hit onto the wrong id and drops the tail, with no error
    /// to show for it. One slot per input position is what stops that.
    #[test]
    fn split_local_hits_keeps_a_slot_per_input_position() {
        let a = DocumentId::new(0, 0);
        let absent = DocumentId::new(9, 0);
        let b = DocumentId::new(1, 0);
        let local = build_local_content(&owned_rag_data());

        let (hits, misses) = split_local_hits(&local, &[a, absent, b]);

        assert_eq!(
            hits,
            vec![Some("alpha".to_string()), None, Some("beta".to_string())],
            "a miss must leave a hole where it sat, not shorten the row of hits"
        );
        assert_eq!(
            misses,
            vec![absent],
            "only what the map could not answer for is asked of the remote, in input order"
        );
    }

    #[tokio::test]
    async fn fetch_content_serves_an_all_local_id_list_without_a_request() {
        let data = owned_rag_data();
        let provider = unroutable_provider(build_local_content(&data));
        let a = DocumentId::new(0, 0);
        let vectorless = DocumentId::new(0, 1);
        let b = DocumentId::new(1, 0);

        // Scrambled relative to corpus order: the result must follow the argument.
        let out = provider
            .fetch_content(&[b, vectorless, a])
            .await
            .expect("every id hits the map, so no request is made — an Err means one was");

        assert_eq!(
            out,
            vec![
                (b, "beta".to_string()),
                (vectorless, "vectorless".to_string()),
                (a, "alpha".to_string()),
            ],
            "an all-local fetch must still emit in input order"
        );
    }

    #[tokio::test]
    async fn an_attached_provider_has_no_local_content_and_goes_to_the_remote() {
        let attached = RagData {
            driver: "qdrant".to_string(),
            attached: true,
            ..Default::default()
        };
        assert!(
            build_local_content(&attached).is_empty(),
            "attached data holds no files, so there is nothing to cache from it"
        );

        // What the attached arm of `load_async` leaves behind: `with_local_content`
        // is skipped, so the map stays empty.
        let provider = unroutable_provider(HashMap::new());

        assert!(
            provider
                .fetch_content(&[DocumentId::new(0, 0)])
                .await
                .is_err(),
            "with an empty map every id is a miss and the remote is the only source"
        );
    }

    #[test]
    fn build_local_content_keys_on_files_not_vectors() {
        let mut data = owned_rag_data();
        let orphan = DocumentId::new(9, 0);
        data.vectors.insert(orphan, vec![0.0, 1.0]);

        let map = build_local_content(&data);

        assert!(
            !map.contains_key(&orphan),
            "an id present only in `vectors` has no text behind it and must not resolve"
        );
        assert_eq!(
            map.get(&DocumentId::new(0, 1)).map(String::as_str),
            Some("vectorless"),
            "a chunk with no vector still has text, and this map is what serves it"
        );
    }

    const RAG_ID: &str = "3f1a0c00-0000-4000-8000-000000000001";
    const WRITER_ID: &str = "3f1a0c00-0000-4000-8000-0000000000aa";
    const MODEL: &str = "text-embedding-3-small";

    fn base_ctx() -> ReconcileCtx {
        ReconcileCtx {
            collection: "docs".to_string(),
            host: "http://localhost:6333".to_string(),
            rag_id: RAG_ID.to_string(),
            writer_id: WRITER_ID.to_string(),
            embedding_model: MODEL.to_string(),
            vectors_empty: false,
            creating_collection: false,
        }
    }

    fn point_ids(file_id: FileId, chunks: impl IntoIterator<Item = usize>) -> BTreeSet<u64> {
        chunks
            .into_iter()
            .map(|chunk| wire_point_id(DocumentId::new(file_id, chunk)))
            .collect()
    }

    fn desired_file(file_id: FileId, hash: &str, path: &str, chunks: usize) -> DesiredFile {
        DesiredFile {
            hash: hash.to_string(),
            path: path.to_string(),
            ids: point_ids(file_id, 0..chunks),
        }
    }

    fn scroll_file(
        remote: &mut BTreeMap<u64, RemotePoint>,
        file_id: FileId,
        hash: &str,
        path: &str,
        chunks: impl IntoIterator<Item = usize>,
    ) {
        for chunk in chunks {
            remote.insert(
                wire_point_id(DocumentId::new(file_id, chunk)),
                RemotePoint {
                    file_id: Some(file_id as u64),
                    file_hash: hash.to_string(),
                    coyote_rag_id: RAG_ID.to_string(),
                    writer_id: WRITER_ID.to_string(),
                    embedding_model: MODEL.to_string(),
                    path: path.to_string(),
                },
            );
        }
    }

    fn all_ids(desired: &BTreeMap<FileId, DesiredFile>) -> BTreeSet<u64> {
        desired
            .values()
            .flat_map(|file| file.ids.iter().copied())
            .collect()
    }

    fn upserted(out: &Reconcile) -> Vec<FileId> {
        out.upsert_files.iter().map(|file| file.file_id).collect()
    }

    #[test]
    fn wire_point_id_shifts_a_fixed_32() {
        assert_eq!(wire_point_id(DocumentId::new(0, 0)), 0);
        assert_eq!(wire_point_id(DocumentId::new(7, 3)), (7u64 << 32) | 3);
        assert_eq!(wire_file_index(wire_point_id(DocumentId::new(7, 3))), 7);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn owned_wire_ids_round_trip_through_the_integer_fast_path() {
        let id = DocumentId::new(1234, 56);
        let wire = wire_point_id(id);

        // The shift-32 wire format and the 32/32 local packing coincide here,
        // which is what lets the read path parse owned points unchanged.
        assert_eq!(wire, id.0 as u64);
        assert_eq!(
            PointIdInterner::default().document_id(&Value::from(wire)),
            Some(id)
        );
    }

    #[test]
    fn check_wire_range_bails_at_two_to_the_31() {
        assert!(check_wire_range(0).is_ok());
        assert!(check_wire_range((1 << 31) - 1).is_ok());

        let err = check_wire_range(1 << 31).expect_err("2^31 does not fit the wire format");
        assert!(err.to_string().contains("limit 2^31"), "got: {err}");
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn the_largest_allowed_wire_id_stays_below_the_synthetic_tag() {
        let largest = wire_point_id(DocumentId::new((1 << 31) - 1, u32::MAX as usize));

        assert!(largest < SYNTHETIC_ID_TAG as u64);
    }

    #[test]
    fn hash_skipped_file_survives() {
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 3))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..3);

        let out = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false).unwrap();

        assert!(out.upsert_files.is_empty(), "{:?}", out.upsert_files);
        assert!(out.delete_ids.is_empty(), "{:?}", out.delete_ids);
    }

    #[test]
    fn retired_file_id_is_deleted() {
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 2))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..2);
        scroll_file(&mut remote, 2, "hb", "b.md", 0..2);

        let out = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false).unwrap();

        assert!(out.upsert_files.is_empty(), "{:?}", out.upsert_files);
        assert_eq!(
            out.delete_ids,
            point_ids(2, 0..2).into_iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn equal_chunk_remint_deletes_the_stale_tail() {
        // An interrupted sync wrote 10 points for file 5 = fileA; the retry gives
        // file 5 to fileB, also 10 chunks, and can only embed chunks 0..5. The
        // tail is still inside the desired id range, so only the hash conjunct
        // stops fileA's text staying searchable under a FileId that local state
        // believes is fileB.
        let desired = BTreeMap::from([(5, desired_file(5, "hb", "b.md", 10))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 5, "ha", "a.md", 0..10);

        let out = reconcile(&desired, &point_ids(5, 0..5), &remote, &base_ctx(), false).unwrap();

        assert_eq!(upserted(&out), vec![5]);
        assert_eq!(
            out.delete_ids,
            point_ids(5, 5..10).into_iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn partial_re_embed_does_not_livelock_on_an_unembeddable_chunk() {
        // Chunk 3 has no vector and can never exist remotely. Comparing the
        // desired set instead of the achievable one would re-upsert this file on
        // every sync forever.
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 4))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..3);

        let out = reconcile(&desired, &point_ids(1, 0..3), &remote, &base_ctx(), false).unwrap();

        assert!(out.upsert_files.is_empty(), "{:?}", out.upsert_files);
        assert!(out.delete_ids.is_empty(), "{:?}", out.delete_ids);
        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
        assert!(
            out.warnings[0].contains("no embedding yet"),
            "{}",
            out.warnings[0]
        );
    }

    #[test]
    fn partial_re_embed_does_not_livelock_when_the_remote_holds_more() {
        // An earlier sync embedded chunks 2 and 3; this one cannot. Testing set
        // inequality instead of a subset would re-upsert forever.
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 4))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..4);

        let out = reconcile(&desired, &point_ids(1, 0..2), &remote, &base_ctx(), false).unwrap();

        assert!(out.upsert_files.is_empty(), "{:?}", out.upsert_files);
        // 2 and 3 are unachievable now, but their payload hash still agrees, so
        // they are older copies of the same content and stay.
        assert!(out.delete_ids.is_empty(), "{:?}", out.delete_ids);
    }

    #[test]
    fn selected_file_rewrites_every_achievable_point() {
        // The remote already holds chunks 0 and 1; the file is selected because
        // its hash changed and must write ALL four achievable points, not the two
        // the remote is missing. That delta is empty in the re-minted-FileId case,
        // which would leave the old hash in place and retain stale points forever.
        let desired = BTreeMap::from([(1, desired_file(1, "new", "a.md", 4))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "old", "a.md", 0..2);

        let out = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false).unwrap();

        assert_eq!(
            out.upsert_files,
            vec![UpsertFile {
                file_id: 1,
                point_ids: point_ids(1, 0..4).into_iter().collect(),
            }]
        );
    }

    #[test]
    fn an_unembeddable_new_file_is_surfaced_as_an_empty_upsert() {
        let desired = BTreeMap::from([
            (1, desired_file(1, "ha", "a.md", 2)),
            (2, desired_file(2, "hb", "b.md", 1)),
        ]);

        let out = reconcile(
            &desired,
            &point_ids(2, 0..1),
            &BTreeMap::new(),
            &base_ctx(),
            false,
        )
        .unwrap();

        assert_eq!(upserted(&out), vec![1, 2]);
        assert!(
            out.upsert_files[0].point_ids.is_empty(),
            "a file with nothing writable must be visible as a no-op, not a request"
        );
        assert_eq!(
            out.upsert_files[1].point_ids,
            point_ids(2, 0..1).into_iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_empty_remote_is_a_pure_insert() {
        let desired: BTreeMap<FileId, DesiredFile> = (0..3usize)
            .map(|n| (n, desired_file(n, "h", &format!("doc-{n}.md"), 2)))
            .collect();

        let out = reconcile(
            &desired,
            &all_ids(&desired),
            &BTreeMap::new(),
            &base_ctx(),
            true,
        )
        .unwrap();

        assert_eq!(upserted(&out), vec![0, 1, 2]);
        assert!(out.delete_ids.is_empty(), "{:?}", out.delete_ids);
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    }

    #[test]
    fn guard_a_trips_when_the_desired_set_is_empty() {
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..3);

        let err = reconcile(
            &BTreeMap::new(),
            &BTreeSet::new(),
            &remote,
            &base_ctx(),
            true,
        )
        .expect_err("an empty files map against a populated remote is lost local state");

        assert!(err.to_string().contains("has no documents"), "got: {err}");
    }

    #[test]
    fn guard_b_trips_when_the_vectors_map_is_empty() {
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 3))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..3);
        let ctx = ReconcileCtx {
            vectors_empty: true,
            ..base_ctx()
        };

        let err = reconcile(&desired, &BTreeSet::new(), &remote, &ctx, false)
            .expect_err("documents without vectors is a truncated or hand-edited YAML");

        assert!(err.to_string().contains("holds no vectors"), "got: {err}");
    }

    #[test]
    fn guard_b_trips_on_the_create_arm_without_a_populated_remote() {
        // The "remote is populated" conjunct is false by definition when the
        // collection does not exist yet, so keeping it would let a truncated YAML
        // create the collection, write nothing, and report success.
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 3))]);
        let ctx = ReconcileCtx {
            vectors_empty: true,
            creating_collection: true,
            ..base_ctx()
        };

        let err = reconcile(&desired, &BTreeSet::new(), &BTreeMap::new(), &ctx, true)
            .expect_err("a truncated YAML must not create a silently empty collection");

        assert!(err.to_string().contains("holds no vectors"), "got: {err}");
    }

    #[test]
    fn guard_c_trips_on_a_foreign_ownership_marker() {
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 1))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..1);
        remote
            .get_mut(&wire_point_id(DocumentId::new(1, 0)))
            .unwrap()
            .coyote_rag_id = "someone-elses-rag".to_string();

        let err = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false)
            .expect_err("two RAGs on one collection would diff-delete each other");

        let msg = err.to_string();
        assert!(msg.contains("do not belong to this RAG"), "got: {msg}");
        assert!(
            msg.contains("not recoverable from inside Coyote"),
            "got: {msg}"
        );
    }

    #[test]
    fn ownership_is_judged_before_the_mandatory_payload_fields() {
        // A foreign collection must report that it is foreign, not that one of
        // its points happens to lack a `path`.
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 1))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..1);
        let point = remote
            .get_mut(&wire_point_id(DocumentId::new(1, 0)))
            .unwrap();
        point.coyote_rag_id = "someone-elses-rag".to_string();
        point.path = String::new();

        let err = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false)
            .expect_err("a foreign collection must bail on ownership");

        assert!(
            err.to_string().contains("do not belong to this RAG"),
            "got: {err}"
        );
    }

    #[test]
    fn guard_c2_warns_and_does_not_bail() {
        // Two machines using one owned RAG serially is legitimate; only
        // concurrent use is mutual deletion. Bailing would break the legitimate
        // workflow.
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 2))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..2);
        for point in remote.values_mut() {
            point.writer_id = "another-install".to_string();
        }

        let out = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false).unwrap();

        assert!(out.delete_ids.is_empty(), "{:?}", out.delete_ids);
        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
        assert!(
            out.warnings[0].contains("different Coyote install"),
            "{}",
            out.warnings[0]
        );
    }

    #[test]
    fn guard_d_trips_on_a_large_shrinkage() {
        let mut remote = BTreeMap::new();
        for file_id in 0..100usize {
            scroll_file(
                &mut remote,
                file_id,
                "h",
                &format!("doc-{file_id:03}.md"),
                0..1,
            );
        }
        let desired: BTreeMap<FileId, DesiredFile> = (0..10usize)
            .map(|n| (n, desired_file(n, "h", &format!("doc-{n:03}.md"), 1)))
            .collect();

        let err = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false)
            .expect_err("dropping 90 of 100 documents is unresolved source paths, not intent");

        let msg = err.to_string();
        assert!(msg.contains("delete 90 of 100 documents"), "got: {msg}");
        assert!(
            msg.contains("doc-010.md") && msg.contains("doc-019.md"),
            "the message must name example paths: {msg}"
        );
    }

    #[test]
    fn guard_d_does_not_trip_on_a_full_rebuild_turnover() {
        // `.rebuild rag` retires every FileId and mints a new one, so nearly every
        // remote point is legitimately replaced while the paths are untouched.
        // Counting points or deleted ids instead of paths would refuse here. A
        // *completed* turnover leaves the file-id count unchanged, so this shape
        // alone does not separate paths from file ids — the interrupted turnover
        // below is what does that.
        let mut remote = BTreeMap::new();
        for file_id in 0..100usize {
            scroll_file(
                &mut remote,
                file_id,
                "old",
                &format!("doc-{file_id:03}.md"),
                0..1,
            );
        }
        let desired: BTreeMap<FileId, DesiredFile> = (0..100usize)
            .map(|n| {
                (
                    n + 100,
                    desired_file(n + 100, "new", &format!("doc-{n:03}.md"), 1),
                )
            })
            .collect();

        let out = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), true).unwrap();

        assert_eq!(out.upsert_files.len(), 100);
        assert_eq!(out.delete_ids.len(), 100);
    }

    #[test]
    fn guard_d_does_not_trip_on_an_interrupted_turnover() {
        // A `.rebuild rag` killed partway leaves the remote holding BOTH
        // generations of FileIds while local state rolled back — deletes run last
        // and the YAML is only saved on success. This is the shape that separates
        // counting paths from counting file ids, and it is why the unit has to be
        // paths: 100 files with 40 of them re-minted before the crash gives 140
        // remote file ids against 100 local files, which clears the threshold and
        // would refuse to recover from precisely the state this reconcile exists
        // to repair. The paths never moved, so the path count sees no shrinkage.
        let mut remote = BTreeMap::new();
        for file_id in 0..100usize {
            scroll_file(
                &mut remote,
                file_id,
                "old",
                &format!("doc-{file_id:03}.md"),
                0..1,
            );
        }
        // The 40 re-minted before the interruption, over the same paths.
        for n in 0..40usize {
            scroll_file(&mut remote, n + 100, "new", &format!("doc-{n:03}.md"), 0..1);
        }

        let desired: BTreeMap<FileId, DesiredFile> = (0..100usize)
            .map(|n| {
                (
                    n + 100,
                    desired_file(n + 100, "new", &format!("doc-{n:03}.md"), 1),
                )
            })
            .collect();

        // 100 distinct remote paths against 100 distinct local paths: no
        // shrinkage. Counting distinct file ids instead would see 140 against
        // 100, and 40 > max(10, 140 / 4) would bail.
        let out = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), true)
            .expect("an interrupted turnover must stay recoverable");

        // The stale generation goes; the 40 already-written points are rewritten.
        assert_eq!(out.delete_ids.len(), 100);
        assert_eq!(out.upsert_files.len(), 100);
    }

    #[test]
    fn guard_d_does_not_trip_when_the_corpus_grows() {
        // The `usize` underflow catch: a plain `-` panics in debug and wraps to
        // ~1.8e19 in release, which clears every threshold and makes guard D
        // refuse every ordinary sync.
        let mut remote = BTreeMap::new();
        for file_id in 0..3usize {
            scroll_file(
                &mut remote,
                file_id,
                "h",
                &format!("doc-{file_id:03}.md"),
                0..1,
            );
        }
        let desired: BTreeMap<FileId, DesiredFile> = (0..300usize)
            .map(|n| (n, desired_file(n, "h", &format!("doc-{n:03}.md"), 1)))
            .collect();

        let out = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false)
            .expect("a growing corpus must not look like a mass deletion");

        assert_eq!(out.upsert_files.len(), 297);
        assert!(out.delete_ids.is_empty(), "{:?}", out.delete_ids);
    }

    #[test]
    fn a_differing_embedding_model_bails() {
        // Two models of the same width pass every dimension check and leave the
        // collection mixing two vector spaces.
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 1))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..1);
        for point in remote.values_mut() {
            point.embedding_model = "text-embedding-ada-002".to_string();
        }

        let err = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false)
            .expect_err("mixed embedding models make similarity scores meaningless");

        assert!(
            err.to_string().contains("Mixing embedding models"),
            "got: {err}"
        );
    }

    #[test]
    fn a_missing_embedding_model_only_warns() {
        // Absence just means a pre-upgrade write; only divergence is corruption.
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 1))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..1);
        for point in remote.values_mut() {
            point.embedding_model = String::new();
        }

        let out = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false).unwrap();

        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
        assert!(
            out.warnings[0].contains("no embedding_model marker"),
            "{}",
            out.warnings[0]
        );
    }

    #[test]
    fn points_missing_mandatory_payload_fields_bail() {
        /// Blanks one mandatory payload field on an otherwise healthy point.
        type Blank = fn(&mut RemotePoint);

        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 1))]);
        let id = wire_point_id(DocumentId::new(1, 0));
        let blanks: [(&str, Blank); 3] = [
            ("file_id", |point| point.file_id = None),
            ("path", |point| point.path = String::new()),
            ("file_hash", |point| point.file_hash = String::new()),
        ];

        for (field, blank) in blanks {
            let mut remote = BTreeMap::new();
            scroll_file(&mut remote, 1, "ha", "a.md", 0..1);
            blank(remote.get_mut(&id).unwrap());

            let Err(err) = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false)
            else {
                panic!("a point with no `{field}` must bail, never be guessed at");
            };
            assert!(
                err.to_string().contains(&format!("no `{field}`")),
                "got: {err}"
            );
        }
    }

    #[test]
    fn the_id_to_payload_file_id_cross_check_bails_on_disagreement() {
        // Catches corrupt or foreign writes that carry the right ownership marker.
        let desired = BTreeMap::from([(1, desired_file(1, "ha", "a.md", 1))]);
        let mut remote = BTreeMap::new();
        scroll_file(&mut remote, 1, "ha", "a.md", 0..1);
        remote
            .get_mut(&wire_point_id(DocumentId::new(1, 0)))
            .unwrap()
            .file_id = Some(9);

        let err = reconcile(&desired, &all_ids(&desired), &remote, &base_ctx(), false)
            .expect_err("an id and a payload that disagree must not be reconciled");

        assert!(err.to_string().contains("claims file_id 9"), "got: {err}");
    }

    /// Sized from what `serde_json` actually produces, since the batcher measures
    /// rather than estimates.
    fn sized_point(id: u64, text: &str) -> PendingPoint {
        let payload = point_payload(0, id as usize, "h", "a.md", text, &base_ctx());
        pending_point(
            upsert_point(id, &[0.5f32; 4], payload),
            format!("chunk {id} of 'a.md'"),
        )
        .unwrap()
    }

    #[test]
    fn the_payload_carries_every_field_with_integer_ids() {
        let payload = point_payload(7, 3, "hash-a", "notes.md", "chunk text", &base_ctx());

        let fields: BTreeSet<&str> = payload
            .as_object()
            .expect("a payload is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            fields,
            BTreeSet::from([
                "chunk_index",
                "coyote_rag_id",
                "embedding_model",
                "file_hash",
                "file_id",
                "page_content",
                "path",
                "schema_version",
                "writer_id",
            ])
        );

        // The `file_id` payload index is typed `integer`; a string does not match
        // it, and the difference is invisible until a filter returns nothing.
        assert_eq!(payload["file_id"].as_u64(), Some(7));
        assert_eq!(payload["chunk_index"].as_u64(), Some(3));
        assert!(!payload["file_id"].is_string());
        assert!(!payload["chunk_index"].is_string());
        // The read path hardcodes this key.
        assert_eq!(payload["page_content"].as_str(), Some("chunk text"));
        assert_eq!(payload["file_hash"].as_str(), Some("hash-a"));
        assert_eq!(payload["path"].as_str(), Some("notes.md"));
        assert_eq!(payload["coyote_rag_id"].as_str(), Some(RAG_ID));
        assert_eq!(payload["writer_id"].as_str(), Some(WRITER_ID));
        assert_eq!(payload["embedding_model"].as_str(), Some(MODEL));
        assert_eq!(payload["schema_version"].as_u64(), Some(1));
    }

    #[test]
    fn batches_flush_before_the_byte_cap_is_exceeded() {
        let points: Vec<PendingPoint> = (0..3).map(|id| sized_point(id, "x")).collect();
        let each = points[0].bytes;
        assert!(
            points.iter().all(|point| point.bytes == each),
            "the fixture only measures a cap if its points are the same size"
        );

        let batches = batch_points(points, each * 2).unwrap();

        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![2, 1],
            "the third point must start a batch rather than overflow the cap"
        );
    }

    #[test]
    fn a_file_with_nothing_to_write_sends_no_request() {
        assert!(batch_points(vec![], UPSERT_BYTE_CAP).unwrap().is_empty());
    }

    #[test]
    fn a_point_too_large_for_one_request_bails_naming_the_chunk() {
        let err = batch_points(vec![sized_point(4, "x")], 8)
            .expect_err("a point that cannot fit alone would only 400 on the server");

        let msg = err.to_string();
        assert!(msg.contains("chunk 4 of 'a.md'"), "got: {msg}");
        assert!(
            msg.contains("chunk size"),
            "the message must say what to change: {msg}"
        );
    }

    #[test]
    fn a_selected_point_with_no_local_chunk_stops_the_sync() {
        assert!(check_selection_complete("a.md", 4, 4).is_ok());

        // Skipping it instead reports success, leaves the file's hash matching
        // from then on, and never writes or deletes the point again.
        let err = check_selection_complete("notes/a.md", 3, 4)
            .expect_err("a selected point that cannot be built is lost silently");

        let msg = err.to_string();
        assert!(msg.contains("notes/a.md"), "got: {msg}");
        assert!(msg.contains("selected 4 points"), "got: {msg}");
        assert!(msg.contains("only 3"), "got: {msg}");
    }

    #[test]
    fn the_scroll_asks_for_every_field_a_guard_reads() {
        let body = scroll_request(None);

        let include: Vec<&str> = body["with_payload"]["include"]
            .as_array()
            .expect("the include list is an array")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        // Dropping any of these disables a guard silently — it stops firing
        // rather than failing — which is what this test exists to prevent.
        for field in [
            "file_id",
            "file_hash",
            "coyote_rag_id",
            "writer_id",
            "embedding_model",
            "path",
        ] {
            assert!(include.contains(&field), "{field} missing from {include:?}");
        }
        assert_eq!(include.len(), 6);
        assert_eq!(body["with_vector"], serde_json::json!(false));
        assert!(
            body.get("offset").is_none(),
            "the first page is requested without a cursor"
        );

        let next = scroll_request(Some(&serde_json::json!(4096)));
        assert_eq!(next["offset"], serde_json::json!(4096));
    }

    #[test]
    fn a_scrolled_payload_maps_absent_fields_onto_the_empty_string() {
        let full = remote_point(&serde_json::json!({
            "file_id": 2,
            "file_hash": "h",
            "coyote_rag_id": RAG_ID,
            "writer_id": WRITER_ID,
            "embedding_model": MODEL,
            "path": "a.md",
        }));
        assert_eq!(full.file_id, Some(2));
        assert_eq!(full.path, "a.md");
        assert_eq!(full.coyote_rag_id, RAG_ID);

        // `file_id` is the one field with no usable empty value, so its absence
        // stays distinguishable and the reconcile can refuse on it.
        let bare = remote_point(&serde_json::json!({}));
        assert_eq!(bare.file_id, None);
        assert_eq!(bare, RemotePoint::default());
    }

    #[test]
    fn both_write_requests_pin_their_verb_and_wait_for_the_write_to_apply() {
        let (upsert_method, upsert) = upsert_request("http://localhost:6333", "docs");
        let (delete_method, delete) = delete_request("http://localhost:6333", "docs");

        // `POST` on the upsert path is `fetch_content`'s get-by-id route, and
        // `DELETE` on the delete path is a routing-level 404 with an empty body.
        // Both mistakes share their path with the right verb, so only the verb
        // itself separates them.
        assert_eq!(upsert_method, Method::PUT);
        assert_eq!(delete_method, Method::POST);
        // Sent in the body instead, `wait` is silently ignored and the server
        // answers before the write is durable.
        assert_eq!(
            upsert,
            "http://localhost:6333/collections/docs/points?wait=true"
        );
        assert_eq!(
            delete,
            "http://localhost:6333/collections/docs/points/delete?wait=true"
        );
    }

    #[test]
    fn an_acknowledged_write_is_not_a_successful_one() {
        let completed = serde_json::json!({"result": {"operation_id": 1, "status": "completed"}});
        assert!(check_write_completed(&completed, "upsert").is_ok());

        let acknowledged = serde_json::json!({"result": {"status": "acknowledged"}});
        let err = check_write_completed(&acknowledged, "upsert")
            .expect_err("acknowledged means queued, not durable");
        assert!(err.to_string().contains("acknowledged"), "got: {err}");

        let junk = serde_json::json!({"result": {}});
        assert!(check_write_completed(&junk, "delete").is_err());
    }

    #[test]
    fn deletes_are_chunked_at_a_thousand_ids() {
        let ids: Vec<u64> = (0..2500).collect();

        let batches: Vec<&[u64]> = delete_id_batches(&ids).collect();

        assert_eq!(
            batches.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            vec![1000, 1000, 500]
        );
        assert_eq!(batches.concat(), ids, "no id may be dropped or reordered");
        assert_eq!(
            delete_id_batches(&[]).count(),
            0,
            "an empty delete list sends nothing"
        );
    }

    #[test]
    fn check_collection_config_rejects_what_the_read_and_write_paths_cannot_share() {
        let ok = serde_json::json!({
            "result": {"config": {"params": {"vectors": {"size": 4, "distance": "Cosine"}}}}
        });
        assert!(check_collection_config(&ok, "docs", "http://h", Some(4)).is_ok());
        // No local vector to measure against: the check is skipped, never guessed
        // from the model name.
        assert!(check_collection_config(&ok, "docs", "http://h", None).is_ok());

        let err = check_collection_config(&ok, "docs", "http://h", Some(1536))
            .expect_err("a dimension mismatch 400s on every batch");
        assert!(err.to_string().contains("1536-dimensional"), "got: {err}");

        let named = serde_json::json!({
            "result": {"config": {"params": {"vectors": {"t": {"size": 4, "distance": "Cosine"}}}}}
        });
        let err = check_collection_config(&named, "docs", "http://h", Some(4))
            .expect_err("a named-vector collection rejects every unnamed query");
        assert!(err.to_string().contains("named vectors"), "got: {err}");

        let euclid = serde_json::json!({
            "result": {"config": {"params": {"vectors": {"size": 4, "distance": "Euclid"}}}}
        });
        let err = check_collection_config(&euclid, "docs", "http://h", Some(4))
            .expect_err("only Cosine scores on the scale the read path filters against");
        assert!(err.to_string().contains("Euclid"), "got: {err}");
    }

    #[test]
    fn plan_owned_collection_creates_adopts_or_refuses() {
        let empty = serde_json::json!({
            "result": {
                "points_count": 0,
                "config": {"params": {"vectors": {"size": 4, "distance": "Cosine"}}}
            }
        });

        assert_eq!(
            plan_owned_collection(None, "docs", "h:6333", 4, "m").unwrap(),
            CollectionAction::Create,
            "an absent collection is the ordinary case and is created"
        );
        assert_eq!(
            plan_owned_collection(Some(&empty), "docs", "h:6333", 4, "m").unwrap(),
            CollectionAction::Adopt,
            "reclaiming an empty compatible collection is what makes a create-then-crash \
             retryable"
        );

        let populated = serde_json::json!({
            "result": {
                "points_count": 12,
                "config": {"params": {"vectors": {"size": 4, "distance": "Cosine"}}}
            }
        });
        let err = plan_owned_collection(Some(&populated), "docs", "h:6333", 4, "m")
            .expect_err("the first sync would delete points this RAG never wrote");
        assert!(err.to_string().contains("12 point(s)"), "got: {err}");

        let err = plan_owned_collection(Some(&empty), "docs", "h:6333", 1536, "embed-3-small")
            .expect_err("a dimension clash must fail before the corpus is embedded");
        let report = format!("{err:#}");
        assert!(report.contains("embed-3-small"), "got: {report}");
        assert!(report.contains("1536-dimensional"), "got: {report}");

        // Absent, not zero. Reading "no answer" as "empty" would adopt a
        // collection whose contents the next sync then deletes.
        let unreadable = serde_json::json!({
            "result": {"config": {"params": {"vectors": {"size": 4, "distance": "Cosine"}}}}
        });
        plan_owned_collection(Some(&unreadable), "docs", "h:6333", 4, "m")
            .expect_err("an unreadable point count is not an empty collection");
    }

    /// The write path end to end, over a fixture that answers every endpoint a
    /// rebuild touches, pinning the two orderings the sync depends on.
    ///
    /// Neither is visible from any one function, so swapping them still passes
    /// every other test in this file; the damage only shows up later as
    /// documents that quietly stop being findable.
    #[tokio::test]
    async fn a_rebuild_scans_before_it_writes_and_upserts_before_it_deletes() {
        const COMPLETED: &str = r#"{"result":{"status":"completed"}}"#;

        let point = |file_id: FileId, path: &str, hash: &str| {
            serde_json::json!({
                "id": wire_point_id(DocumentId::new(file_id, 0)),
                "payload": {
                    "file_id": file_id as u64,
                    "file_hash": hash,
                    "path": path,
                    "coyote_rag_id": RAG_ID,
                    // Left empty deliberately: a writer id from another install
                    // only adds a warning, and a warning says nothing about
                    // ordering.
                    "writer_id": "",
                    "embedding_model": MODEL,
                },
            })
        };
        let scroll = serde_json::json!({
            "result": {
                // The two files the corpus still holds, plus the point left
                // behind by one it no longer does. Without something to retire
                // the plan produces no delete at all and the second ordering
                // below could not be observed.
                "points": [
                    point(0, "/tmp/a.md", "h0"),
                    point(1, "/tmp/b.md", "h1"),
                    point(7, "/tmp/retired.md", "h7"),
                ],
                "next_page_offset": null,
            }
        })
        .to_string();

        // Most specific first: scroll, upsert and delete all live under
        // `/points`, and the exists probe shares `/collections/` with the plain
        // collection read the preflight makes.
        let (host, requests) = scripted_server(vec![
            ("/points/scroll", scroll),
            ("/points/delete", COMPLETED.to_string()),
            ("/points?wait=true", COMPLETED.to_string()),
            ("/index?wait=true", COMPLETED.to_string()),
            ("/exists", r#"{"result":{"exists":true}}"#.to_string()),
            (
                "GET /collections/",
                r#"{"result":{"config":{"params":{"vectors":{"size":2,"distance":"Cosine"}}}}}"#
                    .to_string(),
            ),
        ]);

        let mut data = owned_rag_data();
        data.driver_config
            .insert("coyote_rag_id".to_string(), RAG_ID.to_string());
        let mut provider = QdrantProvider {
            client: Client::new(),
            base_url: host,
            collection: "c".to_string(),
            point_ids: Arc::default(),
            local_content: HashMap::new(),
            attached: false,
        };

        provider.rebuild_indexes(&data, true).await.unwrap();

        let seen = requests.read().clone();
        let position = |needle: &str| seen.iter().position(|line| line.contains(needle));
        // Every one of these is an `expect`, so a refactor that stops writing
        // cannot turn this into a test that passes over an empty request log.
        let scroll = position("/points/scroll")
            .expect("the plan is built from a scan, so the scan has to happen");
        let upsert = position("/points?wait=true")
            .expect("a full rebuild of a corpus with vectors has to write points");
        let delete = position("/points/delete")
            .expect("the point left behind by a retired file has to be deleted");

        assert!(
            scroll < upsert.min(delete),
            "the scan has to finish before anything is written, because Qdrant's scroll is a \
             cursor and not a snapshot: a write interleaved with it can move a point past the \
             cursor and out of the scan entirely. Requests were: {seen:?}"
        );
        assert!(
            upsert < delete,
            "upsert-first leaves stale duplicates, delete-first leaves documents missing, and \
             for a retrieval system stale-but-present beats silently absent. Requests were: \
             {seen:?}"
        );
    }

    /// An incremental sync has no mandate to recreate a collection, so when it
    /// finds one gone it has to name the command that does. A bare error here
    /// reads as a broken RAG rather than as a one-command recovery that costs no
    /// re-embedding.
    #[tokio::test]
    async fn an_incremental_sync_against_a_missing_collection_points_at_rebuild_rag() {
        // The exists probe is the only request made before the bail, so a single
        // canned answer covers the whole run.
        let (host, _) = canned_server("200 OK", r#"{"result":{"exists":false}}"#);

        let mut data = owned_rag_data();
        data.driver_config
            .insert("coyote_rag_id".to_string(), RAG_ID.to_string());
        let mut provider = QdrantProvider {
            client: Client::new(),
            base_url: host,
            collection: "c".to_string(),
            point_ids: Arc::default(),
            local_content: HashMap::new(),
            attached: false,
        };

        let err = provider.rebuild_indexes(&data, false).await.expect_err(
            "an incremental sync must not report success against a collection that is not there",
        );

        let report = format!("{err:#}");
        assert!(report.contains(".rebuild rag"), "got: {report}");
        assert!(
            report.contains("'c'"),
            "the message has to name the collection that went missing: {report}"
        );
    }

    /// A 404 from search means two different things either side of ownership: a
    /// collection Coyote wrote is recoverable from the local copy at no
    /// embedding cost, and one it merely attached to is not Coyote's to
    /// recreate — offering to would be a promise it cannot keep.
    #[tokio::test]
    async fn a_search_404_offers_rebuild_rag_only_for_a_collection_coyote_owns() {
        let (host, _) = canned_server(
            "404 Not Found",
            r#"{"status":{"error":"Collection 'c' doesn't exist!"}}"#,
        );
        let provider = |attached: bool| QdrantProvider {
            client: Client::new(),
            base_url: host.clone(),
            collection: "c".to_string(),
            point_ids: Arc::default(),
            local_content: HashMap::new(),
            attached,
        };

        let owned = provider(false);
        let err = owned
            .vector_search(&[0.0, 1.0], 5, 0.0)
            .await
            .expect_err("a 404 is not an empty result set");
        assert!(format!("{err:#}").contains(".rebuild rag"), "got: {err:#}");

        let attached = provider(true);
        let err = attached
            .vector_search(&[0.0, 1.0], 5, 0.0)
            .await
            .expect_err("a 404 is not an empty result set");
        assert!(
            !format!("{err:#}").contains(".rebuild rag"),
            "Coyote must not offer to rebuild a collection somebody else owns: {err:#}"
        );
    }

    #[tokio::test]
    #[ignore = "requires a running Qdrant instance at localhost:6333"]
    async fn ensure_owned_collection_creates_then_adopts_requires_running_instance() {
        const HOST: &str = "http://localhost:6333";
        const COLLECTION: &str = "coyote-ensure-owned-collection";

        let created = QdrantProvider::ensure_owned_collection(HOST, COLLECTION, None, 4, "m")
            .await
            .unwrap();
        assert_eq!(created, CollectionAction::Create);

        // The create-then-crash retry: the same wizard run again finds its own
        // empty collection and reclaims it instead of refusing.
        let adopted = QdrantProvider::ensure_owned_collection(HOST, COLLECTION, None, 4, "m")
            .await
            .unwrap();
        assert_eq!(adopted, CollectionAction::Adopt);

        QdrantProvider::ensure_owned_collection(HOST, COLLECTION, None, 8, "m")
            .await
            .expect_err("a collection built for another width must not be adopted");

        Client::new()
            .delete(format!("{HOST}/collections/{COLLECTION}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a running Qdrant instance at localhost:6333"]
    async fn qdrant_owned_round_trip_requires_running_instance() {
        const HOST: &str = "http://localhost:6333";
        const COLLECTION: &str = "coyote-owned-round-trip";

        let client = QdrantProvider::make_client(HOST, None).unwrap();
        if !QdrantProvider::collection_exists(&client, HOST, COLLECTION)
            .await
            .unwrap()
        {
            QdrantProvider::create_collection(&client, HOST, COLLECTION, 4)
                .await
                .unwrap();
        }
        let provider = QdrantProvider::new_owned(HOST, COLLECTION, None)
            .await
            .unwrap();
        let ctx = base_ctx();
        let ids: Vec<u64> = (0..2)
            .map(|chunk| wire_point_id(DocumentId::new(1, chunk)))
            .collect();
        let points: Vec<Value> = ids
            .iter()
            .enumerate()
            .map(|(chunk, id)| {
                let payload = point_payload(1, chunk, "hash", "round-trip.md", "text", &ctx);
                upsert_point(*id, &[0.5f32; 4], payload)
            })
            .collect();

        provider.upsert_points(points).await.unwrap();

        let remote = provider.scroll_all().await.unwrap();
        for id in &ids {
            let point = remote
                .get(id)
                .unwrap_or_else(|| panic!("point {id} must scroll back after a completed upsert"));
            assert_eq!(point.file_id, Some(1));
            assert_eq!(point.coyote_rag_id, RAG_ID);
            assert_eq!(point.path, "round-trip.md");
        }

        provider.delete_points(&ids).await.unwrap();

        let remote = provider.scroll_all().await.unwrap();
        assert!(
            ids.iter().all(|id| !remote.contains_key(id)),
            "the delete pass must remove exactly the ids it was given"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn qdrant_list_collections_requires_running_instance() {
        let collections = QdrantProvider::list_collections("http://localhost:6333", None)
            .await
            .unwrap();

        assert!(!collections.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn qdrant_vector_search_returns_results() {
        // Read-only against a collection this test does not own, so it is built
        // the way an attached RAG is: the collection must already be there, and
        // its absence is a setup error rather than something to recreate.
        let provider =
            QdrantProvider::new_attached("http://localhost:6333", "test-collection", None)
                .await
                .unwrap();
        let embedding = vec![0.0f32; 1536];

        let results = provider.vector_search(&embedding, 5, 0.0).await.unwrap();

        assert!(results.len() <= 5);
    }

    /// The failure `new_owned` exists for, end to end: the collection is deleted
    /// out from under a live RAG, and a cold start has to rebuild it from the
    /// local copy alone.
    ///
    /// Construction against the absent collection is the step that used to fail,
    /// which left `.rebuild rag` unreachable and a paid re-embed as the only way
    /// back. Comparing the whole scrolled map rather than a count is what proves
    /// the recovery is a real repopulation and not merely a collection of the
    /// right size.
    #[tokio::test]
    #[ignore = "requires a running Qdrant instance at localhost:6333"]
    async fn rebuild_recovers_a_deleted_collection_requires_running_instance() {
        const HOST: &str = "http://localhost:6333";
        const COLLECTION: &str = "coyote-cold-start-recovery";

        let mut data = owned_rag_data();
        data.driver_config
            .insert("coyote_rag_id".to_string(), RAG_ID.to_string());

        let mut provider = QdrantProvider::new_owned(HOST, COLLECTION, None)
            .await
            .unwrap();
        provider.rebuild_indexes(&data, true).await.unwrap();
        let before = provider.scroll_all().await.unwrap();
        assert!(
            !before.is_empty(),
            "the baseline must actually hold points, or the comparison below is vacuous"
        );

        Client::new()
            .delete(format!("{HOST}/collections/{COLLECTION}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        let mut recovered = QdrantProvider::new_owned(HOST, COLLECTION, None)
            .await
            .expect("an absent collection must not stop a provider being built for it");
        recovered.rebuild_indexes(&data, true).await.unwrap();

        let after = recovered.scroll_all().await.unwrap();
        assert_eq!(
            after, before,
            "the recreated collection must carry the same ids and the same payloads as the \
             one that was deleted"
        );

        // A third rebuild, over a corpus that has changed, against a collection
        // that is now there. This is the arm that retires one file and admits
        // another in a single pass, and the only one that re-asserts the payload
        // index on a collection Coyote found rather than made.
        let survivor = wire_point_id(DocumentId::new(0, 0));
        let arrival = wire_point_id(DocumentId::new(2, 0));
        let survivor_payload = after
            .get(&survivor)
            .expect("the survivor has to be there before it can be shown to have stayed")
            .clone();

        data.files.swap_remove(&1);
        data.vectors.swap_remove(&DocumentId::new(1, 0));
        data.files.insert(
            2,
            RagFile {
                hash: "h2".to_string(),
                path: "/tmp/c.md".to_string(),
                documents: vec![RagDocument {
                    page_content: "gamma".to_string(),
                    metadata: Default::default(),
                }],
            },
        );
        data.vectors.insert(DocumentId::new(2, 0), vec![0.0, 1.0]);

        recovered.rebuild_indexes(&data, true).await.unwrap();
        let changed = recovered.scroll_all().await.unwrap();

        assert_eq!(
            changed.keys().copied().collect::<BTreeSet<u64>>(),
            BTreeSet::from([survivor, arrival]),
            "a changed corpus must leave exactly the points it now describes: the retired \
             file's point gone, the new file's written, and nothing else"
        );
        assert_eq!(
            changed.get(&survivor),
            Some(&survivor_payload),
            "a file nobody touched must come through the pass unchanged — a delete-then-insert \
             would leave a window in which a query cannot find it"
        );

        Client::new()
            .delete(format!("{HOST}/collections/{COLLECTION}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
}
