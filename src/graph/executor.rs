use super::agent::AgentNodeExecutor;
use super::llm::LlmNodeExecutor;
use super::logging::GraphLogger;
use super::map::MapNodeExecutor;
use super::progress::{BranchProgressHandle, BranchProgressTracker};
use super::rag::RagNodeExecutor;
use super::script::ScriptExecutor;
use super::staging::BranchWrites;
use super::state::StateManager;
use super::types::{EndNode, Graph, Node, NodeType};
use super::user_interaction::{ApprovalNodeExecutor, InputNodeExecutor};
use super::validator::{AgentValidationContext, GraphValidator};
use crate::config::{RenderMode, RequestContext};
use crate::utils::AbortSignal;
use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::join_all;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

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

    pub async fn execute(
        self,
        ctx: &mut RequestContext,
        abort_signal: AbortSignal,
    ) -> Result<String> {
        let mut logger =
            GraphLogger::new(&self.graph.name, self.graph.settings.log_state_snapshots);
        let result = self.run(&mut logger, ctx, abort_signal).await;
        if let Err(e) = &result {
            logger.graph_error(e);
        }
        result
    }

    async fn run(
        self,
        logger: &mut GraphLogger,
        ctx: &mut RequestContext,
        abort_signal: AbortSignal,
    ) -> Result<String> {
        let GraphExecutor { graph, base_dir } = self;

        if graph.settings.validate_before_run {
            let mut validator = GraphValidator::new(&base_dir);
            if let Some(agent) = &ctx.agent {
                validator = validator.with_agent_context(AgentValidationContext::from_agent(
                    agent,
                    Arc::clone(&ctx.app.config),
                ));
            }
            let result = validator.validate(&graph);
            for w in &result.warnings {
                logger.validation_warning(w.node_id.as_deref(), &w.message);
            }
            result.into_result()?;
        }

        let mut state = StateManager::new(graph.initial_state.clone());
        let script_executor = ScriptExecutor::new(&base_dir);
        let max_iterations = graph.settings.max_loop_iterations;
        let graph_timeout = graph.settings.timeout.map(Duration::from_secs);
        let max_concurrency = graph.settings.max_concurrency;
        // Wrap in Arc so spawned branch tasks can cheaply share the Graph for
        // node lookup (especially the map executor, which needs to resolve its
        // `branch:` target from inside a spawned task).
        let graph = Arc::new(graph);
        let start = Instant::now();

        let mut frontier: HashSet<String> = HashSet::from([graph.start.clone()]);
        logger.graph_start(&graph.start, graph.nodes.len());

        loop {
            if frontier.is_empty() {
                bail!(
                    "Graph '{}' frontier emptied without reaching an End node",
                    graph.name
                );
            }

            if abort_signal.aborted() {
                bail!(
                    "Graph '{}' aborted before super-step with frontier {:?}",
                    graph.name,
                    sorted_frontier(&frontier)
                );
            }
            if let Some(t) = graph_timeout
                && start.elapsed() > t
            {
                bail!(
                    "Graph '{}' timed out after {}s before super-step with frontier {:?}",
                    graph.name,
                    t.as_secs(),
                    sorted_frontier(&frontier)
                );
            }

            // Loop-count and visit tracking on live state, BEFORE forking.
            // This counts every entry to a node toward max_loop_iterations
            // regardless of how many parallel branches converged on it.
            for node_id in &frontier {
                state.state_mut().visit_node(node_id);
                let visits = state.state().loop_count(node_id);
                if visits > max_iterations {
                    bail!(
                        "Node '{}' visited {} times (max_loop_iterations={}). \
                         Possible infinite loop.",
                        node_id,
                        visits,
                        max_iterations
                    );
                }
            }

            for node_id in &frontier {
                let node = graph.get_node(node_id).ok_or_else(|| {
                    anyhow!("Node '{}' not found in graph '{}'", node_id, graph.name)
                })?;
                let visits = state.state().loop_count(node_id);
                logger.node_entry(node, visits);
            }
            let snapshot_label = if frontier.len() == 1 {
                frontier.iter().next().cloned().unwrap_or_default()
            } else {
                format!("super-step {{{}}}", sorted_frontier(&frontier).join(","))
            };
            logger.state_snapshot(&snapshot_label, &state);

            let snapshot = state.read_snapshot();
            let semaphore = Arc::new(Semaphore::new(max_concurrency));

            let frontier_size = frontier.len();
            let progress_tracker = if frontier_size > 1 {
                Some(BranchProgressTracker::new())
            } else {
                None
            };
            let mut branch_tasks = Vec::with_capacity(frontier_size);
            for node_id in &frontier {
                let node = graph
                    .get_node(node_id)
                    .ok_or_else(|| {
                        anyhow!("Node '{}' not found in graph '{}'", node_id, graph.name)
                    })?
                    .clone();
                let branch_state = state.fork_for_branch_state();
                let mut branch_ctx = ctx.fork_for_branch();
                if frontier_size > 1 {
                    branch_ctx.render_mode = RenderMode::Silent;
                }
                let script_exec_clone = script_executor.clone();
                let graph_clone = Arc::clone(&graph);
                let current = node_id.clone();
                let sem_clone = semaphore.clone();
                let abort_clone = abort_signal.clone();
                let progress_handle: Option<BranchProgressHandle> =
                    progress_tracker.as_ref().map(|t| t.add_branch(node_id));

                let task = tokio::spawn(async move {
                    let mut progress_handle = progress_handle;
                    let _permit = sem_clone
                        .acquire()
                        .await
                        .expect("semaphore should not be closed");
                    if abort_clone.aborted() {
                        if let Some(h) = progress_handle.take() {
                            h.fail("aborted");
                        }
                        return (
                            current.clone(),
                            branch_state,
                            Err(anyhow!("branch aborted")),
                            Duration::default(),
                        );
                    }
                    let node_start = Instant::now();
                    let mut state = branch_state;
                    let mut ctx = branch_ctx;
                    let step_ctx = StepContext {
                        graph: graph_clone.as_ref(),
                        script_executor: &script_exec_clone,
                        max_concurrency,
                        abort_signal: &abort_clone,
                    };
                    let result = step(&node, &mut state, &mut ctx, &step_ctx, &current).await;
                    let elapsed = node_start.elapsed();
                    if let Some(h) = progress_handle.take() {
                        match &result {
                            Ok(_) => h.complete(),
                            Err(e) => h.fail(&e.to_string()),
                        }
                    }
                    (current, state, result, elapsed)
                });
                branch_tasks.push(task);
            }

            let joined = join_all(branch_tasks).await;
            if let Some(t) = &progress_tracker {
                t.clear();
            }

            let mut branch_writes: Vec<BranchWrites> = Vec::new();
            let mut next_frontier: HashSet<String> = HashSet::new();
            let mut end_results: Vec<(String, StateManager, String)> = Vec::new();

            for join_result in joined {
                let (node_id, branch_state, step_result, elapsed) =
                    join_result.map_err(|e| anyhow!("Branch task panicked: {e}"))?;
                logger.record_timing(&node_id, elapsed);

                let step_outcome = step_result.with_context(|| format!("at node '{node_id}'"))?;

                match step_outcome {
                    StepResult::Continue(target) => {
                        logger.routing(&node_id, &target);
                        let diff = branch_state.diff_against(snapshot.as_ref());
                        branch_writes.push(BranchWrites {
                            node_id: node_id.clone(),
                            invocation_index: 0,
                            writes: diff,
                        });
                        next_frontier.insert(target);
                    }
                    StepResult::End(output) => {
                        end_results.push((node_id.clone(), branch_state, output));
                    }
                }
            }

            if end_results.len() > 1 {
                let mut ids: Vec<String> =
                    end_results.iter().map(|(id, _, _)| id.clone()).collect();
                ids.sort();
                bail!(
                    "super-step ended with multiple End targets ({}). \
                     Fan-out branches must converge at a join node before \
                     terminating. To fix: route all parallel branches to a \
                     single shared next-node, then terminate from there.",
                    ids.join(", ")
                );
            }

            // Sort by (node_id, invocation_index) so non-commutative reducers
            // like Concat/Merge produce deterministic output across runs.
            branch_writes.sort_by(|a, b| {
                a.node_id
                    .cmp(&b.node_id)
                    .then(a.invocation_index.cmp(&b.invocation_index))
            });
            state.apply_branch_writes(branch_writes, &graph.reducers)?;

            if let Some((node_id, end_state, output)) = end_results.into_iter().next() {
                let diff = end_state.diff_against(snapshot.as_ref());
                state.apply_branch_writes(
                    vec![BranchWrites {
                        node_id: node_id.clone(),
                        invocation_index: 0,
                        writes: diff,
                    }],
                    &graph.reducers,
                )?;
                logger.graph_complete(&node_id, start.elapsed());
                return Ok(output);
            }

            frontier = next_frontier;
        }
    }
}

