use super::state::StateManager;
use super::types::{ApprovalNode, InputNode};
use crate::config::RequestContext;
use crate::function::user_interaction::{
    HEADLESS_GUIDANCE, USER_FUNCTION_PREFIX, handle_user_tool, headless_default_line,
    headless_without_acp,
};
use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

const CHOICE_KEY: &str = "choice";
const INPUT_KEY: &str = "input";
const MAX_HEADLESS_ANSWERS_PER_NODE: usize = 25;
const REPEATS_BEFORE_SECOND_OPTION: usize = 2;

/// Counts headless auto-answers for one graph execution so a loop that keeps
/// re-asking the same question eventually takes a different option, and a
/// node that never stops asking fails the run. Clones share the counts.
#[derive(Clone, Default)]
pub struct HeadlessAnswerGuard {
    counts: Arc<Mutex<HeadlessAnswerCounts>>,
}

#[derive(Default)]
struct HeadlessAnswerCounts {
    per_question: HashMap<(String, String), usize>,
    per_node: HashMap<String, usize>,
}

impl HeadlessAnswerGuard {
    /// Records one auto-answer and returns how many times this node already
    /// auto-answered this exact question.
    fn record(&self, node_id: &str, question: &str) -> Result<usize> {
        let mut counts = self.counts.lock();
        let node_total = counts.per_node.entry(node_id.to_string()).or_default();
        if *node_total >= MAX_HEADLESS_ANSWERS_PER_NODE {
            bail!("headless: node {node_id} auto-answered {MAX_HEADLESS_ANSWERS_PER_NODE} times");
        }
        *node_total += 1;

        let repeats = counts
            .per_question
            .entry((node_id.to_string(), question.to_string()))
            .or_default();
        let previous = *repeats;
        *repeats += 1;
        Ok(previous)
    }
}

pub struct ApprovalNodeExecutor;

impl ApprovalNodeExecutor {
    pub async fn execute(
        node_id: &str,
        node: &ApprovalNode,
        state_manager: &mut StateManager,
        ctx: &mut RequestContext,
        guard: &HeadlessAnswerGuard,
    ) -> Result<String> {
        let question = state_manager
            .interpolate(&node.question)
            .context("Failed to interpolate approval question")?;

        let choice = if headless_without_acp() {
            let choice = headless_approval_choice(node_id, node, &question, guard)?;
            eprintln!(
                "{}",
                headless_default_line(Some(node_id), "approval", Some(&choice))
            );
            choice
        } else {
            prompt_approval(node, &question, ctx).await?
        };

        apply_state_updates_with_var(&node.state_updates, state_manager, CHOICE_KEY, &choice);

        resolve_approval_route(node, &choice)
    }
}

async fn prompt_approval(
    node: &ApprovalNode,
    question: &str,
    ctx: &mut RequestContext,
) -> Result<String> {
    let response = handle_user_tool(
        ctx,
        &format!("{USER_FUNCTION_PREFIX}select"),
        &json!({ "question": question, "options": node.options }),
    )
    .await
    .context("user__select failed")?;

    if let Some(err) = response.get("error").and_then(Value::as_str) {
        bail!("Approval interaction failed: {err}");
    }

    Ok(response
        .get("answer")
        .and_then(Value::as_str)
        .context("Approval response missing 'answer' field")?
        .to_string())
}

fn headless_approval_choice(
    node_id: &str,
    node: &ApprovalNode,
    question: &str,
    guard: &HeadlessAnswerGuard,
) -> Result<String> {
    let Some(first) = node.options.first() else {
        bail!("headless: approval node {node_id} has no options to default to");
    };
    let repeats = guard.record(node_id, question)?;
    let choice = if repeats >= REPEATS_BEFORE_SECOND_OPTION {
        node.options.get(1).unwrap_or(first)
    } else {
        first
    };
    Ok(choice.clone())
}

pub struct InputNodeExecutor;

