use super::{FunctionDeclaration, JsonSchema};
use crate::config::RequestContext;
use crate::hooks::{self, HookEvent};
use crate::prompt::{NeedsHuman, PromptAction, PromptAnswer, PromptRequest, headless_reply};
use crate::supervisor::escalation::{EscalationRequest, new_escalation_id};
use crate::utils::{ACP_SERVER, AbortSignal, HEADLESS, queue_acp_permission, wait_abort_signal};

use anyhow::{Result, anyhow, bail};
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::oneshot;

pub const USER_FUNCTION_PREFIX: &str = "user__";
pub const HEADLESS_GUIDANCE: &str = "No human is available to answer. Go with the recommended option; if none is marked, use your best judgment.";

const DEFAULT_ESCALATION_TIMEOUT_SECS: u64 = 0;

pub fn user_interaction_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{USER_FUNCTION_PREFIX}select"),
            description: "Present a list of named options and ask the user to pick exactly one. \
                          Indicate the recommended choice if there is one. \
                          Use this — not `confirm` — whenever there are 2+ named options to choose \
                          between. Returns the selected option.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "question".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The question to present to the user".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "options".to_string(),
                        JsonSchema {
                            type_value: Some("array".to_string()),
                            description: Some("List of options for the user to choose from".into()),
                            items: Some(Box::new(JsonSchema {
                                type_value: Some("string".to_string()),
                                ..Default::default()
                            })),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["question".to_string(), "options".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{USER_FUNCTION_PREFIX}confirm"),
            description: "Ask a genuinely binary yes/no question with no other choices. Do NOT \
                          use for \"A or B?\" situations — use `select` instead. Returns \"yes\" \
                          or \"no\".".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "question".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The yes/no question to ask the user".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["question".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{USER_FUNCTION_PREFIX}input"),
            description: "Collect free-form text from the user when no predefined options exist. \
                          Returns the text entered.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "question".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The prompt/question to display".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["question".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{USER_FUNCTION_PREFIX}checkbox"),
            description: "Ask the user to pick one or more options from a list (multi-select). \
                          Use when multiple answers are valid simultaneously. Returns an array \
                          of selected options.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "question".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The question to present to the user".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "options".to_string(),
                        JsonSchema {
                            type_value: Some("array".to_string()),
                            description: Some("List of options the user can select from (multiple selections allowed)".into()),
                            items: Some(Box::new(JsonSchema {
                                type_value: Some("string".to_string()),
                                ..Default::default()
                            })),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["question".to_string(), "options".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
    ]
}

pub async fn handle_user_tool(
    ctx: &mut RequestContext,
    cmd_name: &str,
    args: &Value,
) -> Result<Value> {
    let action = parse_action(
        cmd_name
            .strip_prefix(USER_FUNCTION_PREFIX)
            .unwrap_or(cmd_name),
    )?;

    if ACP_SERVER.load(Ordering::SeqCst) {
        let result = headless_value(ctx, headless_request(ctx, action, args))?;
        queue_acp_permission(json!({
            "action": action.name(),
            "question": result["question"],
            "options": result["options"],
        }));

        return Ok(result);
    }

    if HEADLESS.load(Ordering::SeqCst) {
        emit_headless_default(
            None,
            &format!("{USER_FUNCTION_PREFIX}{}", action.name()),
            None,
        );
        return headless_value(ctx, headless_request(ctx, action, args));
    }

    let depth = ctx.current_depth;

    if depth == 0 {
        handle_direct(ctx, action, args)
    } else {
        handle_escalated(ctx, action, args).await
    }
}

/// True when no human can answer and no ACP client relays the question;
/// ACP also sets `HEADLESS` but routes questions through permission requests.
pub fn headless_without_acp() -> bool {
    HEADLESS.load(Ordering::SeqCst) && !ACP_SERVER.load(Ordering::SeqCst)
}

pub fn headless_default_line(node: Option<&str>, kind: &str, answer: Option<&str>) -> String {
    let answer = answer.map_or_else(
        || Value::Null.to_string(),
        |a| Value::String(a.to_string()).to_string(),
    );
    format!(
        "[headless-default] node={} type={kind} answer={answer}",
        node.unwrap_or("-")
    )
}

pub(crate) fn emit_headless_default(node: Option<&str>, kind: &str, answer: Option<&str>) {
    let line = headless_default_line(node, kind, answer);
    eprintln!("{line}");
    #[cfg(test)]
    headless_capture::CAPTURED.lock().push(line);
}

#[cfg(test)]
pub(crate) mod headless_capture {
    pub(super) static CAPTURED: parking_lot::Mutex<Vec<String>> =
        parking_lot::Mutex::new(Vec::new());

    /// Drains every line emitted so far. Callers must be `#[serial]` and
    /// drain once before acting so earlier tests' lines do not leak in.
    pub(crate) fn take_captured_headless_lines() -> Vec<String> {
        std::mem::take(&mut *CAPTURED.lock())
    }
}

/// Sets the process-wide headless and ACP flags for a test and restores the
/// previous values on drop. Tests holding one must be `#[serial]`, and so
/// must any test that reaches `handle_user_tool` or an approval or input
/// graph node, since those read the flags.
#[cfg(test)]
pub(crate) struct HeadlessModeGuard {
    headless: bool,
    acp: bool,
}

#[cfg(test)]
impl HeadlessModeGuard {
    pub(crate) fn set(headless: bool, acp: bool) -> Self {
        Self {
            headless: HEADLESS.swap(headless, Ordering::SeqCst),
            acp: ACP_SERVER.swap(acp, Ordering::SeqCst),
        }
    }
}

#[cfg(test)]
impl Drop for HeadlessModeGuard {
    fn drop(&mut self) {
        HEADLESS.store(self.headless, Ordering::SeqCst);
        ACP_SERVER.store(self.acp, Ordering::SeqCst);
    }
}

/// The prompts a `user__*` tool can raise: the [`PromptAction`]s a model may
/// ask of the user, without the secret-bearing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserAction {
    Select,
    Confirm,
    Input,
    Checkbox,
}

impl UserAction {
    fn name(self) -> &'static str {
        PromptAction::from(self).name()
    }
}

