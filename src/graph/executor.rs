//! Main execution loop for graph workflows.
//!
//! Dispatches each node to its type-specific executor, handles routing
//! (static `Node.next`, script `_next` override, approval `routes`, input
//! `on_timeout`), enforces `max_loop_iterations` and an optional
//! whole-graph timeout, and resolves the final `End` node's `output`
//! template as the graph's return value.

use super::agent::AgentNodeExecutor;
use super::parser::GraphParser;
use super::script::ScriptExecutor;
use super::state::StateManager;
use super::types::{EndNode, Graph, Node, NodeType};
use super::user_interaction::{ApprovalNodeExecutor, InputNodeExecutor};
use super::validator::GraphValidator;
use crate::config::RequestContext;
use crate::utils::{AbortSignal, dimmed_text};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct GraphExecutor {
    graph: Graph,
    base_dir: PathBuf,
}

impl GraphExecutor {
    pub fn new(graph: Graph, base_dir: impl Into<PathBuf>) -> Self {
        Self {
            graph,
            base_dir: base_dir.into(),
        }
    }

    /// Load a graph from disk and construct the executor in one step.
    /// `base_dir` is also used to resolve relative script paths.
    pub fn from_path(graph_path: impl AsRef<Path>, base_dir: impl Into<PathBuf>) -> Result<Self> {
        let base_dir = base_dir.into();
        let parser = GraphParser::new(&base_dir);
        let graph = parser.load_from_file(graph_path)?;
        Ok(Self::new(graph, base_dir))
    }

    /// Run the graph to completion. Returns the resolved `output` template
    /// of the terminal `End` node.
    pub async fn execute(
        self,
        ctx: &mut RequestContext,
        abort_signal: AbortSignal,
    ) -> Result<String> {
        let GraphExecutor { graph, base_dir } = self;

        if graph.settings.validate_before_run {
            let validator = GraphValidator::new(&base_dir);
            let result = validator.validate(&graph);
            for w in &result.warnings {
                let where_ = w
                    .node_id
                    .as_deref()
                    .map(|id| format!("[{id}] "))
                    .unwrap_or_default();
                warn!("[graph:{}] {}{}", graph.name, where_, w.message);
            }
            result.into_result()?;
        }

        let mut state = StateManager::new(graph.initial_state.clone());
        let script_executor = ScriptExecutor::new(&base_dir);
        let max_iterations = graph.settings.max_loop_iterations;
        let graph_timeout = graph.settings.timeout.map(Duration::from_secs);
        let start = Instant::now();

        let mut current = graph.start.clone();
        info!("[graph:{}] start at '{}'", graph.name, current);
        eprintln!(
            "{}",
            dimmed_text(&format!("▸ graph: {} (start: {})", graph.name, current))
        );

        let output = loop {
            if abort_signal.aborted() {
                bail!("Graph '{}' aborted at '{}'", graph.name, current);
            }
            if let Some(t) = graph_timeout
                && start.elapsed() > t
            {
                bail!(
                    "Graph '{}' timed out after {}s at '{}'",
                    graph.name,
                    t.as_secs(),
                    current
                );
            }

            state.state_mut().visit_node(&current);
            let visits = state.state().loop_count(&current);
            if visits > max_iterations {
                bail!(
                    "Node '{}' visited {} times (max_loop_iterations={}). \
                     Possible infinite loop.",
                    current,
                    visits,
                    max_iterations
                );
            }

            let node = graph
                .get_node(&current)
                .ok_or_else(|| anyhow!("Node '{}' not found in graph '{}'", current, graph.name))?;

            debug!(
                "[graph:{}] entering '{}' (visit {})",
                graph.name, current, visits
            );
            eprintln!(
                "{}",
                dimmed_text(&format!("▸ {} ({})", current, node_type_label(node)))
            );

            let next = step(
                node,
                &mut state,
                ctx,
                &script_executor,
                &graph.name,
                &current,
            )
            .await
            .with_context(|| format!("at node '{current}'"))?;

            match next {
                StepResult::Continue(next_id) => {
                    debug!("[graph:{}] {} -> {}", graph.name, current, next_id);
                    current = next_id;
                }
                StepResult::End(out) => {
                    info!(
                        "[graph:{}] end '{}' (elapsed {:?})",
                        graph.name,
                        current,
                        start.elapsed()
                    );
                    eprintln!(
                        "{}",
                        dimmed_text(&format!(
                            "▸ graph done in {:.2}s",
                            start.elapsed().as_secs_f64()
                        ))
                    );
                    break out;
                }
            }
        };

        Ok(output)
    }
}

enum StepResult {
    Continue(String),
    End(String),
}

fn node_type_label(node: &Node) -> &'static str {
    match &node.node_type {
        NodeType::Agent(_) => "agent",
        NodeType::Script(_) => "script",
        NodeType::Approval(_) => "approval",
        NodeType::Input(_) => "input",
        NodeType::End(_) => "end",
    }
}

