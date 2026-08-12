use crate::rag::provider::RagProvider;
use crate::rag::{DocumentId, RagData};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use parking_lot::RwLock;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;
use std::collections::HashMap;
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

/// Query-only client for an external Qdrant collection.
///
/// Attach-only: this provider never writes to the remote collection. Coyote does
/// not own the data, and `rebuild_indexes` refuses rather than pretending to.
pub struct QdrantProvider {
    client: Client,
    base_url: String,
    collection: String,
    point_ids: Arc<RwLock<PointIdInterner>>,
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

    pub async fn new(host: &str, collection: &str, api_key: Option<&str>) -> Result<Self> {
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

        Ok(Self {
            client,
            base_url,
            collection: collection.to_string(),
            point_ids: Arc::default(),
        })
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
        if !resp.status().is_success() {
            bail!(
                "Qdrant search on '{}' failed: {}",
                self.collection,
                Self::error_message(resp).await
            );
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
        let url = format!("{}/collections/{}/points", self.base_url, self.collection);
        // Qdrant is asked for the ids it issued, never for a synthetic handle.
        let id_list = self.point_ids.read().outbound_ids(ids);
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
        let mut rows = {
            let mut interner = self.point_ids.write();
            parse_points(&mut interner, &data)?
        };
        // `/points` does not guarantee response order matches request order, and the
        // caller's RRF ranking is carried by that order. Restore it.
        let position: HashMap<DocumentId, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        rows.sort_by_key(|(id, _)| position.get(id).copied().unwrap_or(usize::MAX));

        Ok(rows)
    }

    async fn rebuild_indexes(&mut self, data: &RagData, _full_rebuild: bool) -> Result<()> {
        // Both arms refuse. A silent `Ok(())` would make `.rebuild rag` and
        // `.edit rag-docs` look like they worked while writing nothing to the
        // remote, leaving the user believing the collection was updated.
        if data.attached {
            bail!(
                "This RAG is attached to an external Qdrant collection. Coyote does not own \
                 its documents and cannot rebuild it. Manage the collection directly, or \
                 create a Coyote-owned RAG with `.rag <name>`."
            );
        }
        bail!("Writing to Qdrant is not supported yet (attach-only).");
    }

    fn duplicate(&self, _data: &RagData) -> Box<dyn RagProvider> {
        // Cloning the client shares the connection pool and the injected api-key
        // header. Sharing is correct: both handles address the same remote
        // collection, and neither of them writes to it.
        //
        // The point-id map is shared for the same reason, and because it MUST be:
        // `Rag::clone()` hands the clone `DocumentId`s that the original minted,
        // so a fresh map would resolve them to nothing and `fetch_content` would
        // ask Qdrant for a synthetic handle — zero results, no error. Resetting it
        // would also re-mint handles for ids the original still holds.
        Box::new(Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            collection: self.collection.clone(),
            point_ids: Arc::clone(&self.point_ids),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn rebuild_indexes_refuses_for_attached_and_unattached_alike() {
        let mut provider = QdrantProvider {
            client: Client::new(),
            base_url: "http://localhost:6333".to_string(),
            collection: "c".to_string(),
            point_ids: Arc::default(),
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

        // `attached: false` is reserved for the (unimplemented) write path. It must
        // also refuse: silently succeeding would run a full paid embedding pass and
        // then discard every vector.
        let owned = RagData {
            driver: "qdrant".to_string(),
            attached: false,
            ..Default::default()
        };
        let err = provider
            .rebuild_indexes(&owned, true)
            .await
            .expect_err("writing to qdrant is unimplemented and must fail loudly");
        assert!(err.to_string().contains("not supported yet"), "got: {err}");
    }

    #[tokio::test]
    async fn fetch_content_short_circuits_on_an_empty_id_list() {
        let provider = QdrantProvider {
            client: Client::new(),
            base_url: "http://127.0.0.1:1".to_string(),
            collection: "c".to_string(),
            point_ids: Arc::default(),
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
        let provider = QdrantProvider::new("http://localhost:6333", "test-collection", None)
            .await
            .unwrap();
        let embedding = vec![0.0f32; 1536];

        let results = provider.vector_search(&embedding, 5, 0.0).await.unwrap();

        assert!(results.len() <= 5);
    }
}
