pub mod agent;
pub mod dispatch;
pub mod executor;
pub mod llm;
pub mod logging;
pub mod map;
pub mod parser;
pub mod rag;
pub mod reducer;
pub mod script;
pub mod staging;
pub mod state;
pub mod state_updates;
pub mod structured;
pub mod types;
pub mod user_interaction;
pub mod validator;

use anyhow::Error;
pub use dispatch::{
    active_agent_graph_name, run_active_agent_graph, run_active_agent_graph_with_inputs,
};
pub use executor::GraphExecutor;
pub use parser::{GraphParser, agent_has_graph};
use serde_json::Value;
use std::time::Duration;
pub use types::{Graph, NodeType};

pub const GRAPH_SCHEMA_VERSION: &str = "1.0";

pub const DEFAULT_MAX_LOOP_ITERATIONS: usize = 100;

/// A timeout of `0` disables the wall-clock bound.
pub(crate) fn wall_clock(secs: u64) -> Option<Duration> {
    (secs != 0).then(|| Duration::from_secs(secs))
}

/// An HTTP status in the chain is authoritative — message text never
/// overrides it. String anchors are only consulted for untyped errors.
pub(crate) fn is_transient_error(err: &Error) -> bool {
    if err.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.is_connect() || e.is_timeout())
            || c.is::<tokio::time::error::Elapsed>()
    }) {
        return true;
    }
    if let Some(api) = err
        .chain()
        .find_map(|c| c.downcast_ref::<crate::client::ApiStatusError>())
    {
        return is_transient_status(api.status);
    }
    let s = format!("{err:#}").to_lowercase();
    s.contains("timed out")
        || s.contains("rate limit")
        || s.contains("http 429")
        || s.contains("status 429")
        || s.contains("429 too many")
        || s.contains("connection reset")
        || s.contains("connection refused")
        || s.contains("produced no output")
        || s.contains("broken pipe")
        || s.contains("connection error")
        || s.contains("error sending request")
}

/// Transient HTTP statuses worth a from-scratch retry: 429 (rate limit),
/// 500/502/503/504 (upstream hiccups), 529 (Anthropic overloaded).
/// Deterministic statuses — 4xx other than 429 (including 408) and
/// 501/505-class capability errors — are excluded.
fn is_transient_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504 | 529)
}

pub const MAX_STATE_SIZE_BYTES: usize = 32 * 1024;

