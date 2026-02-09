use super::{FunctionDeclaration, JsonSchema};
use crate::config::GlobalConfig;

use anyhow::{Result, bail};
use indexmap::IndexMap;
use serde_json::{Value, json};

pub const TODO_FUNCTION_PREFIX: &str = "todo__";

pub fn todo_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{TODO_FUNCTION_PREFIX}init"),
            description: "Initialize a new todo list with a goal. Clears any existing todos."
                .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "goal".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some(
                            "The overall goal to achieve when all todos are completed".into(),
                        ),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["goal".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{TODO_FUNCTION_PREFIX}add"),
            description: "Add a new todo item to the list.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "task".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("Description of the todo task".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["task".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{TODO_FUNCTION_PREFIX}done"),
            description: "Mark a todo item as done by its id.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "id".to_string(),
                    JsonSchema {
                        type_value: Some("integer".to_string()),
                        description: Some("The id of the todo item to mark as done".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{TODO_FUNCTION_PREFIX}list"),
            description: "Display the current todo list with status of each item.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                ..Default::default()
            },
            agent: false,
        },
    ]
}

pub fn handle_todo_tool(config: &GlobalConfig, cmd_name: &str, args: &Value) -> Result<Value> {
    let action = cmd_name
        .strip_prefix(TODO_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);

    match action {
        "init" => {
            let goal = args.get("goal").and_then(Value::as_str).unwrap_or_default();
            let mut cfg = config.write();
            let agent = cfg.agent.as_mut();
            match agent {
                Some(agent) => {
                    agent.init_todo_list(goal);
                    Ok(json!({"status": "ok", "message": "Initialized new todo list"}))
                }
                None => bail!("No active agent"),
            }
        }
        "add" => {
            let task = args.get("task").and_then(Value::as_str).unwrap_or_default();
            if task.is_empty() {
                return Ok(json!({"error": "task description is required"}));
            }
            let mut cfg = config.write();
            let agent = cfg.agent.as_mut();
            match agent {
                Some(agent) => {
                    let id = agent.add_todo(task);
                    Ok(json!({"status": "ok", "id": id}))
                }
                None => bail!("No active agent"),
            }
        }
        "done" => {
            let id = args
                .get("id")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .map(|v| v as usize);
            match id {
                Some(id) => {
                    let mut cfg = config.write();
                    let agent = cfg.agent.as_mut();
                    match agent {
                        Some(agent) => {
                            if agent.mark_todo_done(id) {
                                Ok(
                                    json!({"status": "ok", "message": format!("Marked todo {id} as done")}),
                                )
                            } else {
                                Ok(json!({"error": format!("Todo {id} not found")}))
                            }
                        }
                        None => bail!("No active agent"),
                    }
                }
                None => Ok(json!({"error": "id is required and must be a number"})),
            }
        }
        "list" => {
            let cfg = config.read();
            let agent = cfg.agent.as_ref();
            match agent {
                Some(agent) => {
                    let list = agent.todo_list();
                    if list.is_empty() {
                        Ok(json!({"goal": "", "todos": []}))
                    } else {
                        Ok(serde_json::to_value(list)
                            .unwrap_or(json!({"error": "serialization failed"})))
                    }
                }
                None => bail!("No active agent"),
            }
        }
        _ => bail!("Unknown todo action: {action}"),
    }
}
