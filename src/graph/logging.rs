use super::state::StateManager;
use super::types::{Node, NodeType};
use crate::utils::dimmed_text;
use chrono::Local;
use indexmap::IndexMap;
use std::cmp::Reverse;
use std::time::Duration;

fn ts() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

fn fmt_secs(elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs < 1.0 {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{secs:.2}s")
    }
}

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
    silent: bool,
    timings: IndexMap<String, NodeTiming>,
}

impl GraphLogger {
    pub fn with_visibility(graph_name: &str, log_state_snapshots: bool, silent: bool) -> Self {
        Self {
            graph_name: graph_name.to_string(),
            log_state_snapshots,
            silent,
            timings: IndexMap::new(),
        }
    }

    pub fn graph_start(&self, start_node: &str, node_count: usize) {
        info!(
            "[graph:{}] start at '{}' ({} nodes)",
            self.graph_name, start_node, node_count
        );
        if !self.silent {
            eprintln!(
                "{}",
                dimmed_text(&format!(
                    "▸ graph: {} (start: {start_node})",
                    self.graph_name
                ))
            );
        }
    }

    pub fn graph_complete(&self, end_node: &str, elapsed: Duration) {
        info!(
            "[graph:{}] end '{}' (elapsed {:?})",
            self.graph_name, end_node, elapsed
        );
        if !self.silent {
            eprintln!(
                "{}",
                dimmed_text(&format!("▸ graph done in {:.2}s", elapsed.as_secs_f64()))
            );
        }
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
    }

    pub fn silent(&self) -> bool {
        self.silent
    }

    pub fn node_start(&self, node: &Node, in_super_step: bool) {
        narrate_node_start(self.silent, node, in_super_step);
    }

    pub fn super_step_start(&self, branches: &[String]) {
        if self.silent {
            return;
        }
        eprintln!(
            "{}",
            dimmed_text(&format!(
                "▸ {} super-step start: {}",
                ts(),
                branches.join(", ")
            ))
        );
    }

    pub fn super_step_end(&self, targets: &[String]) {
        if self.silent {
            return;
        }
        let route = if targets.is_empty() {
            String::new()
        } else {
            format!(" -> {}", targets.join(", "))
        };
        eprintln!(
            "{}",
            dimmed_text(&format!("▸ {} super-step end{route}", ts()))
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

pub fn narrate_node_start(silent: bool, node: &Node, in_super_step: bool) {
    if silent {
        return;
    }
    let indent = if in_super_step { "  " } else { "" };
    let label = node_type_label(node);
    eprintln!(
        "{}",
        dimmed_text(&format!("▸ {} {indent}{} ({label}) start", ts(), node.id))
    );
}

pub fn narrate_node_complete(
    silent: bool,
    node: &Node,
    elapsed: Duration,
    next_target: Option<&str>,
    in_super_step: bool,
) {
    if silent {
        return;
    }
    let indent = if in_super_step { "  " } else { "" };
    let label = node_type_label(node);
    let dur = fmt_secs(elapsed);
    let route = next_target.map(|t| format!(" -> {t}")).unwrap_or_default();
    eprintln!(
        "{}",
        dimmed_text(&format!(
            "▸ {} {indent}{} ({label}) done in {dur}{route}",
            ts(),
            node.id
        ))
    );
}

pub fn narrate_node_failed(
    silent: bool,
    node: &Node,
    elapsed: Duration,
    err: &str,
    in_super_step: bool,
) {
    if silent {
        return;
    }
    let indent = if in_super_step { "  " } else { "" };
    let label = node_type_label(node);
    let dur = fmt_secs(elapsed);
    let excerpt: String = err.chars().take(120).collect();
    eprintln!(
        "{}",
        dimmed_text(&format!(
            "▸ {} {indent}{} ({label}) FAILED in {dur} -- {excerpt}",
            ts(),
            node.id
        ))
    );
}

pub(super) fn node_type_label(node: &Node) -> &'static str {
    match &node.node_type {
        NodeType::Agent(_) => "agent",
        NodeType::Script(_) => "script",
        NodeType::Approval(_) => "approval",
        NodeType::Input(_) => "input",
        NodeType::Llm(_) => "llm",
        NodeType::Rag(_) => "rag",
        NodeType::End(_) => "end",
        NodeType::Map(_) => "map",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_aggregates_node_timings() {
        let mut logger = GraphLogger::with_visibility("g", false, false);
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
        let logger = GraphLogger::with_visibility("g", true, false);

        assert!(logger.timings.is_empty());
        assert!(logger.log_state_snapshots);
    }
}
