use super::llm::{CompletionFuture, quiet_transport};
use crate::client::ApiStatusError;
use crate::client::structured_output::{SchemaDialect, StructuredOutputSpec, strip_injected_nulls};
use crate::config::{Input, RequestContext, Role, RoleLike};
use crate::utils::AbortSignal;
use anyhow::{Context, Result, bail};
use log::warn;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

const EXTRACTOR_ROLE_NAME: &str = "__structured_output__";

const EXTRACTOR_SCHEMA_NAME: &str = "structured_output";

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

/// Native structured-output bookkeeping for one node run. A rejection is
/// sticky: once the provider refuses the format, later calls in the same
/// run go out without it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct NativeFormatState {
    pub rejected: bool,
    pub last_dialect: Option<SchemaDialect>,
    pub final_turn: Option<(String, SchemaDialect)>,
}

/// The extractor's model call, injectable so tests can fail the first
/// attempt without a provider.
type ExtractorTransport<'f> =
    dyn for<'b> FnMut(&'b Input, &'b mut RequestContext) -> CompletionFuture<'b> + Send + 'f;

/// Sends `input` with its native format when `dialect` is set and the run
/// has not seen a rejection. A 400 or an undecodable response is taken as
/// the provider refusing the format: the call is repeated once without it.
/// Every other error propagates after the first call.
pub(crate) async fn with_native_format_fallback<'a, T>(
    dialect: Option<SchemaDialect>,
    input: &'a Input,
    ctx: &'a mut RequestContext,
    mut call: impl for<'b> FnMut(
        &'b Input,
        &'b mut RequestContext,
    ) -> Pin<Box<dyn Future<Output = Result<T>> + Send + 'b>>,
) -> Result<T> {
    let dialect = if ctx.native_format.rejected {
        None
    } else {
        dialect
    };
    let Some(dialect) = dialect else {
        let result = if input.role().structured_output().is_some() {
            let plain = input.without_structured_output();
            call(&plain, ctx).await
        } else {
            call(input, ctx).await
        };
        ctx.native_format.last_dialect = None;
        return result;
    };

    let err = match call(input, ctx).await {
        Ok(out) => {
            ctx.native_format.last_dialect = Some(dialect);
            return Ok(out);
        }
        Err(err) => err,
    };
    let Some(reason) = format_rejection_reason(&err) else {
        return Err(err);
    };
    warn!("provider rejected the native structured-output format ({reason}); retrying without it");
    ctx.native_format.rejected = true;
    let plain = input.without_structured_output();
    let result = call(&plain, ctx).await;
    ctx.native_format.last_dialect = None;
    result
}

fn format_rejection_reason(err: &anyhow::Error) -> Option<String> {
    err.chain().find_map(|cause| {
        if let Some(api) = cause.downcast_ref::<ApiStatusError>() {
            (api.status == 400).then(|| api.message.clone())
        } else {
            cause
                .downcast_ref::<reqwest::Error>()
                .filter(|e| e.is_decode())
                .map(|_| format!("{err:#}"))
        }
    })
}

/// `producer_dialect` is the native format the model that wrote `raw` ran
/// under; OpenAI strict mode forces optional properties into nullable
/// required ones, so those injected nulls are stripped again.
pub async fn extract(
    raw: &str,
    schema: &Value,
    parent_ctx: &mut RequestContext,
    abort: &AbortSignal,
    producer_dialect: Option<SchemaDialect>,
) -> Result<Value> {
    if let Some(mut parsed) = try_parse_json(raw) {
        strip_nulls_for(producer_dialect, &mut parsed, schema);
        return Ok(parsed);
    }

    let mut transport = quiet_transport(abort.clone());
    extract_via_extractor(raw, schema, parent_ctx, abort, &mut transport).await
}

