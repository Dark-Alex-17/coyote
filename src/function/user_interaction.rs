use super::{FunctionDeclaration, JsonSchema};
use crate::config::GlobalConfig;
use crate::supervisor::escalation::{EscalationRequest, new_escalation_id};

use anyhow::{Result, anyhow};
use indexmap::IndexMap;
use inquire::{Confirm, MultiSelect, Select, Text};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::oneshot;

pub const USER_FUNCTION_PREFIX: &str = "user__";

const ESCALATION_TIMEOUT: Duration = Duration::from_secs(300);

pub fn user_interaction_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{USER_FUNCTION_PREFIX}ask"),
            description: "Ask the user to select one option from a list. Returns the selected option. Indicate the recommended choice if there is one.".to_string(),
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
            description: "Ask the user a yes/no question. Returns \"yes\" or \"no\".".to_string(),
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
            description: "Ask the user for free-form text input. Returns the text entered.".to_string(),
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
            description: "Ask the user to select one or more options from a list. Returns an array of selected options.".to_string(),
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
    config: &GlobalConfig,
    cmd_name: &str,
    args: &Value,
) -> Result<Value> {
    let action = cmd_name
        .strip_prefix(USER_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);

    let depth = config.read().current_depth;

    if depth == 0 {
        handle_direct(action, args)
    } else {
        handle_escalated(config, action, args).await
    }
}

fn handle_direct(action: &str, args: &Value) -> Result<Value> {
    match action {
        "ask" => handle_direct_ask(args),
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
    let options = parse_options(args)?;

    let answer = Select::new(question, options).prompt()?;

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

    let answer = Text::new(question).prompt()?;

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

async fn handle_escalated(config: &GlobalConfig, action: &str, args: &Value) -> Result<Value> {
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'question' is required"))?
        .to_string();

    let options: Option<Vec<String>> = args.get("options").and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect()
    });

    let (from_agent_id, from_agent_name, root_queue) = {
        let cfg = config.read();
        let agent_id = cfg
            .self_agent_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let agent_name = cfg
            .agent
            .as_ref()
            .map(|a| a.name().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let queue = cfg
            .root_escalation_queue
            .clone()
            .ok_or_else(|| anyhow!("No escalation queue available; cannot reach parent agent"))?;
        (agent_id, agent_name, queue)
    };

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

    match tokio::time::timeout(ESCALATION_TIMEOUT, rx).await {
        Ok(Ok(reply)) => Ok(json!({ "answer": reply })),
        Ok(Err(_)) => Ok(json!({
            "error": "Escalation was cancelled. The parent agent dropped the request",
            "fallback": "Make your best judgment and proceed",
        })),
        Err(_) => Ok(json!({
            "error": format!(
                "Escalation timed out after {} seconds waiting for user response",
                ESCALATION_TIMEOUT.as_secs()
            ),
            "fallback": "Make your best judgment and proceed",
        })),
    }
}

fn parse_options(args: &Value) -> Result<Vec<String>> {
    args.get("options")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .ok_or_else(|| anyhow!("'options' is required and must be an array of strings"))
}
