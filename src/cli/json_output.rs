use crate::config::RunStatsSnapshot;
use serde::Serialize;
use std::time::Duration;

/// The single object `--json` prints on stdout for a one-shot run. Field
/// order is the output order, and absent values serialise as `null`.
#[derive(Debug, Serialize)]
pub struct JsonResult {
    pub result: Option<String>,
    pub is_error: bool,
    pub error: Option<String>,
    pub model: String,
    pub session_id: Option<String>,
    pub num_turns: u64,
    pub duration_ms: u64,
    pub usage: JsonUsage,
    pub total_cost_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct JsonUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

impl JsonResult {
    /// `outcome` is the final assistant text, or the rendered error.
    pub fn new(
        outcome: Result<String, String>,
        model: String,
        session_id: Option<String>,
        stats: RunStatsSnapshot,
        elapsed: Duration,
    ) -> Self {
        let (result, error) = match outcome {
            Ok(text) => (Some(text), None),
            Err(err) => (None, Some(err)),
        };
        Self {
            is_error: error.is_some(),
            result,
            error,
            model,
            session_id,
            num_turns: stats.model_calls,
            duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            usage: JsonUsage {
                input_tokens: stats.input_tokens,
                output_tokens: stats.output_tokens,
                cache_read_input_tokens: stats.cache_read_input_tokens,
                cache_creation_input_tokens: stats.cache_creation_input_tokens,
            },
            total_cost_usd: stats.total_cost_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_serialises_every_key_in_order_with_nulls() {
        let stats = RunStatsSnapshot {
            model_calls: 2,
            input_tokens: Some(10),
            output_tokens: Some(4),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            total_cost_usd: None,
        };

        let report = JsonResult::new(
            Ok("hello".into()),
            "openai:gpt-4o".into(),
            None,
            stats,
            Duration::from_millis(1234),
        );

        assert_eq!(
            serde_json::to_string(&report).unwrap(),
            r#"{"result":"hello","is_error":false,"error":null,"model":"openai:gpt-4o","session_id":null,"num_turns":2,"duration_ms":1234,"usage":{"input_tokens":10,"output_tokens":4,"cache_read_input_tokens":null,"cache_creation_input_tokens":null},"total_cost_usd":null}"#
        );
    }

    #[test]
    fn failure_carries_the_error_and_no_result() {
        let stats = RunStatsSnapshot {
            model_calls: 1,
            total_cost_usd: Some(0.0),
            ..Default::default()
        };

        let report = JsonResult::new(
            Err("boom".into()),
            "stub:m".into(),
            Some("s".into()),
            stats,
            Duration::ZERO,
        );

        assert_eq!(
            serde_json::to_string(&report).unwrap(),
            r#"{"result":null,"is_error":true,"error":"boom","model":"stub:m","session_id":"s","num_turns":1,"duration_ms":0,"usage":{"input_tokens":null,"output_tokens":null,"cache_read_input_tokens":null,"cache_creation_input_tokens":null},"total_cost_usd":0.0}"#
        );
    }
}
