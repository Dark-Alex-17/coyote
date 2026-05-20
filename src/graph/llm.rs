use super::state::StateManager;
use super::structured;
use super::types::LlmNode;
use crate::client::{Model, ModelType, call_chat_completions};
use crate::config::{Input, RequestContext, Role, RoleLike};
use crate::utils::{create_abort_signal, dimmed_text};
use anyhow::{Context, Error, Result, anyhow, bail};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const OUTPUT_KEY: &str = "output";

/// What happened during an LLM node's execution, from the caller's routing
/// perspective. `Continue` means the caller should advance via the node's
/// declared `next:` targets (whether the LLM actually succeeded or failed
/// without a fallback — either way, the executor uses node.next). `FellBack`
/// means the LLM failed after retries and the node had a `fallback:` declared,
/// so routing should go to that fallback target only.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LlmExecutionOutcome {
    Continue,
    FellBack(String),
}

pub struct LlmNodeExecutor;

impl LlmNodeExecutor {
    pub(super) async fn execute(
        node: &LlmNode,
        state_manager: &mut StateManager,
        parent_ctx: &mut RequestContext,
    ) -> Result<LlmExecutionOutcome> {
        let result = run(node, state_manager, parent_ctx).await;
        let (output, failed) = match result {
            Ok(raw) => match &node.output_schema {
                Some(schema) => match structured::extract(&raw, schema, parent_ctx).await {
                    Ok(value) => (value, false),
                    Err(e) => {
                        warn!("llm node structured extraction failed: {e}");
                        (
                            Value::String(format!("LLM node structured-extraction failed: {e}")),
                            true,
                        )
                    }
                },
                None => (Value::String(raw), false),
            },
            Err(e) => {
                warn!("llm node failed: {e}");
                (Value::String(format!("LLM node failed: {e}")), true)
            }
        };

        apply_state_updates_with_output(node, state_manager, &output);
        Ok(outcome_from(failed, node.fallback.as_deref()))
    }
}

fn outcome_from(failed: bool, fallback: Option<&str>) -> LlmExecutionOutcome {
    if failed && let Some(fb) = fallback {
        LlmExecutionOutcome::FellBack(fb.to_string())
    } else {
        LlmExecutionOutcome::Continue
    }
}

async fn run(
    node: &LlmNode,
    state_manager: &mut StateManager,
    parent_ctx: &mut RequestContext,
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

    eprintln!(
        "{}",
        dimmed_text(&format!(
            "▸   llm call: model={} tools={}",
            node.model.as_deref().unwrap_or("<active>"),
            describe_tools_filter(node.tools.as_deref())
        ))
    );

    let role = build_inline_role(
        node,
        instructions.as_deref(),
        &regular_tools,
        &mcp_servers,
        parent_ctx,
    )?;

    let saved_role = parent_ctx.role.clone();
    parent_ctx.role = Some(role);
    let result = match node.timeout {
        Some(secs) => match timeout(
            Duration::from_secs(secs),
            run_with_retries(node, &prompt, parent_ctx),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(anyhow!("llm node timed out after {secs}s")),
        },
        None => run_with_retries(node, &prompt, parent_ctx).await,
    };
    parent_ctx.role = saved_role;
    result
}

