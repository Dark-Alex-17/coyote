use crate::client::call_chat_completions;
use crate::config::{Input, RequestContext, Role, RoleLike};
use crate::utils::create_abort_signal;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::sync::Arc;

const EXTRACTOR_ROLE_NAME: &str = "__structured_output__";

const EXTRACTOR_ROLE_PROMPT: &str = "\
Extract a JSON object from the user's input that strictly conforms to the provided JSON Schema.

Rules:
- Output ONLY the JSON object. No prose, no explanation, no markdown fences, no <think> tokens.
- The first character of your response must be `{` and the last must be `}`.
- Every key marked `required` in the schema MUST appear in the output.
- All values MUST match the types specified in the schema.
- If the input is already a valid JSON object matching the schema, return it unchanged.
- If a field cannot be determined from the input, use `null` (when allowed) or your best inferred value.
- Do NOT invent fields not present in the schema.";

pub async fn extract(raw: &str, schema: &Value, parent_ctx: &mut RequestContext) -> Result<Value> {
    if let Some(parsed) = try_parse_json(raw) {
        return Ok(parsed);
    }

    extract_via_extractor(raw, schema, parent_ctx, false).await
}

async fn extract_via_extractor(
    raw: &str,
    schema: &Value,
    parent_ctx: &mut RequestContext,
    is_repair: bool,
) -> Result<Value> {
    let role = build_extractor_role()?;
    let prompt = build_extractor_prompt(raw, schema, is_repair);

    let saved_role = parent_ctx.role.clone();
    parent_ctx.role = Some(role);
    let result = run_one_shot(&prompt, parent_ctx).await;
    parent_ctx.role = saved_role;

    let output = result.context("Structured-output extractor LLM call failed")?;

    match try_parse_json(&output) {
        Some(value) => Ok(value),
        None if is_repair => bail!(
            "Structured-output extractor failed to produce valid JSON after repair retry. \
             Last response:\n{output}"
        ),
        None => Box::pin(extract_via_extractor(&output, schema, parent_ctx, true)).await,
    }
}

fn build_extractor_role() -> Result<Role> {
    let mut role = Role::new(EXTRACTOR_ROLE_NAME, EXTRACTOR_ROLE_PROMPT);
    role.set_enabled_tools(Some(Vec::new()));
    role.set_enabled_mcp_servers(Some(Vec::new()));
    Ok(role)
}

fn build_extractor_prompt(raw: &str, schema: &Value, is_repair: bool) -> String {
    let schema_json = serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string());
    if is_repair {
        format!(
            "Your previous response was not valid JSON. Output ONLY a JSON object \
             matching this schema. No prose, no fences.\n\nSchema:\n{schema_json}\n\nInput:\n{raw}"
        )
    } else {
        format!("Schema:\n{schema_json}\n\nInput:\n{raw}")
    }
}

async fn run_one_shot(prompt: &str, ctx: &mut RequestContext) -> Result<String> {
    let abort = create_abort_signal();
    let app_cfg = Arc::clone(&ctx.app.config);
    let role_for_input = ctx.role.clone();
    let input = Input::from_str(ctx, prompt, role_for_input)?;
    let client = input.create_client()?;
    ctx.before_chat_completion(&input)?;
    let (output, tool_results) =
        call_chat_completions(&input, false, false, client.as_ref(), ctx, abort).await?;
    ctx.after_chat_completion(app_cfg.as_ref(), &input, &output, &tool_results)?;

    Ok(output)
}

fn try_parse_json(raw: &str) -> Option<Value> {
    let cleaned = strip_code_fences(raw.trim());

    serde_json::from_str(cleaned).ok()
}

fn strip_code_fences(s: &str) -> &str {
    let after_open = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .map(str::trim_start)
        .unwrap_or(s);
    after_open
        .strip_suffix("```")
        .map(str::trim_end)
        .unwrap_or(after_open)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn try_parse_json_accepts_plain_object() {
        let v = try_parse_json(r#"{"a": 1}"#).unwrap();

        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn try_parse_json_strips_json_fences() {
        let raw = "```json\n{\"a\": 1}\n```";

        let v = try_parse_json(raw).unwrap();

        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn try_parse_json_strips_bare_fences() {
        let raw = "```\n{\"a\": 1}\n```";

        let v = try_parse_json(raw).unwrap();

        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn try_parse_json_tolerates_whitespace() {
        let v = try_parse_json("   \n  {\"x\": true}\n\n").unwrap();

        assert_eq!(v, json!({"x": true}));
    }

    #[test]
    fn try_parse_json_returns_none_on_prose() {
        assert!(try_parse_json("Here is the result: it's good").is_none());
    }

    #[test]
    fn try_parse_json_returns_none_on_partial_json() {
        assert!(try_parse_json("{\"a\": ").is_none());
    }

    #[test]
    fn try_parse_json_accepts_arrays() {
        let v = try_parse_json("[1, 2, 3]").unwrap();

        assert_eq!(v, json!([1, 2, 3]));
    }

    #[test]
    fn build_extractor_prompt_includes_schema_and_input() {
        let schema = json!({"type": "object"});

        let prompt = build_extractor_prompt("hello", &schema, false);

        assert!(prompt.contains("Schema:"));
        assert!(prompt.contains("Input:"));
        assert!(prompt.contains("hello"));
    }

    #[test]
    fn build_extractor_prompt_repair_includes_repair_instruction() {
        let schema = json!({"type": "object"});

        let prompt = build_extractor_prompt("oops", &schema, true);

        assert!(prompt.contains("previous response"));
        assert!(prompt.contains("oops"));
    }

    #[test]
    fn build_extractor_role_disables_tools_and_mcp() {
        let role = build_extractor_role().expect("builtin role must exist");

        assert_eq!(role.enabled_tools().as_deref(), Some([].as_slice()));
        assert_eq!(role.enabled_mcp_servers().as_deref(), Some([].as_slice()));
    }
}
