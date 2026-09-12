use super::agent::{AgentExecutionOutcome, AgentNodeExecutor};
use super::llm::{LlmExecutionOutcome, LlmNodeExecutor};
use super::logging::{GraphLogger, narrate_node_complete, narrate_node_failed};
use super::map::MapNodeExecutor;
use super::rag::RagNodeExecutor;
use super::script::ScriptExecutor;
use super::staging::BranchWrites;
use super::state::StateManager;
use super::types::{EndNode, Graph, Node, NodeType};
use super::user_interaction::{ApprovalNodeExecutor, InputNodeExecutor};
use super::validator::{AgentValidationContext, GraphValidator};
use super::wall_clock;
use crate::config::{AgentVariable, AgentVariables, RenderMode, RequestContext};
use crate::supervisor::mailbox::{Inbox, PeerAssignment, PeerRegistry, graph_agent_id};
use crate::utils::{AbortSignal, wait_abort_signal, wait_user_interrupt};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::join_all;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::task::{AbortHandle, JoinHandle};

/// Test-only hook invoked inside a frontier branch task right after its
/// teammate identity is retired, with the super-step's registry and the
/// retired peer id. Runs before the task returns, so an observer sees the
/// registry as siblings still in flight see it.
#[cfg(test)]
type FrontierObserver = Arc<dyn Fn(&PeerRegistry, &str) + Send + Sync>;

pub struct GraphExecutor {
    graph: Graph,
    base_dir: PathBuf,
    #[cfg(test)]
    frontier_observer: Option<FrontierObserver>,
}

impl GraphExecutor {
    pub fn new(graph: Graph, base_dir: impl Into<PathBuf>) -> Self {
        Self {
            graph,
            base_dir: base_dir.into(),
            #[cfg(test)]
            frontier_observer: None,
        }
    }

    #[cfg(test)]
    fn with_frontier_observer(mut self, observer: FrontierObserver) -> Self {
        self.frontier_observer = Some(observer);
        self
    }