pub(crate) fn strip_nulls_for(dialect: Option<SchemaDialect>, value: &mut Value, schema: &Value) {
    if dialect == Some(SchemaDialect::OpenAiStrict) {
        strip_injected_nulls(value, schema);
    }
}

/// Runs the extractor, repair pass included, on a fresh native-format state
/// so a rejection seen by the first pass also holds for the repair pass,
/// then hands the caller its own state back.
async fn extract_via_extractor(
    raw: &str,
    schema: &Value,
    parent_ctx: &mut RequestContext,
    abort: &AbortSignal,
    transport: &mut ExtractorTransport<'_>,
) -> Result<Value> {
    let saved_native_format = std::mem::take(&mut parent_ctx.native_format);
    let result = extract_with_repair(raw, schema, parent_ctx, abort, false, transport).await;
    parent_ctx.native_format = saved_native_format;
    result
}

async fn extract_with_repair(
    raw: &str,
    schema: &Value,
    parent_ctx: &mut RequestContext,
    abort: &AbortSignal,
    is_repair: bool,
    transport: &mut ExtractorTransport<'_>,
) -> Result<Value> {
    let mut role = build_extractor_role(parent_ctx);
    role.set_structured_output(Some(StructuredOutputSpec {
        name: EXTRACTOR_SCHEMA_NAME.to_string(),
        schema: schema.clone(),
    }));
    let prompt = build_extractor_prompt(raw, schema, is_repair);

    let saved_role = parent_ctx.role.clone();
    parent_ctx.role = Some(role);
    let result = run_one_shot(&prompt, parent_ctx, abort, transport).await;
    parent_ctx.role = saved_role;
    let extractor_dialect = parent_ctx.native_format.last_dialect;

    let output = result.context("Structured-output extractor LLM call failed")?;

    match try_parse_json(&output) {
        Some(mut value) => {
            strip_nulls_for(extractor_dialect, &mut value, schema);
            Ok(value)
        }
        None if is_repair => bail!(
            "Structured-output extractor failed to produce valid JSON after repair retry. \
             Last response:\n{output}"
        ),
        None => {
            Box::pin(extract_with_repair(
                &output, schema, parent_ctx, abort, true, transport,
            ))
            .await
        }
    }
}