fn sorted_frontier(frontier: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = frontier.iter().cloned().collect();
    v.sort();
    v
}

// Bundles the engine-config refs that every `step()` call needs to thread
// through. Constructed once per spawned branch task (or once at the call site
// for sequential paths) so step() and downstream executors (MapNodeExecutor)
// take one parameter instead of five.
pub(super) struct StepContext<'a> {
    pub graph: &'a Graph,
    pub script_executor: &'a ScriptExecutor,
    pub max_concurrency: usize,
    pub abort_signal: &'a AbortSignal,
}

impl StepContext<'_> {
    pub fn graph_name(&self) -> &str {
        &self.graph.name
    }
}

enum StepResult {
    Continue(String),
    End(String),
}

async fn step(
    node: &Node,
    state: &mut StateManager,
    ctx: &mut RequestContext,
    step_ctx: &StepContext<'_>,
    current: &str,
) -> Result<StepResult> {
    match &node.node_type {
        NodeType::Agent(agent_node) => {
            AgentNodeExecutor::execute(agent_node, state, ctx).await?;
            let next = node
                .next_single()?
                .ok_or_else(|| {
                    anyhow!("agent node '{current}' has no `next` and is not an end node")
                })?
                .to_string();
            Ok(StepResult::Continue(next))
        }
        NodeType::Script(script_node) => {
            let dynamic = match step_ctx.script_executor.execute(script_node, state).await {
                Ok(n) => n,
                Err(e) => {
                    if let Some(fallback) = &script_node.fallback {
                        warn!(
                            "[graph:{}] script '{}' failed, routing to fallback '{}': {}",
                            step_ctx.graph_name(),
                            current,
                            fallback,
                            e
                        );
                        return Ok(StepResult::Continue(fallback.clone()));
                    }
                    return Err(e);
                }
            };
            let next = match dynamic {
                Some(n) => n,
                None => node
                    .next_single()?
                    .ok_or_else(|| {
                        anyhow!(
                            "script node '{current}' did not emit `_next` and has no static `next`"
                        )
                    })?
                    .to_string(),
            };
            Ok(StepResult::Continue(next))
        }
        NodeType::Approval(approval_node) => {
            let next = ApprovalNodeExecutor::execute(approval_node, state, ctx).await?;
            Ok(StepResult::Continue(next))
        }
        NodeType::Input(input_node) => {
            let next_id = node.next_single()?;
            let next = InputNodeExecutor::execute(input_node, next_id, state, ctx).await?;
            Ok(StepResult::Continue(next))
        }
        NodeType::Llm(llm_node) => {
            let next_id = node.next_single()?;
            let next = LlmNodeExecutor::execute(llm_node, next_id, state, ctx).await?;
            Ok(StepResult::Continue(next))
        }
        NodeType::Rag(rag_node) => {
            let next_id = node.next_single()?;
            let next = RagNodeExecutor::execute(rag_node, current, next_id, state, ctx).await?;
            Ok(StepResult::Continue(next))
        }
        NodeType::End(end_node) => Ok(StepResult::End(resolve_end_output(end_node, state))),
        NodeType::Map(map_node) => {
            let next = node
                .next_single()?
                .ok_or_else(|| {
                    anyhow!("map node '{current}' has no `next` and is not an end node")
                })?
                .to_string();
            MapNodeExecutor::execute(map_node, state, ctx, step_ctx, current).await?;
            Ok(StepResult::Continue(next))
        }
    }
}

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
}
