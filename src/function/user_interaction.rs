use super::{FunctionDeclaration, JsonSchema};
use crate::config::RequestContext;
use crate::supervisor::escalation::{EscalationRequest, new_escalation_id};
use crate::utils::{ACP_SERVER, HEADLESS, queue_acp_permission};

use anyhow::{Result, anyhow, bail};
use indexmap::IndexMap;
use inquire::{Confirm, MultiSelect, Select, Text};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::oneshot;

pub const USER_FUNCTION_PREFIX: &str = "user__";

const DEFAULT_ESCALATION_TIMEOUT_SECS: u64 = 0;
const CUSTOM_MULTI_CHOICE_ANSWER_OPTION: &str = "Other (custom)";

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
    let action = cmd_name
        .strip_prefix(USER_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);

    if ACP_SERVER.load(Ordering::SeqCst) {
        let result = handle_headless(action, args);
        queue_acp_permission(json!({
            "action": action,
            "question": result["question"],
            "options": result["options"],
        }));

        return Ok(result);
    }

    if HEADLESS.load(Ordering::SeqCst) {
        return Ok(handle_headless(action, args));
    }

    let depth = ctx.current_depth;

    if depth == 0 {
        handle_direct(action, args)
    } else {
        handle_escalated(ctx, action, args).await
    }
}

fn handle_headless(action: &str, args: &Value) -> Value {
    let question = args.get("question").and_then(Value::as_str).unwrap_or("");
    let options: Vec<Value> = args
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    json!({
        "needs_human": true,
        "action": action,
        "question": question,
        "options": options,
        "guidance": "No human is present. Apply a sensible default or abort the task.",
    })
}

fn handle_direct(action: &str, args: &Value) -> Result<Value> {
    match action {
        "select" => handle_direct_ask(args),
        "confirm" => handle_direct_confirm(args),
        "input" => handle_direct_input(args),
        "checkbox" => handle_direct_checkbox(args),
        _ => Err(anyhow!("Unknown user interaction: {action}")),
    }
}

fn handle_direct_ask(args: &Value) -> Result<Value> {
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'question' is required"))?;
    let mut options = parse_options(args)?;
    options.push(CUSTOM_MULTI_CHOICE_ANSWER_OPTION.to_string());

    let mut answer = Select::new(question, options)
        .without_filtering()
        .with_help_message("↑↓ to move, enter to select")
        .prompt()?;

    if answer == CUSTOM_MULTI_CHOICE_ANSWER_OPTION {
        answer = Text::new("Custom response:").prompt()?
    }

    Ok(json!({ "answer": answer }))
}

fn handle_direct_confirm(args: &Value) -> Result<Value> {
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'question' is required"))?;

    let answer = Confirm::new(question).with_default(true).prompt()?;

    Ok(json!({ "answer": if answer { "yes" } else { "no" } }))
}

fn handle_direct_input(args: &Value) -> Result<Value> {
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'question' is required"))?;

    let answer = Text::new(&format!("{question}\nYour answer: ")).prompt()?;

    Ok(json!({ "answer": answer }))
}

fn handle_direct_checkbox(args: &Value) -> Result<Value> {
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'question' is required"))?;
    let options = parse_options(args)?;

    let answers = MultiSelect::new(question, options).prompt()?;

    Ok(json!({ "answers": answers }))
}

async fn handle_escalated(ctx: &RequestContext, action: &str, args: &Value) -> Result<Value> {
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'question' is required"))?
        .to_string();

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

    let request = EscalationRequest {
        id: escalation_id.clone(),
        from_agent_id,
        from_agent_name: from_agent_name.clone(),
        question: format!("[{action}] {question}"),
        options,
        reply_tx: tx,
    };

    root_queue.submit(request);

    await_escalation_reply(rx, timeout_secs).await
}

/// Waits for the parent's reply. `timeout_secs == 0` waits indefinitely; a
/// dropped sender (the parent cancelled the request) resolves either way.
async fn await_escalation_reply(rx: oneshot::Receiver<String>, timeout_secs: u64) -> Result<Value> {
    let reply = if timeout_secs == 0 {
        rx.await
    } else {
        match tokio::time::timeout(Duration::from_secs(timeout_secs), rx).await {
            Ok(reply) => reply,
            Err(_) => {
                return Ok(json!({
                    "error": format!(
                        "Escalation timed out after {timeout_secs} seconds waiting for user response"
                    ),
                    "fallback": "Make your best judgment and proceed",
                }));
            }
        }
    };

    match reply {
        Ok(reply) => Ok(json!({ "answer": reply })),
        Err(_) => Ok(json!({
            "error": "Escalation was cancelled. The parent agent dropped the request",
            "fallback": "Make your best judgment and proceed",
        })),
    }
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
    use super::*;

    #[test]
    fn headless_select_returns_structured_json() {
        let args = json!({"question": "pick one", "options": ["a", "b"]});
        let v = handle_headless("select", &args);
        assert_eq!(v["needs_human"], true);
        assert_eq!(v["action"], "select");
        assert_eq!(v["question"], "pick one");
        assert_eq!(v["options"], json!(["a", "b"]));
        assert!(v["guidance"].is_string());
    }

    #[test]
    fn headless_confirm_returns_empty_options_when_absent() {
        let args = json!({"question": "yes or no?"});
        let v = handle_headless("confirm", &args);
        assert_eq!(v["needs_human"], true);
        assert_eq!(v["action"], "confirm");
        assert_eq!(v["options"], json!([]));
    }

    #[test]
    fn escalation_timeout_defaults_to_unlimited() {
        assert_eq!(DEFAULT_ESCALATION_TIMEOUT_SECS, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_timeout_waits_indefinitely_for_the_reply() {
        let (tx, rx) = oneshot::channel();
        let wait = tokio::spawn(await_escalation_reply(rx, 0));

        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(!wait.is_finished());

        tx.send("approved".to_string()).unwrap();
        let v = wait.await.unwrap().unwrap();
        assert_eq!(v, json!({ "answer": "approved" }));
    }

    #[tokio::test(start_paused = true)]
    async fn nonzero_timeout_still_expires_with_the_existing_message() {
        let (tx, rx) = oneshot::channel::<String>();
        let wait = tokio::spawn(await_escalation_reply(rx, 5));
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

        let v = tokio::time::timeout(Duration::from_secs(1), await_escalation_reply(rx, 0))
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

    #[test]
    fn stale_timeout_wording_is_gone_from_shipped_text() {
        // Split so this file does not match its own needles.
        let needles = [
            concat!("(5-minute", " timeout)"),
            concat!("escalation_timeout:", " 300"),
            concat!("default:", " 5 minutes"),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let example = root.join("config.agent.example.yaml");
        let example_text = std::fs::read_to_string(&example).unwrap();
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
            let text = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
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

    fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
}
