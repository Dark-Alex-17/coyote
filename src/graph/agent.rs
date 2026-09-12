use super::is_transient_error;
use super::state::StateManager;
use super::state_updates;
use super::structured;
use super::types::AgentNode;
use super::wall_clock;
use crate::config::RequestContext;
use crate::function::agents::run_agent_for_graph;
use crate::supervisor::mailbox::{Inbox, PeerRegistry};
use anyhow::{Context, Error, Result, anyhow};
use log::{debug, warn};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::time::timeout;

const DEFAULT_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AgentExecutionOutcome {
    /// Carries the raw agent output for tests; the executor's agent arm
    /// ignores the payload.
    Continue(String),
    FellBack(String),
}

pub struct AgentNodeExecutor;

impl AgentNodeExecutor {
    pub(super) async fn execute(
        node_id: &str,
        node: &AgentNode,
        state_manager: &mut StateManager,
        parent_ctx: &mut RequestContext,
        retire_peer_on_return: bool,
    ) -> Result<AgentExecutionOutcome> {
        let result = run(
            node_id,
            node,
            state_manager,
            parent_ctx,
            retire_peer_on_return,
        )
        .await;
        outcome_from(node_id, node, state_manager, result)
    }
}

/// Turns the node run's final result into a routing outcome. With a
/// `fallback` declared, any failure — retries exhausted, hard error, or
/// extraction failure — is written into state as
/// `"Agent node failed: <chain>"` and routes to the fallback. Only the
/// node's `state_updates` can capture that string (bound as `{{output}}`
/// during interpolation); the output-schema auto-merge never applies to it,
/// because it only merges object values and the failure is a plain string.
/// Without a fallback the error propagates unchanged; unlike llm nodes
/// there is no teaching bail, because agent nodes always failed loudly and
/// callers assert on the existing "Agent 'X' failed" chains.
fn outcome_from(
    node_id: &str,
    node: &AgentNode,
    state_manager: &mut StateManager,
    result: Result<String>,
) -> Result<AgentExecutionOutcome> {
    match result {
        Ok(raw) => Ok(AgentExecutionOutcome::Continue(raw)),
        Err(e) => match &node.fallback {
            Some(fb) => {
                warn!("agent node '{node_id}' failed, routing to fallback '{fb}': {e:#}");
                state_updates::apply(
                    state_manager,
                    &Value::String(format!("Agent node failed: {e:#}")),
                    node.output_schema.is_some(),
                    node.state_updates.as_ref(),
                );
                Ok(AgentExecutionOutcome::FellBack(fb.clone()))
            }
            None => Err(e),
        },
    }
}

async fn run(
    node_id: &str,
    node: &AgentNode,
    state_manager: &mut StateManager,
    parent_ctx: &mut RequestContext,
    retire_peer_on_return: bool,
) -> Result<String> {
    let prompt = state_manager
        .interpolate(&node.prompt)
        .with_context(|| format!("Failed to interpolate prompt for agent '{}'", node.agent))?;

    let graph_inputs = resolve_inputs(node, state_manager)?;
    if let Some(inputs) = &graph_inputs {
        let mut keys: Vec<&String> = inputs.keys().collect();
        keys.sort_unstable();
        debug!("Agent '{}' graph inputs: {keys:?}", node.agent);
    }

    let secs = node.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let agent_name = node.agent.clone();

    let mut run_attempt = attempt_runner(move |ctx| {
        let agent_name = agent_name.clone();
        let prompt = prompt.clone();
        let graph_inputs = graph_inputs.clone();
        boxed_attempt(async move {
            bounded_attempt(
                &agent_name,
                secs,
                run_agent_for_graph(ctx, &agent_name, &prompt, graph_inputs),
            )
            .await
        })
    });
    attempt_and_extract(
        node_id,
        node,
        state_manager,
        parent_ctx,
        retire_peer_on_return,
        &mut run_attempt,
    )
    .await
}

