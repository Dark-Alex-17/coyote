//! Structured logging and per-node timing for graph execution.
//!
//! Two output channels, both owned by [`GraphLogger`]:
//! - **`tracing`** (`info!`/`debug!`/`warn!`/`error!`) — respects
//!   `RUST_LOG`; this is the developer-facing channel.
//! - **stderr narration** — the dimmed `▸` lines the user follows along
//!   with during execution.
//!
//! The logger also accumulates per-node wall-clock timings and emits a
//! performance summary (slowest-first) when the graph completes.

use std::cmp::Reverse;
use super::state::StateManager;
use super::types::{Node, NodeType};
use crate::utils::dimmed_text;
use indexmap::IndexMap;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
struct NodeTiming {
    count: usize,
    total: Duration,
    max: Duration,
}

impl NodeTiming {
    fn record(&mut self, elapsed: Duration) {
        self.count += 1;
        self.total += elapsed;
        if elapsed > self.max {
            self.max = elapsed;
        }
    }
}

pub struct GraphLogger {
    graph_name: String,
    log_state_snapshots: bool,
    timings: IndexMap<String, NodeTiming>,
}

impl GraphLogger {
    pub fn new(graph_name: &str, log_state_snapshots: bool) -> Self {
        Self {
            graph_name: graph_name.to_string(),
            log_state_snapshots,
            timings: IndexMap::new(),
        }
    }

    pub fn graph_start(&self, start_node: &str, node_count: usize) {
        info!(
            "[graph:{}] start at '{}' ({} nodes)",
            self.graph_name, start_node, node_count
        );
        eprintln!(
            "{}",
            dimmed_text(&format!(
                "▸ graph: {} (start: {start_node})",
                self.graph_name
            ))
        );
    }

    pub fn graph_complete(&self, end_node: &str, elapsed: Duration) {
        info!(
            "[graph:{}] end '{}' (elapsed {:?})",
            self.graph_name, end_node, elapsed
        );
        eprintln!(
            "{}",
            dimmed_text(&format!("▸ graph done in {:.2}s", elapsed.as_secs_f64()))
        );
        self.log_performance_summary();
    }

    pub fn graph_error(&self, error: &anyhow::Error) {
        error!("[graph:{}] execution failed: {error:#}", self.graph_name);
    }

    pub fn node_entry(&self, node: &Node, visit: usize) {
        debug!(
            "[graph:{}] entering '{}' (visit {visit})",
            self.graph_name, node.id
        );
        eprintln!(
            "{}",
            dimmed_text(&format!("▸ {} ({})", node.id, node_type_label(node)))
        );
    }

    pub fn record_timing(&mut self, node_id: &str, elapsed: Duration) {
        self.timings
            .entry(node_id.to_string())
            .or_default()
            .record(elapsed);
    }

    pub fn routing(&self, from: &str, to: &str) {
        debug!("[graph:{}] {from} -> {to}", self.graph_name);
    }

    pub fn validation_warning(&self, node_id: Option<&str>, message: &str) {
        match node_id {
            Some(id) => warn!("[graph:{}] [{id}] {message}", self.graph_name),
            None => warn!("[graph:{}] {message}", self.graph_name),
        }
    }

    /// Log a state snapshot before a node runs. No-op unless the graph's
    /// `log_state_snapshots` setting is enabled. Keys + byte size go to
    /// `debug`; the full state goes to `trace` (it may contain secrets,
    /// so it is never logged at a more visible level).
    pub fn state_snapshot(&self, node_id: &str, state: &StateManager) {
        if !self.log_state_snapshots {
            return;
        }
        let snapshot = state.snapshot();
        let mut keys: Vec<&str> = snapshot.keys().map(String::as_str).collect();
        keys.sort_unstable();
        debug!(
            "[graph:{}] [{node_id}] state: {} bytes, keys={:?}",
            self.graph_name,
            state.size_bytes(),
            keys
        );
        trace!(
            "[graph:{}] [{node_id}] full state: {:?}",
            self.graph_name, snapshot
        );
    }

    fn log_performance_summary(&self) {
        if self.timings.is_empty() {
            return;
        }
        let mut rows: Vec<(&String, &NodeTiming)> = self.timings.iter().collect();
        rows.sort_by_key(|b| Reverse(b.1.total));
        info!(
            "[graph:{}] performance summary (slowest first):",
            self.graph_name
        );
        for (node_id, t) in rows {
            let avg = t.total / t.count.max(1) as u32;
            info!(
                "[graph:{}]   {node_id}: {} visit(s), total {}ms, avg {}ms, max {}ms",
                self.graph_name,
                t.count,
                t.total.as_millis(),
                avg.as_millis(),
                t.max.as_millis(),
            );
        }
    }
}

fn node_type_label(node: &Node) -> &'static str {
    match &node.node_type {
        NodeType::Agent(_) => "agent",
        NodeType::Script(_) => "script",
        NodeType::Approval(_) => "approval",
        NodeType::Input(_) => "input",
        NodeType::Llm(_) => "llm",
        NodeType::End(_) => "end",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_aggregates_node_timings() {
        let mut logger = GraphLogger::new("g", false);
        logger.record_timing("a", Duration::from_millis(100));
        logger.record_timing("a", Duration::from_millis(300));
        logger.record_timing("b", Duration::from_millis(50));

        let a = logger.timings.get("a").unwrap();
        assert_eq!(a.count, 2);
        assert_eq!(a.total, Duration::from_millis(400));
        assert_eq!(a.max, Duration::from_millis(300));

        let b = logger.timings.get("b").unwrap();
        assert_eq!(b.count, 1);
        assert_eq!(b.total, Duration::from_millis(50));
    }

    #[test]
    fn node_timing_max_tracks_largest() {
        let mut t = NodeTiming::default();
        t.record(Duration::from_millis(10));
        t.record(Duration::from_millis(80));
        t.record(Duration::from_millis(40));
        assert_eq!(t.max, Duration::from_millis(80));
        assert_eq!(t.count, 3);
        assert_eq!(t.total, Duration::from_millis(130));
    }

    #[test]
    fn new_logger_has_no_timings() {
        let logger = GraphLogger::new("g", true);
        assert!(logger.timings.is_empty());
        assert!(logger.log_state_snapshots);
    }
}