async fn run_with_retries(
    node: &LlmNode,
    prompt: &str,
    ctx: &mut RequestContext,
) -> Result<String> {
    let mut last_err: Option<Error> = None;
    for attempt in 1..=node.max_attempts {
        match run_chat_loop(node, prompt, ctx).await {
            Ok(out) => return Ok(out),
            Err(e) if is_transient(&e) && attempt < node.max_attempts => {
                warn!("llm node attempt {attempt} failed (transient): {e}; retrying");
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("llm node exhausted retries")))
}

async fn run_chat_loop(node: &LlmNode, prompt: &str, ctx: &mut RequestContext) -> Result<String> {
    let abort = create_abort_signal();
    let app_cfg = Arc::clone(&ctx.app.config);
    let role_for_input = ctx.role.clone();
    let mut input = Input::from_str(ctx, prompt, role_for_input);
    let mut accumulated = String::new();

    for turn in 0..node.max_iterations {
        let client = input.create_client()?;
        ctx.before_chat_completion(&input)?;
        let (output, tool_results) =
            call_chat_completions(&input, false, false, client.as_ref(), ctx, abort.clone())
                .await?;
        ctx.after_chat_completion(app_cfg.as_ref(), &input, &output, &tool_results)?;

        if !output.is_empty() {
            if !accumulated.is_empty() {
                accumulated.push('\n');
            }
            accumulated.push_str(&output);
        }

        if tool_results.is_empty() {
            return Ok(accumulated);
        }

        if turn + 1 == node.max_iterations {
            bail!(
                "llm node hit max_iterations ({}) before LLM concluded",
                node.max_iterations
            );
        }

        input = input.merge_tool_results(output, tool_results);
    }

    bail!("llm node ended without producing output")
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

    if node.tools.as_deref().unwrap_or_default().is_empty() {
        role.set_enabled_tools(Some(String::new()));
        role.set_enabled_mcp_servers(Some(String::new()));
    } else {
        if !regular_tools.is_empty() {
            role.set_enabled_tools(Some(regular_tools.join(",")));
        } else {
            role.set_enabled_tools(Some(String::new()));
        }
        if !mcp_servers.is_empty() {
            role.set_enabled_mcp_servers(Some(mcp_servers.join(",")));
        } else {
            role.set_enabled_mcp_servers(Some(String::new()));
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

fn is_transient(err: &Error) -> bool {
    let s = format!("{err:#}");
    s.contains("timed out")
        || s.contains("rate limit")
        || s.contains("429")
        || s.contains("Connection reset")
        || s.contains("Connection refused")
        || s.contains("produced no output")
}

fn apply_state_updates_with_output(
    node: &LlmNode,
    state_manager: &mut StateManager,
    output: &Value,
) {
    if node.output_schema.is_some()
        && let Some(obj) = output.as_object()
    {
        for (k, v) in obj {
            state_manager.state_mut().set(k.clone(), v.clone());
        }
    }

    let Some(updates) = &node.state_updates else {
        return;
    };
    let prev_output = state_manager.state().get(OUTPUT_KEY).cloned();
    state_manager
        .state_mut()
        .set(OUTPUT_KEY.into(), output.clone());

    for (key, template) in updates {
        let value = state_manager.interpolate_lenient(template);
        state_manager
            .state_mut()
            .set(key.clone(), Value::String(value));
    }

    match prev_output {
        Some(v) => state_manager.state_mut().set(OUTPUT_KEY.into(), v),
        None => {
            state_manager
                .state_mut()
                .set(OUTPUT_KEY.into(), Value::Null);
        }
    }
}

fn format_schema_hint(schema: &Value) -> String {
    let schema_json = serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string());
    format!(
        "Respond with a JSON object that matches this schema. Output ONLY the JSON \
         object with no surrounding prose or markdown fences.\n\nSchema:\n{schema_json}"
    )
}

fn describe_tools_filter(tools: Option<&[String]>) -> String {
    match tools {
        Some(t) if !t.is_empty() => t.join(","),
        _ => "<none>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::*;
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

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
            model: None,
            temperature: None,
            top_p: None,
            fallback: None,
            max_attempts: 1,
            max_iterations: 10,
            state_updates: updates,
            output_schema: None,
            timeout: None,
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

        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!(null)));
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
            outcome_from(false, Some("fb")),
            LlmExecutionOutcome::Continue
        );
        assert_eq!(outcome_from(false, None), LlmExecutionOutcome::Continue);
    }

    #[test]
    fn outcome_from_failure_with_fallback_is_fell_back() {
        assert_eq!(
            outcome_from(true, Some("fb")),
            LlmExecutionOutcome::FellBack("fb".to_string())
        );
    }

    #[test]
    fn outcome_from_failure_without_fallback_is_continue() {
        // Failed but no fallback: caller routes via node.next as if successful.
        // The error has already been recorded to state via the OUTPUT_KEY by
        // execute(); the caller's `static_next_targets` will error if node.next
        // is also missing.
        assert_eq!(outcome_from(true, None), LlmExecutionOutcome::Continue);
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
    fn describe_tools_filter_renders_each_case() {
        assert_eq!(describe_tools_filter(None), "<none>");
        assert_eq!(describe_tools_filter(Some(&[])), "<none>");
        let tools = vec!["a".to_string(), "b".to_string()];
        assert_eq!(describe_tools_filter(Some(&tools)), "a,b");
    }

    #[test]
    fn categorize_tools_splits_mcp_and_regular() {
        let entries = vec![
            "read_query".to_string(),
            "mcp:pubmed-search".to_string(),
            "web_search_loki".to_string(),
            "mcp:github".to_string(),
        ];

        let (regular, mcp) = categorize_tools(Some(&entries));

        assert_eq!(regular, vec!["read_query", "web_search_loki"]);
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
    fn is_transient_matches_expected_signatures() {
        assert!(is_transient(&anyhow!("request timed out after 30s")));
        assert!(is_transient(&anyhow!("rate limit reached")));
        assert!(is_transient(&anyhow!("429 too many requests")));
        assert!(is_transient(&anyhow!("Connection reset by peer")));
        assert!(is_transient(&anyhow!("Connection refused")));
        assert!(is_transient(&anyhow!("llm produced no output")));
    }

    #[test]
    fn is_transient_rejects_non_transient_errors() {
        assert!(!is_transient(&anyhow!("Unknown model 'foo'")));
        assert!(!is_transient(&anyhow!(
            "llm node references unknown tool 'bad'"
        )));
        assert!(!is_transient(&anyhow!("hit max_iterations")));
        assert!(!is_transient(&anyhow!("authentication failed")));
    }
}