/// Drives the retry loop, then extraction, then state updates. Extraction
/// stays outside the retry loop: by the time it runs the agent itself
/// succeeded, so an extraction failure is never retried.
async fn attempt_and_extract(
    node_id: &str,
    node: &AgentNode,
    state_manager: &mut StateManager,
    parent_ctx: &mut RequestContext,
    retire_peer_on_return: bool,
    run_attempt: &mut AttemptRunner<'_>,
) -> Result<String> {
    let raw = run_with_retries(
        node_id,
        node,
        parent_ctx,
        retire_peer_on_return,
        run_attempt,
    )
    .await?;

    let output_value = match &node.output_schema {
        Some(schema) => structured::extract(&raw, schema, parent_ctx)
            .await
            .with_context(|| {
                format!(
                    "Agent '{}' output failed structured-output extraction",
                    node.agent
                )
            })?,
        None => Value::String(raw.clone()),
    };

    apply_state_updates(node, state_manager, &output_value);

    Ok(raw)
}

/// One boxed attempt against the ctx. Boxed (rather than an opaque
/// `AsyncFnMut` future) because agent nodes run inside `tokio::spawn`ed map
/// branches, where higher-ranked opaque futures trip the `Send` auto-trait
/// solver ("implementation of `Send` is not general enough").
type AttemptFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// The per-attempt runner the retry loop drives. The attempt future may only
/// borrow the ctx it is handed, so runners move owned clones of everything
/// else into the future.
type AttemptRunner<'f> = dyn for<'a> FnMut(&'a mut RequestContext) -> AttemptFuture<'a> + Send + 'f;

/// Identity funnel that pins a closure to the runner's higher-ranked
/// signature, so inline closures at call sites infer the right lifetimes.
fn attempt_runner<F>(f: F) -> F
where
    F: for<'a> FnMut(&'a mut RequestContext) -> AttemptFuture<'a> + Send,
{
    f
}

fn boxed_attempt<'a>(fut: impl Future<Output = Result<String>> + Send + 'a) -> AttemptFuture<'a> {
    Box::pin(fut)
}

/// Runs the node's agent up to `node.max_attempts` times, retrying only
/// failures that `is_transient_error` recognizes.
///
/// `run_agent_for_graph` takes the peer identity off the ctx and never
/// restores it, so BOTH halves — the (id, inbox) assignment and the registry
/// — are captured up front and re-armed before every retry; re-arming only
/// the assignment would give a retried attempt its identity back but no
/// roster and a broken `agent__send_message`. A frontier peer is retired
/// exactly once, after the final attempt, the moment the agent stops — not
/// after extraction. Chains re-arm the same identity for later steps and
/// leave retirement to the chain runner.
async fn run_with_retries(
    node_id: &str,
    node: &AgentNode,
    parent_ctx: &mut RequestContext,
    retire_peer_on_return: bool,
    run_attempt: &mut AttemptRunner<'_>,
) -> Result<String> {
    let assignment = parent_ctx.peer_assignment.clone();
    let registry = parent_ctx.peer_registry.clone();

    let result = retry_transient(
        node_id,
        node,
        parent_ctx,
        &assignment,
        &registry,
        run_attempt,
    )
    .await;

    if retire_peer_on_return && let (Some(registry), Some((id, _))) = (&registry, &assignment) {
        registry.mark_finished(id);
    }

    result
}

