use super::state::StateManager;
use super::state_updates;
use super::structured;
use super::types::LlmNode;
use super::{is_transient_error, wall_clock};
use crate::client::{Model, ModelType, call_chat_completions_streaming_quiet};
use crate::config::prompts::DEFAULT_SKILL_INSTRUCTIONS;
use crate::config::{
    Input, RequestContext, Role, RoleLike, SkillPolicy, should_inject_skill_instructions,
};
use crate::function::ToolResult;
use crate::function::agents::{GuardrailAction, check_pending_tasks_guardrail};
use crate::function::jobs::reap_jobs;
use crate::function::skill::skill_function_declarations;
use crate::utils::AbortSignal;
use anyhow::{Context, Error, Result, anyhow, bail};
use log::warn;
use serde_json::Value;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::time::timeout;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LlmExecutionOutcome {
    Continue,
    FellBack(String),
}

pub struct LlmNodeExecutor;

impl LlmNodeExecutor {
    pub(super) async fn execute(
        node_id: &str,
        node: &LlmNode,
        state_manager: &mut StateManager,
        parent_ctx: &mut RequestContext,
        abort: &AbortSignal,
    ) -> Result<LlmExecutionOutcome> {
        let result = run(node_id, node, state_manager, parent_ctx, abort).await;
        finish(node, state_manager, parent_ctx, result, abort).await
    }
}

/// Extraction, state_updates, and the outcome for an already-finished run.
/// Split from `execute` so tests can feed a raw run result without a model.
async fn finish(
    node: &LlmNode,
    state_manager: &mut StateManager,
    parent_ctx: &mut RequestContext,
    result: Result<String>,
    abort: &AbortSignal,
) -> Result<LlmExecutionOutcome> {
    let (output, failure_reason) = match result {
        Ok(raw) => match &node.output_schema {
            Some(schema) => match structured::extract(&raw, schema, parent_ctx, abort).await {
                Ok(value) => (value, None),
                Err(e) => {
                    warn!("llm node structured extraction failed: {e:#}");
                    (
                        Value::String(format!("LLM node structured-extraction failed: {e:#}")),
                        Some(format!("structured-extraction failed: {e:#}")),
                    )
                }
            },
            None => (Value::String(raw), None),
        },
        Err(e) => {
            warn!("llm node failed: {e:#}");
            (
                Value::String(format!("LLM node failed: {e:#}")),
                Some(format!("LLM call failed: {e:#}")),
            )
        }
    };

    apply_state_updates_with_output(node, state_manager, &output);
    outcome_from(failure_reason.as_deref(), node.fallback.as_deref())
}

fn outcome_from(
    failure_reason: Option<&str>,
    fallback: Option<&str>,
) -> Result<LlmExecutionOutcome> {
    match (failure_reason, fallback) {
        (None, _) => Ok(LlmExecutionOutcome::Continue),
        (Some(_), Some(fb)) => Ok(LlmExecutionOutcome::FellBack(fb.to_string())),
        (Some(reason), None) => bail!(
            "LLM node failed and no fallback declared: {reason}. \
             Add a `fallback:` route on the node to route on failure, \
             or fix the underlying error."
        ),
    }
}