impl From<UserAction> for PromptAction {
    fn from(action: UserAction) -> Self {
        match action {
            UserAction::Select => PromptAction::Select,
            UserAction::Confirm => PromptAction::Confirm,
            UserAction::Input => PromptAction::Input,
            UserAction::Checkbox => PromptAction::Checkbox,
        }
    }
}

fn parse_action(action: &str) -> Result<UserAction> {
    match action {
        "select" => Ok(UserAction::Select),
        "confirm" => Ok(UserAction::Confirm),
        "input" => Ok(UserAction::Input),
        "checkbox" => Ok(UserAction::Checkbox),
        _ => Err(anyhow!("Unknown user interaction: {action}")),
    }
}

fn required_question(args: &Value) -> Result<&str> {
    args.get("question")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'question' is required"))
}

/// The request a run with no human answers for itself. It tolerates the
/// gaps a live prompt rejects (no question, unusable options) so the
/// structured reply still reaches the model. Options are normalised the
/// way a live prompt reads them: a string-encoded array is decoded and
/// non-string elements are dropped.
fn headless_request(ctx: &RequestContext, action: UserAction, args: &Value) -> PromptRequest {
    let question = args.get("question").and_then(Value::as_str).unwrap_or("");
    let options = parse_options(args).unwrap_or_default();
    ctx.prompt_request(action.into(), question, options, None)
}

/// The structured reply a run with no human hands the model. The prompter
/// answers a confirm with [`PromptAnswer::NeedsHuman`] and refuses every
/// other kind; both become the same reply here, since the model is the
/// one party that can act on it.
fn headless_value(ctx: &RequestContext, req: PromptRequest) -> Result<Value> {
    let reply = headless_reply(&req);
    match ctx.prompt(req) {
        Ok(PromptAnswer::NeedsHuman) => Ok(reply),
        Ok(other) => bail!("Unexpected answer to a headless prompt: {other:?}"),
        Err(err) if NeedsHuman::caused(&err) => Ok(reply),
        Err(err) => Err(err),
    }
}

fn handle_direct(ctx: &RequestContext, action: UserAction, args: &Value) -> Result<Value> {
    let question = required_question(args)?;
    match action {
        UserAction::Select => {
            let req =
                ctx.prompt_request(PromptAction::Select, question, parse_options(args)?, None);
            Ok(json!({ "answer": ctx.prompt(req)?.into_selected()? }))
        }
        UserAction::Confirm => {
            let req = ctx.prompt_request(
                PromptAction::Confirm,
                question,
                Vec::new(),
                Some(json!(true)),
            );
            let answer = ctx.prompt(req)?.into_confirmed()?;
            Ok(json!({ "answer": if answer { "yes" } else { "no" } }))
        }
        UserAction::Input => {
            let req = ctx.prompt_request(
                PromptAction::Input,
                format!("{question}\nYour answer: "),
                Vec::new(),
                None,
            );
            Ok(json!({ "answer": ctx.prompt(req)?.into_text()? }))
        }
        UserAction::Checkbox => {
            let req =
                ctx.prompt_request(PromptAction::Checkbox, question, parse_options(args)?, None);
            Ok(json!({ "answers": ctx.prompt(req)?.into_checked()? }))
        }
    }
}