impl InputNodeExecutor {
    pub async fn execute(
        node_id: &str,
        node: &InputNode,
        node_next: Option<&str>,
        state_manager: &mut StateManager,
        ctx: &mut RequestContext,
        guard: &HeadlessAnswerGuard,
    ) -> Result<String> {
        let question = build_input_question(node, state_manager)?;

        let input_text = if headless_without_acp() {
            guard.record(node_id, &question)?;
            let text = headless_input_text(node, state_manager);
            eprintln!(
                "{}",
                headless_default_line(Some(node_id), "input", Some(&text))
            );
            text
        } else {
            prompt_input(node, &question, state_manager, ctx).await?
        };

        if let Some(expr) = &node.validation
            && !validate_length(&input_text, expr)?
        {
            bail!(
                "Input failed validation '{}' (got {} chars)",
                expr,
                input_text.chars().count()
            );
        }

        apply_state_updates_with_var(&node.state_updates, state_manager, INPUT_KEY, &input_text);

        node_next
            .map(String::from)
            .ok_or_else(|| anyhow!("Input node has no `next` set"))
    }
}

async fn prompt_input(
    node: &InputNode,
    question: &str,
    state_manager: &StateManager,
    ctx: &mut RequestContext,
) -> Result<String> {
    let response = handle_user_tool(
        ctx,
        &format!("{USER_FUNCTION_PREFIX}input"),
        &json!({ "question": question }),
    )
    .await
    .context("user__input failed")?;

    if let Some(err) = response.get("error").and_then(Value::as_str) {
        bail!("Input interaction failed: {err}");
    }

    let raw = response
        .get("answer")
        .and_then(Value::as_str)
        .context("Input response missing 'answer' field")?
        .to_string();

    Ok(if raw.is_empty() {
        node.default
            .as_ref()
            .map(|t| state_manager.interpolate_lenient(t))
            .unwrap_or_default()
    } else {
        raw
    })
}

fn headless_input_text(node: &InputNode, state_manager: &StateManager) -> String {
    node.default
        .as_ref()
        .map(|t| state_manager.interpolate_lenient(t))
        .filter(|default| !default.is_empty())
        .unwrap_or_else(|| HEADLESS_GUIDANCE.to_string())
}

fn build_input_question(node: &InputNode, state_manager: &StateManager) -> Result<String> {
    let mut question = state_manager
        .interpolate(&node.question)
        .context("Failed to interpolate input question")?;

    if let Some(default_template) = &node.default {
        let default = state_manager.interpolate_lenient(default_template);
        if !default.is_empty() {
            question = format!("{question} [default: {default}]");
        }
    }

    Ok(question)
}

fn resolve_approval_route(node: &ApprovalNode, choice: &str) -> Result<String> {
    if let Some(target) = node.routes.get(choice) {
        return Ok(target.clone());
    }

    Ok(node.on_other.clone())
}

fn apply_state_updates_with_var(
    updates: &Option<HashMap<String, String>>,
    state_manager: &mut StateManager,
    var_name: &str,
    var_value: &str,
) {
    let Some(updates) = updates else {
        return;
    };
    let prev = state_manager.state().get(var_name).cloned();
    state_manager
        .state_mut()
        .set(var_name.into(), Value::String(var_value.to_string()));

    for (key, template) in updates {
        let value = state_manager.interpolate_lenient(template);
        state_manager
            .state_mut()
            .set(key.clone(), Value::String(value));
    }

    match prev {
        Some(v) => state_manager.state_mut().set(var_name.into(), v),
        None => {
            state_manager.state_mut().set(var_name.into(), Value::Null);
        }
    }
}

