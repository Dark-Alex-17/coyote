use crate::rag::provider::RagProvider;
use crate::rag::{DocumentId, RagData};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;
use std::collections::HashMap;

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
}

impl QdrantProvider {
    fn make_client(api_key: Option<&str>) -> Result<Client> {
        let mut headers = HeaderMap::new();
        if let Some(key) = api_key {
            let mut value =
                HeaderValue::from_str(key).context("api-key header value is not valid ASCII")?;
            value.set_sensitive(true);
            headers.insert("api-key", value);
        }
        Client::builder()
            .default_headers(headers)
            .build()
            .context("Failed to build reqwest client")
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
        let client = Self::make_client(api_key)?;
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
        let client = Self::make_client(api_key)?;
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
        })
    }

    pub async fn list_collections(host: &str, api_key: Option<&str>) -> Result<Vec<String>> {
        let base_url = Self::normalize_base_url(host);
        let client = Self::make_client(api_key)?;
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
        let client = Self::make_client(api_key)?;
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
        // does not pin the distance metric, so filter locally instead.
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
        let results = data["result"]
            .as_array()
            .context("Unexpected /points/search response shape")?
            .iter()
            .filter_map(|pt| {
                // String (UUID) IDs yield None here and are dropped. The attach
                // wizard rejects such collections up front so this cannot silently
                // become "zero results, no error".
                let id = pt["id"].as_u64()? as usize;
                let score = pt["score"].as_f64()? as f32;
                Some((DocumentId(id), score))
            })
            .filter(|(_, score)| *score > min_score)
            .collect();

        Ok(results)
    }

    async fn fetch_content(&self, ids: &[DocumentId]) -> Result<Vec<(DocumentId, String)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let url = format!("{}/collections/{}/points", self.base_url, self.collection);
        let id_list: Vec<u64> = ids.iter().map(|d| d.0 as u64).collect();
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
        let mut rows: Vec<(DocumentId, String)> = data["result"]
            .as_array()
            .context("Unexpected /points response shape")?
            .iter()
            .filter_map(|pt| {
                let id = pt["id"].as_u64()? as usize;
                let text = pt["payload"]["page_content"].as_str()?.to_string();
                Some((DocumentId(id), text))
            })
            .collect();
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
        Box::new(Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            collection: self.collection.clone(),
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
        };

        assert!(provider.fetch_content(&[]).await.unwrap().is_empty());
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