async fn run(
    node_id: &str,
    node: &LlmNode,
    state_manager: &mut StateManager,
    parent_ctx: &mut RequestContext,
    abort: &AbortSignal,
) -> Result<String> {
    let mut instructions: Option<String> = match &node.instructions {
        Some(s) => Some(
            state_manager
                .interpolate(s)
                .context("Failed to interpolate llm node instructions")?,
        ),
        None => None,
    };
    let mut prompt = state_manager
        .interpolate(&node.prompt)
        .context("Failed to interpolate llm node prompt")?;

    if let Some(schema) = &node.output_schema {
        let hint = format_schema_hint(schema);
        match instructions.as_mut() {
            Some(s) => {
                s.push_str("\n\n");
                s.push_str(&hint);
            }
            None => {
                prompt.push_str("\n\n");
                prompt.push_str(&hint);
            }
        }
    }

    let (regular_tools, mcp_servers) = categorize_tools(node.tools.as_deref());
    validate_tools_subset(&regular_tools, &mcp_servers, parent_ctx)?;

    let mut role = build_inline_role(
        node,
        instructions.as_deref(),
        &regular_tools,
        &mcp_servers,
        parent_ctx,
    )?;

    let saved_agent_skill_state = swap_in_node_skill_policy(node, parent_ctx);

    let policy = match SkillPolicy::effective(
        &parent_ctx.app.config,
        parent_ctx.role.as_ref(),
        parent_ctx.agent.as_ref(),
        parent_ctx.session.as_ref(),
    ) {
        Ok(p) => p,
        Err(e) => {
            restore_agent_skill_policy(parent_ctx, saved_agent_skill_state);
            return Err(e);
        }
    };

    if policy.skills_enabled {
        let mut tools = role.enabled_tools().map(|v| v.to_vec()).unwrap_or_default();
        for decl in skill_function_declarations() {
            if !tools.contains(&decl.name) {
                tools.push(decl.name);
            }
        }
        role.set_enabled_tools(Some(tools));
    }

    if should_inject_skill_instructions(&parent_ctx.app.config, &policy) {
        let app = &parent_ctx.app.config;
        let agent = parent_ctx.agent.as_ref();
        let inject = node
            .inject_skill_instructions
            .or_else(|| agent.map(|a| a.inject_skill_instructions()))
            .unwrap_or(app.inject_skill_instructions);

        if inject {
            let instructions = node
                .skill_instructions
                .clone()
                .or_else(|| agent.and_then(|a| a.skill_instructions_value()))
                .or_else(|| app.skill_instructions.clone());
            let separator = if role.is_empty_prompt() { "" } else { "\n\n" };

            role.append_to_prompt(separator);
            role.append_to_prompt(
                instructions
                    .as_deref()
                    .unwrap_or(DEFAULT_SKILL_INSTRUCTIONS),
            );
        }
    }

    let composed_role = parent_ctx.skill_registry.effective_role(&role, &policy);

    let saved_role = parent_ctx.role.clone();
    parent_ctx.role = Some(composed_role);
    // Jobs are node-local: everything job__start registers while this node
    // runs is recorded here and reaped on every exit path below.
    let saved_job_scope = parent_ctx.node_job_scope.replace(Vec::new());
    // The node's tool filter layer lives in tracked context state so any
    // mid-node filter recompute (e.g. a skill load) re-applies it last.
    let saved_node_mcp_tools = std::mem::replace(
        &mut parent_ctx.active_node_mcp_tools,
        node.mcp_tools.clone().map(|map| (node_id.to_string(), map)),
    );
    parent_ctx.refresh_mcp_tool_filters();
    let result = bounded_run(
        node,
        &prompt,
        parent_ctx,
        abort,
        &mut default_completion_runner(),
    )
    .await;
    parent_ctx.role = saved_role;
    let node_jobs =
        std::mem::replace(&mut parent_ctx.node_job_scope, saved_job_scope).unwrap_or_default();
    reap_jobs(parent_ctx.supervisor.as_ref(), &node_jobs).await;
    parent_ctx.active_node_mcp_tools = saved_node_mcp_tools;
    parent_ctx.refresh_mcp_tool_filters();
    restore_agent_skill_policy(parent_ctx, saved_agent_skill_state);
    result
}

struct SavedAgentSkillPolicy {
    skills_enabled: Option<bool>,
    enabled_skills: Option<Vec<String>>,
}

fn swap_in_node_skill_policy(
    node: &LlmNode,
    ctx: &mut RequestContext,
) -> Option<SavedAgentSkillPolicy> {
    let agent = ctx.agent.as_mut()?;
    let saved = SavedAgentSkillPolicy {
        skills_enabled: agent.skills_enabled(),
        enabled_skills: agent.enabled_skills().map(|s| s.to_vec()),
    };

    if let Some(b) = node.skills_enabled {
        agent.set_skills_enabled(Some(b));
    }

    if let Some(names) = &node.enabled_skills {
        agent.set_enabled_skills(Some(names.clone()));
    }

    Some(saved)
}

fn restore_agent_skill_policy(ctx: &mut RequestContext, saved: Option<SavedAgentSkillPolicy>) {
    let Some(saved) = saved else { return };
    let Some(agent) = ctx.agent.as_mut() else {
        return;
    };

    agent.set_skills_enabled(saved.skills_enabled);
    agent.set_enabled_skills(saved.enabled_skills);
}

/// One boxed model call against the input and ctx. Boxed for the same
/// reason as `AttemptFuture` in agent.rs: llm nodes run inside
/// `tokio::spawn`ed map branches, where higher-ranked opaque futures trip
/// the `Send` auto-trait solver.
type CompletionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(String, Vec<ToolResult>)>> + Send + 'a>>;

/// The per-call runner the chat loop drives; production uses the quiet
/// streaming transport, tests substitute paced or aborting fakes. The runner
/// owns client construction and the before-hook so a failed `create_client`
/// never records a live `last_message`.
type CompletionRunner<'f> = dyn for<'a> FnMut(&'a Input, &'a mut RequestContext, AbortSignal) -> CompletionFuture<'a>
    + Send
    + 'f;

