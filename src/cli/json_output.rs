use crate::config::RunStatsSnapshot;
use crate::config::{RequestContext, RunStats};
use anyhow::Result;
use fancy_regex::Regex;
use log::error;
use serde::Serialize;
use std::sync::LazyLock;
use std::time::Duration;

static THINK_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<think>.*?</think>\s*").unwrap());

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
    /// `outcome` is the final assistant text, or the rendered error. Every
    /// `<think>` block is dropped from the text, wherever it appears, and
    /// surrounding whitespace is trimmed.
    pub fn new(
        outcome: Result<String, String>,
        model: String,
        session_id: Option<String>,
        stats: RunStatsSnapshot,
        elapsed: Duration,
    ) -> Self {
        let (result, error) = match outcome {
            Ok(text) => (
                Some(THINK_BLOCK_RE.replace_all(&text, "").trim().to_string()),
                None,
            ),
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

/// What a one-shot directive produced. The session and model are read
/// before the directive exits its session, which clears them.
pub struct DirectiveOutcome {
    pub output: String,
    pub model: String,
    pub session_id: Option<String>,
}

impl DirectiveOutcome {
    pub fn new(ctx: &RequestContext, output: String) -> Self {
        Self {
            output,
            model: ctx.current_model().id(),
            session_id: ctx.session.as_ref().map(|s| s.name().to_string()),
        }
    }
}

/// Prints the `--json` report for a finished one-shot run. On an error the
/// report is the only stdout output; the returned error still goes to
/// stderr through `render_error`.
pub fn emit(
    result: &Result<DirectiveOutcome>,
    fallback_model: String,
    fallback_session: Option<String>,
    stats: &RunStats,
    elapsed: Duration,
) {
    let report = report(
        result,
        fallback_model,
        fallback_session,
        stats.snapshot(),
        elapsed,
    );
    match serde_json::to_string(&report) {
        Ok(line) => println!("{line}"),
        Err(err) => error!("Failed to serialize the --json report: {err}"),
    }
}

/// A failed directive has no outcome, so its report falls back to the model
/// and session read before the run started.
fn report(
    result: &Result<DirectiveOutcome>,
    fallback_model: String,
    fallback_session: Option<String>,
    stats: RunStatsSnapshot,
    elapsed: Duration,
) -> JsonResult {
    let (outcome, model, session_id) = match result {
        Ok(outcome) => (
            Ok(outcome.output.clone()),
            outcome.model.clone(),
            outcome.session_id.clone(),
        ),
        Err(err) => (Err(format!("{err:#}")), fallback_model, fallback_session),
    };
    JsonResult::new(outcome, model, session_id, stats, elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

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
    fn a_leading_think_block_is_stripped_from_the_result() {
        let report = JsonResult::new(
            Ok("<think>\nweighing it up\n</think>\n\nfinal".into()),
            "stub:m".into(),
            None,
            RunStatsSnapshot::default(),
            Duration::ZERO,
        );

        assert_eq!(report.result.as_deref(), Some("final"));
    }

    fn result_of(text: &str) -> Option<String> {
        JsonResult::new(
            Ok(text.into()),
            "stub:m".into(),
            None,
            RunStatsSnapshot::default(),
            Duration::ZERO,
        )
        .result
    }

    #[test]
    fn a_mid_text_think_block_is_stripped_from_the_result() {
        assert_eq!(
            result_of("Sure. <think>x</think> done").as_deref(),
            Some("Sure. done")
        );
    }

    #[test]
    fn every_think_block_is_stripped_from_the_result() {
        assert_eq!(
            result_of("<think>a</think>\none <think>\nb\n</think> two").as_deref(),
            Some("one two")
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

    #[test]
    fn report_takes_model_and_session_from_the_outcome_or_the_fallback() {
        let succeeded: Result<DirectiveOutcome> = Ok(DirectiveOutcome {
            output: "done".into(),
            model: "outcome:m".into(),
            session_id: Some("outcome-session".into()),
        });
        let failed: Result<DirectiveOutcome> = Err(anyhow!("inner cause").context("outer failure"));

        let ok = report(
            &succeeded,
            "fallback:m".into(),
            Some("fallback-session".into()),
            RunStatsSnapshot::default(),
            Duration::ZERO,
        );
        let err = report(
            &failed,
            "fallback:m".into(),
            Some("fallback-session".into()),
            RunStatsSnapshot::default(),
            Duration::ZERO,
        );

        assert_eq!(ok.result.as_deref(), Some("done"));
        assert_eq!(ok.error, None);
        assert_eq!(ok.model, "outcome:m");
        assert_eq!(ok.session_id.as_deref(), Some("outcome-session"));
        assert_eq!(err.result, None);
        assert_eq!(err.error.as_deref(), Some("outer failure: inner cause"));
        assert_eq!(err.model, "fallback:m");
        assert_eq!(err.session_id.as_deref(), Some("fallback-session"));
    }
}