async fn handle_escalated(ctx: &RequestContext, action: UserAction, args: &Value) -> Result<Value> {
    let question = required_question(args)?.to_string();

    let options: Option<Vec<String>> = if args.get("options").is_some() {
        Some(parse_options(args)?)
    } else {
        None
    };

    let from_agent_id = ctx
        .self_agent_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let from_agent_name = ctx
        .agent
        .as_ref()
        .map(|a| a.name().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let root_queue = ctx
        .root_escalation_queue()
        .cloned()
        .ok_or_else(|| anyhow!("No escalation queue available; cannot reach parent agent"))?;
    let timeout_secs = ctx
        .agent
        .as_ref()
        .map(|a| a.escalation_timeout())
        .unwrap_or(DEFAULT_ESCALATION_TIMEOUT_SECS);

    let escalation_id = new_escalation_id();
    let (tx, rx) = oneshot::channel();

    let question = format!("[{}] {question}", action.name());
    let request = EscalationRequest {
        id: escalation_id.clone(),
        from_agent_id: from_agent_id.clone(),
        from_agent_name: from_agent_name.clone(),
        question: question.clone(),
        options,
        reply_tx: tx,
    };

    root_queue.submit(request);

    hooks::fire(
        HookEvent::EscalationRaised,
        ctx,
        &[
            ("COYOTE_ESCALATION_ID", escalation_id),
            ("COYOTE_ESCALATION_FROM_AGENT_ID", from_agent_id),
            ("COYOTE_ESCALATION_FROM_AGENT_NAME", from_agent_name),
            ("COYOTE_ESCALATION_QUESTION", question),
        ],
        None,
    );

    await_escalation_reply(rx, timeout_secs, ctx.task_abort.clone()).await
}

/// Waits for the parent's reply. `timeout_secs == 0` waits indefinitely; a
/// dropped sender (the parent cancelled the request) resolves either way.
/// So does `abort` (the asking agent's own signal) being set: the receiver
/// is dropped with the wait, which takes the question off the queue's
/// pending list. The fallback arms resolve without touching the escalation
/// queue, so none fires an escalation event: the request was never answered.
async fn await_escalation_reply(
    rx: oneshot::Receiver<String>,
    timeout_secs: u64,
    abort: Option<AbortSignal>,
) -> Result<Value> {
    let reply = async {
        if timeout_secs == 0 {
            Some(rx.await)
        } else {
            tokio::time::timeout(Duration::from_secs(timeout_secs), rx)
                .await
                .ok()
        }
    };
    let aborted = async {
        match abort.as_ref() {
            Some(abort) => wait_abort_signal(abort).await,
            None => std::future::pending().await,
        }
    };
    let reply = tokio::select! {
        reply = reply => reply,
        () = aborted => {
            return Ok(json!({
                "error": "Escalation withdrawn: this agent was cancelled",
                "fallback": "Make your best judgment and proceed",
            }));
        }
    };

    match reply {
        Some(Ok(reply)) => Ok(json!({ "answer": reply })),
        Some(Err(_)) => Ok(escalation_cancelled()),
        None => Ok(json!({
            "error": format!(
                "Escalation timed out after {timeout_secs} seconds waiting for user response"
            ),
            "fallback": "Make your best judgment and proceed",
        })),
    }
}

fn escalation_cancelled() -> Value {
    json!({
        "error": "Escalation was cancelled. The parent agent dropped the request",
        "fallback": "Make your best judgment and proceed",
    })
}

fn parse_options(args: &Value) -> Result<Vec<String>> {
    let raw = args
        .get("options")
        .ok_or_else(|| anyhow!("'options' is required and must be an array of strings"))?;

    let arr: Vec<Value> = match raw {
        Value::Array(arr) => arr.clone(),
        Value::String(s) => serde_json::from_str::<Vec<Value>>(s).map_err(|_| {
            anyhow!(
                "'options' was a string but did not parse as a JSON array. \
                 Pass options as a native JSON array, e.g. [\"yes\", \"no\"]."
            )
        })?,
        _ => bail!("'options' is required and must be an array of strings"),
    };

    Ok(arr
        .iter()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::headless_capture::take_captured_headless_lines;
    use super::*;
    use crate::config::{AppConfig, AppState, WorkingMode};
    use crate::events::{Event, Origin, RecordingSink};
    use crate::function::agents::handle_agent_tool;
    use crate::hooks::{HookDef, HooksMap, test_sink};
    use crate::prompt::{NEEDS_HUMAN_MARKER, ScriptedPrompter, prompt_events};
    use crate::supervisor::escalation::EscalationQueue;
    use crate::testing::AtomicFlagGuard;
    use crate::utils::drain_acp_permissions;
    use serial_test::serial;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn cmd_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    fn scripted_ctx(answers: Vec<PromptAnswer>) -> (RequestContext, Arc<ScriptedPrompter>) {
        let mut ctx = cmd_ctx();
        let prompter = Arc::new(ScriptedPrompter::new(answers));
        ctx.prompter = prompter.clone();
        (ctx, prompter)
    }

    fn run_user_tool(ctx: &mut RequestContext, cmd_name: &str, args: Value) -> Value {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(handle_user_tool(ctx, cmd_name, &args))
            .unwrap()
    }

    fn headless_json(action: &str, question: &str, options: Value) -> Value {
        json!({
            "needs_human": true,
            "action": action,
            "question": question,
            "options": options,
            "guidance": HEADLESS_GUIDANCE,
        })
    }

    #[test]
    #[serial]
    fn headless_drops_non_string_options() {
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        let mut ctx = cmd_ctx();
        let args = json!({ "question": "q-hl-mixed-3f", "options": [1, "a", null] });
        let v = run_user_tool(&mut ctx, "user__select", args);
        assert_eq!(v, headless_json("select", "q-hl-mixed-3f", json!(["a"])));
    }

    #[test]
    #[serial]
    fn headless_accepts_a_string_encoded_options_array() {
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        let mut ctx = cmd_ctx();
        let args = json!({ "question": "q-hl-str-8d", "options": "[\"a\",\"b\"]" });
        let v = run_user_tool(&mut ctx, "user__select", args);
        assert_eq!(v, headless_json("select", "q-hl-str-8d", json!(["a", "b"])));
    }

    #[test]
    #[serial]
    fn headless_select_returns_structured_json() {
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        let mut ctx = cmd_ctx();
        let v = run_user_tool(
            &mut ctx,
            "user__select",
            json!({"question": "q-hl-7k2", "options": ["a", "b"]}),
        );
        let expected = headless_json("select", "q-hl-7k2", json!(["a", "b"]));
        assert_eq!(v, expected);
        assert_eq!(v.to_string(), expected.to_string());
    }

    /// The refusal a run with no human gives a select still closes the
    /// prompt's event pair, named as a refusal rather than a failure.
    #[test]
    #[serial]
    fn a_headless_select_publishes_needs_human_as_its_answer() {
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        let sink = RecordingSink::install();
        let mut ctx = cmd_ctx();
        run_user_tool(
            &mut ctx,
            "user__select",
            json!({"question": "q-hl-evt-4p", "options": ["a", "b"]}),
        );

        let (requested, answered) = prompt_events(sink.drain(), "q-hl-evt-4p");
        let [Event::PromptRequested { id, action, .. }] = requested.as_slice() else {
            panic!("{requested:?}");
        };
        assert_eq!(*action, PromptAction::Select);
        assert_eq!(
            answered,
            vec![Event::PromptAnswered {
                id: id.clone(),
                answer: NEEDS_HUMAN_MARKER.to_string(),
                auto: false,
            }]
        );
    }

    /// A headless confirm is answered rather than refused, and both the
    /// reply the model reads and the published answer match the select's.
    #[test]
    #[serial]
    fn a_headless_confirm_hands_back_the_json_and_publishes_needs_human() {
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        let sink = RecordingSink::install();
        let mut ctx = cmd_ctx();
        let v = run_user_tool(
            &mut ctx,
            "user__confirm",
            json!({"question": "q-hl-cfm-8w", "options": ["a", "b"]}),
        );

        let expected = headless_json("confirm", "q-hl-cfm-8w", json!(["a", "b"]));
        assert_eq!(v.to_string(), expected.to_string());
        let (requested, answered) = prompt_events(sink.drain(), "q-hl-cfm-8w");
        let [Event::PromptRequested { id, action, .. }] = requested.as_slice() else {
            panic!("{requested:?}");
        };
        assert_eq!(*action, PromptAction::Confirm);
        assert_eq!(
            answered,
            vec![Event::PromptAnswered {
                id: id.clone(),
                answer: NEEDS_HUMAN_MARKER.to_string(),
                auto: false,
            }]
        );
    }

    #[test]
    #[serial]
    fn headless_confirm_returns_empty_options_when_absent() {
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        let mut ctx = cmd_ctx();
        let v = run_user_tool(
            &mut ctx,
            "user__confirm",
            json!({"question": "yes or no? hl-c3n"}),
        );
        assert_eq!(v, headless_json("confirm", "yes or no? hl-c3n", json!([])));
    }

    #[test]
    #[serial]
    fn headless_tolerates_a_missing_question() {
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        let mut ctx = cmd_ctx();
        let v = run_user_tool(&mut ctx, "user__input", json!({}));
        assert_eq!(v, headless_json("input", "", json!([])));
    }

    #[test]
    #[serial]
    fn acp_select_returns_the_headless_json_and_queues_one_permission() {
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        let _acp = AtomicFlagGuard::set(&ACP_SERVER, true);
        drain_acp_permissions();
        let mut ctx = cmd_ctx();
        let v = run_user_tool(
            &mut ctx,
            "user__select",
            json!({"question": "q-acp-5n", "options": ["a", "b"]}),
        );
        assert_eq!(v, headless_json("select", "q-acp-5n", json!(["a", "b"])));
        assert_eq!(
            drain_acp_permissions(),
            vec![json!({
                "action": "select",
                "question": "q-acp-5n",
                "options": ["a", "b"],
            })]
        );
    }

    #[test]
    #[serial]
    fn headless_rejects_an_unknown_action() {
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        let mut ctx = cmd_ctx();
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(handle_user_tool(&mut ctx, "user__dance", &json!({})))
            .unwrap_err();
        assert_eq!(err.to_string(), "Unknown user interaction: dance");
    }

    #[tokio::test]
    #[serial]
    async fn direct_select_round_trips_through_the_prompter() {
        let (mut ctx, prompter) = scripted_ctx(vec![PromptAnswer::Selected("b".to_string())]);
        let v = handle_user_tool(
            &mut ctx,
            "user__select",
            &json!({"question": "pick one sel-8d1", "options": ["a", "b"]}),
        )
        .await
        .unwrap();
        assert_eq!(v, json!({ "answer": "b" }));

        let requests = prompter.requests();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(req.action, PromptAction::Select);
        assert_eq!(req.question, "pick one sel-8d1");
        assert_eq!(req.options, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(req.default, None);
        assert_eq!(req.tool, None);
        assert_eq!(req.call_id, None);
        assert_eq!(req.job_id, None);
        assert_eq!(req.origin, Origin::from(&ctx));
    }

    /// The built-in select is open on both prompters, so an answer the
    /// options did not offer comes back as typed.
    #[tokio::test]
    #[serial]
    async fn direct_select_is_open_and_returns_a_typed_answer() {
        let (mut ctx, prompter) =
            scripted_ctx(vec![PromptAnswer::Selected("typed by hand".to_string())]);
        let v = handle_user_tool(
            &mut ctx,
            "user__select",
            &json!({"question": "pick one sel-4k9", "options": ["a", "b"]}),
        )
        .await
        .unwrap();
        assert_eq!(v, json!({ "answer": "typed by hand" }));

        let [req] = prompter.requests().try_into().unwrap();
        assert_eq!(req.action, PromptAction::Select);
        assert!(!req.closed);
    }

    #[tokio::test]
    #[serial]
    async fn direct_confirm_round_trips_through_the_prompter() {
        let (mut ctx, prompter) = scripted_ctx(vec![PromptAnswer::Confirmed(false)]);
        let v = handle_user_tool(
            &mut ctx,
            "user__confirm",
            &json!({"question": "sure? cf-2q9"}),
        )
        .await
        .unwrap();
        assert_eq!(v, json!({ "answer": "no" }));

        let requests = prompter.requests();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(req.action, PromptAction::Confirm);
        assert_eq!(req.question, "sure? cf-2q9");
        assert!(req.options.is_empty());
        assert_eq!(req.default, Some(json!(true)));
        assert_eq!(req.tool, None);
        assert_eq!(req.call_id, None);
        assert_eq!(req.job_id, None);
        assert_eq!(req.origin, Origin::from(&ctx));
    }

    #[tokio::test]
    #[serial]
    async fn direct_input_round_trips_through_the_prompter() {
        let (mut ctx, prompter) = scripted_ctx(vec![PromptAnswer::Text("blue".to_string())]);
        let v = handle_user_tool(
            &mut ctx,
            "user__input",
            &json!({"question": "favourite colour? in-5v7"}),
        )
        .await
        .unwrap();
        assert_eq!(v, json!({ "answer": "blue" }));

        let requests = prompter.requests();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(req.action, PromptAction::Input);
        assert_eq!(req.question, "favourite colour? in-5v7\nYour answer: ");
        assert!(req.options.is_empty());
        assert_eq!(req.default, None);
        assert_eq!(req.tool, None);
        assert_eq!(req.call_id, None);
        assert_eq!(req.job_id, None);
        assert_eq!(req.origin, Origin::from(&ctx));
    }

    #[tokio::test]
    #[serial]
    async fn direct_checkbox_round_trips_through_the_prompter() {
        let (mut ctx, prompter) = scripted_ctx(vec![PromptAnswer::Checked(vec![
            "a".to_string(),
            "c".to_string(),
        ])]);
        let v = handle_user_tool(
            &mut ctx,
            "user__checkbox",
            &json!({"question": "which? cb-9x4", "options": ["a", "b", "c"]}),
        )
        .await
        .unwrap();
        assert_eq!(v, json!({ "answers": ["a", "c"] }));

        let requests = prompter.requests();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(req.action, PromptAction::Checkbox);
        assert_eq!(req.question, "which? cb-9x4");
        assert_eq!(
            req.options,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(req.default, None);
        assert_eq!(req.tool, None);
        assert_eq!(req.call_id, None);
        assert_eq!(req.job_id, None);
        assert_eq!(req.origin, Origin::from(&ctx));
    }

    #[tokio::test]
    #[serial]
    async fn direct_prompt_requires_a_question() {
        let (mut ctx, prompter) = scripted_ctx(vec![]);
        let err = handle_user_tool(&mut ctx, "user__confirm", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "'question' is required");
        assert!(prompter.requests().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn direct_unknown_action_is_reported_before_the_missing_question() {
        let (mut ctx, prompter) = scripted_ctx(vec![]);
        let err = handle_user_tool(&mut ctx, "user__dance", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Unknown user interaction: dance");
        assert!(prompter.requests().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn escalated_unknown_action_is_reported_before_reaching_the_queue() {
        let queue = Arc::new(EscalationQueue::new());
        let (mut ctx, prompter) = scripted_ctx(vec![]);
        ctx.current_depth = 1;
        ctx.escalation_queue = Some(Arc::clone(&queue));
        let err = handle_user_tool(&mut ctx, "user__dance", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Unknown user interaction: dance");
        assert!(queue.pending_summary().is_empty());
        assert!(prompter.requests().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn direct_cancel_surfaces_as_the_inquire_wording() {
        let (mut ctx, _prompter) = scripted_ctx(vec![PromptAnswer::Cancelled]);
        let err = handle_user_tool(
            &mut ctx,
            "user__confirm",
            &json!({"question": "sure? cx-1a2"}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "Operation was canceled by the user");
    }

    #[test]
    #[serial]
    fn direct_prompt_publishes_requested_then_answered() {
        let sink = RecordingSink::install();
        let (mut ctx, _prompter) = scripted_ctx(vec![PromptAnswer::Confirmed(true)]);
        let v = run_user_tool(
            &mut ctx,
            "user__confirm",
            json!({"question": "evt-probe-x9q"}),
        );
        assert_eq!(v, json!({ "answer": "yes" }));

        let (requested, answered) = prompt_events(sink.drain(), "evt-probe-x9q");
        let [
            Event::PromptRequested {
                id,
                action,
                options,
                tool,
                origin,
                ..
            },
        ] = requested.as_slice()
        else {
            panic!("{requested:?}");
        };
        assert_eq!(*action, PromptAction::Confirm);
        assert!(options.is_empty());
        assert_eq!(*tool, None);
        assert_eq!(*origin, Origin::from(&ctx));
        assert_eq!(
            answered,
            vec![Event::PromptAnswered {
                id: id.clone(),
                answer: "yes".to_string(),
                auto: false,
            }]
        );
    }

    #[test]
    fn headless_guidance_is_neutral_and_names_no_terminal_action() {
        assert_eq!(
            HEADLESS_GUIDANCE,
            "No human is available to answer. Go with the recommended option; if none is marked, use your best judgment."
        );
        let lowered = HEADLESS_GUIDANCE.to_lowercase();
        for word in ["abort", "stop", "cancel", "quit", "skip", "reject"] {
            assert!(!lowered.contains(word), "guidance contains {word:?}");
        }
    }

    #[test]
    fn headless_default_line_for_prose_tools_has_no_node_and_null_answer() {
        assert_eq!(
            headless_default_line(None, "user__select", None),
            "[headless-default] node=- type=user__select answer=null"
        );
    }

    #[test]
    fn headless_default_line_for_graph_nodes_json_quotes_the_answer() {
        assert_eq!(
            headless_default_line(Some("gate"), "approval", Some("yes")),
            r#"[headless-default] node=gate type=approval answer="yes""#
        );
        assert_eq!(
            headless_default_line(Some("ask"), "input", Some("say \"hi\"\nbye")),
            r#"[headless-default] node=ask type=input answer="say \"hi\"\nbye""#
        );
    }

    #[tokio::test]
    #[serial]
    async fn headless_prose_question_still_returns_needs_human_without_answer() {
        let _mode = HeadlessModeGuard::set(true, false);
        take_captured_headless_lines();
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let args = json!({ "question": "headless prose pick", "options": ["a", "b"] });

        let result = handle_user_tool(&mut ctx, "user__select", &args)
            .await
            .unwrap();

        assert_eq!(
            result,
            headless_json("select", "headless prose pick", json!(["a", "b"]))
        );
        assert_eq!(result["guidance"], HEADLESS_GUIDANCE);
        assert!(result.get("answer").is_none());
        assert_eq!(
            take_captured_headless_lines(),
            vec!["[headless-default] node=- type=user__select answer=null"]
        );
    }

    #[tokio::test]
    #[serial]
    async fn headless_prose_question_logs_exactly_one_line_per_action() {
        let _mode = HeadlessModeGuard::set(true, false);
        take_captured_headless_lines();
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let args = json!({ "question": "headless prose any", "options": ["a", "b"] });

        for action in ["select", "confirm", "input", "checkbox"] {
            handle_user_tool(&mut ctx, &format!("user__{action}"), &args)
                .await
                .unwrap();
            assert_eq!(
                take_captured_headless_lines(),
                vec![format!(
                    "[headless-default] node=- type=user__{action} answer=null"
                )]
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn acp_question_keeps_needs_human_result_and_queues_permission() {
        let _mode = HeadlessModeGuard::set(true, true);
        take_captured_headless_lines();
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let question = "acp pick unchanged-q7v";
        let args = json!({ "question": question, "options": ["a", "b"] });

        let result = handle_user_tool(&mut ctx, "user__select", &args)
            .await
            .unwrap();

        assert_eq!(result, headless_json("select", question, json!(["a", "b"])));
        assert!(result.get("answer").is_none());
        let queued: Vec<Value> = crate::utils::drain_acp_permissions()
            .into_iter()
            .filter(|p| p["question"] == question)
            .collect();
        assert_eq!(
            queued,
            vec![json!({ "action": "select", "question": question, "options": ["a", "b"] })]
        );
        assert!(take_captured_headless_lines().is_empty());
    }

    #[test]
    fn escalation_timeout_defaults_to_unlimited() {
        assert_eq!(DEFAULT_ESCALATION_TIMEOUT_SECS, 0);
    }

    fn ctx_with_escalation_hooks(marker: &str) -> RequestContext {
        let mut hooks = HooksMap::default();
        for event in ["escalation.raised", "escalation.answered"] {
            hooks.insert(
                event.to_string(),
                vec![HookDef {
                    name: format!("{marker}-{event}"),
                    command: "true".to_string(),
                }],
            );
        }
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            hooks,
            ..Default::default()
        });
        RequestContext::new(Arc::new(app), WorkingMode::Cmd)
    }

    fn marker_captures(marker: &str, event: &str) -> Vec<test_sink::Capture> {
        test_sink::snapshot()
            .into_iter()
            .filter(|capture| capture.hook_name == format!("{marker}-{event}"))
            .collect()
    }

    /// `escalation.raised` (child seam) and `escalation.answered` (parent
    /// seam) must describe the same origin agent for one escalation id: the
    /// answered envs come from the taken queue request, never from the
    /// replying context.
    #[tokio::test]
    #[serial]
    async fn escalation_raised_and_answered_agree_on_origin_envs() {
        let _sink = test_sink::install();
        let marker = "esc-parity-k4w";

        let queue = Arc::new(EscalationQueue::new());
        let mut child_ctx = ctx_with_escalation_hooks(marker);
        child_ctx.escalation_queue = Some(Arc::clone(&queue));
        child_ctx.self_agent_id = Some("esc-child-1".to_string());
        let mut parent_ctx = ctx_with_escalation_hooks(marker);
        parent_ctx.escalation_queue = Some(Arc::clone(&queue));

        let raise_args = json!({ "question": "which db?" });
        let raise = handle_escalated(&child_ctx, UserAction::Input, &raise_args);
        let reply = async {
            // Bounded poll: on the current-thread runtime the raise future
            // submits before its first await, so this resolves immediately.
            let mut escalation_id = None;
            for _ in 0..1000 {
                if let Some(entry) = queue.pending_summary().first() {
                    escalation_id = entry["escalation_id"].as_str().map(str::to_string);
                    break;
                }
                tokio::task::yield_now().await;
            }
            let escalation_id = escalation_id.expect("escalation must reach the queue");
            handle_agent_tool(
                &mut parent_ctx,
                "agent__reply_escalation",
                &json!({ "escalation_id": escalation_id, "reply": "postgres" }),
            )
            .await
            .unwrap()
        };
        let (raised_result, replied) = tokio::join!(raise, reply);
        assert_eq!(raised_result.unwrap()["answer"], "postgres");
        assert_eq!(replied["status"], "ok");

        let raised = marker_captures(marker, "escalation.raised");
        let answered = marker_captures(marker, "escalation.answered");
        assert_eq!(raised.len(), 1, "{raised:?}");
        assert_eq!(answered.len(), 1, "{answered:?}");
        for key in [
            "COYOTE_ESCALATION_ID",
            "COYOTE_ESCALATION_FROM_AGENT_ID",
            "COYOTE_ESCALATION_FROM_AGENT_NAME",
            "COYOTE_ESCALATION_QUESTION",
        ] {
            assert!(raised[0].envs.contains_key(key), "{key} missing on raised");
            assert_eq!(raised[0].envs.get(key), answered[0].envs.get(key), "{key}");
        }
        assert_eq!(
            raised[0]
                .envs
                .get("COYOTE_ESCALATION_FROM_AGENT_ID")
                .map(String::as_str),
            Some("esc-child-1")
        );
        assert_eq!(
            raised[0]
                .envs
                .get("COYOTE_ESCALATION_QUESTION")
                .map(String::as_str),
            Some("[input] which db?")
        );
        assert_eq!(
            answered[0]
                .envs
                .get("COYOTE_ESCALATION_REPLY")
                .map(String::as_str),
            Some("postgres")
        );
        assert!(!raised[0].envs.contains_key("COYOTE_ESCALATION_REPLY"));
    }

    /// `COYOTE_ESCALATION_QUESTION` rides through the standard env
    /// truncation: a huge question is trimmed to 2048 bytes on the raised
    /// seam, never passed through whole.
    #[tokio::test]
    #[serial]
    async fn escalation_question_env_is_truncated_to_2048_bytes() {
        let _sink = test_sink::install();
        let marker = "esc-trunc-p9j";

        let queue = Arc::new(EscalationQueue::new());
        let mut child_ctx = ctx_with_escalation_hooks(marker);
        child_ctx.escalation_queue = Some(Arc::clone(&queue));
        child_ctx.self_agent_id = Some("esc-child-trunc".to_string());

        let raise_args = json!({ "question": "q".repeat(3000) });
        let raise = handle_escalated(&child_ctx, UserAction::Input, &raise_args);
        let reply = async {
            // Bounded poll: on the current-thread runtime the raise future
            // submits before its first await, so this resolves immediately.
            let mut request = None;
            for _ in 0..1000 {
                if let Some(entry) = queue.pending_summary().first() {
                    let id = entry["escalation_id"].as_str().unwrap().to_string();
                    request = queue.take(&id);
                    break;
                }
                tokio::task::yield_now().await;
            }
            let request = request.expect("escalation must reach the queue");
            let _ = request.reply_tx.send("ok".to_string());
        };
        let (raised_result, ()) = tokio::join!(raise, reply);
        assert_eq!(raised_result.unwrap()["answer"], "ok");

        let raised = marker_captures(marker, "escalation.raised");
        assert_eq!(raised.len(), 1, "{raised:?}");
        let question = raised[0]
            .envs
            .get("COYOTE_ESCALATION_QUESTION")
            .expect("QUESTION env must be present on escalation.raised");
        assert_eq!(
            question.len(),
            2048,
            "QUESTION env must be 2048-byte truncated"
        );
        assert!(question.starts_with("[input] qqq"));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_timeout_waits_indefinitely_for_the_reply() {
        let (tx, rx) = oneshot::channel();
        let wait = tokio::spawn(await_escalation_reply(rx, 0, None));

        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(!wait.is_finished());

        tx.send("approved".to_string()).unwrap();
        let v = wait.await.unwrap().unwrap();
        assert_eq!(v, json!({ "answer": "approved" }));
    }

    #[tokio::test(start_paused = true)]
    async fn nonzero_timeout_still_expires_with_the_existing_message() {
        let (tx, rx) = oneshot::channel::<String>();
        let wait = tokio::spawn(await_escalation_reply(rx, 5, None));
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(!wait.is_finished());

        tokio::time::advance(Duration::from_secs(2)).await;
        let v = wait.await.unwrap().unwrap();
        assert_eq!(
            v["error"],
            "Escalation timed out after 5 seconds waiting for user response"
        );
        assert_eq!(v["fallback"], "Make your best judgment and proceed");
        assert!(tx.send("too late".to_string()).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn zero_timeout_resolves_when_the_sender_is_dropped() {
        let (tx, rx) = oneshot::channel::<String>();
        drop(tx);

        let v = tokio::time::timeout(Duration::from_secs(1), await_escalation_reply(rx, 0, None))
            .await
            .expect("the wait must complete once the sender is gone")
            .unwrap();
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .starts_with("Escalation was cancelled"),
            "{v}"
        );
        assert_eq!(v["fallback"], "Make your best judgment and proceed");
    }

    /// A cancelled asker stops waiting: the wait returns the cancelled
    /// outcome and drops its receiver, so the queue no longer lists the
    /// question.
    #[tokio::test(start_paused = true)]
    async fn the_wait_ends_when_the_askers_signal_is_set() {
        let queue = EscalationQueue::new();
        let (reply_tx, rx) = oneshot::channel::<String>();
        queue.submit(EscalationRequest {
            id: "esc_ab1".to_string(),
            from_agent_id: "agent_ab1".to_string(),
            from_agent_name: "asker".to_string(),
            question: "[confirm] Still there?".to_string(),
            options: None,
            reply_tx,
        });
        let abort = crate::utils::create_abort_signal();
        let wait = tokio::spawn(await_escalation_reply(rx, 0, Some(abort.clone())));

        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(!wait.is_finished());
        assert_eq!(queue.pending().len(), 1);

        abort.set_ctrlc();
        let v = tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .expect("the wait must end once the asker's signal is set")
            .unwrap()
            .unwrap();
        assert_eq!(
            v,
            json!({
                "error": "Escalation withdrawn: this agent was cancelled",
                "fallback": "Make your best judgment and proceed",
            })
        );
        assert!(queue.pending().is_empty());
    }

    #[test]
    fn stale_timeout_wording_is_gone_from_shipped_text() {
        // Split so this file does not match its own needles.
        let needles = [
            concat!("(5-minute", " timeout)"),
            concat!("escalation_timeout:", " 300"),
            concat!("default:", " 5 minutes"),
        ];
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let example = root.join("config.agent.example.yaml");
        let example_text = fs::read_to_string(&example).unwrap();
        for needle in ["escalation_timeout: 0", "0 = wait indefinitely"] {
            assert!(
                example_text.contains(needle),
                "{} lost the shipped wording {needle:?}",
                example.display()
            );
        }

        let mut files = vec![example];
        collect_files(&root.join("src"), &mut files);
        collect_files(&root.join("assets"), &mut files);
        let this_file = root.join(file!());
        assert!(
            files.contains(&this_file),
            "walk did not reach {}",
            this_file.display()
        );

        let mut hits = Vec::new();
        for path in &files {
            let text = String::from_utf8_lossy(&fs::read(path).unwrap()).into_owned();
            for needle in needles {
                if text.contains(needle) {
                    hits.push(format!("{}: {needle:?}", path.display()));
                }
            }
        }
        assert!(
            hits.is_empty(),
            "stale timeout wording found:\n{}",
            hits.join("\n")
        );
    }

    fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
}