fn default_completion_runner()
-> impl for<'a> FnMut(&'a Input, &'a mut RequestContext, AbortSignal) -> CompletionFuture<'a> + Send
{
    |input, ctx, abort| {
        Box::pin(async move {
            let client = input.create_client()?;
            ctx.before_chat_completion(input)?;
            call_chat_completions_streaming_quiet(input, client.as_ref(), ctx, abort).await
        })
    }
}

/// Applies the node's wall-clock bound around the whole retry loop.
async fn bounded_run(
    node: &LlmNode,
    prompt: &str,
    ctx: &mut RequestContext,
    abort: &AbortSignal,
    runner: &mut CompletionRunner<'_>,
) -> Result<String> {
    match node.timeout.and_then(wall_clock) {
        Some(d) => match timeout(d, run_with_retries(node, prompt, ctx, abort, runner)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow!("llm node timed out after {}s", d.as_secs())),
        },
        None => run_with_retries(node, prompt, ctx, abort, runner).await,
    }
}

async fn run_with_retries(
    node: &LlmNode,
    prompt: &str,
    ctx: &mut RequestContext,
    abort: &AbortSignal,
    runner: &mut CompletionRunner<'_>,
) -> Result<String> {
    let mut last_err: Option<Error> = None;
    for attempt in 1..=node.max_attempts {
        match run_chat_loop(node, prompt, ctx, abort, runner).await {
            Ok(out) => return Ok(out),
            Err(e) if is_transient_error(&e) && attempt < node.max_attempts => {
                warn!("llm node attempt {attempt} failed (transient): {e:#}; retrying");
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("llm node exhausted retries")))
}

/// Whether `turn` (0-based) is the final turn the node's cap allows. A cap of 0 means no cap.
pub(crate) fn is_last_turn(turn: u32, max_iterations: u32) -> bool {
    max_iterations > 0 && turn == max_iterations - 1
}

async fn run_chat_loop(
    node: &LlmNode,
    prompt: &str,
    ctx: &mut RequestContext,
    abort: &AbortSignal,
    runner: &mut CompletionRunner<'_>,
) -> Result<String> {
    let abort = abort.clone();
    let app_cfg = Arc::clone(&ctx.app.config);
    let role_for_input = ctx.role.clone();
    let mut input = Input::from_str(ctx, prompt, role_for_input)?;
    let mut accumulated = String::new();

    let mut turn: u32 = 0;
    loop {
        if abort.aborted() {
            bail!("llm node aborted");
        }
        let (output, tool_results) = runner(&input, ctx, abort.clone()).await?;
        // Defence in depth: the transport bails on abort itself; this closes
        // the window between its check and ours.
        if abort.aborted() {
            bail!("llm node aborted");
        }
        ctx.after_chat_completion(app_cfg.as_ref(), &input, &output, &tool_results)?;

        if !output.is_empty() {
            if !accumulated.is_empty() {
                accumulated.push('\n');
            }
            accumulated.push_str(&output);
        }

        if tool_results.is_empty() {
            match check_pending_tasks_guardrail(ctx) {
                GuardrailAction::NoAction => return Ok(accumulated),
                GuardrailAction::ForceTerminate(ids) => {
                    warn!(
                        "Pending-agent guardrail force-cancelled {} agent(s) after max reminders: {:?}",
                        ids.len(),
                        ids
                    );
                    return Ok(accumulated);
                }
                GuardrailAction::Inject(prompt) => {
                    if is_last_turn(turn, node.max_iterations) {
                        bail!(
                            "llm node hit max_iterations ({}) before LLM concluded",
                            node.max_iterations
                        );
                    }
                    let role = ctx.role.clone();
                    input = Input::from_str(ctx, &prompt, role)?;
                }
            }
        } else {
            if is_last_turn(turn, node.max_iterations) {
                bail!(
                    "llm node hit max_iterations ({}) before LLM concluded",
                    node.max_iterations
                );
            }
            input = input.merge_tool_results(output, tool_results);
        }
        turn = turn.saturating_add(1);
    }
}

fn build_inline_role(
    node: &LlmNode,
    instructions: Option<&str>,
    regular_tools: &[String],
    mcp_servers: &[String],
    parent_ctx: &RequestContext,
) -> Result<Role> {
    let mut role = Role::new("llm_node", instructions.unwrap_or(""));

    let model = match &node.model {
        Some(model_id) => {
            Model::retrieve_model(parent_ctx.app.config.as_ref(), model_id, ModelType::Chat)
                .with_context(|| format!("Unknown model '{model_id}' on llm node"))?
        }
        None => parent_ctx.current_model().clone(),
    };
    role.set_model(model);

    if let Some(t) = node.temperature {
        role.set_temperature(Some(t));
    }
    if let Some(p) = node.top_p {
        role.set_top_p(Some(p));
    }
    if let Some(v) = &node.reasoning_effort {
        role.set_reasoning_effort(Some(v.clone()));
    }

    if node.tools.as_deref().unwrap_or_default().is_empty() {
        role.set_enabled_tools(Some(Vec::new()));
        role.set_enabled_mcp_servers(Some(Vec::new()));
    } else {
        if !regular_tools.is_empty() {
            role.set_enabled_tools(Some(regular_tools.to_vec()));
        } else {
            role.set_enabled_tools(Some(Vec::new()));
        }
        if !mcp_servers.is_empty() {
            role.set_enabled_mcp_servers(Some(mcp_servers.to_vec()));
        } else {
            role.set_enabled_mcp_servers(Some(Vec::new()));
        }
    }

    Ok(role)
}

fn categorize_tools(entries: Option<&[String]>) -> (Vec<String>, Vec<String>) {
    let mut regular = Vec::new();
    let mut mcp = Vec::new();
    let Some(entries) = entries else {
        return (regular, mcp);
    };

    for e in entries {
        if let Some(server) = e.strip_prefix("mcp:") {
            mcp.push(server.to_string());
        } else {
            regular.push(e.clone());
        }
    }

    (regular, mcp)
}

fn validate_tools_subset(
    regular: &[String],
    mcp_servers: &[String],
    parent_ctx: &RequestContext,
) -> Result<()> {
    let agent = parent_ctx
        .agent
        .as_ref()
        .ok_or_else(|| anyhow!("llm node requires an active agent"))?;

    if !regular.is_empty() {
        let known: HashSet<&str> = agent
            .functions()
            .declarations()
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        for name in regular {
            if !known.contains(name.as_str()) {
                let mut avail: Vec<&str> = known.iter().copied().collect();
                avail.sort();
                bail!(
                    "llm node references unknown tool '{name}'. Agent '{}' provides: {}",
                    agent.name(),
                    avail.join(", ")
                );
            }
        }
    }

    if !mcp_servers.is_empty() {
        let known: HashSet<&str> = agent
            .mcp_server_names()
            .iter()
            .map(|s| s.as_str())
            .collect();
        for server in mcp_servers {
            if !known.contains(server.as_str()) {
                let mut avail: Vec<&str> = known.iter().copied().collect();
                avail.sort();
                bail!(
                    "llm node references unknown MCP server 'mcp:{server}'. \
                     Agent '{}' has MCP servers: [{}]",
                    agent.name(),
                    avail.join(", ")
                );
            }
        }
    }

    Ok(())
}

fn apply_state_updates_with_output(
    node: &LlmNode,
    state_manager: &mut StateManager,
    output: &Value,
) {
    state_updates::apply(
        state_manager,
        output,
        node.output_schema.is_some(),
        node.state_updates.as_ref(),
    );
}

fn format_schema_hint(schema: &Value) -> String {
    let schema_json = serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string());
    format!(
        "Respond with a JSON object that matches this schema. Output ONLY the JSON \
         object with no surrounding prose or markdown fences.\n\nSchema:\n{schema_json}"
    )
}