async fn step(
    node: &Node,
    state: &mut StateManager,
    ctx: &mut RequestContext,
    script_executor: &ScriptExecutor,
    graph_name: &str,
    current: &str,
) -> Result<StepResult> {
    match &node.node_type {
        NodeType::Agent(agent_node) => {
            AgentNodeExecutor::execute(agent_node, state, ctx).await?;
            let next = node.next.clone().ok_or_else(|| {
                anyhow!("agent node '{current}' has no `next` and is not an end node")
            })?;
            Ok(StepResult::Continue(next))
        }
        NodeType::Script(script_node) => {
            let dynamic = match script_executor.execute(script_node, state).await {
                Ok(n) => n,
                Err(e) => {
                    if let Some(fallback) = &script_node.fallback {
                        warn!(
                            "[graph:{}] script '{}' failed, routing to fallback '{}': {}",
                            graph_name, current, fallback, e
                        );
                        return Ok(StepResult::Continue(fallback.clone()));
                    }
                    return Err(e);
                }
            };
            let next = dynamic.or_else(|| node.next.clone()).ok_or_else(|| {
                anyhow!("script node '{current}' did not emit `_next` and has no static `next`")
            })?;
            Ok(StepResult::Continue(next))
        }
        NodeType::Approval(approval_node) => {
            let next = ApprovalNodeExecutor::execute(approval_node, state, ctx).await?;
            Ok(StepResult::Continue(next))
        }
        NodeType::Input(input_node) => {
            let next =
                InputNodeExecutor::execute(input_node, node.next.as_deref(), state, ctx).await?;
            Ok(StepResult::Continue(next))
        }
        NodeType::End(end_node) => Ok(StepResult::End(resolve_end_output(end_node, state))),
    }
}

/// Apply the end node's `state_updates`, then interpolate its `output`
/// template against the resulting state. Both use lenient interpolation
/// so the graph still produces a result even when some keys are absent.
fn resolve_end_output(end_node: &EndNode, state: &mut StateManager) -> String {
    apply_simple_state_updates(end_node.state_updates.as_ref(), state);
    state.interpolate_lenient(&end_node.output)
}

fn apply_simple_state_updates(updates: Option<&HashMap<String, String>>, state: &mut StateManager) {
    let Some(updates) = updates else {
        return;
    };
    for (key, template) in updates {
        let value = state.interpolate_lenient(template);
        state.state_mut().set(key.clone(), Value::String(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state_with(pairs: &[(&str, Value)]) -> StateManager {
        let mut map = HashMap::new();
        for (k, v) in pairs {
            map.insert((*k).into(), v.clone());
        }
        StateManager::new(map)
    }

    fn end_node(output: &str, updates: Option<HashMap<String, String>>) -> EndNode {
        EndNode {
            output: output.into(),
            state_updates: updates,
        }
    }

    #[test]
    fn resolve_end_output_interpolates_template_against_state() {
        let mut state = state_with(&[("name", json!("alice"))]);
        let node = end_node("done: {{name}}", None);
        assert_eq!(resolve_end_output(&node, &mut state), "done: alice");
    }

    #[test]
    fn resolve_end_output_applies_state_updates_before_interpolation() {
        let mut updates = HashMap::new();
        updates.insert("summary".into(), "completed for {{user}}".into());
        let node = end_node("RESULT: {{summary}}", Some(updates));
        let mut state = state_with(&[("user", json!("bob"))]);
        assert_eq!(
            resolve_end_output(&node, &mut state),
            "RESULT: completed for bob"
        );
        assert_eq!(
            state.state().get("summary"),
            Some(&json!("completed for bob"))
        );
    }

    #[test]
    fn resolve_end_output_with_empty_template_returns_empty_string() {
        let mut state = state_with(&[]);
        let node = end_node("", None);
        assert_eq!(resolve_end_output(&node, &mut state), "");
    }

    #[test]
    fn resolve_end_output_lenient_on_missing_keys() {
        let mut state = state_with(&[]);
        let node = end_node("hello {{unknown}}!", None);
        assert_eq!(resolve_end_output(&node, &mut state), "hello !");
    }

    #[test]
    fn apply_simple_state_updates_does_nothing_when_none() {
        let mut state = state_with(&[("k", json!("v"))]);
        apply_simple_state_updates(None, &mut state);
        assert_eq!(state.state().get("k"), Some(&json!("v")));
    }

    #[test]
    fn apply_simple_state_updates_overwrites_existing_values() {
        let mut updates = HashMap::new();
        updates.insert("k".into(), "new-{{k}}".into());
        let mut state = state_with(&[("k", json!("old"))]);
        apply_simple_state_updates(Some(&updates), &mut state);
        assert_eq!(state.state().get("k"), Some(&json!("new-old")));
    }

    #[test]
    fn from_path_loads_and_constructs_executor() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "loki-graph-executor-test-{}.yaml",
            std::process::id()
        ));
        let yaml = r#"
name: test_graph
start: only
nodes:
  only:
    type: end
    output: hello
"#;
        std::fs::write(&path, yaml).unwrap();

        let parent = path.parent().unwrap().to_path_buf();
        let executor = GraphExecutor::from_path(&path, &parent).unwrap();
        assert_eq!(executor.graph.name, "test_graph");
        assert_eq!(executor.graph.start, "only");

        let _ = std::fs::remove_file(&path);
    }
}
