use crate::client::TokenUsage;
use parking_lot::Mutex;

/// Usage and cost accumulated across every model call of one run, shared by
/// the root context, its parallel branches and its spawned children.
#[derive(Debug, Default)]
pub struct RunStats {
    inner: Mutex<RunStatsInner>,
}

#[derive(Debug, Default)]
struct RunStatsInner {
    model_calls: u64,
    usage: TokenUsage,
    cost_usd: f64,
    cost_unknown: bool,
}

/// The result of one attempted model call, as seen by [`RunStats`].
#[derive(Debug, Clone, Copy)]
pub enum RunCallOutcome<'a> {
    Succeeded {
        usage: Option<&'a TokenUsage>,
        cost: Option<f64>,
    },
    /// The call errored or was aborted. It counts as a call but contributes
    /// no usage, and does not make the run's cost unknown.
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunStatsSnapshot {
    pub model_calls: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    /// `None` when any succeeded call lacked usage or a computable cost.
    pub total_cost_usd: Option<f64>,
}

impl RunStats {
    pub fn record_outcome(&self, outcome: RunCallOutcome) {
        match outcome {
            RunCallOutcome::Succeeded { usage, cost } => self.record(usage, cost),
            RunCallOutcome::Failed => self.record_failed(),
        }
    }

    /// Records one succeeded call.
    pub fn record(&self, usage: Option<&TokenUsage>, cost: Option<f64>) {
        let mut inner = self.inner.lock();
        inner.model_calls = inner.model_calls.saturating_add(1);
        match usage {
            Some(usage) => inner.usage.accumulate(usage),
            None => inner.cost_unknown = true,
        }
        match cost {
            Some(cost) => inner.cost_usd += cost,
            None => inner.cost_unknown = true,
        }
    }

    pub fn record_failed(&self) {
        let mut inner = self.inner.lock();
        inner.model_calls = inner.model_calls.saturating_add(1);
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "no production reader yet"))] // TASK-431: remove once --json reads the snapshot
    pub fn snapshot(&self) -> RunStatsSnapshot {
        let inner = self.inner.lock();
        RunStatsSnapshot {
            model_calls: inner.model_calls,
            input_tokens: inner.usage.input_tokens,
            output_tokens: inner.usage.output_tokens,
            cache_read_input_tokens: inner.usage.cache_read_input_tokens,
            cache_creation_input_tokens: inner.usage.cache_creation_input_tokens,
            total_cost_usd: (!inner.cost_unknown).then_some(inner.cost_usd),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cache_read: Option<u64>) -> TokenUsage {
        TokenUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: None,
        }
    }

    #[test]
    fn empty_run_has_no_calls_usage_or_unknown_cost() {
        let snapshot = RunStats::default().snapshot();
        assert_eq!(
            snapshot,
            RunStatsSnapshot {
                total_cost_usd: Some(0.0),
                ..Default::default()
            }
        );
    }

    #[test]
    fn succeeded_calls_sum_per_field_and_cost() {
        let stats = RunStats::default();
        stats.record(Some(&usage(10, 5, None)), Some(0.25));
        stats.record(Some(&usage(3, 1, Some(2))), Some(0.5));

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.model_calls, 2);
        assert_eq!(snapshot.input_tokens, Some(13));
        assert_eq!(snapshot.output_tokens, Some(6));
        assert_eq!(snapshot.cache_read_input_tokens, Some(2));
        assert_eq!(snapshot.cache_creation_input_tokens, None);
        let cost = snapshot.total_cost_usd.expect("every call had a cost");
        assert!((cost - 0.75).abs() < 1e-12);
    }

    #[test]
    fn a_succeeded_call_without_usage_makes_the_cost_unknown() {
        let stats = RunStats::default();
        stats.record(Some(&usage(10, 5, None)), Some(0.25));
        stats.record(None, None);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.model_calls, 2);
        assert_eq!(snapshot.input_tokens, Some(10));
        assert_eq!(snapshot.total_cost_usd, None);
    }

    #[test]
    fn a_succeeded_call_without_a_cost_makes_the_cost_unknown() {
        let stats = RunStats::default();
        stats.record(Some(&usage(10, 5, None)), Some(0.25));
        stats.record(Some(&usage(3, 1, None)), None);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.model_calls, 2);
        assert_eq!(snapshot.input_tokens, Some(13));
        assert_eq!(snapshot.total_cost_usd, None);
    }

    #[test]
    fn a_failed_call_counts_but_changes_neither_usage_nor_cost() {
        let stats = RunStats::default();
        stats.record_outcome(RunCallOutcome::Succeeded {
            usage: Some(&usage(10, 5, None)),
            cost: Some(0.25),
        });
        let before = stats.snapshot();

        stats.record_outcome(RunCallOutcome::Failed);

        let after = stats.snapshot();
        assert_eq!(after.model_calls, 2);
        assert_eq!(
            after,
            RunStatsSnapshot {
                model_calls: 2,
                ..before
            }
        );
        assert_eq!(after.total_cost_usd, Some(0.25));
    }

    #[test]
    fn a_failed_call_alone_leaves_the_cost_known() {
        let stats = RunStats::default();
        stats.record_outcome(RunCallOutcome::Failed);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.model_calls, 1);
        assert_eq!(snapshot.input_tokens, None);
        assert_eq!(snapshot.total_cost_usd, Some(0.0));
    }
}