#[cfg(test)]
mod tests {
    use super::super::state_updates::OUTPUT_KEY;
    use super::super::types::*;
    use super::*;
    use crate::config::{Agent, AgentConfig, AppState, WorkingMode};
    use crate::utils::create_abort_signal;
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;

    fn manager_with(pairs: &[(&str, Value)]) -> StateManager {
        let mut map = HashMap::new();
        for (k, v) in pairs {
            map.insert((*k).into(), v.clone());
        }
        StateManager::new(map)
    }

    fn node_with(updates: Option<HashMap<String, String>>) -> LlmNode {
        LlmNode {
            instructions: Some("sys".into()),
            prompt: "user".into(),
            tools: None,
            mcp_tools: None,
            model: None,
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            fallback: None,
            max_attempts: 1,
            max_iterations: 10,
            state_updates: updates,
            output_schema: None,
            timeout: None,
            skills_enabled: None,
            enabled_skills: None,
            inject_skill_instructions: None,
            skill_instructions: None,
        }
    }

    #[test]
    fn state_updates_expose_output_during_evaluation() {
        let mut u = HashMap::new();
        u.insert("response".into(), "{{output}}".into());
        let node = node_with(Some(u));
        let mut state = manager_with(&[]);

        apply_state_updates_with_output(&node, &mut state, &json!("the answer"));

        assert_eq!(state.state().get("response"), Some(&json!("the answer")));
    }

    #[test]
    fn state_updates_can_mix_existing_keys_with_output() {
        let mut u = HashMap::new();
        u.insert("summary".into(), "{{topic}}: {{output}}".into());
        let node = node_with(Some(u));
        let mut state = manager_with(&[("topic", json!("LOINC"))]);

        apply_state_updates_with_output(&node, &mut state, &json!("abc"));

        assert_eq!(state.state().get("summary"), Some(&json!("LOINC: abc")));
    }

