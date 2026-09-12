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

/// Whether an error looks like a transient transport/provider failure worth
/// retrying. Typed transport errors anywhere in the chain (reqwest
/// connect/timeout failures and tokio's `Elapsed`) are recognized first;
/// providers that surface failures only as rendered text fall back to
/// substring matching over the whole context chain.
pub(crate) fn is_transient_error(err: &anyhow::Error) -> bool {
    if err.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.is_connect() || e.is_timeout())
            || c.is::<tokio::time::error::Elapsed>()
    }) {
        return true;
    }
    let s = format!("{err:#}");
    s.contains("timed out")
        || s.contains("rate limit")
        || s.contains("HTTP 429")
        || s.contains("status 429")
        || s.contains("429 Too Many")
        || s.contains("Connection reset")
        || s.contains("Connection refused")
        || s.contains("produced no output")
        || s.contains("broken pipe")
        || s.contains("connection error")
        || s.contains("error sending request")
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

    #[tokio::test(start_paused = true)]
    async fn is_transient_error_recognizes_typed_elapsed_without_matching_text() {
        let elapsed = tokio::time::timeout(Duration::from_secs(1), std::future::pending::<()>())
            .await
            .expect_err("pending future must time out");
        let err = anyhow::Error::new(elapsed).context("calling provider");

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
        assert!(is_transient_error(&anyhow::Error::new(err)));
    }
}