    pub async fn execute(
        self,
        ctx: &mut RequestContext,
        abort_signal: AbortSignal,
    ) -> Result<String> {
        let is_nested = ctx.current_depth > 0;
        let mut logger = GraphLogger::with_visibility(
            &self.graph.name,
            self.graph.settings.log_state_snapshots,
            is_nested,
        );
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
        let GraphExecutor {
            graph,
            base_dir,
            #[cfg(test)]
            frontier_observer,
        } = self;

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

        let mut initial_state = graph.initial_state.clone();
        seed_variables(
            &mut initial_state,
            &graph.variables,
            ctx.agent.as_ref().map(|a| a.variables()),
        );
        let mut state = StateManager::new(initial_state);
        let agent_envs = ctx
            .agent
            .as_ref()
            .map(|a| a.variable_envs())
            .unwrap_or_default();
        let script_executor = ScriptExecutor::new(&base_dir).with_envs(agent_envs);
        let max_iterations = graph.settings.max_loop_iterations;
        let graph_timeout = graph.settings.timeout.and_then(wall_clock);
        let max_concurrency = graph.settings.max_concurrency;
        if max_concurrency > Semaphore::MAX_PERMITS {
            bail!(
                "Graph '{}': settings.max_concurrency {max_concurrency} exceeds the runtime limit of {}",
                graph.name,
                Semaphore::MAX_PERMITS
            );
        }
        let graph = Arc::new(graph);
        let start = Instant::now();

        // Maps a user interrupt (SIGINT, or the enclosing turn aborting) onto
        // this run's abort flag for as long as the run lives. Without a
        // session signal (ACP, bare executor) SIGINT handling is unchanged.
        let _bridge = match ctx.session_abort.clone() {
            Some(session) if session.aborted() => {
                abort_signal.set_ctrlc();
                None
            }
            Some(session) => {
                let graph_abort = abort_signal.clone();
                Some(TaskCancelGuard::spawn_one(async move {
                    wait_user_interrupt(Some(&session)).await;
                    graph_abort.set_ctrlc();
                }))
            }
            None => None,
        };

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
            // A cap of 0 disables the check.
            for node_id in &frontier {
                state.state_mut().visit_node(node_id);
                let visits = state.state().loop_count(node_id);
                if max_iterations > 0 && visits > max_iterations {
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
            let in_super_step = frontier_size > 1;
            let silent = logger.silent();

            if in_super_step {
                let mut branches = sorted_frontier(&frontier);
                branches.sort();
                logger.super_step_start(&branches);
            }

            // Pre-provision teammate identities for `teammates: true` agent
            // nodes running concurrently in this super-step, before any branch
            // starts, so an early finisher can message one still waiting on
            // the semaphore. A lone flagged node has no peers.
            let flagged = teammate_flagged_nodes(&graph, &frontier);
            let (peer_registry, mut peer_assignments) = provision_frontier_peers(flagged);

            let mut branch_tasks = Vec::with_capacity(frontier_size);
            for node_id in &frontier {
                let node = graph
                    .get_node(node_id)
                    .ok_or_else(|| {
                        anyhow!("Node '{}' not found in graph '{}'", node_id, graph.name)
                    })?
                    .clone();
                logger.node_start(&node, in_super_step);
                let branch_state = state.fork_for_branch_state();
                let mut branch_ctx = ctx.fork_for_branch();
                let mut peer_id: Option<String> = None;
                if let Some(assignment) = peer_assignments.remove(node_id) {
                    peer_id = Some(assignment.0.clone());
                    branch_ctx.peer_registry = peer_registry.clone();
                    branch_ctx.peer_assignment = Some(assignment);
                }
                let registry_for_task = peer_registry.clone();
                if in_super_step {
                    branch_ctx.render_mode = RenderMode::Silent;
                }
                let script_exec_clone = script_executor.clone();
                let graph_clone = Arc::clone(&graph);
                let current = node_id.clone();
                let sem_clone = semaphore.clone();
                let abort_clone = abort_signal.clone();
                #[cfg(test)]
                let observer = frontier_observer.clone();

                // Retires the teammate identity however this task ends:
                // dropped explicitly right after step() on the normal path,
                // by unwinding on the abort return, task abort, or panic.
                // Built before the spawn so a task aborted before its first
                // poll still drops it with the future.
                let retire = registry_for_task
                    .zip(peer_id)
                    .map(|(registry, id)| PeerRetireGuard {
                        registry,
                        id,
                        #[cfg(test)]
                        observer,
                    });
                let task = tokio::spawn(async move {
                    let retire = retire;
                    let _permit = sem_clone
                        .acquire()
                        .await
                        .expect("semaphore should not be closed");
                    if abort_clone.aborted() {
                        narrate_node_failed(
                            silent,
                            &node,
                            Duration::default(),
                            "aborted",
                            in_super_step,
                        );
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
                        graph: Arc::clone(&graph_clone),
                        script_executor: &script_exec_clone,
                        max_concurrency,
                        abort_signal: &abort_clone,
                        branch_mode: false,
                    };
                    let result = step(&node, &mut state, &mut ctx, &step_ctx, &current).await;
                    drop(retire);
                    let elapsed = node_start.elapsed();
                    match &result {
                        Ok(StepResult::Continue(targets)) => {
                            let route = if targets.is_empty() {
                                None
                            } else {
                                Some(targets.join(", "))
                            };
                            narrate_node_complete(
                                silent,
                                &node,
                                elapsed,
                                route.as_deref(),
                                in_super_step,
                            );
                        }
                        Ok(StepResult::End(_)) => {
                            narrate_node_complete(
                                silent,
                                &node,
                                elapsed,
                                Some("END"),
                                in_super_step,
                            );
                        }
                        Err(e) => {
                            narrate_node_failed(
                                silent,
                                &node,
                                elapsed,
                                &e.to_string(),
                                in_super_step,
                            );
                        }
                    }
                    (current, state, result, elapsed)
                });
                branch_tasks.push(task);
            }

            // Owns the branch tasks until the join completes: a timeout or
            // abort bail drops it and cancels whatever is still in flight.
            let _cancel = TaskCancelGuard::new(&branch_tasks);
            let bounded_join = async {
                match graph_timeout {
                    Some(t) => {
                        let remaining = t.saturating_sub(start.elapsed());
                        tokio::time::timeout(remaining, join_all(branch_tasks))
                            .await
                            .map_err(|_| {
                                anyhow!(
                                    "Graph '{}' timed out after {}s during super-step with frontier {:?}",
                                    graph.name,
                                    t.as_secs(),
                                    sorted_frontier(&frontier)
                                )
                            })
                    }
                    None => Ok(join_all(branch_tasks).await),
                }
            };
            let joined = tokio::select! {
                joined = bounded_join => joined?,
                _ = wait_abort_signal(&abort_signal) => bail!(
                    "Graph '{}' aborted during super-step with frontier {:?}",
                    graph.name,
                    sorted_frontier(&frontier)
                ),
            };

            let mut branch_writes: Vec<BranchWrites> = Vec::new();
            let mut next_frontier: HashSet<String> = HashSet::new();
            let mut end_results: Vec<(String, StateManager, String)> = Vec::new();

            for join_result in joined {
                let (node_id, branch_state, step_result, elapsed) =
                    join_result.map_err(|e| anyhow!("Branch task panicked: {e}"))?;
                logger.record_timing(&node_id, elapsed);

                let step_outcome = step_result.with_context(|| format!("at node '{node_id}'"))?;

                match step_outcome {
                    StepResult::Continue(targets) => {
                        for target in &targets {
                            logger.routing(&node_id, target);
                        }
                        let diff = branch_state.diff_against(snapshot.as_ref());
                        branch_writes.push(BranchWrites {
                            node_id: node_id.clone(),
                            invocation_index: 0,
                            writes: diff,
                        });
                        next_frontier.extend(targets);
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

            if in_super_step {
                logger.super_step_end(&sorted_frontier(&next_frontier));
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

fn seed_variables(
    initial_state: &mut HashMap<String, Value>,
    variables: &[AgentVariable],
    agent_vars: Option<&AgentVariables>,
) {
    for var in variables {
        let resolved = agent_vars
            .and_then(|vars| vars.get(&var.name))
            .cloned()
            .or_else(|| var.default.clone());
        if let Some(value) = resolved {
            initial_state
                .entry(var.name.clone())
                .or_insert(Value::String(value));
        }
    }
}

fn teammate_flagged_nodes(graph: &Graph, frontier: &HashSet<String>) -> Vec<(String, String)> {
    frontier
        .iter()
        .filter_map(
            |node_id| match graph.get_node(node_id).map(|n| &n.node_type) {
                Some(NodeType::Agent(n)) if n.teammates => Some((node_id.clone(), n.agent.clone())),
                _ => None,
            },
        )
        .collect()
}

fn provision_frontier_peers(
    flagged: Vec<(String, String)>,
) -> (Option<Arc<PeerRegistry>>, HashMap<String, PeerAssignment>) {
    if flagged.len() < 2 {
        return (None, HashMap::new());
    }

    let registry = Arc::new(PeerRegistry::new());
    let mut assignments = HashMap::new();
    for (node_id, agent_name) in flagged {
        let id = graph_agent_id(&agent_name);
        let inbox = Arc::new(Inbox::new());
        registry.insert(id.clone(), node_id.clone(), Arc::clone(&inbox));
        assignments.insert(node_id, (id, inbox));
    }
    (Some(registry), assignments)
}

pub(super) struct TaskCancelGuard(Vec<AbortHandle>);

impl TaskCancelGuard {
    pub(super) fn new<T>(tasks: &[JoinHandle<T>]) -> Self {
        Self(tasks.iter().map(|task| task.abort_handle()).collect())
    }

    pub(super) fn spawn_one<F>(fut: F) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self(vec![tokio::spawn(fut).abort_handle()])
    }
}

impl Drop for TaskCancelGuard {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

/// Retires a teammate identity when dropped, so a branch or item task that
/// is aborted, panics, or returns early still leaves its peers seeing it as
/// finished. `mark_finished` is idempotent; an explicit earlier call is fine.
pub(super) struct PeerRetireGuard {
    pub(super) registry: Arc<PeerRegistry>,
    pub(super) id: String,
    #[cfg(test)]
    pub(super) observer: Option<FrontierObserver>,
}

impl Drop for PeerRetireGuard {
    fn drop(&mut self) {
        self.registry.mark_finished(&self.id);
        #[cfg(test)]
        if let Some(observe) = &self.observer {
            observe(&self.registry, &self.id);
        }
    }
}

pub(super) struct StepContext<'a> {
    pub graph: Arc<Graph>,
    pub script_executor: &'a ScriptExecutor,
    pub max_concurrency: usize,
    pub abort_signal: &'a AbortSignal,
    pub branch_mode: bool,
}

impl StepContext<'_> {
    pub fn graph_name(&self) -> &str {
        &self.graph.name
    }
}

pub(super) enum StepResult {
    // The set of next-node ids the executor should add to the next super-step's
    // frontier. A `Vec` of length 1 for sequential routing (default) and the
    // full target list for fan-out (`next: [a, b, ...]`). Dynamic single-route
    // decisions (script `_next`, approval routes, LLM/RAG fallback) always emit
    // a single-element vec.
    Continue(Vec<String>),
    End(String),
}

pub(super) async fn step(
    node: &Node,
    state: &mut StateManager,
    ctx: &mut RequestContext,
    step_ctx: &StepContext<'_>,
    current: &str,
) -> Result<StepResult> {
    match &node.node_type {
        NodeType::Agent(agent_node) => {
            let outcome =
                AgentNodeExecutor::execute(agent_node, state, ctx, !step_ctx.branch_mode).await?;
            let targets = match outcome {
                AgentExecutionOutcome::Continue(_) => {
                    static_next_targets(node, current, "agent", step_ctx.branch_mode)?
                }
                AgentExecutionOutcome::FellBack(target) => vec![target],
            };
            Ok(StepResult::Continue(targets))
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
                        return Ok(StepResult::Continue(vec![fallback.clone()]));
                    }
                    return Err(e);
                }
            };
            let targets = match dynamic {
                Some(n) => vec![n],
                None => static_next_targets(node, current, "script", step_ctx.branch_mode)?,
            };
            Ok(StepResult::Continue(targets))
        }
        NodeType::Approval(approval_node) => {
            let next = ApprovalNodeExecutor::execute(approval_node, state, ctx).await?;
            Ok(StepResult::Continue(vec![next]))
        }
        NodeType::Input(input_node) => {
            let next_id = first_next_target(node);
            let next = InputNodeExecutor::execute(input_node, next_id, state, ctx).await?;
            Ok(StepResult::Continue(vec![next]))
        }
        NodeType::Llm(llm_node) => {
            let outcome =
                LlmNodeExecutor::execute(current, llm_node, state, ctx, step_ctx.abort_signal)
                    .await?;
            let targets = match outcome {
                LlmExecutionOutcome::Continue => {
                    static_next_targets(node, current, "llm", step_ctx.branch_mode)?
                }
                LlmExecutionOutcome::FellBack(target) => vec![target],
            };
            Ok(StepResult::Continue(targets))
        }
        NodeType::Rag(rag_node) => {
            RagNodeExecutor::execute(rag_node, current, state, ctx).await?;
            let targets = static_next_targets(node, current, "rag", step_ctx.branch_mode)?;
            Ok(StepResult::Continue(targets))
        }
        NodeType::End(end_node) => Ok(StepResult::End(resolve_end_output(end_node, state))),
        NodeType::Map(map_node) => {
            let targets = static_next_targets(node, current, "map", step_ctx.branch_mode)?;
            MapNodeExecutor::execute(map_node, state, ctx, step_ctx, current).await?;
            Ok(StepResult::Continue(targets))
        }
    }
}

/// Inside a map branch a node without `next` simply ends the item's chain; on
/// the main flow it is a routing error.
fn static_next_targets(
    node: &Node,
    current: &str,
    kind: &str,
    branch_mode: bool,
) -> Result<Vec<String>> {
    if node.next.is_none() && branch_mode {
        return Ok(Vec::new());
    }
    node.next
        .as_ref()
        .map(|t| t.as_slice().to_vec())
        .ok_or_else(|| anyhow!("{kind} node '{current}' has no `next` and is not an end node"))
}

fn first_next_target(node: &Node) -> Option<&str> {
    node.next
        .as_ref()
        .and_then(|t| t.as_slice().first().map(|s| s.as_str()))
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
    use super::super::types::{AgentNode, GraphSettings, NextTargets};
    use super::*;
    use indexmap::IndexMap;
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

    fn variable(name: &str, default: Option<&str>) -> AgentVariable {
        AgentVariable {
            name: name.into(),
            default: default.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn seed_variables_fills_declared_default_when_key_absent() {
        let mut state = HashMap::new();

        seed_variables(&mut state, &[variable("foo", Some("bar"))], None);

        assert_eq!(state.get("foo"), Some(&json!("bar")));
    }

    #[test]
    fn seed_variables_agent_value_wins_over_declared_default() {
        let mut state = HashMap::new();
        let agent_vars: AgentVariables =
            IndexMap::from([("foo".to_string(), "override".to_string())]);

        seed_variables(
            &mut state,
            &[variable("foo", Some("bar"))],
            Some(&agent_vars),
        );

        assert_eq!(state.get("foo"), Some(&json!("override")));
    }

    #[test]
    fn seed_variables_never_overwrites_existing_key() {
        let mut state = HashMap::from([("foo".to_string(), json!("explicit"))]);
        let agent_vars: AgentVariables =
            IndexMap::from([("foo".to_string(), "override".to_string())]);

        seed_variables(
            &mut state,
            &[variable("foo", Some("bar"))],
            Some(&agent_vars),
        );

        assert_eq!(state.get("foo"), Some(&json!("explicit")));
    }

    #[test]
    fn seed_variables_skips_variable_with_no_value_anywhere() {
        let mut state = HashMap::new();

        seed_variables(&mut state, &[variable("foo", None)], None);

        assert!(!state.contains_key("foo"));
    }

    fn agent_node(id: &str, teammates: bool) -> Node {
        Node {
            id: id.into(),
            description: String::new(),
            node_type: NodeType::Agent(AgentNode {
                agent: format!("{id}-agent"),
                prompt: "p".into(),
                state_updates: None,
                output_schema: None,
                timeout: None,
                max_attempts: 1,
                fallback: None,
                inputs: None,
                teammates,
            }),
            next: None,
        }
    }

    fn terminal_node(id: &str) -> Node {
        Node {
            id: id.into(),
            description: String::new(),
            node_type: NodeType::End(EndNode {
                output: String::new(),
                state_updates: None,
            }),
            next: None,
        }
    }

    fn graph_of(nodes: Vec<Node>) -> Graph {
        let start = nodes[0].id.clone();
        let mut map: IndexMap<String, Node> = IndexMap::new();
        for node in nodes {
            map.insert(node.id.clone(), node);
        }

        Graph {
            name: "t".into(),
            description: String::new(),
            version: "1.0".into(),
            model: None,
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            max_concurrent_jobs: None,
            can_spawn_agents: None,
            max_concurrent_agents: None,
            max_agent_depth: None,
            global_tools: Vec::new(),
            mcp_servers: Vec::new(),
            mcp_tools: None,
            skills_enabled: None,
            enabled_skills: None,
            inject_skill_instructions: None,
            skill_instructions: None,
            conversation_starters: Vec::new(),
            variables: Vec::new(),
            settings: GraphSettings::default(),
            initial_state: HashMap::new(),
            reducers: HashMap::new(),
            start,
            nodes: map,
        }
    }

    #[test]
    fn teammate_flagged_nodes_selects_only_flagged_agent_nodes() {
        let graph = graph_of(vec![
            agent_node("a", true),
            agent_node("b", true),
            agent_node("c", false),
            terminal_node("done"),
        ]);
        let frontier: HashSet<String> = ["a", "b", "c", "done"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mut flagged = teammate_flagged_nodes(&graph, &frontier);
        flagged.sort();

        assert_eq!(
            flagged,
            vec![
                ("a".to_string(), "a-agent".to_string()),
                ("b".to_string(), "b-agent".to_string()),
            ]
        );
    }

    #[test]
    fn provision_frontier_peers_two_flagged_share_one_registry() {
        let flagged = vec![
            ("a".to_string(), "worker".to_string()),
            ("b".to_string(), "worker".to_string()),
        ];

        let (registry, assignments) = provision_frontier_peers(flagged);

        let registry = registry.expect("two flagged nodes should get a registry");
        assert_eq!(assignments.len(), 2);
        let roster = registry.roster();
        assert_eq!(roster.len(), 2);

        let (id_a, inbox_a) = &assignments["a"];
        let (id_b, _) = &assignments["b"];
        assert_ne!(id_a, id_b);
        let resolved = registry.get(id_a).expect("assigned id should resolve");
        assert!(Arc::ptr_eq(&resolved, inbox_a));
        assert!(roster.iter().any(|(id, label)| id == id_a && label == "a"));
        assert!(roster.iter().any(|(id, label)| id == id_b && label == "b"));
    }

    #[test]
    fn provision_frontier_peers_single_flagged_gets_no_registry() {
        let (registry, assignments) =
            provision_frontier_peers(vec![("a".to_string(), "worker".to_string())]);

        assert!(registry.is_none());
        assert!(assignments.is_empty());
    }

    #[test]
    fn provision_frontier_peers_empty_frontier_gets_no_registry() {
        let (registry, assignments) = provision_frontier_peers(Vec::new());

        assert!(registry.is_none());
        assert!(assignments.is_empty());
    }

    #[test]
    fn static_next_targets_branch_mode_treats_missing_next_as_terminal() {
        let node = agent_node("n", false);

        let targets = static_next_targets(&node, "n", "agent", true).unwrap();

        assert!(targets.is_empty());
    }

    #[test]
    fn static_next_targets_main_flow_still_errors_without_next() {
        let node = agent_node("n", false);

        let msg = static_next_targets(&node, "n", "agent", false)
            .unwrap_err()
            .to_string();

        assert!(
            msg.contains("agent node 'n' has no `next` and is not an end node"),
            "{msg}"
        );
    }

    #[test]
    fn static_next_targets_returns_declared_targets_in_both_modes() {
        let mut node = agent_node("n", false);
        node.next = Some(NextTargets::Many(vec!["a".into(), "b".into()]));

        for branch_mode in [false, true] {
            let targets = static_next_targets(&node, "n", "agent", branch_mode).unwrap();
            assert_eq!(targets, vec!["a".to_string(), "b".to_string()]);
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::config::paths;
    use crate::config::{AppState, WorkingMode};
    #[cfg(unix)]
    use crate::function::jobs::RingBuf;
    #[cfg(unix)]
    use crate::supervisor::{JobHandle, JobResult, JobState, JobStatus, Supervisor, notification};
    use crate::utils::{create_abort_signal, get_env_name, temp_file};
    use parking_lot::Mutex;
    use serial_test::serial;
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::mem;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn cmd_available(name: &str) -> bool {
        which::which(name).is_ok()
    }

    struct TestWorkspace {
        dir: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let dir = temp_file("-graph-integration-", "");
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn write_script(&self, name: &str, contents: &str) {
            fs::write(self.dir.join(name), contents).unwrap();
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn make_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    #[tokio::test]
    async fn static_fan_out_merges_branch_writes_via_append_reducer() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_script("dispatcher.sh", "#!/bin/bash\necho '{}'\n");
        ws.write_script(
            "worker_a.sh",
            "#!/bin/bash\necho '{\"results\": \"alpha\"}'\n",
        );
        ws.write_script(
            "worker_b.sh",
            "#!/bin/bash\necho '{\"results\": \"beta\"}'\n",
        );

        let yaml = r#"
name: static_fan_out_test
start: dispatcher
reducers:
  results: append
nodes:
  dispatcher:
    type: script
    script: dispatcher.sh
    state_updates: {}
    next: [worker_a, worker_b]
  worker_a:
    type: script
    script: worker_a.sh
    state_updates: {}
    next: join
  worker_b:
    type: script
    script: worker_b.sh
    state_updates: {}
    next: join
  join:
    type: end
    output: "{{results}}"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        let mut ctx = make_ctx();
        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await
            .unwrap_or_else(|e| panic!("executor failed: {e:#}"));

        let parsed: Value = serde_json::from_str(&result)
            .unwrap_or_else(|_| panic!("expected JSON array, got: {result}"));
        let arr = parsed.as_array().expect("results should be an array");
        assert_eq!(arr.len(), 2, "expected 2 elements, got: {result}");
        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert!(strs.contains(&"alpha"), "missing 'alpha' in {strs:?}");
        assert!(strs.contains(&"beta"), "missing 'beta' in {strs:?}");
    }

    /// Two parallel agent branches whose `state_updates` only read `{{output}}`
    /// leave the key as they found it (absent), so the join sees no write to
    /// `output` from either side and needs no reducer for it.
    #[tokio::test]
    #[serial]
    async fn parallel_state_updates_branches_do_not_contend_on_output() {
        if !cmd_available("bash") || !cmd_available("python3") {
            eprintln!("skipping: bash or python3 not available");
            return;
        }
        let _guard = TestConfigDirGuard::new();
        materialize_probe_agent(0.0);
        let ws = TestWorkspace::new();
        ws.write_script("dispatcher.sh", "#!/bin/bash\necho '{}'\n");

        let yaml = format!(
            r#"
name: t
settings:
  validate_before_run: false
start: dispatcher
nodes:
  dispatcher:
    type: script
    script: dispatcher.sh
    state_updates: {{}}
    next: [a, b]
  a:
    type: agent
    agent: {PROBE_AGENT}
    prompt: "p"
    state_updates:
      note_a: "{{{{output}}}}"
    next: join
  b:
    type: agent
    agent: {PROBE_AGENT}
    prompt: "p"
    state_updates:
      note_b: "{{{{output}}}}"
    next: join
  join:
    type: end
    output: "{{{{note_a}}}}|{{{{note_b}}}}"
"#
        );
        let graph: Graph = serde_yaml::from_str(&yaml).unwrap();
        let mut ctx = make_ctx();
        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await
            .unwrap_or_else(|e| panic!("executor failed: {e:#}"));

        assert_eq!(result, "held|held");
    }

    /// Declared variables are seeded into state at run start, so the agent
    /// node's strict prompt interpolation of `{{foo}}` succeeds instead of
    /// bailing with "not found in state". `ctx.agent` is None here, so each
    /// variable resolves to its declared default — except `kept`, whose
    /// explicit `initial_state` entry wins over seeding.
    #[tokio::test]
    #[serial]
    async fn declared_variables_are_seeded_into_state() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let _guard = TestConfigDirGuard::new();
        materialize_probe_agent(0.0);
        let ws = TestWorkspace::new();

        let yaml = format!(
            r#"
name: variable_seeding_test
settings:
  validate_before_run: false
start: worker
variables:
  - name: foo
    description: seeded from its default
    default: bar
  - name: kept
    description: shadowed by initial_state
    default: from-default
initial_state:
  kept: explicit
nodes:
  worker:
    type: agent
    agent: {PROBE_AGENT}
    prompt: "value is {{{{foo}}}}"
    state_updates:
      note: "{{{{foo}}}}-{{{{kept}}}}"
    next: done
  done:
    type: end
    output: "{{{{note}}}}"
"#
        );
        let graph: Graph = serde_yaml::from_str(&yaml).unwrap();
        let mut ctx = make_ctx();
        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await
            .unwrap_or_else(|e| panic!("executor failed: {e:#}"));

        assert_eq!(result, "bar-explicit");
    }

    #[tokio::test]
    async fn map_over_list_collects_outputs_in_input_order() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_script(
            "doubler.py",
            r#"#!/usr/bin/env python3
import os, json
state = json.loads(os.environ.get("GRAPH_STATE", "{}"))
val = state["item"]
print(json.dumps({"output": val * 2}))
"#,
        );

        let yaml = r#"
name: map_input_order_test
start: fan_out
initial_state:
  items: [1, 2, 3, 4, 5]
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: doubler
    collect_into: doubled
    next: done
  doubler:
    type: script
    script: doubler.py
    state_updates: {}
    timeout: 60
  done:
    type: end
    output: "{{doubled}}"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        let mut ctx = make_ctx();
        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await
            .unwrap_or_else(|e| panic!("executor failed: {e:#}"));

        let parsed: Value = serde_json::from_str(&result)
            .unwrap_or_else(|_| panic!("expected JSON array, got: {result}"));
        let arr = parsed.as_array().expect("doubled should be an array");
        let nums: Vec<i64> = arr
            .iter()
            .map(|v| v.as_i64().expect("each item should be int"))
            .collect();

        assert_eq!(
            nums,
            vec![2, 4, 6, 8, 10],
            "map outputs should be in input order, not finish order"
        );
    }

    #[tokio::test]
    async fn parallel_branch_error_aborts_super_step() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_script("dispatcher.sh", "#!/bin/bash\necho '{}'\n");
        ws.write_script(
            "worker_ok.sh",
            "#!/bin/bash\necho '{\"results\": \"ok\"}'\n",
        );
        ws.write_script(
            "worker_fail.sh",
            "#!/bin/bash\necho 'simulated failure' >&2\nexit 1\n",
        );

        let yaml = r#"
name: branch_error_test
start: dispatcher
reducers:
  results: append
nodes:
  dispatcher:
    type: script
    script: dispatcher.sh
    state_updates: {}
    next: [worker_ok, worker_fail]
  worker_ok:
    type: script
    script: worker_ok.sh
    state_updates: {}
    next: join
  worker_fail:
    type: script
    script: worker_fail.sh
    state_updates: {}
    next: join
  join:
    type: end
    output: "{{results}}"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        let mut ctx = make_ctx();
        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await;

        assert!(result.is_err(), "expected branch error to propagate");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("worker_fail"),
            "error should mention failing node: {err}"
        );
    }

    /// Two flagged agent nodes share one super-step under a single permit, so
    /// the first branch retires its identity while its sibling has not even
    /// started. The observer runs inside the branch task, before that task
    /// returns, so the snapshot it takes is what a still-running sibling
    /// would see if it messaged the finished node.
    #[tokio::test]
    async fn frontier_branch_is_finished_before_super_step_join() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_script("dispatcher.sh", "#!/bin/bash\necho '{}'\n");

        let yaml = r#"
name: frontier_finish_test
start: dispatcher
settings:
  max_concurrency: 1
  validate_before_run: false
nodes:
  dispatcher:
    type: script
    script: dispatcher.sh
    state_updates: {}
    next: [worker_a, worker_b]
  worker_a:
    type: agent
    agent: no-such-agent
    prompt: "p"
    teammates: true
    next: join
  worker_b:
    type: agent
    agent: no-such-agent
    prompt: "p"
    teammates: true
    next: join
  join:
    type: end
    output: "done"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        let mut ctx = make_ctx();
        let abort = create_abort_signal();

        // Each observation: (retired peer id, every roster (label, is_finished)).
        type RosterSnapshot = Vec<(String, bool)>;
        let observations: Arc<Mutex<Vec<(String, RosterSnapshot)>>> = Arc::default();
        let sink = Arc::clone(&observations);
        let observer: FrontierObserver = Arc::new(move |registry, id| {
            let snapshot = registry
                .roster()
                .into_iter()
                .map(|(peer_id, label)| (label, registry.is_finished(&peer_id)))
                .collect();
            sink.lock().push((id.to_string(), snapshot));
        });

        let result = GraphExecutor::new(graph, &ws.dir)
            .with_frontier_observer(observer)
            .execute(&mut ctx, abort)
            .await;

        assert!(result.is_err(), "both agent branches fail at Agent::init");
        let observations = observations.lock();
        assert_eq!(
            observations.len(),
            2,
            "one observation per flagged branch: {observations:?}"
        );

        let (_, first) = &observations[0];
        let finished_first: Vec<&str> = first
            .iter()
            .filter(|(_, finished)| *finished)
            .map(|(label, _)| label.as_str())
            .collect();
        assert_eq!(
            finished_first.len(),
            1,
            "the first branch to complete is retired while its sibling is still \
             unfinished, so this read happened before the super-step join: {first:?}"
        );
        assert!(
            first.iter().all(|(label, _)| label.starts_with("worker_")),
            "{first:?}"
        );

        let (_, second) = &observations[1];
        assert!(
            second.iter().all(|(_, finished)| *finished),
            "both identities are retired once the last branch completes: {second:?}"
        );
        assert_ne!(
            observations[0].0, observations[1].0,
            "each branch retires its own identity exactly once"
        );
    }

    #[tokio::test]
    async fn multi_end_in_super_step_is_rejected() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_script("dispatcher.sh", "#!/bin/bash\necho '{}'\n");

        let yaml = r#"
name: multi_end_test
start: dispatcher
nodes:
  dispatcher:
    type: script
    script: dispatcher.sh
    state_updates: {}
    next: [end_a, end_b]
  end_a:
    type: end
    output: "from a"
  end_b:
    type: end
    output: "from b"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        let mut ctx = make_ctx();
        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await;

        assert!(result.is_err(), "expected multi-End to be rejected");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("multiple End targets"),
            "error should explain multi-End cause: {err}"
        );
        assert!(
            err.contains("end_a") && err.contains("end_b"),
            "error should list both End nodes: {err}"
        );
    }

    #[tokio::test]
    async fn graph_timeout_interrupts_in_flight_super_step() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_script("sleeper.sh", "#!/bin/bash\nsleep 10\necho '{}'\n");

        let yaml = r#"
name: inflight_timeout_test
start: sleeper
settings:
  timeout: 1
nodes:
  sleeper:
    type: script
    script: sleeper.sh
    state_updates: {}
    next: done
  done:
    type: end
    output: "done"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        let mut ctx = make_ctx();
        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await;

        assert!(result.is_err(), "expected in-flight timeout to error");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("timed out after 1s during super-step"),
            "error should report during-super-step timeout: {err}"
        );
        assert!(err.contains("sleeper"), "error should name frontier: {err}");
    }