    #[test]
    fn output_key_is_cleared_after_state_updates() {
        let mut u = HashMap::new();
        u.insert("k".into(), "{{output}}".into());
        let node = node_with(Some(u));
        let mut state = manager_with(&[]);

        apply_state_updates_with_output(&node, &mut state, &json!("anything"));

        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn pre_existing_output_value_is_restored() {
        let mut u = HashMap::new();
        u.insert("greeting".into(), "{{output}}".into());
        let node = node_with(Some(u));
        let mut state = manager_with(&[("output", json!("preserved"))]);

        apply_state_updates_with_output(&node, &mut state, &json!("new"));

        assert_eq!(state.state().get("greeting"), Some(&json!("new")));
        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!("preserved")));
    }

    #[test]
    fn no_state_updates_is_a_noop() {
        let node = node_with(None);
        let mut state = manager_with(&[("k", json!("v"))]);

        apply_state_updates_with_output(&node, &mut state, &json!("x"));

        assert_eq!(state.state().get("k"), Some(&json!("v")));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn outcome_from_success_is_continue() {
        assert_eq!(
            outcome_from(None, Some("fb")).unwrap(),
            LlmExecutionOutcome::Continue
        );
        assert_eq!(
            outcome_from(None, None).unwrap(),
            LlmExecutionOutcome::Continue
        );
    }

    #[test]
    fn outcome_from_failure_with_fallback_is_fell_back() {
        assert_eq!(
            outcome_from(Some("HTTP 404"), Some("fb")).unwrap(),
            LlmExecutionOutcome::FellBack("fb".to_string())
        );
    }

    #[test]
    fn outcome_from_failure_without_fallback_propagates_error() {
        let err = outcome_from(Some("HTTP 404"), None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no fallback declared"), "got: {msg}");
        assert!(msg.contains("HTTP 404"), "got: {msg}");
    }

    fn node_with_schema(updates: Option<HashMap<String, String>>, schema: Value) -> LlmNode {
        let mut n = node_with(updates);
        n.output_schema = Some(schema);
        n
    }

    #[test]
    fn output_schema_auto_merges_top_level_keys() {
        let node = node_with_schema(None, json!({"type": "object"}));
        let mut state = manager_with(&[]);
        let output = json!({"goal": "do X", "summary": "details"});

        apply_state_updates_with_output(&node, &mut state, &output);

        assert_eq!(state.state().get("goal"), Some(&json!("do X")));
        assert_eq!(state.state().get("summary"), Some(&json!("details")));
    }

    #[test]
    fn output_schema_preserves_nested_value_types() {
        let node = node_with_schema(None, json!({"type": "object"}));
        let mut state = manager_with(&[]);
        let output = json!({
            "tags": ["a", "b"],
            "config": { "key": "value" },
            "count": 42
        });

        apply_state_updates_with_output(&node, &mut state, &output);

        assert_eq!(state.state().get("tags"), Some(&json!(["a", "b"])));
        assert_eq!(state.state().get("config"), Some(&json!({"key": "value"})));
        assert_eq!(state.state().get("count"), Some(&json!(42)));
    }

    #[test]
    fn output_schema_explicit_state_updates_override_auto_merge() {
        let mut u = HashMap::new();
        u.insert("goal".into(), "renamed-{{output.goal}}".into());
        let node = node_with_schema(Some(u), json!({"type": "object"}));
        let mut state = manager_with(&[]);
        let output = json!({"goal": "do X"});

        apply_state_updates_with_output(&node, &mut state, &output);

        assert_eq!(state.state().get("goal"), Some(&json!("renamed-do X")));
    }

    #[test]
    fn output_schema_skips_auto_merge_for_non_object() {
        let node = node_with_schema(None, json!({"type": "array"}));
        let mut state = manager_with(&[]);
        let output = json!([1, 2, 3]);

        apply_state_updates_with_output(&node, &mut state, &output);

        assert!(state.state().get("0").is_none());
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn no_schema_does_not_auto_merge() {
        let node = node_with(None);
        let mut state = manager_with(&[]);
        let output = json!({"goal": "do X"});

        apply_state_updates_with_output(&node, &mut state, &output);

        assert!(state.state().get("goal").is_none());
    }

    #[test]
    fn format_schema_hint_includes_schema_and_instruction() {
        let schema = json!({"type": "object", "properties": {"goal": {"type": "string"}}});

        let hint = format_schema_hint(&schema);

        assert!(hint.contains("Schema:"));
        assert!(hint.contains("\"goal\""));
        assert!(hint.contains("JSON"));
        assert!(hint.contains("ONLY"));
    }

    #[test]
    fn categorize_tools_splits_mcp_and_regular() {
        let entries = vec![
            "read_query".to_string(),
            "mcp:pubmed-search".to_string(),
            "web_search_coyote".to_string(),
            "mcp:github".to_string(),
        ];

        let (regular, mcp) = categorize_tools(Some(&entries));

        assert_eq!(regular, vec!["read_query", "web_search_coyote"]);
        assert_eq!(mcp, vec!["pubmed-search", "github"]);
    }

    #[test]
    fn categorize_tools_with_none_returns_empty() {
        let (regular, mcp) = categorize_tools(None);

        assert!(regular.is_empty());
        assert!(mcp.is_empty());
    }

    #[test]
    fn categorize_tools_with_empty_returns_empty() {
        let (regular, mcp) = categorize_tools(Some(&[]));

        assert!(regular.is_empty());
        assert!(mcp.is_empty());
    }

    #[test]
    fn zero_timeout_resolves_to_no_bound() {
        assert!(Some(0u64).and_then(wall_clock).is_none());
        assert_eq!(
            Some(5u64).and_then(wall_clock),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn is_last_turn_bounded() {
        assert!(is_last_turn(0, 1));
        assert!(!is_last_turn(0, 10));
        assert!(is_last_turn(9, 10));
        assert!(!is_last_turn(10, 10));
    }

    #[test]
    fn is_last_turn_zero_cap_never_fires() {
        assert!(!is_last_turn(0, 0));
        assert!(!is_last_turn(9, 0));
        assert!(!is_last_turn(u32::MAX, 0));
    }

    #[test]
    fn is_last_turn_at_u32_max_does_not_overflow() {
        assert!(is_last_turn(u32::MAX - 1, u32::MAX));
        assert!(!is_last_turn(u32::MAX, u32::MAX));
    }

    fn plain_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    /// Identity funnel that pins a closure to the runner's higher-ranked
    /// signature, so inline closures at call sites infer the right lifetimes.
    fn completion_runner<F>(f: F) -> F
    where
        F: for<'a> FnMut(&'a Input, &'a mut RequestContext, AbortSignal) -> CompletionFuture<'a>
            + Send,
    {
        f
    }

    fn boxed_completion<'a>(
        fut: impl Future<Output = Result<(String, Vec<ToolResult>)>> + Send + 'a,
    ) -> CompletionFuture<'a> {
        Box::pin(fut)
    }

    fn paced_reply(secs: u64) -> CompletionFuture<'static> {
        boxed_completion(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            Ok(("done".to_string(), vec![]))
        })
    }

    /// A 600s generation finishes under a 900s node timeout: the node's
    /// own deadline is what bounds a reply. The runner is a fake, so this
    /// does not exercise reqwest's `read_timeout`; the real-socket tests in
    /// graph/mod.rs pin that it fires on a stall
    /// (`sse_read_timeout_stall_is_transient`) and not on a slow but
    /// continuous stream (`sse_slow_but_continuous_stream_outlives_read_timeout`).
    #[tokio::test(start_paused = true)]
    async fn node_timeout_above_generation_time_lets_the_reply_complete() {
        let mut node = node_with(None);
        node.timeout = Some(900);
        let mut ctx = plain_ctx();
        let abort = create_abort_signal();

        let out = bounded_run(
            &node,
            "user",
            &mut ctx,
            &abort,
            &mut completion_runner(|_input, _ctx, _abort| paced_reply(600)),
        )
        .await
        .unwrap();

        assert_eq!(out, "done");
    }

    #[tokio::test(start_paused = true)]
    async fn node_timeout_below_generation_time_fails_transiently() {
        let mut node = node_with(None);
        node.timeout = Some(300);
        let mut ctx = plain_ctx();
        let abort = create_abort_signal();

        let err = bounded_run(
            &node,
            "user",
            &mut ctx,
            &abort,
            &mut completion_runner(|_input, _ctx, _abort| paced_reply(600)),
        )
        .await
        .expect_err("a 600s reply must not survive a 300s node timeout");

        assert_eq!(err.to_string(), "llm node timed out after 300s");
        assert!(is_transient_error(&err));
    }

    /// A typed stall (an `Elapsed` under the provider context, as the
    /// transport surfaces it) is retried; the second attempt's reply wins.
    #[tokio::test(start_paused = true)]
    async fn run_with_retries_retries_a_typed_stall_once() {
        let mut node = node_with(None);
        node.max_attempts = 2;
        let mut ctx = plain_ctx();
        let abort = create_abort_signal();
        let mut attempts = 0u32;

        let out = run_with_retries(
            &node,
            "user",
            &mut ctx,
            &abort,
            &mut completion_runner(|_input, _ctx, _abort| {
                attempts += 1;
                if attempts == 1 {
                    boxed_completion(async {
                        let elapsed = tokio::time::timeout(
                            Duration::from_secs(1),
                            std::future::pending::<()>(),
                        )
                        .await
                        .expect_err("pending future must time out");
                        Err(Error::new(elapsed).context("Failed to call chat-completions api"))
                    })
                } else {
                    boxed_completion(async { Ok(("recovered".to_string(), vec![])) })
                }
            }),
        )
        .await
        .unwrap();

        assert_eq!(out, "recovered");
        assert_eq!(attempts, 2);
    }

    /// The production runner builds the client before running the
    /// before-hook, so a request that never leaves records no live
    /// `last_message`.
    #[tokio::test]
    async fn default_completion_runner_leaves_no_last_message_when_create_client_fails() {
        let mut ctx = plain_ctx();
        let input = Input::from_str(&ctx, "hi", None).unwrap();
        let mut runner = default_completion_runner();

        let err = runner(&input, &mut ctx, create_abort_signal())
            .await
            .expect_err("the test AppState has no model to build a client for");

        assert!(
            format!("{err:#}").contains("Invalid model"),
            "expected create_client to fail, got: {err:#}"
        );
        assert!(ctx.last_message.is_none());
    }

    /// A call that observes the abort but still returns text must not be
    /// treated as a completed turn; the loop discards it and fails.
    #[tokio::test]
    async fn run_chat_loop_discards_output_from_a_call_aborted_midway() {
        let node = node_with(None);
        let mut ctx = plain_ctx();
        let abort = create_abort_signal();

        let err = run_chat_loop(
            &node,
            "user",
            &mut ctx,
            &abort,
            &mut completion_runner(|_input, _ctx, abort| {
                boxed_completion(async move {
                    abort.set_ctrlc();
                    Ok(("partial".to_string(), vec![]))
                })
            }),
        )
        .await
        .expect_err("partial output from an aborted call is not a success");

        assert_eq!(err.to_string(), "llm node aborted");
    }

    /// The turn-top abort check runs before any client is created, so an
    /// already-aborted graph never issues a model call and the node fails
    /// with the abort reason rather than a connection error.
    #[tokio::test]
    async fn run_chat_loop_bails_before_first_call_when_aborted() {
        let node = node_with(None);
        let mut state = manager_with(&[]);
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(AgentConfig::default()));
        let abort = create_abort_signal();
        abort.set_ctrlc();

        let err = LlmNodeExecutor::execute("think", &node, &mut state, &mut ctx, &abort)
            .await
            .expect_err("a pre-set abort must fail the node");

        let chain = format!("{err:#}");
        assert!(
            chain.contains(
                "LLM node failed and no fallback declared: LLM call failed: llm node aborted"
            ),
            "{chain}"
        );
        assert_eq!(
            state.state().get(OUTPUT_KEY),
            None,
            "no output is recorded for an aborted node without state_updates"
        );
    }

    /// The fault text written through state_updates must carry the full
    /// anyhow chain, not just the outermost context, so downstream fault
    /// scripts can surface the root cause.
    #[tokio::test]
    async fn failure_text_in_state_updates_carries_full_error_chain() {
        let mut u = HashMap::new();
        u.insert("captured".into(), "{{output}}".into());
        let mut node = node_with(Some(u));
        node.prompt = "{{nope}}".into();
        node.fallback = Some("fb".into());
        let mut state = manager_with(&[]);
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(AgentConfig::default()));
        let abort = create_abort_signal();

        let inner = state
            .interpolate(&node.prompt)
            .context("Failed to interpolate llm node prompt")
            .expect_err("missing reference must fail interpolation");
        assert_ne!(
            format!("{inner:#}"),
            format!("{inner}"),
            "the chosen error must have more than one level of context"
        );

        let outcome = LlmNodeExecutor::execute("think", &node, &mut state, &mut ctx, &abort)
            .await
            .expect("a declared fallback turns the failure into a route");

        assert_eq!(outcome, LlmExecutionOutcome::FellBack("fb".into()));
        let captured = state
            .state()
            .get("captured")
            .and_then(Value::as_str)
            .expect("captured failure text is a string")
            .to_string();
        assert!(captured.starts_with("LLM node failed: "), "{captured}");
        assert!(
            captured.contains(
                "Failed to interpolate llm node prompt: Template interpolation failed: 'nope' not found in state"
            ),
            "{captured}"
        );
    }

    /// Without a fallback the node bails, and that bail must wrap the same
    /// full chain the state_updates text carries.
    #[tokio::test]
    async fn bail_without_fallback_carries_full_error_chain() {
        let mut u = HashMap::new();
        u.insert("captured".into(), "{{output}}".into());
        let mut node = node_with(Some(u));
        node.prompt = "{{nope}}".into();
        node.fallback = None;
        let mut state = manager_with(&[]);
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(AgentConfig::default()));
        let abort = create_abort_signal();

        let err = LlmNodeExecutor::execute("think", &node, &mut state, &mut ctx, &abort)
            .await
            .expect_err("no fallback means the failure bails");

        let chain = format!("{err:#}");
        assert!(
            chain.contains(
                "LLM node failed and no fallback declared: LLM call failed: Failed to interpolate llm node prompt: Template interpolation failed: 'nope' not found in state"
            ),
            "{chain}"
        );
        let captured = state
            .state()
            .get("captured")
            .and_then(Value::as_str)
            .expect("state_updates still run before the bail")
            .to_string();
        assert!(captured.starts_with("LLM node failed: "), "{captured}");
    }

    /// A run that succeeds but yields unparseable output must fault through
    /// the structured-extraction branch with the extractor's full chain, so
    /// fallback scripts can tell "model spoke prose" from "model call failed".
    #[tokio::test]
    async fn structured_extraction_failure_text_carries_full_error_chain() {
        let mut u = HashMap::new();
        u.insert("captured".into(), "{{output}}".into());
        let mut node = node_with_schema(Some(u), json!({"type": "object"}));
        node.fallback = Some("fb".into());
        let mut state = manager_with(&[]);
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(AgentConfig::default()));

        let outcome = finish(
            &node,
            &mut state,
            &mut ctx,
            Ok("not json".to_string()),
            &create_abort_signal(),
        )
        .await
        .expect("a declared fallback turns the failure into a route");

        assert_eq!(outcome, LlmExecutionOutcome::FellBack("fb".into()));
        let captured = state
            .state()
            .get("captured")
            .and_then(Value::as_str)
            .expect("captured failure text is a string")
            .to_string();
        assert!(
            captured.starts_with("LLM node structured-extraction failed: "),
            "{captured}"
        );
        assert!(
            captured.contains("Structured-output extractor LLM call failed: "),
            "{captured}"
        );
        assert!(captured.contains("Invalid model"), "{captured}");
    }

    /// The extractor runs on the graph abort: an already-aborted graph
    /// never issues the extraction request, and the failure text still
    /// carries the structured-extraction prefix the fallback scripts anchor on.
    #[tokio::test]
    async fn structured_extraction_observes_the_graph_abort() {
        let mut u = HashMap::new();
        u.insert("captured".into(), "{{output}}".into());
        let mut node = node_with_schema(Some(u), json!({"type": "object"}));
        node.fallback = Some("fb".into());
        let mut state = manager_with(&[]);
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(AgentConfig::default()));
        let abort = create_abort_signal();
        abort.set_ctrlc();

        let outcome = finish(
            &node,
            &mut state,
            &mut ctx,
            Ok("not json".to_string()),
            &abort,
        )
        .await
        .expect("a declared fallback turns the failure into a route");

        assert_eq!(outcome, LlmExecutionOutcome::FellBack("fb".into()));
        let captured = state
            .state()
            .get("captured")
            .and_then(Value::as_str)
            .expect("captured failure text is a string")
            .to_string();
        assert!(
            captured.starts_with("LLM node structured-extraction failed: "),
            "{captured}"
        );
        assert!(captured.contains("Aborted."), "{captured}");
        assert!(!captured.contains("Invalid model"), "{captured}");
    }

    /// Without a fallback the extraction fault bails, wrapping the same
    /// extractor chain the state_updates text carries.
    #[tokio::test]
    async fn structured_extraction_failure_without_fallback_bails_with_full_chain() {
        let mut u = HashMap::new();
        u.insert("captured".into(), "{{output}}".into());
        let mut node = node_with_schema(Some(u), json!({"type": "object"}));
        node.fallback = None;
        let mut state = manager_with(&[]);
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(AgentConfig::default()));

        let err = finish(
            &node,
            &mut state,
            &mut ctx,
            Ok("not json".to_string()),
            &create_abort_signal(),
        )
        .await
        .expect_err("no fallback means the failure bails");

        let chain = format!("{err:#}");
        assert!(
            chain.contains(
                "LLM node failed and no fallback declared: structured-extraction failed: Structured-output extractor LLM call failed"
            ),
            "{chain}"
        );
        let captured = state
            .state()
            .get("captured")
            .and_then(Value::as_str)
            .expect("state_updates still run before the bail")
            .to_string();
        assert!(
            captured.starts_with("LLM node structured-extraction failed: "),
            "{captured}"
        );
    }
}