/// Evaluate a `len(input) OP N` expression where OP is one of `>`, `>=`,
/// `<`, `<=`, `==`. Lengths are byte counts (matches Rust's `str::len`).
/// Other expressions are rejected at runtime.
fn validate_length(input: &str, expr: &str) -> Result<bool> {
    let trimmed = expr.trim();
    let after_len = trimmed
        .strip_prefix("len(input)")
        .map(str::trim)
        .ok_or_else(|| {
            anyhow!(
                "Unsupported validation expression '{expr}'; only `len(input) OP N` is supported"
            )
        })?;

    let (op, rhs_str) = if let Some(rest) = after_len.strip_prefix(">=") {
        (">=", rest)
    } else if let Some(rest) = after_len.strip_prefix("<=") {
        ("<=", rest)
    } else if let Some(rest) = after_len.strip_prefix("==") {
        ("==", rest)
    } else if let Some(rest) = after_len.strip_prefix('>') {
        (">", rest)
    } else if let Some(rest) = after_len.strip_prefix('<') {
        ("<", rest)
    } else {
        bail!("No comparison operator in validation expression '{expr}'");
    };

    let rhs: usize = rhs_str
        .trim()
        .parse()
        .with_context(|| format!("Invalid right-hand side in validation '{expr}'"))?;

    let len = input.len();
    Ok(match op {
        ">=" => len >= rhs,
        "<=" => len <= rhs,
        "==" => len == rhs,
        ">" => len > rhs,
        "<" => len < rhs,
        _ => unreachable!(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::types::*;
    use super::*;
    use crate::config::{AppState, WorkingMode};
    use crate::function::user_interaction::HeadlessModeGuard;
    use crate::supervisor::escalation::EscalationQueue;
    use serde_json::json;
    use serial_test::serial;

    fn manager_with(pairs: &[(&str, Value)]) -> StateManager {
        let mut map = HashMap::new();
        for (k, v) in pairs {
            map.insert((*k).into(), v.clone());
        }

        StateManager::new(map)
    }

    fn approval(options: &[&str], routes: &[(&str, &str)], on_other: &str) -> ApprovalNode {
        let mut r = HashMap::new();
        for (k, v) in routes {
            r.insert((*k).into(), (*v).into());
        }

        ApprovalNode {
            question: "?".into(),
            options: options.iter().map(|s| (*s).into()).collect(),
            routes: r,
            on_other: on_other.into(),
            state_updates: None,
        }
    }

    fn input(question: &str) -> InputNode {
        InputNode {
            question: question.into(),
            default: None,
            validation: None,
            state_updates: None,
        }
    }

    fn test_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    fn capture_into(key: &str, var: &str) -> Option<HashMap<String, String>> {
        Some(HashMap::from([(key.to_string(), format!("{{{{{var}}}}}"))]))
    }

    #[tokio::test]
    #[serial]
    async fn headless_approval_takes_first_option_and_routes_to_it() {
        let _mode = HeadlessModeGuard::set(true, false);
        let mut node = approval(
            &["yes", "no"],
            &[("yes", "deploy"), ("no", "cancel")],
            "clarify",
        );
        node.state_updates = capture_into("decision", CHOICE_KEY);
        let mut state = manager_with(&[]);

        let next = ApprovalNodeExecutor::execute(
            "gate",
            &node,
            &mut state,
            &mut test_ctx(),
            &HeadlessAnswerGuard::default(),
        )
        .await
        .unwrap();

        assert_eq!(next, "deploy");
        assert_eq!(state.state().get("decision"), Some(&json!("yes")));
    }

    #[test]
    fn headless_guard_gives_distinct_questions_on_one_node_the_first_option() {
        let node = approval(&["yes", "no"], &[], "clarify");
        let guard = HeadlessAnswerGuard::default();

        for i in 0..5 {
            let choice =
                headless_approval_choice("gate", &node, &format!("question {i}"), &guard).unwrap();
            assert_eq!(choice, "yes", "question {i}");
        }
    }

    #[test]
    fn headless_guard_takes_second_option_from_third_repeat_of_a_question() {
        let node = approval(&["yes", "no"], &[], "clarify");
        let guard = HeadlessAnswerGuard::default();

        let choices: Vec<String> = (0..4)
            .map(|_| headless_approval_choice("gate", &node, "same?", &guard).unwrap())
            .collect();

        assert_eq!(choices, ["yes", "yes", "no", "no"]);
    }

    #[test]
    fn headless_guard_keeps_the_only_option_on_repeats() {
        let node = approval(&["ok"], &[], "clarify");
        let guard = HeadlessAnswerGuard::default();

        for _ in 0..3 {
            assert_eq!(
                headless_approval_choice("gate", &node, "same?", &guard).unwrap(),
                "ok"
            );
        }
    }

    #[test]
    fn headless_guard_counts_start_at_zero_for_a_fresh_guard() {
        let node = approval(&["yes", "no"], &[], "clarify");
        let first_run = HeadlessAnswerGuard::default();
        for _ in 0..3 {
            headless_approval_choice("gate", &node, "same?", &first_run).unwrap();
        }

        let second_run = HeadlessAnswerGuard::default();

        assert_eq!(
            headless_approval_choice("gate", &node, "same?", &second_run).unwrap(),
            "yes"
        );
    }

    /// The backstop boundary: 25 auto-answers on one node succeed and the
    /// 26th ask fails, whatever the questions were.
    #[test]
    fn headless_guard_fails_the_26th_auto_answer_on_one_node() {
        let node = approval(&["yes", "no"], &[], "clarify");
        let guard = HeadlessAnswerGuard::default();
        for i in 0..25 {
            headless_approval_choice("gate", &node, &format!("q{i}"), &guard).unwrap();
        }

        let err = headless_approval_choice("gate", &node, "q25", &guard).unwrap_err();

        assert_eq!(
            err.to_string(),
            "headless: node gate auto-answered 25 times"
        );
        assert_eq!(
            headless_approval_choice("other", &node, "q0", &guard).unwrap(),
            "yes"
        );
    }

    #[tokio::test]
    #[serial]
    async fn headless_input_answers_count_toward_the_node_backstop() {
        let _mode = HeadlessModeGuard::set(true, false);
        let mut node = input("Name?");
        node.default = Some("alice".into());
        let guard = HeadlessAnswerGuard::default();
        let mut state = manager_with(&[]);
        for _ in 0..25 {
            InputNodeExecutor::execute(
                "ask",
                &node,
                Some("next"),
                &mut state,
                &mut test_ctx(),
                &guard,
            )
            .await
            .unwrap();
        }

        let err = InputNodeExecutor::execute(
            "ask",
            &node,
            Some("next"),
            &mut state,
            &mut test_ctx(),
            &guard,
        )
        .await
        .unwrap_err();

        assert_eq!(err.to_string(), "headless: node ask auto-answered 25 times");
    }

    async fn run_headless_input(node: &InputNode, state: &mut StateManager) -> Result<String> {
        InputNodeExecutor::execute(
            "ask",
            node,
            Some("next"),
            state,
            &mut test_ctx(),
            &HeadlessAnswerGuard::default(),
        )
        .await
    }

    #[tokio::test]
    #[serial]
    async fn headless_input_uses_the_interpolated_default() {
        let _mode = HeadlessModeGuard::set(true, false);
        let mut node = input("Name?");
        node.default = Some("{{name}}".into());
        node.state_updates = capture_into("answer", INPUT_KEY);
        let mut state = manager_with(&[("name", json!("alice"))]);

        let next = run_headless_input(&node, &mut state).await.unwrap();

        assert_eq!(next, "next");
        assert_eq!(state.state().get("answer"), Some(&json!("alice")));
    }

    #[tokio::test]
    #[serial]
    async fn headless_input_without_a_default_uses_the_guidance_text() {
        let _mode = HeadlessModeGuard::set(true, false);
        let mut node = input("Name?");
        node.state_updates = capture_into("answer", INPUT_KEY);
        let mut state = manager_with(&[]);

        run_headless_input(&node, &mut state).await.unwrap();

        assert_eq!(state.state().get("answer"), Some(&json!(HEADLESS_GUIDANCE)));
    }

    #[tokio::test]
    #[serial]
    async fn headless_input_with_a_blank_default_uses_the_guidance_text() {
        let _mode = HeadlessModeGuard::set(true, false);
        let mut node = input("Name?");
        node.default = Some("{{missing}}".into());
        node.state_updates = capture_into("answer", INPUT_KEY);
        let mut state = manager_with(&[]);

        run_headless_input(&node, &mut state).await.unwrap();

        assert_eq!(state.state().get("answer"), Some(&json!(HEADLESS_GUIDANCE)));
    }

    #[tokio::test]
    #[serial]
    async fn headless_input_still_runs_validation_on_the_chosen_text() {
        let _mode = HeadlessModeGuard::set(true, false);
        let mut passing = input("Name?");
        passing.default = Some("alice smith".into());
        passing.validation = Some("len(input) > 5".into());
        let mut failing = input("Name?");
        failing.validation = Some("len(input) < 5".into());
        let mut state = manager_with(&[]);

        run_headless_input(&passing, &mut state).await.unwrap();
        let err = run_headless_input(&failing, &mut state).await.unwrap_err();

        assert_eq!(
            err.to_string(),
            format!(
                "Input failed validation 'len(input) < 5' (got {} chars)",
                HEADLESS_GUIDANCE.chars().count()
            )
        );
    }

    #[tokio::test]
    #[serial]
    async fn non_headless_approval_still_asks_and_routes_on_the_human_answer() {
        let _mode = HeadlessModeGuard::set(false, false);
        let node = approval(
            &["yes", "no"],
            &[("yes", "deploy"), ("no", "cancel")],
            "clarify",
        );
        let queue = Arc::new(EscalationQueue::new());
        let mut ctx = test_ctx();
        ctx.current_depth = 1;
        ctx.escalation_queue = Some(Arc::clone(&queue));
        let mut state = manager_with(&[]);

        let guard = HeadlessAnswerGuard::default();
        let ask = ApprovalNodeExecutor::execute("gate", &node, &mut state, &mut ctx, &guard);
        let reply = async {
            for _ in 0..1000 {
                if let Some(entry) = queue.pending_summary().first() {
                    let id = entry["escalation_id"].as_str().unwrap().to_string();
                    let request = queue.take(&id).unwrap();
                    let _ = request.reply_tx.send("no".to_string());
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("approval question must reach the escalation queue");
        };
        let (next, ()) = tokio::join!(ask, reply);

        assert_eq!(next.unwrap(), "cancel");
    }

    #[tokio::test]
    #[serial]
    async fn acp_approval_still_fails_on_the_missing_answer_and_queues_permission() {
        let _mode = HeadlessModeGuard::set(true, true);
        let mut node = approval(&["yes", "no"], &[("yes", "deploy")], "clarify");
        node.question = "acp approval unchanged-m3d".into();
        let mut state = manager_with(&[]);

        let err = ApprovalNodeExecutor::execute(
            "gate",
            &node,
            &mut state,
            &mut test_ctx(),
            &HeadlessAnswerGuard::default(),
        )
        .await
        .unwrap_err();

        assert_eq!(err.to_string(), "Approval response missing 'answer' field");
        let queued = crate::utils::drain_acp_permissions()
            .into_iter()
            .filter(|p| p["question"] == "acp approval unchanged-m3d")
            .count();
        assert_eq!(queued, 1);
    }

    #[test]
    fn validate_length_supports_all_comparison_operators() {
        assert!(validate_length("hello", "len(input) > 0").unwrap());
        assert!(!validate_length("", "len(input) > 0").unwrap());
        assert!(validate_length("hello", "len(input) >= 5").unwrap());
        assert!(!validate_length("hi", "len(input) >= 5").unwrap());
        assert!(validate_length("hello", "len(input) < 10").unwrap());
        assert!(!validate_length("hello world!", "len(input) < 10").unwrap());
        assert!(validate_length("hi", "len(input) <= 2").unwrap());
        assert!(!validate_length("hello", "len(input) <= 2").unwrap());
        assert!(validate_length("hello", "len(input) == 5").unwrap());
        assert!(!validate_length("hello", "len(input) == 3").unwrap());
    }

    #[test]
    fn validate_length_handles_whitespace() {
        assert!(validate_length("hi", "  len(input)   >=   1  ").unwrap());
    }

    #[test]
    fn validate_length_rejects_unsupported_expressions() {
        assert!(validate_length("x", "matches /[a-z]+/").is_err());
        assert!(validate_length("x", "len(input)").is_err());
        assert!(validate_length("x", "len(input) >").is_err());
        assert!(validate_length("x", "len(input) >= abc").is_err());
    }

    #[test]
    fn approval_route_lookup_returns_target_on_match() {
        let node = approval(
            &["yes", "no"],
            &[("yes", "deploy"), ("no", "cancel")],
            "clarify",
        );

        assert_eq!(resolve_approval_route(&node, "yes").unwrap(), "deploy");
        assert_eq!(resolve_approval_route(&node, "no").unwrap(), "cancel");
    }

    #[test]
    fn approval_route_lookup_falls_back_to_on_other_for_unknown_choice() {
        let node = approval(
            &["yes", "no"],
            &[("yes", "deploy"), ("no", "cancel")],
            "clarify",
        );

        assert_eq!(resolve_approval_route(&node, "maybe").unwrap(), "clarify");
        assert_eq!(
            resolve_approval_route(&node, "free-form text").unwrap(),
            "clarify"
        );
    }

    #[test]
    fn state_updates_expose_choice_during_evaluation_only() {
        let mut updates = HashMap::new();
        updates.insert("decision".into(), "{{choice}}".into());
        let mut state = manager_with(&[]);

        apply_state_updates_with_var(&Some(updates), &mut state, CHOICE_KEY, "approve");

        assert_eq!(state.state().get("decision"), Some(&json!("approve")));
        assert_eq!(state.state().get(CHOICE_KEY), Some(&Value::Null));
    }

    #[test]
    fn state_updates_preserve_pre_existing_var_value() {
        let mut updates = HashMap::new();
        updates.insert("decision".into(), "{{choice}}".into());
        let mut state = manager_with(&[("choice", json!("preserved"))]);

        apply_state_updates_with_var(&Some(updates), &mut state, CHOICE_KEY, "approve");

        assert_eq!(state.state().get("decision"), Some(&json!("approve")));
        assert_eq!(state.state().get(CHOICE_KEY), Some(&json!("preserved")));
    }

    #[test]
    fn state_updates_for_input_use_input_key() {
        let mut updates = HashMap::new();
        updates.insert("api_key".into(), "{{input}}".into());
        let mut state = manager_with(&[]);

        apply_state_updates_with_var(&Some(updates), &mut state, INPUT_KEY, "sk-12345");

        assert_eq!(state.state().get("api_key"), Some(&json!("sk-12345")));
        assert_eq!(state.state().get(INPUT_KEY), Some(&Value::Null));
    }

    #[test]
    fn input_question_appends_default_when_present() {
        let state = manager_with(&[("name", json!("alice"))]);
        let mut node = input("Hi, what's your name?");
        node.default = Some("{{name}}".into());

        let q = build_input_question(&node, &state).unwrap();

        assert_eq!(q, "Hi, what's your name? [default: alice]");
    }

    #[test]
    fn input_question_omits_default_when_blank_after_interpolation() {
        let state = manager_with(&[]);
        let mut node = input("Enter value:");
        node.default = Some("{{missing}}".into());

        let q = build_input_question(&node, &state).unwrap();

        assert_eq!(q, "Enter value:");
    }

    #[test]
    fn input_question_uses_no_default_when_field_absent() {
        let state = manager_with(&[]);
        let node = input("Enter value:");

        let q = build_input_question(&node, &state).unwrap();

        assert_eq!(q, "Enter value:");
    }

    #[test]
    fn no_state_updates_means_var_never_appears_in_state() {
        let mut state = manager_with(&[]);

        apply_state_updates_with_var(&None, &mut state, CHOICE_KEY, "approve");

        assert!(state.state().get(CHOICE_KEY).is_none());
    }
}