    #[tokio::test]
    async fn settings_max_concurrency_above_semaphore_limit_errors_instead_of_panicking() {
        let ws = TestWorkspace::new();
        let yaml = r#"
name: oversized_cap_test
start: done
settings:
  validate_before_run: false
nodes:
  done:
    type: end
    output: "done"
"#;
        let mut graph: Graph = serde_yaml::from_str(yaml).unwrap();
        graph.settings.max_concurrency = Semaphore::MAX_PERMITS + 1;
        let mut ctx = make_ctx();
        let err = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, create_abort_signal())
            .await
            .expect_err("an oversized cap is an error, not a panic");
        let msg = format!("{err:#}");
        assert!(msg.contains("exceeds the runtime limit of"), "{msg}");
    }

    #[tokio::test]
    async fn graph_timeout_zero_is_unbounded() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_script("sleeper.sh", "#!/bin/bash\nsleep 1.5\necho '{}'\n");

        let yaml = r#"
name: zero_timeout_test
start: sleeper
settings:
  timeout: 0
nodes:
  sleeper:
    type: script
    script: sleeper.sh
    state_updates: {}
    next: done
  done:
    type: end
    output: "done"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        let mut ctx = make_ctx();
        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await;

        result.unwrap_or_else(|e| panic!("timeout: 0 should not bound the run: {e:#}"));
    }

    /// Bash script that bumps `n` in state and re-enters `looper` while
    /// `n < stop`; once `n == stop` it omits `_next` so the static
    /// `next: done` edge is taken.
    fn looper_script(stop: usize) -> String {
        format!(
            "#!/bin/bash\n\
             t=${{GRAPH_STATE#*'\"n\":'}}\n\
             n=${{t%%[!0-9]*}}\n\
             n=$((n + 1))\n\
             if (( n < {stop} )); then\n\
               printf '{{\"n\": %d, \"_next\": \"looper\"}}' \"$n\"\n\
             else\n\
               printf '{{\"n\": %d}}' \"$n\"\n\
             fi\n"
        )
    }

    fn loop_cap_graph(max_loop_iterations: usize) -> Graph {
        let yaml = format!(
            r#"
name: loop_cap_test
start: looper
initial_state:
  n: 0
settings:
  max_loop_iterations: {max_loop_iterations}
nodes:
  looper:
    type: script
    script: looper.sh
    next: done
  done:
    type: end
    output: "{{{{n}}}}"
"#
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[tokio::test]
    async fn max_loop_iterations_zero_is_unbounded() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        // Loop past the default cap so a fallback-to-default implementation
        // would be caught, not just a lifted small cap.
        let stop = crate::graph::DEFAULT_MAX_LOOP_ITERATIONS + 1;
        let ws = TestWorkspace::new();
        ws.write_script("looper.sh", &looper_script(stop));

        let graph = loop_cap_graph(0);
        let mut ctx = make_ctx();
        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await
            .unwrap_or_else(|e| panic!("max_loop_iterations: 0 should not cap visits: {e:#}"));

        assert_eq!(result, stop.to_string());
    }

    #[tokio::test]
    async fn max_loop_iterations_nonzero_still_bounds() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        // Always re-enter `looper`: only the cap can end this run.
        ws.write_script("looper.sh", "#!/bin/bash\necho '{\"_next\": \"looper\"}'\n");

        let graph = loop_cap_graph(2);
        let mut ctx = make_ctx();
        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await;

        assert!(result.is_err(), "expected the visit cap to abort the run");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains(
                "Node 'looper' visited 3 times (max_loop_iterations=2). Possible infinite loop."
            ),
            "error should report the visit cap: {err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_survives_graph_node_execution() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_script("noop.sh", "#!/bin/bash\necho '{}'\n");

        let yaml = r#"
name: background_job_survival_test
start: noop
nodes:
  noop:
    type: script
    script: noop.sh
    state_updates: {}
    next: done
  done:
    type: end
    output: "done"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let join_handle = rt.spawn(async {
            Ok(JobResult {
                output: Value::Null,
                exit_code: Some(0),
                output_bytes_captured: 0,
            })
        });
        mem::forget(rt);
        let handle = JobHandle {
            id: "job_bg".to_string(),
            tool: "execute_command".to_string(),
            started_at: Instant::now(),
            join_handle,
            abort_signal: create_abort_signal(),
            state: Arc::new(parking_lot::Mutex::new(JobState {
                status: JobStatus::Completed,
                pgid: None,
            })),
            output_buf: Arc::new(parking_lot::Mutex::new(RingBuf::default())),
            no_change_checks: 0,
            last_check_state: None,
        };
        let mut sup = Supervisor::new(0, 3).with_max_concurrent_jobs(4);
        sup.register(handle).unwrap();

        let mut ctx = make_ctx();
        ctx.supervisor = Some(Arc::new(parking_lot::RwLock::new(sup)));
        ctx.notification_queue.push(notification::job_notification(
            "job_bg",
            "execute_command",
            true,
        ));

        let abort = create_abort_signal();
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, abort)
            .await
            .unwrap_or_else(|e| panic!("executor failed: {e:#}"));
        assert_eq!(result, "done");

        assert!(
            ctx.supervisor.as_ref().unwrap().read().has_job("job_bg"),
            "graph execution must not touch registered job handles"
        );
        let events = ctx.notification_queue.drain();
        assert_eq!(events.len(), 1, "queued notification must survive the run");
        assert_eq!(events[0].id, "job_bg");
        assert_eq!(events[0].event, "job_completed");
    }

    struct TestConfigDirGuard {
        key: String,
        previous: Option<std::ffi::OsString>,
        path: PathBuf,
    }

    impl TestConfigDirGuard {
        fn new() -> Self {
            let key = get_env_name("config_dir");
            let previous = env::var_os(&key);
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("coyote-graph-executor-tests-{unique}"));
            fs::create_dir_all(&path).unwrap();
            unsafe {
                env::set_var(&key, &path);
            }
            Self {
                key,
                previous,
                path,
            }
        }
    }

    impl Drop for TestConfigDirGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                unsafe {
                    env::set_var(&self.key, previous);
                }
            } else {
                unsafe {
                    env::remove_var(&self.key);
                }
            }
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    const PROBE_AGENT: &str = "timeout-probe";

    /// Materializes a graph agent under the guarded config dir that
    /// `Agent::init` can load offline; its single script node sleeps for
    /// `hold_secs` and then writes `output`, so an agent node can be held
    /// in flight without any LLM.
    fn materialize_probe_agent(hold_secs: f64) {
        let graph_path = paths::agent_graph_file(PROBE_AGENT);
        let agent_dir = graph_path.parent().unwrap();
        fs::create_dir_all(agent_dir).unwrap();
        fs::write(
            agent_dir.join("hold.py"),
            format!(
                "#!/usr/bin/env python3\nimport json, time\ntime.sleep({hold_secs})\n\
                 print(json.dumps({{\"output\": \"held\"}}))\n"
            ),
        )
        .unwrap();
        fs::write(
            &graph_path,
            format!(
                r#"
name: {PROBE_AGENT}
start: hold
nodes:
  hold:
    type: script
    script: hold.py
    timeout: 30
    next: done
  done:
    type: end
    output: "{{{{output}}}}"
"#
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn task_cancel_guard_aborts_owned_tasks() {
        let tasks: Vec<tokio::task::JoinHandle<()>> = (0..3)
            .map(|_| tokio::spawn(std::future::pending::<()>()))
            .collect();

        drop(TaskCancelGuard::new(&tasks));

        for task in tasks {
            let err = task
                .await
                .expect_err("an owned task is aborted when the guard drops");
            assert!(err.is_cancelled(), "{err}");
        }
    }

    #[tokio::test]
    async fn peer_retire_guard_fires_on_task_abort() {
        let (registry, assignments) = provision_frontier_peers(vec![
            ("a".to_string(), "worker".to_string()),
            ("b".to_string(), "worker".to_string()),
        ]);
        let registry = registry.expect("two flagged nodes should get a registry");
        let id_a = assignments["a"].0.clone();
        let id_b = assignments["b"].0.clone();
        let guard = PeerRetireGuard {
            registry: Arc::clone(&registry),
            id: id_a.clone(),
            observer: None,
        };
        let task = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });

        task.abort();
        let err = task.await.expect_err("the task was aborted");

        assert!(err.is_cancelled(), "{err}");
        assert!(
            registry.is_finished(&id_a),
            "the aborted task's identity is retired by the guard"
        );
        assert!(!registry.is_finished(&id_b), "the sibling is untouched");
    }

    /// A task aborted before its first poll never runs its body, so a guard
    /// built inside the future would never exist. Built outside and moved in,
    /// it drops with the unpolled future and still retires the identity.
    #[tokio::test]
    async fn peer_retire_guard_fires_when_task_is_aborted_before_first_poll() {
        let (registry, assignments) = provision_frontier_peers(vec![
            ("a".to_string(), "worker".to_string()),
            ("b".to_string(), "worker".to_string()),
        ]);
        let registry = registry.expect("two flagged nodes should get a registry");
        let id_a = assignments["a"].0.clone();
        let id_b = assignments["b"].0.clone();

        let outside = PeerRetireGuard {
            registry: Arc::clone(&registry),
            id: id_a.clone(),
            observer: None,
        };
        let inside_registry = Arc::clone(&registry);
        let inside_id = id_b.clone();
        let task = tokio::spawn(async move {
            let _outside = outside;
            let _inside = PeerRetireGuard {
                registry: inside_registry,
                id: inside_id,
                observer: None,
            };
            std::future::pending::<()>().await;
        });
        // No await between spawn and abort: on the current-thread test
        // runtime the task has not been polled yet.
        task.abort();
        let err = task.await.expect_err("the task was aborted");

        assert!(err.is_cancelled(), "{err}");
        assert!(registry.is_finished(&id_a), "the moved-in guard retires");
        assert!(
            !registry.is_finished(&id_b),
            "a guard built inside never existed"
        );
    }

    /// A session already aborted when the run starts trips the synchronous
    /// pre-check: no bridge task, no super-step, the start script never runs.
    #[tokio::test]
    async fn graph_run_aborts_when_session_is_preset() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        let sentinel = ws.dir.join("ran");
        ws.write_script(
            "mark.sh",
            &format!("#!/bin/bash\ntouch '{}'\necho '{{}}'\n", sentinel.display()),
        );

        let yaml = r#"
name: preset_session_abort_test
start: mark
nodes:
  mark:
    type: script
    script: mark.sh
    state_updates: {}
    next: done
  done:
    type: end
    output: "done"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        let mut ctx = make_ctx();
        let session = create_abort_signal();
        session.set_ctrlc();
        ctx.session_abort = Some(session);
        let graph_abort = create_abort_signal();

        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, graph_abort.clone())
            .await;

        let err = format!(
            "{:#}",
            result.expect_err("a pre-set session aborts the run")
        );
        assert!(err.contains("aborted before super-step"), "{err}");
        assert!(
            graph_abort.aborted(),
            "the session flag is mapped onto the graph's own abort"
        );
        assert!(!sentinel.exists(), "the start script must never run");
    }

    /// The graph abort and the session abort are different signals and only
    /// the session one is set, mid-run: the bridge is what must fire, and the
    /// cancelled branch task must kill its script before the `&&` runs.
    #[tokio::test]
    async fn session_abort_mid_run_aborts_graph() {
        if !cmd_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        let sentinel = ws.dir.join("finished");
        ws.write_script(
            "hold.sh",
            &format!(
                "#!/bin/bash\nsleep 1.5 && touch '{}'\necho '{{}}'\n",
                sentinel.display()
            ),
        );

        let yaml = r#"
name: mid_run_session_abort_test
start: hold
nodes:
  hold:
    type: script
    script: hold.sh
    state_updates: {}
    next: done
  done:
    type: end
    output: "done"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        let mut ctx = make_ctx();
        let session = create_abort_signal();
        ctx.session_abort = Some(Arc::clone(&session));
        let graph_abort = create_abort_signal();
        assert!(!Arc::ptr_eq(&session, &graph_abort));

        let trigger = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            session.set_ctrlc();
        });
        let result = GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, graph_abort.clone())
            .await;
        trigger.await.unwrap();

        let err = format!("{:#}", result.expect_err("a session abort ends the run"));
        assert!(err.contains("aborted"), "{err}");
        assert!(graph_abort.aborted(), "the bridge set the graph's abort");

        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !sentinel.exists(),
            "the cancelled branch's script was killed before its `&&` ran"
        );
    }

    /// Two flagged agent nodes under one permit: the graph abort lands while
    /// the first is holding its probe agent open and the second is still
    /// waiting on the semaphore. Both tasks are cancelled by the abort, and
    /// each guard retires its identity as the task unwinds.
    #[tokio::test]
    #[serial]
    async fn aborted_frontier_agents_are_retired() {
        if !cmd_available("bash") || !cmd_available("python3") {
            eprintln!("skipping: bash or python3 not available");
            return;
        }
        let _guard = TestConfigDirGuard::new();
        materialize_probe_agent(5.0);
        let ws = TestWorkspace::new();
        // The abort must land after the worker tasks exist (their retirement
        // guards are created at spawn), i.e. after the dispatcher super-step
        // has run. A fixed delay races bash start-up on slow runners, so the
        // dispatcher leaves a marker and the trigger waits for it.
        let dispatched = ws.dir.join("dispatched");
        ws.write_script(
            "dispatcher.sh",
            &format!(
                "#!/bin/bash\necho '{{}}'\ntouch '{}'\n",
                dispatched.display()
            ),
        );

        let yaml = format!(
            r#"
name: aborted_frontier_test
start: dispatcher
settings:
  max_concurrency: 1
  validate_before_run: false
nodes:
  dispatcher:
    type: script
    script: dispatcher.sh
    state_updates: {{}}
    next: [worker_a, worker_b]
  worker_a:
    type: agent
    agent: {PROBE_AGENT}
    prompt: "p"
    teammates: true
    next: join
  worker_b:
    type: agent
    agent: {PROBE_AGENT}
    prompt: "p"
    teammates: true
    next: join
  join:
    type: end
    output: "done"
"#
        );
        let graph: Graph = serde_yaml::from_str(&yaml).unwrap();
        let mut ctx = make_ctx();
        let abort = create_abort_signal();

        // Each observation: (retired peer id, every roster (peer id, label, is_finished)).
        type RosterSnapshot = Vec<(String, String, bool)>;
        let observations: Arc<Mutex<Vec<(String, RosterSnapshot)>>> = Arc::default();
        let sink = Arc::clone(&observations);
        let observer: FrontierObserver = Arc::new(move |registry, id| {
            let snapshot = registry
                .roster()
                .into_iter()
                .map(|(peer_id, label)| {
                    let finished = registry.is_finished(&peer_id);
                    (peer_id, label, finished)
                })
                .collect();
            sink.lock().push((id.to_string(), snapshot));
        });

        let trigger = {
            let abort = abort.clone();
            let dispatched = dispatched.clone();
            tokio::spawn(async move {
                while !dispatched.exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                abort.set_ctrlc();
                Instant::now()
            })
        };
        let result = GraphExecutor::new(graph, &ws.dir)
            .with_frontier_observer(observer)
            .execute(&mut ctx, abort)
            .await;
        let run_ended = Instant::now();
        let abort_at = trigger.await.unwrap();

        let err = format!("{:#}", result.expect_err("the abort ends the run"));
        assert!(err.contains("aborted"), "{err}");
        // Measured from when the abort lands, not from execute() start:
        // dispatcher bash spawn latency on a loaded runner (notably Windows
        // CI) can eat several seconds before the marker even exists, and
        // that time is not this test's to budget. Joining the in-flight
        // hold instead of unwinding would still show >= ~4.5s here.
        let unwind = run_ended.saturating_duration_since(abort_at);
        assert!(
            unwind < Duration::from_secs(3),
            "the run must not wait for the 5s probe hold after the abort lands: {unwind:?}"
        );

        // Aborted tasks unwind on the runtime's next pass, not inside execute().
        tokio::time::sleep(Duration::from_millis(300)).await;
        let observations = observations.lock();
        assert_eq!(
            observations.len(),
            2,
            "each cancelled branch retires its identity exactly once: {observations:?}"
        );
        for (retired, snapshot) in observations.iter() {
            assert!(
                snapshot
                    .iter()
                    .any(|(peer_id, _, finished)| peer_id == retired && *finished),
                "the retired id reads as finished in its own observation: {observations:?}"
            );
            assert!(
                snapshot
                    .iter()
                    .all(|(_, label, _)| label.starts_with("worker_")),
                "{snapshot:?}"
            );
        }
        assert_ne!(observations[0].0, observations[1].0);
        let (_, last) = &observations[1];
        assert!(
            last.iter().all(|(_, _, finished)| *finished),
            "both identities are retired once the last task unwinds: {last:?}"
        );
    }
}