async fn retry_transient(
    node_id: &str,
    node: &AgentNode,
    parent_ctx: &mut RequestContext,
    assignment: &Option<(String, Arc<Inbox>)>,
    registry: &Option<Arc<PeerRegistry>>,
    run_attempt: &mut AttemptRunner<'_>,
) -> Result<String> {
    let mut last_err: Option<Error> = None;
    for attempt in 1..=node.max_attempts {
        if attempt > 1 {
            if let Some(assignment) = assignment {
                parent_ctx.peer_assignment = Some(assignment.clone());
            }
            if let Some(registry) = registry {
                parent_ctx.peer_registry = Some(Arc::clone(registry));
            }
        }
        match run_attempt(parent_ctx).await {
            Ok(out) => return Ok(out),
            Err(e) if is_transient_error(&e) && attempt < node.max_attempts => {
                warn!(
                    "agent node '{node_id}' attempt {attempt} failed (transient): {e:#}; retrying"
                );
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("agent node exhausted retries")))
}

/// Applies the wall-clock bound and the human-readable failure contexts to a
/// single attempt. The contexts land here — before the caller inspects the
/// error — because `tokio::time::error::Elapsed` renders as "deadline has
/// elapsed", which the transient matcher would not recognize as a timeout.
async fn bounded_attempt(
    agent_name: &str,
    secs: u64,
    fut: impl Future<Output = Result<String>>,
) -> Result<String> {
    let raw_result = match wall_clock(secs) {
        Some(d) => timeout(d, fut).await,
        None => Ok(fut.await),
    };
    raw_result
        .with_context(|| format!("Agent '{agent_name}' timed out after {secs}s"))?
        .with_context(|| format!("Agent '{agent_name}' failed"))
}

/// Resolves each `inputs` template against the parent state. A lone
/// `{{key}}` yields the state value as-is (numbers, arrays, objects and
/// `null` all survive); anything else renders to a string. Every key is
/// strict: a missing reference fails the node before the child starts.
fn resolve_inputs(
    node: &AgentNode,
    state_manager: &StateManager,
) -> Result<Option<HashMap<String, Value>>> {
    let Some(inputs) = &node.inputs else {
        return Ok(None);
    };
    let mut resolved = HashMap::with_capacity(inputs.len());
    for (key, template) in inputs {
        let value = state_manager.interpolate_raw(template).with_context(|| {
            format!(
                "Failed to interpolate inputs.{key} for agent '{}'",
                node.agent
            )
        })?;
        resolved.insert(key.clone(), value);
    }
    Ok(Some(resolved))
}

fn apply_state_updates(node: &AgentNode, state_manager: &mut StateManager, output: &Value) {
    state_updates::apply(
        state_manager,
        output,
        node.output_schema.is_some(),
        node.state_updates.as_ref(),
    );
}

#[cfg(test)]
mod tests {
    use super::super::types::AgentNode;
    use super::*;
    use crate::config::{AppState, WorkingMode, default_max_agent_depth};
    use crate::supervisor::mailbox::{Inbox, PeerRegistry, graph_agent_id};
    use crate::testing::{install_warn_collector, warn_messages};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn manager_with(pairs: &[(&str, Value)]) -> StateManager {
        let mut map = HashMap::new();
        for (k, v) in pairs {
            map.insert((*k).into(), v.clone());
        }
        StateManager::new(map)
    }

    fn node_with(prompt: &str, updates: Option<HashMap<String, String>>) -> AgentNode {
        AgentNode {
            agent: "test_agent".into(),
            prompt: prompt.into(),
            state_updates: updates,
            output_schema: None,
            timeout: None,
            max_attempts: 1,
            fallback: None,
            inputs: None,
            teammates: false,
        }
    }

    fn ctx_at_max_depth_with_peer() -> (RequestContext, Arc<PeerRegistry>, String) {
        let registry = Arc::new(PeerRegistry::new());
        let id = graph_agent_id("test_agent");
        let inbox = Arc::new(Inbox::new());
        registry.insert(id.clone(), "worker[0]".into(), Arc::clone(&inbox));
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.current_depth = default_max_agent_depth();
        ctx.peer_registry = Some(Arc::clone(&registry));
        ctx.peer_assignment = Some((id.clone(), inbox));
        (ctx, registry, id)
    }

    async fn execute_past_max_depth(ctx: &mut RequestContext, retire_peer_on_return: bool) {
        let mut node = node_with("hi", None);
        node.teammates = true;
        let mut state = manager_with(&[]);

        let err =
            AgentNodeExecutor::execute("test_node", &node, &mut state, ctx, retire_peer_on_return)
                .await
                .expect_err("agent past max depth should fail before running");

        let chain = format!("{err:#}");
        assert!(chain.contains("Agent 'test_agent' failed"), "{chain}");
        assert!(chain.contains("Max agent depth exceeded"), "{chain}");
    }

    #[tokio::test]
    async fn execute_retires_peer_identity_on_frontier_when_agent_fails() {
        let (mut ctx, registry, id) = ctx_at_max_depth_with_peer();

        execute_past_max_depth(&mut ctx, true).await;

        assert!(registry.is_finished(&id));
    }

    #[tokio::test]
    async fn execute_leaves_peer_identity_live_in_branch_mode_when_agent_fails() {
        let (mut ctx, registry, id) = ctx_at_max_depth_with_peer();

        execute_past_max_depth(&mut ctx, false).await;

        assert!(!registry.is_finished(&id));
    }

    fn plain_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    fn retryable_node(max_attempts: u32) -> AgentNode {
        let mut node = node_with("hi", None);
        node.max_attempts = max_attempts;
        node
    }

    #[tokio::test]
    async fn run_with_retries_retries_transient_failure_and_succeeds() {
        let mut ctx = plain_ctx();
        let node = retryable_node(2);
        let mut attempts = 0u32;

        let out = run_with_retries(
            "test_node",
            &node,
            &mut ctx,
            false,
            &mut attempt_runner(|_ctx| {
                attempts += 1;
                let result = if attempts == 1 {
                    Err(anyhow!("error sending request for url"))
                } else {
                    Ok("recovered".to_string())
                };
                boxed_attempt(async move { result })
            }),
        )
        .await
        .unwrap();

        assert_eq!(out, "recovered");
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn run_with_retries_does_not_retry_non_transient_failures() {
        let mut ctx = plain_ctx();
        let node = retryable_node(3);
        let mut attempts = 0u32;

        let err = run_with_retries(
            "test_node",
            &node,
            &mut ctx,
            false,
            &mut attempt_runner(|_ctx| {
                attempts += 1;
                boxed_attempt(async move { Err::<String, _>(anyhow!("Unknown model 'foo'")) })
            }),
        )
        .await
        .expect_err("non-transient failure must propagate immediately");

        assert_eq!(attempts, 1);
        assert!(format!("{err:#}").contains("Unknown model 'foo'"));
    }

    #[tokio::test]
    async fn run_with_retries_stops_at_max_attempts_and_keeps_failure_context() {
        let mut ctx = plain_ctx();
        let node = retryable_node(2);
        let mut attempts = 0u32;

        let err = run_with_retries(
            "test_node",
            &node,
            &mut ctx,
            false,
            &mut attempt_runner(|_ctx| {
                attempts += 1;
                boxed_attempt(bounded_attempt("test_agent", 0, async {
                    Err(anyhow!("connection error: unexpected end of stream"))
                }))
            }),
        )
        .await
        .expect_err("exhausted retries must propagate the last error");

        assert_eq!(attempts, 2);
        let chain = format!("{err:#}");
        assert!(chain.contains("Agent 'test_agent' failed"), "{chain}");
        assert!(chain.contains("connection error"), "{chain}");
    }

    #[tokio::test(start_paused = true)]
    async fn run_with_retries_retries_a_timeout_and_succeeds() {
        let mut ctx = plain_ctx();
        let node = retryable_node(2);
        let mut attempts = 0u32;

        let out = run_with_retries(
            "test_node",
            &node,
            &mut ctx,
            false,
            &mut attempt_runner(|_ctx| {
                attempts += 1;
                if attempts == 1 {
                    boxed_attempt(bounded_attempt(
                        "test_agent",
                        1,
                        std::future::pending::<Result<String>>(),
                    ))
                } else {
                    boxed_attempt(async move { Ok("recovered".to_string()) })
                }
            }),
        )
        .await
        .unwrap();

        assert_eq!(out, "recovered");
        assert_eq!(attempts, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_attempt_timeout_context_is_transient() {
        let err = bounded_attempt("test_agent", 1, std::future::pending::<Result<String>>())
            .await
            .expect_err("pending future must hit the wall clock");

        let chain = format!("{err:#}");
        assert!(
            chain.contains("Agent 'test_agent' timed out after 1s"),
            "{chain}"
        );
        assert!(is_transient_error(&err));
    }

    #[tokio::test]
    async fn run_with_retries_rearms_the_same_peer_identity_for_each_retry() {
        let (mut ctx, registry, id) = ctx_at_max_depth_with_peer();
        let expected_inbox = Arc::clone(&ctx.peer_assignment.as_ref().unwrap().1);
        let node = retryable_node(2);
        let mut attempts = 0u32;
        let mut observations: Vec<(String, bool, bool)> = Vec::new();

        let out = run_with_retries(
            "test_node",
            &node,
            &mut ctx,
            true,
            &mut attempt_runner(|ctx| {
                attempts += 1;
                // Mirror run_agent_for_graph: consume both identity halves.
                let (peer_id, inbox) = ctx.peer_assignment.take().expect("assignment armed");
                let reg = ctx.peer_registry.take().expect("registry armed");
                observations.push((
                    peer_id,
                    Arc::ptr_eq(&inbox, &expected_inbox),
                    Arc::ptr_eq(&reg, &registry),
                ));
                assert!(
                    !registry.is_finished(&id),
                    "peer must not be retired between attempts"
                );
                let result = if attempts == 1 {
                    Err(anyhow!("stream closed because of a broken pipe"))
                } else {
                    Ok("done".to_string())
                };
                boxed_attempt(async move { result })
            }),
        )
        .await
        .unwrap();

        assert_eq!(out, "done");
        assert_eq!(observations.len(), 2);
        for (peer_id, same_inbox, same_registry) in &observations {
            assert_eq!(peer_id, &id);
            assert!(same_inbox, "retry must reuse the same inbox Arc");
            assert!(same_registry, "retry must reuse the same registry Arc");
        }
        assert!(registry.is_finished(&id));
    }

    #[tokio::test]
    async fn run_with_retries_retires_frontier_peer_after_the_final_failed_attempt() {
        let (mut ctx, registry, id) = ctx_at_max_depth_with_peer();
        let node = retryable_node(2);
        let mut attempts = 0u32;

        let err = run_with_retries(
            "test_node",
            &node,
            &mut ctx,
            true,
            &mut attempt_runner(|ctx| {
                attempts += 1;
                ctx.peer_assignment.take();
                ctx.peer_registry.take();
                assert!(
                    !registry.is_finished(&id),
                    "peer must not be retired between attempts"
                );
                boxed_attempt(async move { Err::<String, _>(anyhow!("Connection reset by peer")) })
            }),
        )
        .await
        .expect_err("both attempts fail");

        assert_eq!(attempts, 2);
        assert!(registry.is_finished(&id));
        assert!(format!("{err:#}").contains("Connection reset"));
    }

    #[tokio::test]
    async fn run_with_retries_leaves_peer_live_when_not_retiring() {
        let (mut ctx, registry, id) = ctx_at_max_depth_with_peer();
        let node = retryable_node(1);

        run_with_retries(
            "test_node",
            &node,
            &mut ctx,
            false,
            &mut attempt_runner(|ctx| {
                ctx.peer_assignment.take();
                ctx.peer_registry.take();
                boxed_attempt(async move { Ok("done".to_string()) })
            }),
        )
        .await
        .unwrap();

        assert!(!registry.is_finished(&id));
    }

    #[tokio::test]
    async fn run_with_retries_zero_attempts_reports_exhausted_retries() {
        let mut ctx = plain_ctx();
        let node = retryable_node(0);
        let mut attempts = 0u32;

        let err = run_with_retries(
            "test_node",
            &node,
            &mut ctx,
            false,
            &mut attempt_runner(|_ctx| {
                attempts += 1;
                boxed_attempt(async move { Ok("never".to_string()) })
            }),
        )
        .await
        .expect_err("zero attempts cannot succeed");

        assert_eq!(attempts, 0);
        assert!(format!("{err:#}").contains("agent node exhausted retries"));
    }

    #[tokio::test]
    async fn retry_transient_warns_once_per_retried_attempt_with_the_context_chain() {
        install_warn_collector();
        let mut ctx = plain_ctx();
        let node = retryable_node(3);
        let mut attempts = 0u32;

        run_with_retries(
            "warn_capture",
            &node,
            &mut ctx,
            false,
            &mut attempt_runner(|_ctx| {
                attempts += 1;
                boxed_attempt(async move {
                    Err::<String, _>(
                        anyhow!("connection error: warn-capture reset")
                            .context("Agent 'test_agent' failed"),
                    )
                })
            }),
        )
        .await
        .expect_err("all attempts fail");

        assert_eq!(attempts, 3);
        let warns: Vec<String> = warn_messages()
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.contains("'warn_capture'"))
            .cloned()
            .collect();
        assert_eq!(
            warns.len(),
            2,
            "one warn per retried attempt (the final attempt propagates instead): {warns:?}"
        );
        for (i, warn) in warns.iter().enumerate() {
            let attempt = i + 1;
            assert!(
                warn.contains(&format!(
                    "agent node 'warn_capture' attempt {attempt} failed (transient)"
                )),
                "{warn}"
            );
            assert!(
                warn.contains("Agent 'test_agent' failed: connection error: warn-capture reset"),
                "warn must carry the full context chain: {warn}"
            );
        }
    }

    #[tokio::test]
    async fn extraction_failure_after_a_successful_run_is_never_retried() {
        let mut ctx = plain_ctx();
        let mut node = retryable_node(2);
        node.output_schema = Some(json!({"type": "object"}));
        let mut state = manager_with(&[]);
        let mut attempts = 0u32;

        let err = attempt_and_extract(
            "test_node",
            &node,
            &mut state,
            &mut ctx,
            false,
            &mut attempt_runner(|_ctx| {
                attempts += 1;
                boxed_attempt(async move { Ok("not json".to_string()) })
            }),
        )
        .await
        .expect_err("extraction must fail without an extractor model");

        assert_eq!(
            attempts, 1,
            "an extraction failure must not re-run the agent"
        );
        let chain = format!("{err:#}");
        assert!(
            chain.contains("output failed structured-output extraction"),
            "{chain}"
        );
    }

    fn failure_capture_updates() -> HashMap<String, String> {
        let mut u = HashMap::new();
        u.insert("failure".into(), "{{output}}".into());
        u
    }

    #[tokio::test]
    async fn execute_failure_with_fallback_routes_and_records_the_failure() {
        let mut ctx = plain_ctx();
        ctx.current_depth = default_max_agent_depth();
        let mut node = node_with("hi", Some(failure_capture_updates()));
        node.fallback = Some("recover".into());
        let mut state = manager_with(&[]);

        let outcome = AgentNodeExecutor::execute("test_node", &node, &mut state, &mut ctx, false)
            .await
            .unwrap();

        assert_eq!(outcome, AgentExecutionOutcome::FellBack("recover".into()));
        let failure = state
            .state()
            .get("failure")
            .and_then(Value::as_str)
            .expect("failure text must land in state for the fallback node")
            .to_string();
        assert!(failure.starts_with("Agent node failed: "), "{failure}");
        assert!(failure.contains("Agent 'test_agent' failed"), "{failure}");
        assert!(failure.contains("Max agent depth exceeded"), "{failure}");
    }

    fn extraction_error() -> Error {
        anyhow!("no JSON object found in output")
            .context("Agent 'test_agent' output failed structured-output extraction")
    }

    #[test]
    fn outcome_from_success_is_continue() {
        let node = node_with("hi", None);
        let mut state = manager_with(&[]);

        let outcome = outcome_from("test_node", &node, &mut state, Ok("raw".into())).unwrap();

        assert_eq!(outcome, AgentExecutionOutcome::Continue("raw".into()));
    }

    #[test]
    fn outcome_from_extraction_failure_with_fallback_falls_back() {
        let mut node = node_with("hi", Some(failure_capture_updates()));
        node.fallback = Some("recover".into());
        let mut state = manager_with(&[]);

        let outcome =
            outcome_from("test_node", &node, &mut state, Err(extraction_error())).unwrap();

        assert_eq!(outcome, AgentExecutionOutcome::FellBack("recover".into()));
        let failure = state
            .state()
            .get("failure")
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            failure.contains("structured-output extraction"),
            "{failure}"
        );
        assert!(failure.contains("no JSON object found"), "{failure}");
    }

    #[test]
    fn outcome_from_failure_without_fallback_propagates_the_error_unchanged() {
        let node = node_with("hi", Some(failure_capture_updates()));
        let mut state = manager_with(&[]);

        let err = outcome_from("test_node", &node, &mut state, Err(extraction_error()))
            .expect_err("no fallback must propagate");

        assert_eq!(
            format!("{err:#}"),
            "Agent 'test_agent' output failed structured-output extraction: \
             no JSON object found in output"
        );
        assert!(state.state().get("failure").is_none());
    }

    #[tokio::test]
    async fn fallback_engages_only_after_transient_retries_exhaust() {
        let mut ctx = plain_ctx();
        let mut node = retryable_node(2);
        node.fallback = Some("recover".into());
        let mut attempts = 0u32;

        let result = run_with_retries(
            "test_node",
            &node,
            &mut ctx,
            false,
            &mut attempt_runner(|_ctx| {
                attempts += 1;
                boxed_attempt(async move {
                    Err::<String, _>(anyhow!("connection error: reset by peer"))
                })
            }),
        )
        .await;
        let mut state = manager_with(&[]);
        let outcome = outcome_from("test_node", &node, &mut state, result).unwrap();

        assert_eq!(attempts, 2, "fallback must not preempt the retry budget");
        assert_eq!(outcome, AgentExecutionOutcome::FellBack("recover".into()));
    }

    fn node_with_inputs(pairs: &[(&str, &str)]) -> AgentNode {
        let mut node = node_with("hi", None);
        node.inputs = Some(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        );
        node
    }

    #[test]
    fn resolve_inputs_is_none_when_node_has_no_inputs() {
        let node = node_with("hi", None);
        let state = manager_with(&[("n", json!(3))]);

        assert_eq!(resolve_inputs(&node, &state).unwrap(), None);
    }

    #[test]
    fn resolve_inputs_lone_reference_keeps_the_array_value() {
        let node = node_with_inputs(&[("items", "{{list}}")]);
        let state = manager_with(&[("list", json!(["a", "b"]))]);

        let inputs = resolve_inputs(&node, &state).unwrap().unwrap();

        assert_eq!(inputs.get("items"), Some(&json!(["a", "b"])));
    }

    #[test]
    fn resolve_inputs_mixed_text_renders_to_a_string() {
        let node = node_with_inputs(&[("label", "n={{n}}")]);
        let state = manager_with(&[("n", json!(3))]);

        let inputs = resolve_inputs(&node, &state).unwrap().unwrap();

        assert_eq!(inputs.get("label"), Some(&json!("n=3")));
    }

    #[test]
    fn resolve_inputs_lone_reference_to_null_yields_null() {
        let node = node_with_inputs(&[("x", "{{k}}")]);
        let state = manager_with(&[("k", Value::Null)]);

        let inputs = resolve_inputs(&node, &state).unwrap().unwrap();

        assert_eq!(inputs.get("x"), Some(&Value::Null));
    }

    #[test]
    fn resolve_inputs_missing_key_errors_naming_the_input() {
        let node = node_with_inputs(&[("width", "{{nope}}")]);
        let state = manager_with(&[]);

        let err = resolve_inputs(&node, &state).expect_err("missing key must fail");

        let chain = format!("{err:#}");
        assert!(chain.contains("inputs.width"), "{chain}");
        assert!(chain.contains("for agent 'test_agent'"), "{chain}");
        assert!(chain.contains("'nope' not found in state"), "{chain}");
    }

    #[test]
    fn state_updates_use_output_placeholder() {
        let node = {
            let mut u = HashMap::new();
            u.insert("findings".into(), "{{output}}".into());
            node_with("hi", Some(u))
        };
        let mut state = manager_with(&[]);

        apply_state_updates(&node, &mut state, &json!("agent finished its work"));

        assert_eq!(
            state.state().get("findings"),
            Some(&json!("agent finished its work"))
        );
    }

    #[test]
    fn state_updates_can_reference_existing_keys_and_output() {
        let node = {
            let mut u = HashMap::new();
            u.insert("summary".into(), "{{topic}}: {{output}}".into());
            node_with("hi", Some(u))
        };
        let mut state = manager_with(&[("topic", json!("auth"))]);

        apply_state_updates(&node, &mut state, &json!("JWT vs sessions"));

        assert_eq!(
            state.state().get("summary"),
            Some(&json!("auth: JWT vs sessions"))
        );
    }

    #[test]
    fn output_key_is_cleaned_up_after_state_updates() {
        let node = {
            let mut u = HashMap::new();
            u.insert("findings".into(), "{{output}}".into());
            node_with("hi", Some(u))
        };
        let mut state = manager_with(&[]);

        apply_state_updates(&node, &mut state, &json!("anything"));

        assert!(state.state().get("output").is_none());
    }

    #[test]
    fn pre_existing_output_value_is_preserved() {
        let node = {
            let mut u = HashMap::new();
            u.insert("greeting".into(), "{{output}}".into());
            node_with("hi", Some(u))
        };
        let mut state = manager_with(&[("output", json!("preserved"))]);

        apply_state_updates(&node, &mut state, &json!("new agent output"));

        assert_eq!(
            state.state().get("greeting"),
            Some(&json!("new agent output"))
        );
        assert_eq!(state.state().get("output"), Some(&json!("preserved")));
    }

    #[test]
    fn no_state_updates_is_a_noop() {
        let node = node_with("hi", None);
        let mut state = manager_with(&[("k", json!("v"))]);

        apply_state_updates(&node, &mut state, &json!("ignored"));

        assert_eq!(state.state().get("k"), Some(&json!("v")));
        assert!(state.state().get("output").is_none());
    }

    #[test]
    fn interpolate_lenient_on_state_updates_handles_missing_keys() {
        let node = {
            let mut u = HashMap::new();
            u.insert("decorated".into(), "[{{missing}}] {{output}}".into());
            node_with("hi", Some(u))
        };
        let mut state = manager_with(&[]);

        apply_state_updates(&node, &mut state, &json!("DATA"));

        assert_eq!(state.state().get("decorated"), Some(&json!("[] DATA")));
    }

    fn node_with_schema(
        prompt: &str,
        updates: Option<HashMap<String, String>>,
        schema: Value,
    ) -> AgentNode {
        let mut n = node_with(prompt, updates);
        n.output_schema = Some(schema);
        n
    }

    #[test]
    fn output_schema_auto_merges_top_level_keys() {
        let node = node_with_schema("hi", None, json!({"type": "object"}));
        let mut state = manager_with(&[]);
        let output = json!({"goal": "do X", "summary": "details"});

        apply_state_updates(&node, &mut state, &output);

        assert_eq!(state.state().get("goal"), Some(&json!("do X")));
        assert_eq!(state.state().get("summary"), Some(&json!("details")));
    }

    #[test]
    fn output_schema_preserves_nested_value_types() {
        let node = node_with_schema("hi", None, json!({"type": "object"}));
        let mut state = manager_with(&[]);
        let output = json!({
            "tags": ["a", "b"],
            "config": { "key": "value" },
            "count": 42
        });

        apply_state_updates(&node, &mut state, &output);

        assert_eq!(state.state().get("tags"), Some(&json!(["a", "b"])));
        assert_eq!(state.state().get("config"), Some(&json!({"key": "value"})));
        assert_eq!(state.state().get("count"), Some(&json!(42)));
    }

    #[test]
    fn output_schema_explicit_state_updates_override_auto_merge() {
        let mut u = HashMap::new();
        u.insert("goal".into(), "renamed-{{output.goal}}".into());
        let node = node_with_schema("hi", Some(u), json!({"type": "object"}));
        let mut state = manager_with(&[]);
        let output = json!({"goal": "do X"});

        apply_state_updates(&node, &mut state, &output);

        assert_eq!(state.state().get("goal"), Some(&json!("renamed-do X")));
    }

    #[test]
    fn no_schema_does_not_auto_merge() {
        let node = node_with("hi", None);
        let mut state = manager_with(&[]);
        let output = json!({"goal": "do X"});

        apply_state_updates(&node, &mut state, &output);

        assert!(state.state().get("goal").is_none());
    }
}