fn build_extractor_role(ctx: &RequestContext) -> Role {
    let mut role = Role::new(EXTRACTOR_ROLE_NAME, EXTRACTOR_ROLE_PROMPT);
    role.set_model(ctx.current_model().clone());
    role.set_enabled_tools(Some(Vec::new()));
    role.set_enabled_mcp_servers(Some(Vec::new()));
    role
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

async fn run_one_shot(
    prompt: &str,
    ctx: &mut RequestContext,
    abort: &AbortSignal,
    transport: &mut ExtractorTransport<'_>,
) -> Result<String> {
    if abort.aborted() {
        bail!("Aborted.");
    }
    let app_cfg = Arc::clone(&ctx.app.config);
    let role_for_input = ctx.role.clone();
    let input = Input::from_str(ctx, prompt, role_for_input)?;
    let client = input.create_client()?;
    let dialect = input.native_structured_output(client.as_ref());
    ctx.before_chat_completion(&input)?;
    let (output, tool_results) =
        with_native_format_fallback(dialect, &input, ctx, transport).await?;
    ctx.after_chat_completion(app_cfg.as_ref(), &input, &output, &tool_results)?;

    Ok(output)
}

pub(crate) fn try_parse_json(raw: &str) -> Option<Value> {
    let cleaned = strip_code_fences(strip_thinking_blocks(raw.trim()));
    serde_json::from_str(cleaned).ok()
}

fn strip_thinking_blocks(s: &str) -> &str {
    let mut s = s.trim_start();
    while s.starts_with("<think>") {
        match s.find("</think>") {
            Some(end) => s = s[end + "</think>".len()..].trim_start(),
            None => break,
        }
    }
    s
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
    use crate::client::{ClientConfig, Model, ModelData};
    use crate::config::{AppConfig, AppState, WorkingMode};
    use crate::graph::llm::test_support::boxed_completion;
    use crate::testing::{install_log_collector, warn_snapshot};
    use crate::utils::create_abort_signal;
    use serde_json::json;

    fn make_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    fn format_call<F>(f: F) -> F
    where
        F: for<'b> FnMut(&'b Input, &'b mut RequestContext) -> CompletionFuture<'b> + Send,
    {
        f
    }

    fn answer_spec() -> StructuredOutputSpec {
        StructuredOutputSpec {
            name: "answer".to_string(),
            schema: json!({"type": "object"}),
        }
    }

    fn input_with_spec(ctx: &RequestContext) -> Input {
        let mut role = Role::new("r", "");
        role.set_structured_output(Some(answer_spec()));
        Input::from_str(ctx, "hi", Some(role)).unwrap()
    }

    fn api_error(status: u16, message: &str) -> anyhow::Error {
        anyhow::Error::new(ApiStatusError {
            status,
            message: message.to_string(),
        })
        .context("Failed to call chat-completions api")
    }

    /// Runs the wrapper over a fake that fails its first call with
    /// `first_err` (when given) and answers "ok" otherwise; returns the
    /// result and the spec each call was sent with.
    async fn run_wrapper(
        dialect: Option<SchemaDialect>,
        input: &Input,
        ctx: &mut RequestContext,
        first_err: Option<anyhow::Error>,
    ) -> (Result<String>, Vec<Option<StructuredOutputSpec>>) {
        let mut first_err = first_err;
        let mut seen = Vec::new();
        let result = with_native_format_fallback(
            dialect,
            input,
            ctx,
            format_call(|input, _ctx| {
                seen.push(input.role().structured_output().cloned());
                let outcome = match first_err.take() {
                    Some(err) => Err(err),
                    None => Ok(("ok".to_string(), vec![])),
                };
                boxed_completion(async move { outcome })
            }),
        )
        .await
        .map(|(output, _)| output);
        (result, seen)
    }

    #[tokio::test]
    async fn native_format_fallback_without_a_dialect_calls_once_without_the_format() {
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);

        let (result, seen) = run_wrapper(None, &input, &mut ctx, None).await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(seen, vec![None]);
        assert_eq!(ctx.native_format, NativeFormatState::default());
    }

    #[tokio::test]
    async fn native_format_fallback_records_the_dialect_on_success() {
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);

        let (result, seen) =
            run_wrapper(Some(SchemaDialect::OpenAiStrict), &input, &mut ctx, None).await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(seen, vec![Some(answer_spec())]);
        assert_eq!(
            ctx.native_format.last_dialect,
            Some(SchemaDialect::OpenAiStrict)
        );
        assert!(!ctx.native_format.rejected);
    }

    #[tokio::test]
    async fn native_format_fallback_retries_once_without_the_format_on_a_400() {
        install_log_collector();
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);
        let marker = "schema keyword unsupported 7f3a";

        let (result, seen) = run_wrapper(
            Some(SchemaDialect::OpenAiStrict),
            &input,
            &mut ctx,
            Some(api_error(400, marker)),
        )
        .await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(seen, vec![Some(answer_spec()), None]);
        assert!(ctx.native_format.rejected);
        assert_eq!(ctx.native_format.last_dialect, None);
        let expected = format!(
            "provider rejected the native structured-output format ({marker}); retrying without it"
        );
        assert!(
            warn_snapshot().iter().any(|w| w == &expected),
            "missing warning: {expected}"
        );
    }

    #[tokio::test]
    async fn native_format_rejection_sticks_until_the_state_is_reset() {
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);
        let dialect = Some(SchemaDialect::OpenAiStrict);
        run_wrapper(dialect, &input, &mut ctx, Some(api_error(400, "rejected")))
            .await
            .0
            .unwrap();

        let (_, sticky) = run_wrapper(dialect, &input, &mut ctx, None).await;
        ctx.native_format = Default::default();
        let (_, fresh) = run_wrapper(dialect, &input, &mut ctx, None).await;

        assert_eq!(sticky, vec![None]);
        assert_eq!(fresh, vec![Some(answer_spec())]);
    }

    #[tokio::test]
    async fn native_format_fallback_propagates_other_statuses_after_one_call() {
        for status in [500, 429] {
            let mut ctx = make_ctx();
            let input = input_with_spec(&ctx);

            let (result, seen) = run_wrapper(
                Some(SchemaDialect::OpenAiStrict),
                &input,
                &mut ctx,
                Some(api_error(status, "busy")),
            )
            .await;

            let err = result.expect_err("a non-400 status must propagate");
            assert_eq!(
                err.chain()
                    .find_map(|c| c.downcast_ref::<ApiStatusError>())
                    .map(|e| e.status),
                Some(status)
            );
            assert_eq!(seen, vec![Some(answer_spec())], "status {status}");
            assert!(!ctx.native_format.rejected);
        }
    }

    #[tokio::test]
    async fn native_format_fallback_retries_after_a_decode_error() {
        let decode = decode_error().await;
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);

        let (result, seen) = run_wrapper(
            Some(SchemaDialect::OpenAiStrict),
            &input,
            &mut ctx,
            Some(anyhow::Error::new(decode).context("Failed to call chat-completions api")),
        )
        .await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(seen, vec![Some(answer_spec()), None]);
        assert!(ctx.native_format.rejected);
    }

    async fn decode_error() -> reqwest::Error {
        let decode = reqwest::Response::from(http::Response::new("not json"))
            .json::<Value>()
            .await
            .expect_err("a non-JSON body must fail to decode");
        assert!(decode.is_decode());
        decode
    }

    fn api_status(err: &anyhow::Error) -> Option<(u16, String)> {
        err.chain()
            .find_map(|c| c.downcast_ref::<ApiStatusError>())
            .map(|e| (e.status, e.message.clone()))
    }

    #[tokio::test]
    async fn inactive_without_dialect_propagates_400_after_one_call() {
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);

        let (result, seen) =
            run_wrapper(None, &input, &mut ctx, Some(api_error(400, "bad request"))).await;

        let err = result.expect_err("an inactive wrapper must not retry");
        assert_eq!(api_status(&err), Some((400, "bad request".to_string())));
        assert_eq!(seen, vec![None]);
        assert!(!ctx.native_format.rejected);
    }

    #[tokio::test]
    async fn inactive_when_rejected_propagates_400_after_one_call() {
        let mut ctx = make_ctx();
        ctx.native_format.rejected = true;
        let input = input_with_spec(&ctx);

        let (result, seen) = run_wrapper(
            Some(SchemaDialect::OpenAiStrict),
            &input,
            &mut ctx,
            Some(api_error(400, "bad request")),
        )
        .await;

        let err = result.expect_err("an inactive wrapper must not retry");
        assert_eq!(api_status(&err), Some((400, "bad request".to_string())));
        assert_eq!(seen, vec![None]);
        assert!(ctx.native_format.rejected);
    }

    #[tokio::test]
    async fn inactive_without_dialect_propagates_decode_error_after_one_call() {
        let decode = decode_error().await;
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);

        let (result, seen) = run_wrapper(
            None,
            &input,
            &mut ctx,
            Some(anyhow::Error::new(decode).context("Failed to call chat-completions api")),
        )
        .await;

        let err = result.expect_err("an inactive wrapper must not retry");
        assert!(
            err.chain()
                .filter_map(|c| c.downcast_ref::<reqwest::Error>())
                .any(reqwest::Error::is_decode),
            "decode error missing from the chain: {err:#}"
        );
        assert_eq!(seen, vec![None]);
        assert!(!ctx.native_format.rejected);
    }

    fn structured_ctx() -> RequestContext {
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            clients: vec![ClientConfig::OpenAIConfig(Default::default())],
            ..AppConfig::default()
        });
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        let mut data = ModelData::new("structured");
        data.supports_structured_output = true;
        let mut role = Role::new("parent", "");
        role.set_model(Model::from_config("openai", &[data]).remove(0));
        ctx.role = Some(role);
        ctx
    }

    #[tokio::test]
    async fn extractor_retries_without_the_format_after_a_400_and_parses_the_reply() {
        let mut ctx = structured_ctx();
        let schema = json!({"type": "object", "properties": {"a": {"type": "integer"}}});
        let mut seen = Vec::new();
        let mut transport = format_call(|input, _ctx| {
            seen.push(input.role().structured_output().cloned());
            let first = seen.len() == 1;
            boxed_completion(async move {
                if first {
                    Err(api_error(400, "rejected"))
                } else {
                    Ok((r#"{"a": 1}"#.to_string(), vec![]))
                }
            })
        });

        let value = extract_via_extractor(
            "prose",
            &schema,
            &mut ctx,
            &create_abort_signal(),
            &mut transport,
        )
        .await
        .unwrap();

        assert_eq!(value, json!({"a": 1}));
        let spec = StructuredOutputSpec {
            name: "structured_output".to_string(),
            schema,
        };
        assert_eq!(seen, vec![Some(spec), None]);
    }

    #[tokio::test]
    async fn extractor_repair_pass_keeps_the_rejection_from_the_first_pass() {
        let mut ctx = structured_ctx();
        let schema = json!({"type": "object"});
        let mut seen = Vec::new();
        let mut transport = format_call(|input, _ctx| {
            seen.push(input.role().structured_output().cloned());
            let call = seen.len();
            boxed_completion(async move {
                match call {
                    1 => Err(api_error(400, "rejected")),
                    2 => Ok(("not json".to_string(), vec![])),
                    _ => Ok((r#"{"a": 1}"#.to_string(), vec![])),
                }
            })
        });

        let value = extract_via_extractor(
            "prose",
            &schema,
            &mut ctx,
            &create_abort_signal(),
            &mut transport,
        )
        .await
        .unwrap();

        assert_eq!(value, json!({"a": 1}));
        let spec = StructuredOutputSpec {
            name: "structured_output".to_string(),
            schema,
        };
        assert_eq!(seen, vec![Some(spec), None, None]);
        assert_eq!(ctx.native_format, NativeFormatState::default());
    }

    #[tokio::test]
    async fn extractor_restores_the_callers_native_format_state() {
        let caller = NativeFormatState {
            rejected: true,
            last_dialect: None,
            final_turn: None,
        };
        for succeeds in [true, false] {
            let mut ctx = structured_ctx();
            ctx.native_format = caller.clone();
            let mut seen = Vec::new();
            let mut transport = format_call(|_input, ctx| {
                seen.push(ctx.native_format.clone());
                boxed_completion(async move {
                    if succeeds {
                        Ok((r#"{"a": 1}"#.to_string(), vec![]))
                    } else {
                        Err(api_error(500, "down"))
                    }
                })
            });

            let result = extract_via_extractor(
                "prose",
                &json!({"type": "object"}),
                &mut ctx,
                &create_abort_signal(),
                &mut transport,
            )
            .await;

            assert_eq!(result.is_ok(), succeeds);
            assert_eq!(seen, vec![NativeFormatState::default()]);
            assert_eq!(ctx.native_format, caller, "succeeds {succeeds}");
        }
    }

    fn groups_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "groups": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "domain": {"type": "string"},
                            "files": {"type": "array", "items": {"type": "string"}},
                            "rationale": {"type": "string"}
                        },
                        "required": ["domain", "files"]
                    }
                }
            },
            "required": ["groups"]
        })
    }

    const GROUPS_WITH_NULL: &str = r#"{"groups":[{"domain":"a","files":[],"rationale":null}]}"#;

    #[tokio::test]
    async fn extract_strips_injected_nulls_for_an_openai_strict_producer() {
        let mut ctx = make_ctx();

        let value = extract(
            GROUPS_WITH_NULL,
            &groups_schema(),
            &mut ctx,
            &create_abort_signal(),
            Some(SchemaDialect::OpenAiStrict),
        )
        .await
        .unwrap();

        assert_eq!(value, json!({"groups": [{"domain": "a", "files": []}]}));
    }

    #[tokio::test]
    async fn extract_keeps_nulls_without_a_producer_dialect() {
        let mut ctx = make_ctx();

        let value = extract(
            GROUPS_WITH_NULL,
            &groups_schema(),
            &mut ctx,
            &create_abort_signal(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            value,
            json!({"groups": [{"domain": "a", "files": [], "rationale": null}]})
        );
    }

    #[tokio::test]
    async fn extractor_strips_nulls_when_its_own_call_ran_openai_strict() {
        let mut ctx = structured_ctx();
        let mut transport = format_call(|_input, _ctx| {
            boxed_completion(async { Ok((GROUPS_WITH_NULL.to_string(), vec![])) })
        });

        let value = extract_via_extractor(
            "prose",
            &groups_schema(),
            &mut ctx,
            &create_abort_signal(),
            &mut transport,
        )
        .await
        .unwrap();

        assert_eq!(value, json!({"groups": [{"domain": "a", "files": []}]}));
    }

    #[tokio::test]
    async fn extractor_keeps_nulls_when_its_own_call_ran_without_the_format() {
        let mut ctx = structured_ctx();
        ctx.native_format.last_dialect = Some(SchemaDialect::OpenAiStrict);
        let mut calls = 0;
        let mut transport = format_call(|_input, _ctx| {
            calls += 1;
            let first = calls == 1;
            boxed_completion(async move {
                if first {
                    Err(api_error(400, "rejected"))
                } else {
                    Ok((GROUPS_WITH_NULL.to_string(), vec![]))
                }
            })
        });

        let value = extract_via_extractor(
            "prose",
            &groups_schema(),
            &mut ctx,
            &create_abort_signal(),
            &mut transport,
        )
        .await
        .unwrap();

        assert_eq!(
            value,
            json!({"groups": [{"domain": "a", "files": [], "rationale": null}]})
        );
    }

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
    fn try_parse_json_strips_thinking_blocks() {
        let raw = "<think>\nsome reasoning\n</think>\n{\"a\": 1}";

        let v = try_parse_json(raw).unwrap();

        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn try_parse_json_strips_empty_thinking_block() {
        let raw = "<think>\n\n</think>\n{\"a\": 1}";

        let v = try_parse_json(raw).unwrap();

        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn try_parse_json_strips_multiple_thinking_blocks() {
        let raw = "<think>first</think>\n<think>second</think>\n{\"a\": 1}";

        let v = try_parse_json(raw).unwrap();

        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn try_parse_json_unclosed_think_tag_returns_none() {
        assert!(try_parse_json("<think>unclosed {\"a\": 1}").is_none());
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
        let ctx = make_ctx();

        let role = build_extractor_role(&ctx);

        assert_eq!(role.enabled_tools().as_deref(), Some([].as_slice()));
        assert_eq!(role.enabled_mcp_servers().as_deref(), Some([].as_slice()));
    }

    #[test]
    fn build_extractor_role_uses_parent_context_model() {
        let mut ctx = make_ctx();
        let mut parent_role = Role::new("parent", "parent prompt");
        parent_role.set_model(Model::new("client-x", "model-y"));
        ctx.role = Some(parent_role);

        let role = build_extractor_role(&ctx);

        assert_eq!(role.model().id(), "client-x:model-y");
    }
}