pub(in crate::graph) fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn wall_clock_zero_is_no_bound() {
        assert!(wall_clock(0).is_none());
    }

    #[test]
    fn wall_clock_nonzero_is_that_many_seconds() {
        assert_eq!(wall_clock(7), Some(Duration::from_secs(7)));
    }

    #[test]
    fn is_transient_error_matches_expected_signatures() {
        assert!(is_transient_error(&anyhow!("request timed out after 30s")));
        assert!(is_transient_error(&anyhow!("rate limit reached")));
        assert!(is_transient_error(&anyhow!("HTTP 429")));
        assert!(is_transient_error(&anyhow!("status 429")));
        assert!(is_transient_error(&anyhow!("429 Too Many Requests")));
        assert!(is_transient_error(&anyhow!("Connection reset by peer")));
        assert!(is_transient_error(&anyhow!("Connection refused")));
        assert!(is_transient_error(&anyhow!("llm produced no output")));
        assert!(is_transient_error(&anyhow!(
            "stream closed because of a broken pipe"
        )));
        assert!(is_transient_error(&anyhow!(
            "connection error: unexpected end of stream"
        )));
        assert!(is_transient_error(&anyhow!(
            "error sending request for url"
        )));
    }

    #[test]
    fn is_transient_error_sees_through_context_chains() {
        let err = anyhow!("stream closed because of a broken pipe")
            .context("Failed to call chat-completions api")
            .context("Agent 'domain-reviewer' failed");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn is_transient_error_rejects_non_transient_errors() {
        assert!(!is_transient_error(&anyhow!("Unknown model 'foo'")));
        assert!(!is_transient_error(&anyhow!(
            "llm node references unknown tool 'bad'"
        )));
        assert!(!is_transient_error(&anyhow!("hit max_iterations")));
        assert!(!is_transient_error(&anyhow!("authentication failed")));
        assert!(!is_transient_error(&anyhow!(
            "script exited 1: see line 429 in a comment"
        )));
    }

    #[test]
    fn is_transient_error_sees_typed_api_status_429_through_context_chain() {
        let data = serde_json::json!({
            "error": {
                "type": "rate_limit_error",
                "message": "Too many requests, please slow down"
            }
        });
        let err = crate::client::catch_error(&data, 429)
            .unwrap_err()
            .context("Failed to call chat-completions api")
            .context("Agent 'domain-reviewer' failed");

        let rendered = format!("{err:#}");
        assert!(
            !is_transient_error(&anyhow!("{rendered}")),
            "the rendered text alone must not be transient — proves the typed arm is what catches it"
        );
        assert!(is_transient_error(&err));
    }

    #[test]
    fn is_transient_error_typed_status_overrides_message_text() {
        let err = crate::client::catch_error(
            &serde_json::json!({ "message": "rate limit reached — connection reset" }),
            400,
        )
        .unwrap_err();
        assert!(
            !is_transient_error(&err),
            "typed 400 must not be retried even when the body matches string anchors"
        );

        let err = crate::client::catch_error(&serde_json::json!({ "message": "x" }), 503)
            .unwrap_err()
            .context("Agent 'x' failed");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn is_transient_error_typed_status_table() {
        let data = serde_json::json!({ "message": "x" });
        for status in [429, 500, 502, 503, 504, 529] {
            let err = crate::client::catch_error(&data, status).unwrap_err();
            assert!(
                is_transient_error(&err),
                "status {status} should be transient"
            );
        }
        for status in [400, 401, 403, 404, 408, 422, 501, 505] {
            let err = crate::client::catch_error(&data, status).unwrap_err();
            assert!(
                !is_transient_error(&err),
                "status {status} should not be transient"
            );
        }
    }

    #[test]
    fn is_transient_error_string_anchors_are_case_insensitive() {
        assert!(is_transient_error(&anyhow!("Rate Limit reached")));
        assert!(is_transient_error(&anyhow!("Request Timed Out")));
        assert!(is_transient_error(&anyhow!("connection reset by peer")));
    }

    #[tokio::test(start_paused = true)]
    async fn is_transient_error_recognizes_typed_elapsed_without_matching_text() {
        let elapsed = tokio::time::timeout(Duration::from_secs(1), std::future::pending::<()>())
            .await
            .expect_err("pending future must time out");
        let err = Error::new(elapsed).context("calling provider");

        // "deadline has elapsed" contains no recognized substring, so only
        // the typed pre-check can classify this as transient.
        let rendered = format!("{err:#}");
        assert!(!rendered.contains("timed out"), "{rendered}");
        assert!(is_transient_error(&err));
    }

    #[tokio::test]
    async fn is_transient_error_recognizes_typed_reqwest_connect_error() {
        // `no_proxy` keeps environments with HTTP(S)_PROXY set from turning
        // the refused connection into a proxied response.
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let err = client
            .get("http://127.0.0.1:1")
            .send()
            .await
            .expect_err("closed local port must refuse the connection");

        assert!(err.is_connect());
        assert!(is_transient_error(&Error::new(err)));
    }

    /// A `read_timeout` stall on the SSE path keeps its typed reqwest error,
    /// so the classifier's typed branch fires rather than depending on the
    /// rendered text. The server sends headers then never writes a body.
    #[tokio::test]
    async fn sse_read_timeout_stall_is_transient() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .read_timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        let err = crate::client::sse_stream(client.get(format!("http://{addr}/")), |_| Ok(false))
            .await
            .expect_err("a body that never arrives must trip the read timeout");
        server.abort();

        assert!(
            err.chain().any(|c| {
                c.downcast_ref::<reqwest::Error>()
                    .is_some_and(|e| e.is_timeout())
            }),
            "typed reqwest timeout missing from chain: {err:#}"
        );
        assert!(is_transient_error(&err));
    }

    /// `read_timeout` resets on every read: a stream whose events keep
    /// arriving well under the limit completes even though the whole body
    /// takes several times longer than the limit.
    #[tokio::test]
    async fn sse_slow_but_continuous_stream_outlives_read_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const EVENTS: usize = 15;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                .await
                .unwrap();
            for i in 0..EVENTS {
                tokio::time::sleep(Duration::from_millis(20)).await;
                sock.write_all(format!("data: chunk{i}\n\n").as_bytes())
                    .await
                    .unwrap();
            }
            sock.write_all(b"data: [DONE]\n\n").await.unwrap();
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .read_timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let mut received = 0usize;
        crate::client::sse_stream(client.get(format!("http://{addr}/")), |message| {
            if message.data == "[DONE]" {
                return Ok(true);
            }
            received += 1;
            Ok(false)
        })
        .await
        .expect("events every 20ms must never trip a 100ms read timeout");
        server.abort();

        assert_eq!(received, EVENTS);
    }
}
