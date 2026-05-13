//! Execution of `approval` and `input` graph nodes via Loki's existing
//! user-interaction system (`user__ask`, `user__input`).
//!
//! Both delegate to [`crate::function::user_interaction::handle_user_tool`],
//! which prompts the user directly at depth 0 (via `inquire`) and escalates
//! to the parent through the escalation queue at depth > 0. We interpret the
//! returned JSON's `answer` field for the user's response and an `error`
//! field for escalation timeout/cancellation.

use super::state::StateManager;
use super::types::{ApprovalNode, InputNode};
use crate::config::RequestContext;
use crate::function::user_interaction::{USER_FUNCTION_PREFIX, handle_user_tool};
use anyhow::{Context, Result, bail, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;

const CHOICE_KEY: &str = "choice";
const INPUT_KEY: &str = "input";

pub struct ApprovalNodeExecutor;

impl ApprovalNodeExecutor {
    /// Prompt the user with the (templated) question and routes the
    /// selected option through the node's `routes` map. Returns the next
    /// node ID. On escalation timeout/error the node routes to
    /// `on_timeout` if set, otherwise propagates the failure.
    pub async fn execute(
        node: &ApprovalNode,
        state_manager: &mut StateManager,
        ctx: &mut RequestContext,
    ) -> Result<String> {
        let question = state_manager
            .interpolate(&node.question)
            .context("Failed to interpolate approval question")?;

        let response = handle_user_tool(
            ctx,
            &format!("{USER_FUNCTION_PREFIX}ask"),
            &json!({ "question": question, "options": node.options }),
        )
        .await
        .context("user__ask failed")?;

        if let Some(err) = response.get("error").and_then(Value::as_str) {
            if let Some(on_timeout) = &node.on_timeout {
                return Ok(on_timeout.clone());
            }
            bail!("Approval interaction failed: {err}");
        }

        let choice = response
            .get("answer")
            .and_then(Value::as_str)
            .context("Approval response missing 'answer' field")?
            .to_string();

        apply_state_updates_with_var(&node.state_updates, state_manager, CHOICE_KEY, &choice);

        resolve_approval_route(node, &choice)
    }
}

pub struct InputNodeExecutor;

impl InputNodeExecutor {
    /// Prompt the user for free-form text. If a `default` is configured
    /// and the user submits an empty response, the default is substituted.
    /// Optional `validation` is evaluated against the final value. Returns
    /// `node_next` (the parent `Node.next`) on success, or `on_timeout` on
    /// escalation timeout/error.
    pub async fn execute(
        node: &InputNode,
        node_next: Option<&str>,
        state_manager: &mut StateManager,
        ctx: &mut RequestContext,
    ) -> Result<String> {
        let question = build_input_question(node, state_manager)?;

        let response = handle_user_tool(
            ctx,
            &format!("{USER_FUNCTION_PREFIX}input"),
            &json!({ "question": question }),
        )
        .await
        .context("user__input failed")?;

        if let Some(err) = response.get("error").and_then(Value::as_str) {
            if let Some(on_timeout) = &node.on_timeout {
                return Ok(on_timeout.clone());
            }
            bail!("Input interaction failed: {err}");
        }

        let raw = response
            .get("answer")
            .and_then(Value::as_str)
            .context("Input response missing 'answer' field")?
            .to_string();

        let input_text = if raw.is_empty() {
            node.default
                .as_ref()
                .map(|t| state_manager.interpolate_lenient(t))
                .unwrap_or_default()
        } else {
            raw
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
    node.routes.get(choice).cloned().ok_or_else(|| {
        let mut available: Vec<&str> = node.routes.keys().map(String::as_str).collect();
        available.sort();
        anyhow!(
            "No route defined for choice '{choice}'. Available routes: {}",
            available.join(", ")
        )
    })
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
    use serde_json::json;

    fn manager_with(pairs: &[(&str, Value)]) -> StateManager {
        let mut map = HashMap::new();
        for (k, v) in pairs {
            map.insert((*k).into(), v.clone());
        }
        StateManager::new(map)
    }

    fn approval(options: &[&str], routes: &[(&str, &str)]) -> ApprovalNode {
        let mut r = HashMap::new();
        for (k, v) in routes {
            r.insert((*k).into(), (*v).into());
        }
        ApprovalNode {
            question: "?".into(),
            options: options.iter().map(|s| (*s).into()).collect(),
            routes: r,
            state_updates: None,
            timeout: None,
            on_timeout: None,
        }
    }

    fn input(question: &str) -> InputNode {
        InputNode {
            question: question.into(),
            default: None,
            validation: None,
            state_updates: None,
            timeout: None,
            on_timeout: None,
        }
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
        let node = approval(&["yes", "no"], &[("yes", "deploy"), ("no", "cancel")]);
        assert_eq!(resolve_approval_route(&node, "yes").unwrap(), "deploy");
        assert_eq!(resolve_approval_route(&node, "no").unwrap(), "cancel");
    }

    #[test]
    fn approval_route_lookup_errors_on_unknown_choice() {
        let node = approval(&["yes", "no"], &[("yes", "deploy"), ("no", "cancel")]);
        let err = resolve_approval_route(&node, "maybe")
            .unwrap_err()
            .to_string();
        assert!(err.contains("'maybe'"), "got: {err}");
        assert!(err.contains("yes") && err.contains("no"), "got: {err}");
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
