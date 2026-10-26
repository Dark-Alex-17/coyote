use super::todo::TODO_FUNCTION_PREFIX;
use super::{FunctionDeclaration, JsonSchema};
use crate::client::{Model, ModelType, call_chat_completions};
use crate::config::child_sessions::{
    self, ChildMeta, ChildSessionRoot, ChildStatus, EffectiveStatus, RootFile,
};
use crate::config::{
    Agent, AgentVariable, AgentVariables, AppConfig, AppState, Input, RequestContext, Role,
    RoleLike, Session, TEMP_SESSION_NAME, child_sessions_enabled, default_max_agent_depth,
    effective_max_concurrent_jobs, jobs_enabled, list_agents_with_descriptions,
    load_agent_variables, paths,
};
use crate::hooks::{self, HookEvent, ResolvedHook};
use crate::mcp::is_auth_required_error;
use crate::supervisor::mailbox::{Envelope, EnvelopePayload, Inbox, PeerRegistry, graph_agent_id};
use crate::supervisor::notification::agent_notification;
use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult, Supervisor, TaskKind};
use crate::utils::process::ProcessStamp;
use crate::utils::{
    AbortSignal, create_abort_signal, latch_user_interrupt, wait_abort_signal, wait_user_interrupt,
};

use crate::graph;
use crate::repl::DEFAULT_CONTINUATION_PROMPT;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use hooks::McpServerHooks;
use indexmap::IndexMap;
use log::{debug, warn};
use parking_lot::RwLock;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tokio::time::Instant;
use uuid::Uuid;

pub const AGENT_FUNCTION_PREFIX: &str = "agent__";

pub const PENDING_TASKS_GUARDRAIL_MAX: u32 = 3;

pub const PARENT_ALIAS: &str = "parent";

fn agent_permitted(whitelist: Option<&[String]>, target: &str) -> bool {
    match whitelist {
        None => true,
        Some(w) => w.iter().any(|a| a == target),
    }
}

fn parse_variables_arg(args: &Value) -> Result<Option<AgentVariables>, String> {
    let Some(value) = args.get("variables") else {
        return Ok(None);
    };
    let Some(obj) = value.as_object() else {
        return Err(
            "'variables' must be a JSON object mapping variable names to string values, e.g. {\"pr\": \"1234\"}".to_string(),
        );
    };
    let mut variables = AgentVariables::new();
    for (key, val) in obj {
        let Some(val) = val.as_str() else {
            return Err(format!("'variables.{key}' must be a string value"));
        };
        variables.insert(key.clone(), val.to_string());
    }
    Ok(Some(variables))
}

fn validate_spawn_variables(
    agent_name: &str,
    declared: &[AgentVariable],
    provided: &AgentVariables,
) -> Result<(), String> {
    let unknown: Vec<&str> = provided
        .keys()
        .filter(|key| !declared.iter().any(|v| v.name == **key))
        .map(String::as_str)
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    if declared.is_empty() {
        return Err(format!(
            "Agent '{agent_name}' declares no variables; remove the 'variables' argument."
        ));
    }

    let declared_list = declared
        .iter()
        .map(|v| format!("{} ({})", v.name, v.description))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Unknown variable(s) {unknown:?} for agent '{agent_name}'. Declared variables: {declared_list}"
    ))
}

fn merge_spawn_variables(inherited: &mut Option<AgentVariables>, provided: AgentVariables) {
    if provided.is_empty() {
        return;
    }

    inherited
        .get_or_insert_with(AgentVariables::new)
        .extend(provided);
}

fn dispatch_spawn_args(
    agent: &str,
    prompt: &str,
    variables: Option<&AgentVariables>,
    task_id: Option<&str>,
) -> Value {
    let mut spawn_args = json!({
        "agent": agent,
        "prompt": prompt,
    });

    if let Some(variables) = variables {
        spawn_args["variables"] = json!(variables);
    }
    if let Some(task_id) = task_id {
        spawn_args["task_id"] = json!(task_id);
    }

    spawn_args
}

fn is_job_task(supervisor: Option<&Arc<RwLock<Supervisor>>>, id: &str) -> bool {
    id.starts_with("job_") || supervisor.is_some_and(|sup| sup.read().has_job(id))
}

fn job_id_teaching_error(id: &str) -> Value {
    json!({
        "status": "error",
        "message": format!(
            "'{id}' is a background job, not an agent — use job__check / job__collect / job__cancel"
        ),
    })
}

pub enum GuardrailAction {
    NoAction,
    Inject(String),
    ForceTerminate(Vec<String>),
}

pub struct PendingTask {
    pub id: String,
    pub kind: TaskKind,
    pub finished: bool,
}

pub fn pending_tasks(ctx: &RequestContext) -> Vec<PendingTask> {
    let Some(sup) = ctx.supervisor.as_ref() else {
        return Vec::new();
    };
    let mut tasks: Vec<PendingTask> = sup
        .read()
        .list_tasks()
        .into_iter()
        .map(|(id, kind, finished)| PendingTask {
            id: id.to_string(),
            kind,
            finished,
        })
        .collect();

    // Inside a graph LLM node, jobs are node-owned: the guardrail must only
    // nag about jobs this node started. Jobs belonging to a parallel branch
    // live in the same shared registry but are that branch's to reclaim.
    if let Some(scope) = ctx.node_job_scope.as_ref() {
        tasks.retain(|t| t.kind != TaskKind::Job || scope.contains(&t.id));
    }

    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    tasks
}

pub fn build_pending_tasks_guardrail_prompt(tasks: &[PendingTask]) -> String {
    let running: Vec<&PendingTask> = tasks.iter().filter(|t| !t.finished).collect();
    let finished: Vec<&PendingTask> = tasks.iter().filter(|t| t.finished).collect();

    let mut sections = Vec::new();
    if !running.is_empty() {
        let id_list = running
            .iter()
            .map(|t| {
                let (kind, collect, cancel) = match t.kind {
                    TaskKind::Agent => ("agent", "agent__collect", "agent__cancel"),
                    TaskKind::Job => ("job", "job__collect", "job__cancel"),
                };
                format!(
                    "- {id} ({kind}): call `{collect}` (blocks until done, returns output) or \
                     `{cancel}` (discards)",
                    id = t.id
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!(
            "Still running ({count}):\n{id_list}\n\nThese will be abandoned if your turn ends \
             now. You MUST reclaim each one before ending your turn. Do NOT emit a text-only \
             response expecting them to 'report back' — they will not.",
            count = running.len()
        ));
    }

    if !finished.is_empty() {
        let cmd_list = finished
            .iter()
            .map(|t| {
                let collect = match t.kind {
                    TaskKind::Agent => "agent__collect",
                    TaskKind::Job => "job__collect",
                };
                format!("- `{collect} --id {id}`", id = t.id)
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!(
            "Completed but UNCOLLECTED — collect NOW ({count}):\n{cmd_list}\n\nCollect returns \
             instantly on a finished task. Their results are LOST if your turn ends without \
             collecting.",
            count = finished.len()
        ));
    }

    format!(
        "[SYSTEM GUARDRAIL] You attempted to end your turn with {count} unreclaimed background \
         task(s).\n\n{body}",
        count = tasks.len(),
        body = sections.join("\n\n")
    )
}

pub fn check_pending_tasks_guardrail(ctx: &mut RequestContext) -> GuardrailAction {
    let pending = pending_tasks(ctx);
    if pending.is_empty() {
        ctx.pending_tasks_guardrail_count = 0;
        return GuardrailAction::NoAction;
    }

    if ctx.pending_tasks_guardrail_count >= PENDING_TASKS_GUARDRAIL_MAX {
        if let Some(sup) = ctx.supervisor.as_ref().cloned() {
            sup.read().cancel_recursive();
            let finished: Vec<&PendingTask> = pending.iter().filter(|t| t.finished).collect();
            if !finished.is_empty() {
                let ids: Vec<&str> = finished.iter().map(|t| t.id.as_str()).collect();
                warn!(
                    "Turn-end guardrail: discarding uncollected result(s) for finished task(s) \
                     after max reminders: {ids:?}"
                );
                let mut sup = sup.write();
                for task in &finished {
                    match task.kind {
                        TaskKind::Agent => {
                            let _ = sup.take(&task.id);
                        }
                        TaskKind::Job => {
                            let _ = sup.take_job(&task.id);
                        }
                    }
                }
            }
        }
        ctx.pending_tasks_guardrail_count = 0;

        return GuardrailAction::ForceTerminate(pending.into_iter().map(|t| t.id).collect());
    }

    ctx.pending_tasks_guardrail_count += 1;
    let mut prompt = build_pending_tasks_guardrail_prompt(&pending);
    if let Some(queue) = ctx.root_escalation_queue()
        && queue.has_pending()
    {
        let summary = serde_json::to_string(&queue.pending_summary()).unwrap_or_default();
        prompt.push_str(&format!(
            "\n\nAdditionally, child agents have pending escalations blocking them. Reply to each \
             via `agent__reply_escalation` first:\n{summary}"
        ));
    }
    GuardrailAction::Inject(prompt)
}

pub fn escalation_function_declarations() -> Vec<FunctionDeclaration> {
    vec![FunctionDeclaration {
        name: format!("{AGENT_FUNCTION_PREFIX}reply_escalation"),
        description: "Reply to a pending escalation from a child agent. The child is blocked waiting for this reply. \
                      Use this after seeing pending_escalations notifications.".to_string(),
        parameters: JsonSchema {
            type_value: Some("object".to_string()),
            properties: Some(IndexMap::from([
                (
                    "escalation_id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The escalation ID from the pending_escalations notification".into()),
                        ..Default::default()
                    },
                ),
                (
                    "reply".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("Your answer to the child agent's question. For ask/confirm questions, use \
                                           the exact option text. For input questions, provide the text response.".into()),
                        ..Default::default()
                    },
                ),
            ])),
            required: Some(vec!["escalation_id".to_string(), "reply".to_string()]),
            ..Default::default()
        },
        agent: false,
    }]
}

pub fn agent_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}spawn"),
            description: "Spawn a subagent to run in the background. Returns an `id` immediately so you can continue \
                          working in parallel. CRITICAL: every spawned agent MUST be reclaimed before you end your \
                          turn — call `agent__collect` to retrieve its output, or `agent__cancel` if you no longer \
                          need it. Ending your turn with pending agents will abandon their work and the system will \
                          reject the turn-end.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "agent".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Name of the agent to spawn (e.g. 'explore', 'coder', 'oracle')".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "prompt".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The task prompt to send to the agent".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "task_id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Optional task queue ID to associate with this agent".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "variables".to_string(),
                        JsonSchema {
                            type_value: Some("object".to_string()),
                            description: Some("Values for the target agent's declared variables (its config/graph `variables:` list), e.g. {\"pr\": \"1234\"}. Overrides values inherited from the parent. String values only. On a `session_id` resume the values must equal the stored ones (omitted = stored); a mismatch is refused. Discover an agent's declared variables via `agent__list_available`.".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "session_id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Resume a child session returned by an earlier spawn/collect (`sess_<agent>_<hex>`): the child wakes with its full transcript and the new `prompt` appended, under a fresh agent id. `null` on a spawn result means that child cannot be resumed (child sessions are off). Omit it to spawn fresh. Unknown ids are an error that lists recent ids; a session that is still running, belongs to another agent, or was spawned with different `variables` is refused.".into()),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["agent".to_string(), "prompt".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}check"),
            description: "Non-blocking status probe: reports whether a spawned agent is still running or finished. \
                          NEVER returns or consumes the result — when finished, call agent__collect to retrieve it.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The agent ID returned by agent__spawn".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}collect"),
            description: "Block until the named spawned agent finishes and return its result. This is your primary \
                          wait primitive — it pauses your execution until the agent completes (or you are interrupted). \
                          Call this for every agent you spawned before ending your turn. Do NOT end your turn assuming \
                          agents will 'report back later' — they will not; they will be abandoned. If you no longer \
                          need an agent's result, call `agent__cancel` instead.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The agent ID returned by agent__spawn".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}list_running"),
            description: "List all subagents YOU have spawned that are still tracked by the supervisor, with their \
                          status. Use this to see which of your background agents are still active. When you run with \
                          concurrent teammates, they appear in a separate `peers` section (message-only, not \
                          collectible). To discover which agent types you can spawn in the first place, use \
                          `agent__list_available` instead.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}list_available"),
            description: "List all agent types installed and available to spawn (name + description + declared \
                          variables). Use this to discover what specialists exist before calling `agent__spawn` — \
                          especially when you're unsure which agent to delegate to. Each agent's `variables` entry \
                          describes the values to pass via the `variables` parameter of `agent__spawn` or \
                          `agent__task_create`. This is the discovery counterpart to `agent__list_running` \
                          (which reports agents you have already spawned).".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}cancel"),
            description: "Cancel a running subagent by its ID. Use this when an agent's output is no longer needed \
                          (e.g. you changed direction, or you're about to end your turn and don't want to wait). \
                          Cancellation cascades: all of the cancelled agent's own descendants are also cancelled. This \
                          call waits briefly for the agent to actually finish cleanup before returning.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The agent ID to cancel".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}task_create"),
            description: "Create a task in the task queue. Returns the task ID.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "subject".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Short title for the task".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "description".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Detailed description of the task".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "blocked_by".to_string(),
                        JsonSchema {
                            type_value: Some("array".to_string()),
                            description: Some("Task IDs that must complete before this task can run".into()),
                            items: Some(Box::new(JsonSchema {
                                type_value: Some("string".to_string()),
                                ..Default::default()
                            })),
                            ..Default::default()
                        },
                    ),
                    (
                        "agent".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Agent to auto-spawn when this task becomes runnable (e.g. 'explore', 'coder'). If set, an agent will be spawned automatically when all dependencies complete.".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "prompt".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Prompt to send to the auto-spawned agent. Required if agent is set.".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "variables".to_string(),
                        JsonSchema {
                            type_value: Some("object".to_string()),
                            description: Some("Values for the auto-spawned agent's declared variables, e.g. {\"pr\": \"1234\"}. Passed to `agent__spawn` at dispatch time. String values only. Discover an agent's declared variables via `agent__list_available`.".into()),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["subject".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}task_list"),
            description: "List all tasks in the task queue with their status and dependencies.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}task_complete"),
            description: "Mark a task as completed. Returns any newly unblocked task IDs.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "task_id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The task ID to mark complete".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["task_id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}task_fail"),
            description: "Mark a task as failed. Dependents will remain blocked.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "task_id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The task ID to mark as failed".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["task_id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
    ]
}

pub fn teammate_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}send_message"),
            description: "Send a text message to another agent's inbox: a child, sibling, or concurrent teammate by its agent id, or your parent via the reserved id 'parent'. Use to share cross-cutting findings or coordinate with teammates.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The target agent ID (including teammate peer ids), or the reserved id 'parent' to message the agent that spawned you".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "message".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The message text to send".into()),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["id".to_string(), "message".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}check_inbox"),
            description: "Check for and drain all pending messages in your inbox from sibling agents, concurrent teammates, or your parent.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
    ]
}

pub async fn handle_agent_tool(
    ctx: &mut RequestContext,
    cmd_name: &str,
    args: &Value,
) -> Result<Value> {
    let action = cmd_name
        .strip_prefix(AGENT_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);

    match action {
        "spawn" => handle_spawn(ctx, args).await,
        "check" => handle_check(ctx, args).await,
        "collect" => handle_collect(ctx, args).await,
        "list_running" => handle_list_running(ctx),
        "list_available" => handle_list_available(ctx),
        "cancel" => handle_cancel(ctx, args).await,
        "send_message" => handle_send_message(ctx, args),
        "check_inbox" => handle_check_inbox(ctx),
        "task_create" => handle_task_create(ctx, args),
        "task_list" => handle_task_list(ctx),
        "task_complete" => handle_task_complete(ctx, args).await,
        "task_fail" => handle_task_fail(ctx, args),
        "reply_escalation" => handle_reply_escalation(ctx, args),
        _ => bail!("Unknown agent action: {action}"),
    }
}

pub fn run_child_agent(
    child_ctx: RequestContext,
    initial_input: Input,
    abort_signal: AbortSignal,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
    run_child_agent_with_graph_inputs(child_ctx, initial_input, abort_signal, None)
}

/// Stops a child agent's own spawned subagents if the future running the
/// child is dropped mid-flight (parent timeout, abort) or the child fails
/// before reaching its own cleanup. Disarmed on the success paths, which
/// already cancel explicitly.
struct CancelOnDrop(Option<Arc<RwLock<Supervisor>>>);

impl CancelOnDrop {
    fn disarm(&mut self) {
        self.0.take();
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(supervisor) = self.0.take() {
            supervisor.read().cancel_recursive();
        }
    }
}

/// Runs a child agent to completion. When the child is a graph agent,
/// `graph_inputs` is overlaid on its `initial_state` before the graph
/// starts. Passing inputs to a config-only agent is rejected before the
/// child runs, since nothing could consume them.
fn run_child_agent_with_graph_inputs(
    mut child_ctx: RequestContext,
    initial_input: Input,
    abort_signal: AbortSignal,
    graph_inputs: Option<HashMap<String, Value>>,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
    Box::pin(async move {
        let mut cancel_on_drop = CancelOnDrop(child_ctx.supervisor.clone());
        if graph::active_agent_graph_name(&child_ctx).is_some() {
            let run = graph::run_active_agent_graph_with_inputs(
                &mut child_ctx,
                &initial_input.text(),
                abort_signal,
                graph_inputs,
            )
            .await?;
            let output = run.result.output;
            // A seated child records the run into its own session, so a
            // resume sees the exchange and the carried state. The headless
            // and REPL callers record on their side.
            if child_ctx.session.is_some() {
                let app = Arc::clone(&child_ctx.app.config);
                child_ctx.record_graph_run(app.as_ref(), &initial_input, &output, run.last_run)?;
            }
            cancel_on_drop.disarm();
            return Ok(output);
        }

        let mut accumulated_output = String::new();
        let mut input = initial_input;
        let app = Arc::clone(&child_ctx.app.config);

        loop {
            let client = input.create_client()?;
            child_ctx.before_chat_completion(&input)?;

            let (output, tool_results) = call_chat_completions(
                &input,
                false,
                false,
                client.as_ref(),
                &mut child_ctx,
                abort_signal.clone(),
            )
            .await?;

            child_ctx.after_chat_completion(app.as_ref(), &input, &output, &tool_results)?;

            if !output.is_empty() {
                if !accumulated_output.is_empty() {
                    accumulated_output.push('\n');
                }
                accumulated_output.push_str(&output);
            }

            if tool_results.is_empty() {
                match check_pending_tasks_guardrail(&mut child_ctx) {
                    GuardrailAction::NoAction => {
                        if let Some(prompt) = todo_continuation_prompt(&mut child_ctx) {
                            input = Input::from_str(&child_ctx, &prompt, None)?;
                            continue;
                        }
                        break;
                    }
                    GuardrailAction::ForceTerminate(ids) => {
                        warn!(
                            "Pending-agent guardrail force-cancelled {} agent(s) after max reminders: {:?}",
                            ids.len(),
                            ids
                        );
                        break;
                    }
                    GuardrailAction::Inject(prompt) => {
                        input = Input::from_str(&child_ctx, &prompt, None)?;
                        continue;
                    }
                }
            }

            input = input.merge_tool_results(output, tool_results);
            let (next, _) = child_ctx.maybe_checkpoint_tool_loop(input)?;
            input = next;
        }

        if let Some(supervisor) = child_ctx.supervisor.clone() {
            supervisor.read().cancel_recursive();
        }
        cancel_on_drop.disarm();

        Ok(accumulated_output)
    })
}

fn todo_continuation_prompt(ctx: &mut RequestContext) -> Option<String> {
    let config = ctx.auto_continue_config();
    if !ctx.app.config.function_calling_support
        || !config.enabled
        || ctx.auto_continue_count >= config.max_continues
        || !ctx.todo_list.has_incomplete()
        || ctx.auto_continue_paused.is_some()
    {
        return None;
    }

    ctx.increment_auto_continue_count();
    debug!(
        "Auto-continuing child agent ({}/{}): {} incomplete todo(s) remain",
        ctx.auto_continue_count,
        config.max_continues,
        ctx.todo_list.incomplete_count()
    );

    let prompt = config
        .continuation_prompt
        .as_deref()
        .unwrap_or(DEFAULT_CONTINUATION_PROMPT);

    Some(format!("{prompt}\n\n{}", ctx.todo_list.render_for_model()))
}

fn render_teammate_roster(self_id: &str, registry: &PeerRegistry) -> String {
    let mut own_label = None;
    let mut lines = Vec::new();

    for (id, label) in registry.roster() {
        if id == self_id {
            own_label = Some(label);
        } else {
            lines.push(format!("- `{id}` — {label}"));
        }
    }

    let own = own_label.map(|l| format!(" ({l})")).unwrap_or_default();

    format!(
        "## Teammate roster\nYou are `{self_id}`{own}. Concurrent teammates:\n{}\n\nMessage a teammate directly via `agent__send_message` when you find something relevant to their slice; check `agent__check_inbox` after completing work phases and before finalizing. You can rediscover peers anytime via `agent__list_running`.",
        lines.join("\n")
    )
}

fn effective_max_agent_depth(parent_ctx: &RequestContext) -> usize {
    if let Some(supervisor) = parent_ctx.supervisor.as_ref() {
        return supervisor.read().max_depth();
    }

    parent_ctx
        .agent
        .as_ref()
        .map(|agent| agent.max_agent_depth())
        .unwrap_or_else(default_max_agent_depth)
}

/// Spawn an agent synchronously from a graph node and return its accumulated
/// output. This is similar to `handle_spawn` but runs the child agent in the
/// current task (no tokio::spawn, no supervisor handle registration) so the
/// graph executor can sequence agent nodes directly.
/// `graph_inputs` seeds the child's graph state and is only accepted when the
/// target is a graph agent.
pub async fn run_agent_for_graph(
    parent_ctx: &mut RequestContext,
    agent_name: &str,
    prompt: &str,
    graph_inputs: Option<HashMap<String, Value>>,
) -> Result<String> {
    let peer_assignment = parent_ctx.peer_assignment.take();
    let assigned_peer = peer_assignment.is_some();
    let peer_registry = if assigned_peer {
        parent_ctx.peer_registry.take()
    } else {
        None
    };
    let (agent_id, child_inbox) =
        peer_assignment.unwrap_or_else(|| (graph_agent_id(agent_name), Arc::new(Inbox::new())));
    let current_depth = parent_ctx.current_depth + 1;

    let max_depth = effective_max_agent_depth(parent_ctx);
    if current_depth > max_depth {
        bail!("Max agent depth exceeded ({current_depth}/{max_depth})");
    }

    if !parent_ctx.app.config.function_calling_support {
        bail!("Function calling support must be enabled to spawn agents.");
    }

    parent_ctx.ensure_root_escalation_queue();
    parent_ctx.ensure_inbox();
    let child_abort = create_abort_signal();

    let app_config = Arc::clone(&parent_ctx.app.config);
    let current_model = parent_ctx.current_model().clone();
    let info_flag = parent_ctx.info_flag;
    let child_app_state = Arc::new(AppState {
        config: Arc::new(app_config.as_ref().clone()),
        vault: parent_ctx.app.vault.clone(),
        mcp_factory: parent_ctx.app.mcp_factory.clone(),
        rag_cache: parent_ctx.app.rag_cache.clone(),
        mcp_config: parent_ctx.app.mcp_config.clone(),
        mcp_log_path: parent_ctx.app.mcp_log_path.clone(),
        mcp_registry: parent_ctx.app.mcp_registry.clone(),
        functions: parent_ctx.app.functions.clone(),
    });

    let agent = Agent::init(
        app_config.as_ref(),
        child_app_state.as_ref(),
        &current_model,
        info_flag,
        agent_name,
        child_abort.clone(),
    )
    .await?;

    let agent_mcp_servers = agent.mcp_server_names().to_vec();
    let session = agent.agent_session().map(|v| v.to_string());
    let child_jobs_enabled = jobs_enabled(Some(&agent), app_config.as_ref());
    let should_init_supervisor = agent.can_spawn_agents() || child_jobs_enabled;
    let agent_max_concurrent_subagents = if agent.can_spawn_agents() {
        agent.max_concurrent_agents()
    } else {
        0
    };
    let agent_max_depth = agent.max_agent_depth();
    let agent_max_jobs = effective_max_concurrent_jobs(Some(&agent), app_config.as_ref());

    let mut child_ctx = RequestContext::new_for_child(
        Arc::clone(&child_app_state),
        parent_ctx,
        current_depth,
        Arc::clone(&child_inbox),
        agent_id.clone(),
    );
    // An agent node runs in the foreground of the enclosing graph turn, so
    // the turn's interrupt is its interrupt. A background child from
    // `agent__spawn` gets no session signal (`new_for_child` leaves it None):
    // the user's Ctrl-C ends only the root turn and leaves background work
    // running until a second Ctrl-C cancels it.
    child_ctx.session_abort = parent_ctx.session_abort.clone();
    child_ctx.task_abort = Some(child_abort.clone());
    child_ctx.rag = agent.rag();
    child_ctx.agent = Some(agent);
    if graph_inputs.is_some() && graph::active_agent_graph_name(&child_ctx).is_none() {
        bail!(
            "agent '{agent_name}' has no graph.yaml; `inputs:` is only valid on agent nodes that target a graph agent"
        );
    }
    if let Some(inputs) = graph_inputs.as_ref() {
        for key in graph::RESERVED_STATE_KEYS {
            if !inputs.contains_key(key) {
                continue;
            }
            let seeded_from = if key == "initial_prompt" {
                " from `prompt:`"
            } else {
                ""
            };
            bail!(
                "agent '{agent_name}': `inputs.{key}` is reserved (the dispatcher seeds it{seeded_from})"
            );
        }
    }
    if assigned_peer {
        child_ctx.peer_registry = peer_registry.clone();
    }
    if should_init_supervisor {
        child_ctx.supervisor = Some(Arc::new(RwLock::new(
            Supervisor::new(agent_max_concurrent_subagents, agent_max_depth)
                .with_max_concurrent_jobs(agent_max_jobs),
        )));
    }

    if let Some(session) = session {
        child_ctx
            .use_session(app_config.as_ref(), Some(&session), child_abort.clone())
            .await?;
        sync_agent_functions_to_ctx(&mut child_ctx)?;
    } else {
        prepare_child_runtime(&mut child_ctx, &agent_mcp_servers).await?;
    }

    // The roster rides the request only, so the stored User message, and
    // any transcript built from it, is the bare prompt.
    let mut input = Input::from_str(&child_ctx, prompt, None)?;
    if let Some(registry) = child_ctx.peer_registry.as_deref() {
        input.set_request_preamble(render_teammate_roster(&agent_id, registry));
    }

    debug!("Spawning agent '{agent_name}' for graph node as '{agent_id}'");

    run_child_agent_with_graph_inputs(child_ctx, input, child_abort, graph_inputs).await
}

/// Connects the agent's MCP servers, syncs its functions and initialises
/// its shared variables on a freshly built child context.
async fn prepare_child_runtime(
    child_ctx: &mut RequestContext,
    server_ids: &[String],
) -> Result<()> {
    populate_agent_mcp_runtime(child_ctx, server_ids).await?;
    child_ctx.refresh_mcp_tool_filters();
    sync_agent_functions_to_ctx(child_ctx)?;
    child_ctx.init_agent_shared_variables()
}

async fn populate_agent_mcp_runtime(ctx: &mut RequestContext, server_ids: &[String]) -> Result<()> {
    if !ctx.app.config.mcp_server_support {
        return Ok(());
    }

    let app = Arc::clone(&ctx.app);
    let server_specs = app
        .mcp_config
        .as_ref()
        .map(|mcp_config| {
            server_ids
                .iter()
                .filter_map(|id| {
                    mcp_config
                        .mcp_servers
                        .get(id)
                        .cloned()
                        .map(|spec| (id.clone(), spec))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mcp_hooks = McpServerHooks::resolve(ctx);
    for (id, spec) in server_specs {
        match app
            .mcp_factory
            .acquire(&id, &spec, app.mcp_log_path.as_deref(), &mcp_hooks)
            .await
        {
            Ok(handle) => {
                ctx.tool_scope.mcp_runtime.insert(id, handle);
            }
            Err(e) if is_auth_required_error(&e) => {
                warn!("spawned agent skipped MCP server '{id}': {e:#}");
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

fn sync_agent_functions_to_ctx(ctx: &mut RequestContext) -> Result<()> {
    let server_features = ctx.tool_scope.mcp_runtime.server_features();
    let functions = {
        let agent = ctx
            .agent
            .as_mut()
            .with_context(|| "Agent should be initialized")?;
        if !server_features.is_empty() {
            agent.append_mcp_meta_functions(server_features);
        }
        agent.functions().clone()
    };

    ctx.tool_scope.functions = functions;
    // Agent::init builds one function set regardless of how the agent runs;
    // this sync is only ever called for spawned children.
    if ctx.self_agent_id.is_some() {
        ctx.tool_scope
            .functions
            .remove_function(&format!("{TODO_FUNCTION_PREFIX}pause"));
    }

    Ok(())
}

/// The `agent.*` extras every event about a spawned child carries. Only a
/// seated child has a session to name, so the key is absent otherwise.
fn spawned_agent_hook_extras(
    agent_id: &str,
    agent_name: &str,
    session_id: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut extras = vec![
        ("COYOTE_AGENT_ID", agent_id.to_string()),
        ("COYOTE_AGENT_NAME", agent_name.to_string()),
    ];
    if let Some(session_id) = session_id {
        extras.push(("COYOTE_AGENT_SESSION_ID", session_id.to_string()));
    }
    extras
}

/// Records a seated child's terminal status on its sidecar. The sidecar is
/// bookkeeping about the child, not part of its result, so a sidecar that
/// cannot be read or written is logged and the caller carries on with the
/// result it already has.
fn finish_child_meta(
    root: Option<&ChildSessionRoot>,
    session_id: Option<&str>,
    status: ChildStatus,
    error: Option<String>,
) {
    let (Some(root), Some(session_id)) = (root, session_id) else {
        return;
    };
    let saved = match root.load_child_meta(session_id) {
        Ok(Some(mut meta)) => {
            meta.finish(status, error);
            root.save_child_meta(&meta)
        }
        Ok(None) => Err(anyhow!(
            "no sidecar at {}",
            root.child_meta_file(session_id).display()
        )),
        Err(error) => Err(error),
    };
    if let Err(error) = saved {
        warn!("Failed to record child session {session_id} as {status:?}: {error:#}");
    }
}

/// Pre-resolved `agent.completed`/`agent.interrupted`/`agent.failed`
/// snapshots for a spawned agent's result arms, captured from the child
/// context before the spawn task takes ownership of it. Resolving against
/// the child keeps global hooks gated by the child's own whitelist, the
/// same gate every other event fired from the child's context goes through.
#[derive(Clone)]
struct SpawnResultHooks {
    completed: Vec<ResolvedHook>,
    failed: Vec<ResolvedHook>,
    interrupted: Vec<ResolvedHook>,
    session_name: Option<String>,
    session_id: Option<String>,
    agent_id: String,
    agent_name: String,
}

impl SpawnResultHooks {
    fn resolve(
        child_ctx: &RequestContext,
        agent_id: &str,
        agent_name: &str,
        session_id: Option<&str>,
    ) -> Self {
        Self {
            completed: child_ctx.resolved_hooks(HookEvent::AgentCompleted),
            failed: child_ctx.resolved_hooks(HookEvent::AgentFailed),
            interrupted: child_ctx.resolved_hooks(HookEvent::AgentInterrupted),
            session_name: child_ctx
                .session
                .as_ref()
                .map(|session| session.name().to_string()),
            session_id: session_id.map(str::to_string),
            agent_id: agent_id.to_string(),
            agent_name: agent_name.to_string(),
        }
    }

    fn fire(self, error: Option<&str>, child_abort: &AbortSignal) {
        let mut extras =
            spawned_agent_hook_extras(&self.agent_id, &self.agent_name, self.session_id.as_deref());
        let (event, resolved) = match error {
            None => (HookEvent::AgentCompleted, self.completed),
            Some(_) if child_abort.aborted_ctrlc() => {
                (HookEvent::AgentInterrupted, self.interrupted)
            }
            Some(error) => {
                extras.push(("COYOTE_AGENT_ERROR", error.to_string()));
                (HookEvent::AgentFailed, self.failed)
            }
        };
        hooks::fire_resolved(
            event,
            resolved,
            hooks::base_envs_parts(event, self.session_name.as_deref(), Some(&self.agent_name)),
            &extras,
            None,
        );
    }
}

/// A spawned child's context and first turn, assembled by
/// `build_child_spawn` and moved into the spawn task by `handle_spawn`.
struct ChildSpawn {
    child_ctx: RequestContext,
    input: Input,
    agent_id: String,
    agent_name: String,
    session_id: Option<String>,
    child_abort: AbortSignal,
    child_inbox: Arc<Inbox>,
    current_depth: usize,
}

enum SpawnBuild {
    Ready(Box<ChildSpawn>),
    /// The error payload the tool answers with instead of spawning.
    Refused(Value),
}

/// The spawner's own session file, recorded in the child root so the root
/// can later be matched back to it. Only a root spawner with a named
/// session that has a file has one; a child spawner's session already
/// lives in the root. Read off the session rather than derived from its
/// name, because the spawner may be mid-way through a delegated chat turn
/// whose agent would derive a different directory.
fn root_spawner_session_file(spawner: &RequestContext) -> Option<PathBuf> {
    if spawner.self_agent_id.is_some() {
        return None;
    }
    let session = spawner.session.as_ref()?;
    if session.name() == TEMP_SESSION_NAME {
        return None;
    }
    session.path().map(Path::to_path_buf)
}

/// Seats the child on a fresh owner-only session file under the spawner's
/// root. Writes `root.yaml` and `children/` first (so the owner-only save
/// has a private directory to land in), then the sidecar, then the empty
/// session file, so all three exist before the child's first turn.
///
/// The session takes its role from `child_ctx.agent`, so it carries the
/// spawned agent's resolved model: the agent's own `model:` when the config
/// pins one, otherwise the spawner's current model that `Agent::init` fell
/// back to. It is never overridden with the spawner's model, because
/// `current_model()` reads the session first once one is attached and a
/// pinned child would otherwise run on the spawner's model.
fn seat_child_session(
    spawner: &RequestContext,
    child_ctx: &mut RequestContext,
    root: &ChildSessionRoot,
    meta: &ChildMeta,
) -> Result<()> {
    let app_config = Arc::clone(&child_ctx.app.config);
    let mut session = Session::new_from_ctx(child_ctx, app_config.as_ref(), &meta.session_id)?;
    let path = root.child_session_file(&meta.session_id);
    session.set_save_session(Some(true));
    session.set_path(path.clone());
    session.set_owner_only(true);
    if let Some(agent) = child_ctx.agent.as_ref() {
        session.sync_agent(agent);
    }
    let parent_path = root_spawner_session_file(spawner);
    root.ensure_dir(&RootFile::seed(meta.stamp(), parent_path.as_deref()))?;
    root.save_child_meta(meta)?;
    // With `save_session: true` already in the app config nothing above
    // dirties the session, and `flush` only writes a dirty one. The write
    // is as fatal as the sidecar's: a child must not start behind a sidecar
    // that says running with no session file next to it.
    session.mark_dirty();
    if let Err(error) = session
        .flush()
        .with_context(|| format!("Failed to write child session {}", path.display()))
    {
        // The sidecar already says running; without this it would read as
        // a crash of a child that never started.
        finish_child_meta(
            Some(root),
            Some(&meta.session_id),
            ChildStatus::Failed,
            Some(format!("{error:#}")),
        );
        return Err(error);
    }
    child_ctx.session = Some(session);
    Ok(())
}

/// Names the setting that turned child sessions off for a spawn, so the
/// refusal to resume points at the file to change. Broadest setting first:
/// the app config, then the spawned agent's own config, then the dry-run
/// flag.
fn knob_source(spawned: Option<&Agent>, app: &AppConfig) -> String {
    if app.child_sessions == Some(false) {
        return format!(
            "child_sessions: false in {}",
            paths::config_file().display()
        );
    }
    if let Some(agent) = spawned.filter(|agent| agent.child_sessions() == Some(false)) {
        return format!(
            "child_sessions: false in {}",
            paths::agent_config_file(agent.name()).display()
        );
    }
    "dry_run: true".to_string()
}

/// The recent child ids an unknown-id refusal lists, newest first, each
/// marked `(crashed)` when its sidecar still says running but the
/// recording process is gone. A stamp recorded with `start` 0 (sysinfo
/// could not list the own process at spawn) is dead unless sysinfo later
/// lists that pid with a start time of 0 as well, so in practice such a
/// child is listed as crashed and stays resumable: fail-closed by design.
fn recent_child_ids_hint(root: &ChildSessionRoot) -> String {
    let (ids, more) = root.recent_child_ids(10);
    if ids.is_empty() {
        return "none".to_string();
    }
    let mut hint = ids
        .iter()
        .map(|id| {
            let crashed = matches!(
                root.load_child_meta(id),
                Ok(Some(meta)) if meta.effective_status() == EffectiveStatus::Crashed
            );
            if crashed {
                format!("{id} (crashed)")
            } else {
                id.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if more > 0 {
        hint.push_str(&format!(" (+{more} more)"));
    }
    hint
}

/// The keys of `provided` whose stored value differs or is missing, sorted.
/// Keys the caller left out are taken as agreement with the stored value.
fn differing_variables(provided: &AgentVariables, stored: &AgentVariables) -> Vec<String> {
    let mut keys: Vec<String> = provided
        .iter()
        .filter(|(key, value)| stored.get(key.as_str()) != Some(*value))
        .map(|(key, _)| key.clone())
        .collect();
    keys.sort();
    keys
}

/// Seats the child on an existing child session under `root` and re-stamps
/// its sidecar as running under the fresh agent id. Every refusal happens
/// before the session file is touched, and the id is validated before any
/// path is built from it. The stored transcript, instructions and variables
/// win over the agent's current config; only the model is replaced, with
/// the spawned agent's resolved one (its own pinned `model:` when the
/// config sets one, otherwise the spawner's current model, exactly what a
/// fresh seat carries), so a stored model id that no longer resolves cannot
/// block the resume.
fn resume_child_session(
    child_ctx: &mut RequestContext,
    root: &ChildSessionRoot,
    id: &str,
    agent_name: &str,
    agent_id: &str,
    variables: Option<&AgentVariables>,
    resolved_model: &Model,
) -> Result<()> {
    if child_sessions::parse_session_id(id).is_none() {
        bail!("'{id}' is not a child session id (expected sess_<agent>_<12 hex>)");
    }
    let Some(mut meta) = root
        .load_child_meta(id)
        .with_context(|| format!("Failed to read the sidecar of child session '{id}'"))?
    else {
        bail!(
            "no child session '{id}' under this session; recent ids: {}",
            recent_child_ids_hint(root)
        );
    };
    if meta.agent != agent_name {
        bail!(
            "'{id}' belongs to agent '{}', not '{agent_name}'",
            meta.agent
        );
    }
    if meta.status == ChildStatus::Running && meta.stamp().is_alive() {
        bail!("'{id}' is still running; collect or cancel it first");
    }

    let path = root.child_session_file(id);
    let app_config = Arc::clone(&child_ctx.app.config);
    let mut session = Session::load_from_path(
        &path,
        resolved_model.clone(),
        child_ctx,
        app_config.as_ref(),
    )?;
    if let Some(provided) = variables {
        let differing = differing_variables(provided, session.agent_variables());
        if !differing.is_empty() {
            bail!(
                "'{id}' was spawned with different variables ({}); resume with the same values or spawn fresh",
                differing.join(", ")
            );
        }
    }
    let tokens = session.tokens();
    let max = resolved_model.max_input_tokens().unwrap_or(usize::MAX);
    if tokens > max {
        bail!(
            "'{id}' holds ~{tokens} tokens, over the {} window ({max}); child compression is not available yet - spawn fresh",
            resolved_model.id()
        );
    }

    session.set_owner_only(true);
    session.set_save_session(Some(true));
    child_ctx.agent_variables = Some(session.agent_variables().clone());
    child_ctx.session = Some(session);

    let stamp = ProcessStamp::current();
    meta.status = ChildStatus::Running;
    meta.agent_id = agent_id.to_string();
    meta.pid = stamp.pid;
    meta.pid_start = stamp.start;
    meta.updated_at = Utc::now();
    meta.error = None;
    root.save_child_meta(&meta)
}

async fn build_child_spawn(ctx: &mut RequestContext, args: &Value) -> Result<SpawnBuild> {
    let agent_name = args
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'agent' is required"))?
        .to_string();
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'prompt' is required"))?
        .to_string();
    let task_id = args
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let requested_session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut variables = match parse_variables_arg(args) {
        Ok(v) => v,
        Err(message) => {
            return Ok(SpawnBuild::Refused(json!({
                "status": "error",
                "message": message,
            })));
        }
    };

    if let Some(parent) = ctx.agent.as_ref()
        && !agent_permitted(parent.spawnable_agents(), &agent_name)
    {
        let whitelist = parent.spawnable_agents().unwrap_or_default();
        return Ok(SpawnBuild::Refused(json!({
            "status": "error",
            "message": format!(
                "Agent '{agent_name}' is not in this agent's `spawnable_agents` whitelist. Allowed: {whitelist:?}. Call `agent__list_available` to see what you can spawn."
            ),
        })));
    }

    let short_uuid = &Uuid::new_v4().to_string()[..8];
    let agent_id = format!("agent_{agent_name}_{short_uuid}");

    let (max_depth, current_depth) = {
        let supervisor = ctx
            .supervisor
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("No supervisor active; Agent spawning not enabled"))?;
        let sup = supervisor.read();
        if sup.active_count() >= sup.max_concurrent() {
            return Ok(SpawnBuild::Refused(json!({
                "status": "error",
                "message": format!(
                    "At capacity: {}/{} agents running. Wait for one to finish or cancel one.",
                    sup.active_count(),
                    sup.max_concurrent()
                ),
            })));
        }
        (sup.max_depth(), ctx.current_depth + 1)
    };

    if current_depth > max_depth {
        return Ok(SpawnBuild::Refused(json!({
            "status": "error",
            "message": format!("Max agent depth exceeded ({current_depth}/{max_depth})"),
        })));
    }

    let child_inbox = Arc::new(Inbox::new());

    ctx.ensure_root_escalation_queue();
    ctx.ensure_inbox();

    let child_abort = create_abort_signal();

    if !ctx.app.config.function_calling_support {
        bail!("Please enable function calling support before using the agent.");
    }

    let app_config = Arc::clone(&ctx.app.config);
    let current_model = ctx.current_model().clone();
    let info_flag = ctx.info_flag;
    let child_app_state = Arc::new(AppState {
        config: Arc::new(app_config.as_ref().clone()),
        vault: ctx.app.vault.clone(),
        mcp_factory: ctx.app.mcp_factory.clone(),
        rag_cache: ctx.app.rag_cache.clone(),
        mcp_config: ctx.app.mcp_config.clone(),
        mcp_log_path: ctx.app.mcp_log_path.clone(),
        mcp_registry: ctx.app.mcp_registry.clone(),
        functions: ctx.app.functions.clone(),
    });
    let agent = Agent::init(
        app_config.as_ref(),
        child_app_state.as_ref(),
        &current_model,
        info_flag,
        &agent_name,
        child_abort.clone(),
    )
    .await?;

    if let Some(provided) = variables.as_ref()
        && let Err(message) =
            validate_spawn_variables(&agent_name, agent.defined_variables(), provided)
    {
        return Ok(SpawnBuild::Refused(json!({
            "status": "error",
            "message": message,
        })));
    }

    let agent_mcp_servers = agent.mcp_server_names().to_vec();
    let resolved_model = agent.model().clone();
    let seat_session = child_sessions_enabled(Some(&agent), app_config.as_ref());
    if requested_session_id.is_some() && !seat_session {
        bail!(
            "child sessions are off ({}); spawn fresh with the context in the prompt",
            knob_source(Some(&agent), app_config.as_ref())
        );
    }
    let child_jobs_enabled = jobs_enabled(Some(&agent), app_config.as_ref());
    let should_init_supervisor = agent.can_spawn_agents() || child_jobs_enabled;
    let max_concurrent_agents = if agent.can_spawn_agents() {
        agent.max_concurrent_agents()
    } else {
        0
    };
    let max_depth = agent.max_agent_depth();
    let max_jobs = effective_max_concurrent_jobs(Some(&agent), app_config.as_ref());
    // Resolved on the spawner before the child copies it. With the app
    // knob off, or under a dry run, nothing is resolved, so a root session
    // is never given a uid for a feature the user turned off.
    let root = if child_sessions_enabled(None, app_config.as_ref()) {
        ctx.ensure_child_session_root()
    } else {
        None
    };
    let mut child_ctx = RequestContext::new_for_child(
        Arc::clone(&child_app_state),
        ctx,
        current_depth,
        Arc::clone(&child_inbox),
        agent_id.clone(),
    );
    child_ctx.task_abort = Some(child_abort.clone());
    child_ctx.child_session_root = root.clone();
    child_ctx.rag = agent.rag();
    child_ctx.agent = Some(agent);
    if should_init_supervisor {
        child_ctx.supervisor = Some(Arc::new(RwLock::new(
            Supervisor::new(max_concurrent_agents, max_depth).with_max_concurrent_jobs(max_jobs),
        )));
    }
    // On a resume the stored variables win, so the provided ones only
    // serve the equality check made when the session is loaded.
    if requested_session_id.is_none()
        && let Some(provided) = variables.take()
    {
        merge_spawn_variables(&mut child_ctx.agent_variables, provided);
    }

    // A resume seats the stored session before the agent's shared variables
    // are initialised, so a variable declared without a default is
    // satisfied from the stored values and the instructions render from
    // them; it also puts the cheap refusals ahead of the MCP spawn.
    let resumed_session_id = match (requested_session_id.as_deref(), root.as_deref()) {
        (Some(id), Some(root)) => {
            resume_child_session(
                &mut child_ctx,
                root,
                id,
                &agent_name,
                &agent_id,
                variables.as_ref(),
                &resolved_model,
            )?;
            Some(id.to_string())
        }
        (Some(id), None) => {
            bail!(
                "cannot resume '{id}': this spawner has no child session root to look it up under"
            );
        }
        (None, _) => None,
    };

    // A resume has already re-stamped the sidecar running under this pid;
    // a failure here must mark it failed or a retry is refused as still
    // running until the spawner exits.
    if let Err(error) = prepare_child_runtime(&mut child_ctx, &agent_mcp_servers).await {
        finish_child_meta(
            root.as_deref(),
            resumed_session_id.as_deref(),
            ChildStatus::Failed,
            Some(format!("{error:#}")),
        );
        return Err(error);
    }

    let session_id = match (resumed_session_id, root) {
        (Some(id), _) => Some(id),
        (None, Some(root)) if seat_session => {
            let session_id = child_sessions::new_session_id(&agent_name);
            // Names the spawner's own child session; stays empty for any
            // other session a spawner may hold (the REPL root's, however it
            // is named, or a temp one).
            let spawner_session_id = ctx
                .session
                .as_ref()
                .filter(|_| ctx.self_agent_id.is_some())
                .map(Session::name)
                .filter(|name| child_sessions::parse_session_id(name).is_some())
                .map(str::to_string);
            let meta = ChildMeta::new_running(
                &session_id,
                &agent_name,
                &agent_id,
                ProcessStamp::current(),
                spawner_session_id,
                ctx.self_agent_id.clone(),
                &prompt,
                task_id,
            );
            seat_child_session(ctx, &mut child_ctx, &root, &meta)?;
            Some(session_id)
        }
        (None, None) if seat_session => {
            debug!(
                "Spawner of '{agent_id}' has no child session root; child runs without a session"
            );
            None
        }
        _ => None,
    };

    let input = match Input::from_str(&child_ctx, &prompt, None) {
        Ok(input) => input,
        Err(error) => {
            finish_child_meta(
                child_ctx.child_session_root.as_deref(),
                session_id.as_deref(),
                ChildStatus::Failed,
                Some(format!("{error:#}")),
            );
            return Err(error);
        }
    };

    debug!("Spawning child agent '{agent_name}' as '{agent_id}'");

    hooks::fire(
        HookEvent::AgentStarted,
        &child_ctx,
        &spawned_agent_hook_extras(&agent_id, &agent_name, session_id.as_deref()),
        None,
    );

    Ok(SpawnBuild::Ready(Box::new(ChildSpawn {
        child_ctx,
        input,
        agent_id,
        agent_name,
        session_id,
        child_abort,
        child_inbox,
        current_depth,
    })))
}

async fn handle_spawn(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let spawn = match build_child_spawn(ctx, args).await? {
        SpawnBuild::Ready(spawn) => spawn,
        SpawnBuild::Refused(payload) => return Ok(payload),
    };
    launch_child_spawn(ctx, spawn)
}

/// Runs a built child on its own task and registers the handle with the
/// spawner's supervisor. From here on the child's sidecar says running, so
/// every exit, including a failed registration, records a terminal status.
fn launch_child_spawn(ctx: &mut RequestContext, spawn: Box<ChildSpawn>) -> Result<Value> {
    let ChildSpawn {
        child_ctx,
        input,
        agent_id,
        agent_name,
        session_id,
        child_abort,
        child_inbox,
        current_depth,
    } = *spawn;

    // Resolved before the spawn: the result arms outlive the child context
    // the snapshots come from.
    let result_hooks =
        SpawnResultHooks::resolve(&child_ctx, &agent_id, &agent_name, session_id.as_deref());
    let refused_hooks = result_hooks.clone();

    let spawn_agent_id = agent_id.clone();
    let spawn_agent_name = agent_name.clone();
    let spawn_session_id = session_id.clone();
    let spawn_abort = child_abort.clone();
    let hook_abort = child_abort.clone();
    let refused_abort = child_abort.clone();
    let spawn_notifications = Arc::clone(&ctx.notification_queue);
    let child_supervisor = child_ctx.supervisor.clone();
    let root = child_ctx.child_session_root.clone();
    let spawn_root = root.clone();

    let join_handle = tokio::spawn(async move {
        let result = run_child_agent(child_ctx, input, spawn_abort).await;

        // A ctrl-c'd child reports cancelled whatever its run returned, so
        // the sidecar agrees with the cancel path that signalled it.
        let (status, error) = if hook_abort.aborted_ctrlc() {
            (ChildStatus::Cancelled, None)
        } else {
            match &result {
                Ok(_) => (ChildStatus::Completed, None),
                Err(e) => (ChildStatus::Failed, Some(format!("{e:#}"))),
            }
        };
        finish_child_meta(
            spawn_root.as_deref(),
            spawn_session_id.as_deref(),
            status,
            error,
        );

        let agent_result = match result {
            Ok(output) => AgentResult {
                id: spawn_agent_id,
                agent_name: spawn_agent_name,
                output,
                exit_status: AgentExitStatus::Completed,
                session_id: spawn_session_id,
            },
            Err(e) => AgentResult {
                id: spawn_agent_id,
                agent_name: spawn_agent_name,
                output: String::new(),
                exit_status: AgentExitStatus::Failed(e.to_string()),
                session_id: spawn_session_id,
            },
        };
        let success = agent_result.exit_status == AgentExitStatus::Completed;
        match &agent_result.exit_status {
            AgentExitStatus::Completed => result_hooks.fire(None, &hook_abort),
            AgentExitStatus::Failed(e) => result_hooks.fire(Some(e), &hook_abort),
        }
        spawn_notifications.push(agent_notification(
            &agent_result.id,
            &agent_result.agent_name,
            success,
        ));

        Ok(agent_result)
    });

    let orphan_abort = join_handle.abort_handle();
    let handle = AgentHandle {
        id: agent_id.clone(),
        agent_name: agent_name.clone(),
        depth: current_depth,
        inbox: child_inbox,
        abort_signal: child_abort,
        join_handle,
        child_supervisor,
        session_id: session_id.clone(),
    };

    let registered = match ctx.supervisor.as_ref() {
        Some(supervisor) => supervisor.write().register(handle),
        None => Err(anyhow!("No supervisor active")),
    };
    if let Err(error) = registered {
        // Aborted before the failed write so the orphan's own exit can
        // never overwrite it with completed/failed, fire result hooks, or
        // notify about a child no supervisor tracks. `agent.started` already
        // fired for this child, so the refusal fires the one `agent.failed`
        // that closes it.
        orphan_abort.abort();
        finish_child_meta(
            root.as_deref(),
            session_id.as_deref(),
            ChildStatus::Failed,
            Some(format!("{error:#}")),
        );
        refused_hooks.fire(Some(&format!("{error:#}")), &refused_abort);
        return Err(error);
    }

    Ok(json!({
        "status": "ok",
        "id": agent_id,
        "agent": agent_name,
        "session_id": session_id,
        "message": format!("Agent '{agent_name}' spawned as '{agent_id}' and is running in the background. CRITICAL: \
                           you MUST reclaim this agent before ending your turn — call `agent__collect` (blocks until \
                           done, returns output) or `agent__cancel` (if you no longer need it). Ending your turn with \
                           unreclaimed agents will be rejected and forces you to handle them. Do NOT assume the agent \
                           will 'report back' on its own."),
    }))
}

async fn handle_check(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let (is_finished, session_id) = {
        let supervisor = ctx
            .supervisor
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("No supervisor active"))?;
        let sup = supervisor.read();
        (sup.is_finished(id), sup.session_id_for(id))
    };

    match is_finished {
        Some(true) => Ok(json!({
            "status": "finished",
            "id": id,
            "session_id": session_id,
            "message": format!(
                "Agent '{id}' has finished; its result is ready and has NOT been consumed. \
                 Call `agent__collect --id {id}` to retrieve it (returns instantly on a \
                 finished agent). The handle stays registered until collected."
            ),
        })),
        Some(false) => {
            let mut result = json!({
                "status": "pending",
                "id": id,
                "session_id": session_id,
                "message": "Agent is still running"
            });

            if let Some(queue) = ctx.root_escalation_queue()
                && queue.has_pending()
            {
                let summary = queue.pending_summary();
                result["pending_escalations"] = json!(summary);
                result["message"] = json!(
                    "Agent is still running. Child agents have pending escalations that need your reply via agent__reply_escalation."
                );
            }

            Ok(result)
        }
        None => {
            if is_job_task(ctx.supervisor.as_ref(), id) {
                return Ok(job_id_teaching_error(id));
            }

            Ok(json!({
                "status": "error",
                "message": format!("No agent found with id '{id}'")
            }))
        }
    }
}

async fn handle_collect(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;

    let (target_abort, session_id) = {
        let sup = supervisor.read();
        if sup.is_finished(id).is_none() {
            if id.starts_with("job_") || sup.has_job(id) {
                return Ok(job_id_teaching_error(id));
            }

            return Ok(json!({
                "status": "error",
                "message": format!("Agent '{id}' not found. Use agent__check to verify it exists and is finished.")
            }));
        }
        (sup.abort_signal_for(id), sup.session_id_for(id))
    };

    let interrupted = || {
        json!({
            "status": "interrupted",
            "id": id,
            "session_id": session_id,
            "message": format!("agent__collect was interrupted by the user; agent '{id}' is still running in the background. Collect it again later, cancel it with agent__cancel, or continue with other work."),
        })
    };

    let user_interrupts_here = ctx.user_interrupts_here();
    loop {
        let is_finished = {
            let sup = supervisor.read();
            sup.is_finished(id).unwrap_or(false)
        };

        if is_finished {
            break;
        }

        if user_interrupts_here && ctx.session_abort.as_ref().is_some_and(|s| s.aborted()) {
            return Ok(interrupted());
        }

        if let Some(queue) = ctx.root_escalation_queue()
            && queue.has_pending()
        {
            let summary = queue.pending_summary();
            return Ok(json!({
                "status": "pending",
                "id": id,
                "session_id": session_id,
                "message": format!("Agent '{id}' is still running, but child agents have pending escalations that need your reply. Reply via agent__reply_escalation, then call agent__collect again."),
                "pending_escalations": summary,
            }));
        }

        match target_abort.as_ref() {
            Some(abort) if abort.aborted() => {
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    if supervisor.read().is_finished(id).unwrap_or(false) {
                        break;
                    }
                    time::sleep(Duration::from_millis(50)).await;
                }
                break;
            }
            Some(abort) => {
                tokio::select! {
                    _ = time::sleep(Duration::from_millis(200)) => {}
                    _ = wait_abort_signal(abort) => {}
                    _ = wait_user_interrupt(ctx.session_abort.as_ref()), if user_interrupts_here => {
                        return Ok(interrupted());
                    }
                    _ = latch_user_interrupt(ctx.session_abort.as_ref()), if !user_interrupts_here => {}
                }
            }
            None => {
                tokio::select! {
                    _ = time::sleep(Duration::from_millis(200)) => {}
                    _ = wait_user_interrupt(ctx.session_abort.as_ref()), if user_interrupts_here => {
                        return Ok(interrupted());
                    }
                    _ = latch_user_interrupt(ctx.session_abort.as_ref()), if !user_interrupts_here => {}
                }
            }
        }
    }

    let handle = {
        let mut sup = supervisor.write();
        sup.take(id)
    };

    match handle {
        Some(handle) => {
            let result = handle
                .join_handle
                .await
                .map_err(|e| anyhow!("Agent task panicked: {e}"))?
                .map_err(|e| anyhow!("Agent failed: {e}"))?;

            let output = summarize_output(ctx, &result.agent_name, &result.output).await?;
            ctx.pending_tasks_guardrail_count = 0;

            Ok(json!({
                "status": "completed",
                "id": result.id,
                "agent": result.agent_name,
                "session_id": result.session_id,
                "exit_status": format!("{:?}", result.exit_status),
                "output": output,
            }))
        }
        None => Ok(json!({
            "status": "error",
            "message": format!("Agent '{id}' completed but could not be collected. It may have been collected by another call.")
        })),
    }
}

fn handle_list_running(ctx: &mut RequestContext) -> Result<Value> {
    let peers: Option<Vec<Value>> = ctx.peer_registry.as_ref().map(|registry| {
        registry
            .roster()
            .into_iter()
            .filter(|(id, _)| Some(id.as_str()) != ctx.self_agent_id.as_deref())
            .map(|(id, label)| {
                let status = if registry.is_finished(&id) {
                    "finished"
                } else {
                    "running"
                };
                json!({ "id": id, "label": label, "kind": "peer", "status": status })
            })
            .collect()
    });

    let supervisor = ctx.supervisor.as_ref().cloned();
    if supervisor.is_none() && peers.is_none() {
        return Err(anyhow!("No supervisor active"));
    }

    let mut result = match supervisor {
        Some(supervisor) => {
            let sup = supervisor.read();
            let agents: Vec<Value> = sup
                .list_agents()
                .into_iter()
                .map(|(id, name)| {
                    let finished = sup.is_finished(id).unwrap_or(false);
                    json!({
                        "id": id,
                        "agent": name,
                        "session_id": sup.session_id_for(id),
                        "status": if finished { "finished" } else { "running" },
                    })
                })
                .collect();
            json!({
                "active_count": sup.active_count(),
                "max_concurrent": sup.max_concurrent(),
                "agents": agents,
            })
        }
        None => json!({}),
    };

    if let Some(peers) = peers {
        result["peers"] = Value::Array(peers);
        result["peers_note"] = json!(
            "Peers are concurrent teammates reachable via agent__send_message; they cannot be checked, collected, or cancelled."
        );
    }

    Ok(result)
}

fn variables_json(vars: &[AgentVariable]) -> Option<Value> {
    if vars.is_empty() {
        return None;
    }
    Some(Value::Array(
        vars.iter()
            .map(|v| {
                let mut var = json!({
                    "name": v.name,
                    "description": v.description,
                    "required": v.default.is_none(),
                });
                if let Some(default) = &v.default {
                    var["default"] = json!(default);
                }
                var
            })
            .collect(),
    ))
}

fn handle_list_available(ctx: &RequestContext) -> Result<Value> {
    let whitelist: Option<Vec<String>> = ctx
        .agent
        .as_ref()
        .and_then(|a| a.spawnable_agents())
        .map(<[String]>::to_vec);

    let entries: Vec<(String, String)> = list_agents_with_descriptions()
        .into_iter()
        .filter(|(name, _)| agent_permitted(whitelist.as_deref(), name))
        .collect();
    let count = entries.len();
    let agents: Vec<Value> = entries
        .into_iter()
        .map(|(name, description)| {
            let mut agent = if description.is_empty() {
                json!({ "name": name })
            } else {
                json!({ "name": name, "description": description })
            };
            if let Some(variables) = variables_json(&load_agent_variables(&name)) {
                agent["variables"] = variables;
            }
            agent
        })
        .collect();

    Ok(json!({
        "count": count,
        "agents": agents,
    }))
}

async fn handle_cancel(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;

    let handle = {
        let mut sup = supervisor.write();
        sup.take(id)
    };

    match handle {
        Some(handle) => {
            let agent_name = handle.agent_name.clone();
            let session_id = handle.session_id.clone();
            if let Some(child_sup) = handle.child_supervisor.as_ref() {
                child_sup.read().cancel_recursive();
            }
            let was_running = !handle.join_handle.is_finished();
            handle.abort_signal.set_ctrlc();
            // Written here as well as by the child's own exit, so the
            // sidecar reads cancelled even when the child outlives the
            // cleanup wait below. A child that had already finished keeps
            // the status its own exit recorded: cancelling an uncollected
            // result is not what ended it. The children live under the
            // spawner's root.
            if was_running {
                finish_child_meta(
                    ctx.child_session_root.as_deref(),
                    session_id.as_deref(),
                    ChildStatus::Cancelled,
                    None,
                );
            }

            let cleanup = tokio::time::timeout(Duration::from_secs(5), handle.join_handle).await;

            ctx.pending_tasks_guardrail_count = 0;

            let message = match cleanup {
                Ok(_) => format!("Cancelled agent '{agent_name}' and waited for cleanup."),
                Err(_) => format!(
                    "Cancelled agent '{agent_name}'; cleanup did not complete within 5s. Its descendants have been signalled and will tear down asynchronously."
                ),
            };

            Ok(json!({
                "status": "ok",
                "id": id,
                "session_id": session_id,
                "message": message,
            }))
        }
        None => {
            if is_job_task(ctx.supervisor.as_ref(), id) {
                return Ok(job_id_teaching_error(id));
            }

            Ok(json!({
                "status": "error",
                "message": format!("No agent found with id '{id}'"),
            }))
        }
    }
}

fn handle_send_message(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'message' is required"))?;

    // The routable identity of this sender from the recipient's point of view.
    // Only spawned agents have a `self_agent_id`; the name fallback covers
    // contexts that cannot be replied to anyway (e.g. macro contexts).
    let self_label = ctx
        .self_agent_id
        .clone()
        .or_else(|| ctx.agent.as_ref().map(|a| a.name().to_string()))
        .unwrap_or_else(|| PARENT_ALIAS.to_string());

    let deliver = |inbox: &Arc<Inbox>, from: String| {
        inbox.deliver(Envelope {
            from,
            to: id.to_string(),
            payload: EnvelopePayload::Text {
                content: message.to_string(),
            },
            timestamp: Utc::now(),
        });
    };

    if id == PARENT_ALIAS {
        return match ctx.parent_inbox.as_ref() {
            Some(inbox) => {
                deliver(inbox, self_label);
                Ok(json!({
                    "status": "ok",
                    "message": "Message delivered to your parent agent's inbox; it will be read on the parent's next check_inbox call.",
                }))
            }
            None => Ok(json!({
                "status": "error",
                "message": "You have no parent agent — the reserved id 'parent' is only routable from a spawned subagent.",
            })),
        };
    }

    let own_child_inbox = ctx
        .supervisor
        .as_ref()
        .and_then(|sup| sup.read().inbox(id).cloned());
    if let Some(inbox) = own_child_inbox {
        deliver(&inbox, PARENT_ALIAS.to_string());
        return Ok(json!({
            "status": "ok",
            "message": format!("Message delivered to agent '{id}'"),
        }));
    }

    let sibling_inbox = ctx
        .parent_supervisor
        .as_ref()
        .and_then(|sup| sup.read().inbox(id).cloned());
    if let Some(inbox) = sibling_inbox {
        deliver(&inbox, self_label);
        return Ok(json!({
            "status": "ok",
            "message": format!("Message delivered to agent '{id}'"),
        }));
    }

    if let Some(registry) = ctx.peer_registry.as_ref()
        && let Some(inbox) = registry.get(id)
    {
        if registry.is_finished(id) {
            return Ok(json!({
                "status": "error",
                "message": format!(
                    "Teammate '{id}' has already finished; message not delivered. \
                     Its results flow through the graph's collected outputs."
                ),
            }));
        }
        deliver(&inbox, self_label);
        return Ok(json!({
            "status": "ok",
            "message": format!("Message delivered to teammate '{id}'"),
        }));
    }

    if is_job_task(ctx.supervisor.as_ref(), id) || is_job_task(ctx.parent_supervisor.as_ref(), id) {
        return Ok(job_id_teaching_error(id));
    }

    Ok(json!({
        "status": "error",
        "message": format!("No agent found with id '{id}'. Agent may not exist or may have already completed."),
    }))
}

fn handle_check_inbox(ctx: &mut RequestContext) -> Result<Value> {
    match ctx.inbox.as_ref() {
        Some(inbox) => {
            let messages: Vec<Value> = inbox
                .drain()
                .into_iter()
                .map(|e| {
                    json!({
                        "from": e.from,
                        "payload": e.payload,
                        "timestamp": e.timestamp.to_rfc3339(),
                    })
                })
                .collect();
            let count = messages.len();
            Ok(json!({
                "messages": messages,
                "count": count,
            }))
        }
        None => Ok(json!({
            "messages": [],
            "count": 0,
        })),
    }
}

fn handle_reply_escalation(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let escalation_id = args
        .get("escalation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'escalation_id' is required"))?;
    let reply = args
        .get("reply")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'reply' is required"))?;

    let queue = ctx
        .escalation_queue
        .clone()
        .ok_or_else(|| anyhow!("No escalation queue available"))?;

    match queue.take(escalation_id) {
        Some(request) => {
            let from_agent = request.from_agent_name.clone();
            let question = request.question.clone();
            let _ = request.reply_tx.send(reply.to_string());
            hooks::fire(
                HookEvent::EscalationAnswered,
                ctx,
                &[
                    ("COYOTE_ESCALATION_ID", escalation_id.to_string()),
                    ("COYOTE_ESCALATION_FROM_AGENT_ID", request.from_agent_id),
                    ("COYOTE_ESCALATION_FROM_AGENT_NAME", from_agent.clone()),
                    ("COYOTE_ESCALATION_QUESTION", question.clone()),
                    ("COYOTE_ESCALATION_REPLY", reply.to_string()),
                ],
                None,
            );
            Ok(json!({
                "status": "ok",
                "message": format!("Reply sent to agent '{from_agent}' for escalation '{escalation_id}'"),
                "original_question": question,
            }))
        }
        None => Ok(json!({
            "status": "error",
            "message": format!("No pending escalation found with id '{escalation_id}'. It may have already been replied to."),
        })),
    }
}

fn handle_task_create(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let subject = args
        .get("subject")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'subject' is required"))?;
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let blocked_by: Vec<String> = args
        .get("blocked_by")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let dispatch_agent = args.get("agent").and_then(Value::as_str).map(String::from);
    let task_prompt = args.get("prompt").and_then(Value::as_str).map(String::from);

    if dispatch_agent.is_some() && task_prompt.is_none() {
        bail!("'prompt' is required when 'agent' is set");
    }

    let variables = match parse_variables_arg(args) {
        Ok(v) => v,
        Err(message) => {
            return Ok(json!({
                "status": "error",
                "message": message,
            }));
        }
    };

    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let mut sup = supervisor.write();

    let task_id = sup.task_queue_mut().create(
        subject.to_string(),
        description.to_string(),
        dispatch_agent.clone(),
        task_prompt,
        variables,
    );

    let mut dep_errors = vec![];
    for dep_id in &blocked_by {
        if let Err(e) = sup.task_queue_mut().add_dependency(&task_id, dep_id) {
            dep_errors.push(e);
        }
    }

    let mut result = json!({
        "status": "ok",
        "task_id": task_id,
    });

    if dispatch_agent.is_some() {
        result["auto_dispatch"] = json!(true);
    }

    if !dep_errors.is_empty() {
        result["warnings"] = json!(dep_errors);
    }

    Ok(result)
}

fn handle_task_list(ctx: &mut RequestContext) -> Result<Value> {
    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let sup = supervisor.read();

    let tasks: Vec<Value> = sup
        .task_queue()
        .list()
        .into_iter()
        .map(|t| {
            json!({
                "id": t.id,
                "subject": t.subject,
                "status": t.status,
                "owner": t.owner,
                "blocked_by": t.blocked_by.iter().collect::<Vec<_>>(),
                "blocks": t.blocks.iter().collect::<Vec<_>>(),
                "agent": t.dispatch_agent,
                "prompt": t.prompt,
                "variables": t.variables,
            })
        })
        .collect();

    Ok(json!({ "tasks": tasks }))
}

async fn handle_task_complete(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let task_id = args
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'task_id' is required"))?;

    let (newly_runnable, dispatchable) = {
        let supervisor = ctx
            .supervisor
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("No supervisor active"))?;
        let mut sup = supervisor.write();

        let newly_runnable_ids = sup.task_queue_mut().complete(task_id);

        let mut newly_runnable = Vec::new();
        let mut to_dispatch: Vec<(String, String, String, Option<AgentVariables>)> = Vec::new();

        for id in &newly_runnable_ids {
            if let Some(t) = sup.task_queue().get(id) {
                newly_runnable.push(json!({
                    "id": t.id,
                    "subject": t.subject,
                    "description": t.description,
                    "agent": t.dispatch_agent,
                }));

                if let (Some(agent), Some(prompt)) = (&t.dispatch_agent, &t.prompt) {
                    to_dispatch.push((
                        id.clone(),
                        agent.clone(),
                        prompt.clone(),
                        t.variables.clone(),
                    ));
                }
            }
        }

        let mut dispatchable = Vec::new();
        for (tid, agent, prompt, variables) in to_dispatch {
            if sup.task_queue_mut().claim(&tid, &format!("auto:{agent}")) {
                dispatchable.push((tid, agent, prompt, variables));
            }
        }

        (newly_runnable, dispatchable)
    };

    let mut spawned = Vec::new();
    for (tid, agent, prompt, variables) in &dispatchable {
        let spawn_args = dispatch_spawn_args(agent, prompt, variables.as_ref(), Some(tid));
        match handle_spawn(ctx, &spawn_args).await {
            Ok(result) => {
                let agent_id = result
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                debug!("Auto-dispatched agent '{}' for task queue", agent_id);
                spawned.push(result);
            }
            Err(e) => {
                spawned.push(json!({
                    "status": "error",
                    "agent": agent,
                    "message": format!("Auto-dispatch failed: {e}"),
                }));
            }
        }
    }

    let mut result = json!({
        "status": "ok",
        "task_id": task_id,
        "newly_runnable": newly_runnable,
    });

    if !spawned.is_empty() {
        result["auto_dispatched"] = json!(spawned);
    }

    Ok(result)
}

fn handle_task_fail(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let task_id = args
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'task_id' is required"))?;

    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let mut sup = supervisor.write();

    let task = sup.task_queue().get(task_id);
    if task.is_none() {
        return Ok(json!({
            "status": "error",
            "message": format!("Task '{task_id}' not found"),
        }));
    }

    let blocked_dependents: Vec<String> = task.unwrap().blocks.iter().cloned().collect();

    sup.task_queue_mut().fail(task_id);

    Ok(json!({
        "status": "ok",
        "task_id": task_id,
        "blocked_dependents": blocked_dependents,
        "message": format!("Task '{task_id}' marked as failed. {} dependent task(s) will remain blocked.", blocked_dependents.len()),
    }))
}

const SUMMARIZATION_PROMPT: &str = r#"You are a precise summarization assistant. Your job is to condense a sub-agent's output into a compact summary that preserves all actionable information.

Rules:
- Preserve ALL code snippets, file paths, error messages, and concrete recommendations
- Remove conversational filler, thinking-out-loud, and redundant explanations
- Keep the summary under 30% of the original length
- Use bullet points for multiple findings
- If the output contains a final answer or conclusion, lead with it"#;

async fn summarize_output(ctx: &RequestContext, agent_name: &str, output: &str) -> Result<String> {
    let Some(agent) = ctx.agent.as_ref() else {
        return Ok(output.to_string());
    };
    let threshold = agent.summarization_threshold();
    let summarization_model_id = agent.summarization_model().map(|s| s.to_string());

    if output.len() < threshold {
        debug!(
            "Output from '{}' is {} chars (threshold {}), skipping summarization",
            agent_name,
            output.len(),
            threshold
        );
        return Ok(output.to_string());
    }

    debug!(
        "Output from '{}' is {} chars (threshold {}), summarizing...",
        agent_name,
        output.len(),
        threshold
    );

    let model = match summarization_model_id {
        Some(ref model_id) => {
            Model::retrieve_model(ctx.app.config.as_ref(), model_id, ModelType::Chat)?
        }
        None => ctx.current_model().clone(),
    };

    let mut role = Role::new("summarizer", SUMMARIZATION_PROMPT);
    role.set_model(model);

    let user_message = format!(
        "Summarize the following sub-agent output from '{}':\n\n{}",
        agent_name, output
    );
    let input = Input::from_str(ctx, &user_message, Some(role))?;

    let summary = input.fetch_chat_text().await?;

    debug!(
        "Summarized output from '{}': {} chars -> {} chars",
        agent_name,
        output.len(),
        summary.len()
    );

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{MessageContent, ModelData};
    use crate::config::child_sessions::ChildStatus;
    use crate::config::test_fixtures::{FixtureServer, fixture_runtime};
    use crate::config::{AgentConfig, AppConfig, AppState, SessionScope, WorkingMode, paths};
    use crate::function::jobs::RingBuf;
    use crate::function::{ToolCall, ToolResult};
    use crate::mcp::{McpServer, McpServersConfig, McpTransportType};
    use crate::supervisor::escalation::{EscalationQueue, EscalationRequest};
    use crate::supervisor::{JobHandle, JobResult, JobState, JobStatus};
    use crate::utils::get_env_name;
    use parking_lot::Mutex;
    use serde_json::json;
    use serial_test::serial;
    use std::fs::{create_dir_all, read_to_string, remove_dir_all, write};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, mem};

    fn default_app_state() -> Arc<AppState> {
        Arc::new(AppState::test_default())
    }

    /// Pins the config dir and the cache dir to fresh temp dirs so a spawn
    /// in a test never reads or writes the developer's real ones; both are
    /// restored on drop.
    struct TestConfigDirGuard {
        keys: [(String, Option<std::ffi::OsString>); 2],
        path: PathBuf,
        cache_root: PathBuf,
    }

    impl TestConfigDirGuard {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("coyote-agents-tests-{unique}"));
            create_dir_all(&path).unwrap();
            let cache_root = env::temp_dir().join(format!("coyote-agents-tests-cache-{unique}"));
            create_dir_all(&cache_root).unwrap();
            let keys = [
                (get_env_name("config_dir"), path.as_os_str()),
                (get_env_name("cache_dir"), cache_root.as_os_str()),
            ]
            .map(|(key, value)| {
                let previous = env::var_os(&key);
                unsafe {
                    env::set_var(&key, value);
                }
                (key, previous)
            });
            Self {
                keys,
                path,
                cache_root,
            }
        }
    }

    impl Drop for TestConfigDirGuard {
        fn drop(&mut self) {
            for (key, previous) in &self.keys {
                if let Some(previous) = previous {
                    unsafe {
                        env::set_var(key, previous);
                    }
                } else {
                    unsafe {
                        env::remove_var(key);
                    }
                }
            }
            let _ = remove_dir_all(&self.path);
            let _ = remove_dir_all(&self.cache_root);
        }
    }

    #[test]
    #[serial]
    fn test_config_dir_guard_pins_the_cache_dir_and_restores_it_on_drop() {
        let key = get_env_name("cache_dir");
        let before = env::var_os(&key);
        let cache_root = {
            let guard = TestConfigDirGuard::new();
            let pinned = env::var_os(&key).expect("cache dir pinned while the guard lives");
            assert_eq!(PathBuf::from(&pinned), guard.cache_root);
            assert!(guard.cache_root.is_dir());
            assert_ne!(guard.cache_root, guard.path);
            guard.cache_root.clone()
        };
        assert_eq!(env::var_os(&key), before);
        assert!(!cache_root.exists());
    }

    fn ctx_with_supervisor(max_concurrent: usize, max_depth: usize) -> RequestContext {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(
            max_concurrent,
            max_depth,
        ))));
        ctx
    }

    /// A spawner whose app config turns child sessions off, so every
    /// spawn from it runs unseated.
    fn ctx_with_child_sessions_off() -> RequestContext {
        let app = AppState {
            config: Arc::new(AppConfig {
                child_sessions: Some(false),
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));
        ctx
    }

    #[test]
    fn effective_max_agent_depth_prefers_supervisor() {
        let ctx = ctx_with_supervisor(4, 7);
        assert_eq!(effective_max_agent_depth(&ctx), 7);
    }

    #[test]
    fn effective_max_agent_depth_falls_back_without_supervisor() {
        let ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        assert!(ctx.supervisor.is_none());
        assert!(ctx.agent.is_none());
        assert_eq!(effective_max_agent_depth(&ctx), default_max_agent_depth());
    }

    #[test]
    #[serial]
    fn effective_max_agent_depth_uses_agent_limit_without_supervisor() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = format!(
            "test_depth_agent_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\nmax_agent_depth: 5\n"),
        )
        .unwrap();

        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let app_config = Arc::clone(&ctx.app.config);
        let model = ctx.current_model().clone();
        let agent = run_async(Agent::init(
            app_config.as_ref(),
            ctx.app.as_ref(),
            &model,
            false,
            &agent_name,
            create_abort_signal(),
        ))
        .unwrap();
        ctx.agent = Some(agent);

        assert!(ctx.supervisor.is_none());
        assert_eq!(effective_max_agent_depth(&ctx), 5);
    }

    fn unique_agent_name(prefix: &str) -> String {
        format!(
            "{prefix}_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn write_config_only_agent(agent_name: &str) {
        write_config_agent_with_setting(agent_name, "");
    }

    /// A config-only agent with one extra top-level `config.yaml` line.
    fn write_config_agent_with_setting(agent_name: &str, setting: &str) {
        let agent_dir = paths::agent_data_dir(agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!("name: {agent_name}\ninstructions: hi\n{setting}\n"),
        )
        .unwrap();
    }

    /// A config-only agent whose instructions interpolate two defaulted
    /// variables, so a spawn can override one and inherit the other.
    fn write_config_agent_with_variables(agent_name: &str) {
        let agent_dir = paths::agent_data_dir(agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!(
                "name: {agent_name}\n\
                 instructions: Say {{{{tone}}}} to {{{{who}}}}\n\
                 variables:\n\
                 \x20 - name: tone\n\
                 \x20   description: t\n\
                 \x20   default: calm\n\
                 \x20 - name: who\n\
                 \x20   description: w\n\
                 \x20   default: them\n"
            ),
        )
        .unwrap();
    }

    fn write_graph_agent(agent_name: &str) {
        let agent_dir = paths::agent_data_dir(agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\nstart: done\nnodes:\n  done:\n    type: end\n    output: done\n"
            ),
        )
        .unwrap();
    }

    /// A graph agent whose graph.yaml wires `agent.*` hooks at its top
    /// level. With `failing`, the start node references a script that does
    /// not exist, so the child run fails at runtime — after the spawn seam
    /// has already fired `agent.started`.
    fn write_graph_agent_with_hooks(agent_name: &str, marker: &str, failing: bool) {
        let agent_dir = paths::agent_data_dir(agent_name);
        create_dir_all(&agent_dir).unwrap();
        let (start, nodes) = if failing {
            (
                "work",
                "  work:\n    type: script\n    script: missing.sh\n    next: done\n  done:\n    type: end\n    output: done\n",
            )
        } else {
            ("done", "  done:\n    type: end\n    output: done\n")
        };
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\n\
                 start: {start}\n\
                 settings:\n\
                 \x20 validate_before_run: false\n\
                 hooks:\n\
                 \x20 agent.started:\n\
                 \x20   - name: {marker}_started\n\
                 \x20     command: \"true\"\n\
                 \x20 agent.completed:\n\
                 \x20   - name: {marker}_completed\n\
                 \x20     command: \"true\"\n\
                 \x20 agent.interrupted:\n\
                 \x20   - name: {marker}_interrupted\n\
                 \x20     command: \"true\"\n\
                 \x20 agent.failed:\n\
                 \x20   - name: {marker}_failed\n\
                 \x20     command: \"true\"\n\
                 nodes:\n\
                 {nodes}"
            ),
        )
        .unwrap();
    }

    fn hook_captures_named(name: &str) -> Vec<hooks::test_sink::Capture> {
        hooks::test_sink::snapshot()
            .into_iter()
            .filter(|capture| capture.hook_name == name)
            .collect()
    }

    /// Spawns an agent on `ctx` through the supervisor seam and awaits its
    /// spawn task, returning the spawn payload. The result arms fire their
    /// hooks before that task returns, so the sink is complete afterwards.
    async fn spawn_and_reclaim(ctx: &mut RequestContext, agent_name: &str) -> Value {
        let spawned = handle_spawn(ctx, &json!({"agent": agent_name, "prompt": "hi"}))
            .await
            .unwrap();
        assert_eq!(spawned["status"], "ok", "{spawned}");
        let id = spawned["id"].as_str().unwrap().to_string();
        let handle = ctx
            .supervisor
            .as_ref()
            .unwrap()
            .write()
            .take(&id)
            .expect("spawned agent must be registered");
        handle
            .join_handle
            .await
            .expect("spawn task must not panic")
            .expect("spawn task must return an AgentResult");
        spawned
    }

    /// Spawns `agent_name` on a fresh spawner, awaits its spawn task and
    /// returns the spawn payload.
    fn spawn_and_collect(agent_name: &str) -> Value {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            spawn_and_reclaim(&mut ctx, agent_name).await
        })
    }

    fn child_meta_of(ctx: &RequestContext, session_id: &str) -> ChildMeta {
        let root = ctx
            .child_session_root
            .as_ref()
            .expect("the spawner keeps the root its children live under");
        read_yaml(&root.child_meta_file(session_id))
    }

    /// Both session keys name the child's own session: `COYOTE_SESSION_ID`
    /// because the child is seated on a session named after the id, and
    /// `COYOTE_AGENT_SESSION_ID` because the spawn seam adds it.
    fn assert_session_envs(capture: &hooks::test_sink::Capture, session_id: &str) {
        assert_eq!(
            capture
                .envs
                .get("COYOTE_AGENT_SESSION_ID")
                .map(String::as_str),
            Some(session_id),
            "{capture:?}"
        );
        assert_eq!(
            capture.envs.get("COYOTE_SESSION_ID").map(String::as_str),
            Some(session_id),
            "{capture:?}"
        );
    }

    #[test]
    #[serial]
    fn spawned_agent_success_fires_started_and_completed_with_matching_id() {
        let _guard = TestConfigDirGuard::new();
        let _sink = hooks::test_sink::install();
        let agent_name = unique_agent_name("test_hook_spawn_ok");
        write_graph_agent_with_hooks(&agent_name, "t037_spawn_ok", false);

        let spawned = spawn_and_collect(&agent_name);
        let id = spawned["id"].as_str().unwrap();
        let session_id = spawned["session_id"]
            .as_str()
            .expect("a seated child reports its session id");

        let started = hook_captures_named("t037_spawn_ok_started");
        assert_eq!(started.len(), 1, "{started:?}");
        assert_eq!(started[0].envs["COYOTE_EVENT"], "agent.started");
        assert_eq!(started[0].envs["COYOTE_AGENT_ID"], id);
        assert_eq!(started[0].envs["COYOTE_AGENT_NAME"], agent_name);
        assert_session_envs(&started[0], session_id);

        let completed = hook_captures_named("t037_spawn_ok_completed");
        assert_eq!(completed.len(), 1, "{completed:?}");
        assert_eq!(completed[0].envs["COYOTE_EVENT"], "agent.completed");
        assert_eq!(completed[0].envs["COYOTE_AGENT_ID"], id);
        assert_eq!(completed[0].envs["COYOTE_AGENT_NAME"], agent_name);
        assert_session_envs(&completed[0], session_id);

        assert!(hook_captures_named("t037_spawn_ok_failed").is_empty());
    }

    #[test]
    #[serial]
    fn spawned_agent_failure_fires_started_and_failed_with_matching_id() {
        let _guard = TestConfigDirGuard::new();
        let _sink = hooks::test_sink::install();
        let agent_name = unique_agent_name("test_hook_spawn_fail");
        write_graph_agent_with_hooks(&agent_name, "t037_spawn_fail", true);

        let spawned = spawn_and_collect(&agent_name);
        let id = spawned["id"].as_str().unwrap();
        let session_id = spawned["session_id"]
            .as_str()
            .expect("a seated child reports its session id");

        let started = hook_captures_named("t037_spawn_fail_started");
        assert_eq!(started.len(), 1, "{started:?}");
        assert_eq!(started[0].envs["COYOTE_AGENT_ID"], id);
        assert_session_envs(&started[0], session_id);

        let failed = hook_captures_named("t037_spawn_fail_failed");
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].envs["COYOTE_EVENT"], "agent.failed");
        assert_eq!(failed[0].envs["COYOTE_AGENT_ID"], id);
        assert_session_envs(&failed[0], session_id);
        assert!(
            !failed[0].envs["COYOTE_AGENT_ERROR"].is_empty(),
            "a failed spawn must carry its error"
        );

        assert!(hook_captures_named("t037_spawn_fail_completed").is_empty());
    }

    /// Spawns `agent_name`, sends it the `agent__cancel` signal before its
    /// spawn task has run, then awaits that task. On this current-thread
    /// runtime the task only progresses once awaited, so its result arm
    /// observes the ctrl-c deterministically.
    async fn spawn_ctrlc_and_reclaim(ctx: &mut RequestContext, agent_name: &str) -> Value {
        let spawned = handle_spawn(ctx, &json!({"agent": agent_name, "prompt": "hi"}))
            .await
            .unwrap();
        assert_eq!(spawned["status"], "ok", "{spawned}");
        let id = spawned["id"].as_str().unwrap().to_string();
        let handle = ctx
            .supervisor
            .as_ref()
            .unwrap()
            .write()
            .take(&id)
            .expect("spawned agent must be registered");
        handle.abort_signal.set_ctrlc();
        handle
            .join_handle
            .await
            .expect("spawn task must not panic")
            .expect("spawn task must return an AgentResult");
        spawned
    }

    #[test]
    #[serial]
    fn cancelled_spawned_agent_fires_interrupted_not_failed() {
        let _guard = TestConfigDirGuard::new();
        let _sink = hooks::test_sink::install();
        let agent_name = unique_agent_name("test_hook_spawn_cancel");
        write_graph_agent_with_hooks(&agent_name, "t049_spawn_cancel", true);
        let mut ctx = ctx_with_supervisor(4, 3);

        let spawned = run_async(spawn_ctrlc_and_reclaim(&mut ctx, &agent_name));
        let id = spawned["id"].as_str().unwrap();
        let session_id = spawned["session_id"]
            .as_str()
            .expect("a seated child reports its session id");

        let interrupted = hook_captures_named("t049_spawn_cancel_interrupted");
        assert_eq!(interrupted.len(), 1, "{interrupted:?}");
        assert_eq!(interrupted[0].envs["COYOTE_EVENT"], "agent.interrupted");
        assert_eq!(interrupted[0].envs["COYOTE_AGENT_ID"], id);
        assert_eq!(interrupted[0].envs["COYOTE_AGENT_NAME"], agent_name);
        assert_session_envs(&interrupted[0], session_id);
        assert!(
            !interrupted[0].envs.contains_key("COYOTE_AGENT_ERROR"),
            "an interruption is not a failure and carries no error env"
        );

        assert!(hook_captures_named("t049_spawn_cancel_failed").is_empty());
        assert!(hook_captures_named("t049_spawn_cancel_completed").is_empty());
    }

    /// With the app knob off no child is seated, so neither session key
    /// reaches the hooks: not the spawn seam's `COYOTE_AGENT_SESSION_ID`,
    /// and not the base `COYOTE_SESSION_ID`, since the child has no session.
    #[test]
    #[serial]
    fn unseated_spawn_hooks_carry_no_session_keys() {
        let _guard = TestConfigDirGuard::new();
        let _sink = hooks::test_sink::install();
        let agent_name = unique_agent_name("test_hook_spawn_unseated");
        write_graph_agent_with_hooks(&agent_name, "sidecar_spawn_unseated", false);
        let mut ctx = ctx_with_child_sessions_off();

        let spawned = run_async(spawn_and_reclaim(&mut ctx, &agent_name));
        assert!(spawned["session_id"].is_null(), "{spawned}");

        for name in [
            "sidecar_spawn_unseated_started",
            "sidecar_spawn_unseated_completed",
        ] {
            let captures = hook_captures_named(name);
            assert_eq!(captures.len(), 1, "{name}: {captures:?}");
            assert_eq!(captures[0].envs["COYOTE_AGENT_ID"], spawned["id"]);
            for key in ["COYOTE_AGENT_SESSION_ID", "COYOTE_SESSION_ID"] {
                assert!(
                    !captures[0].envs.contains_key(key),
                    "{name} carries {key}: {:?}",
                    captures[0].envs
                );
            }
        }
    }

    #[test]
    fn agent_event_base_envs_carry_optional_names_only_when_present() {
        let envs = hooks::base_envs_parts(HookEvent::AgentCompleted, None, None);
        let keys: Vec<&str> = envs.iter().map(|(key, _)| key.as_str()).collect();
        assert!(keys.contains(&"COYOTE_EVENT"));
        assert!(keys.contains(&"COYOTE_CONFIG_DIR"));
        assert!(keys.contains(&"COYOTE_EVENT_TIMESTAMP"));
        assert!(!keys.contains(&"COYOTE_SESSION_ID"));
        assert!(!keys.contains(&"COYOTE_AGENT_NAME"));
        assert!(
            envs.iter()
                .any(|(key, value)| key == "COYOTE_EVENT" && value == "agent.completed")
        );

        let envs = hooks::base_envs_parts(HookEvent::AgentFailed, Some("sess"), Some("bot"));
        assert!(
            envs.iter()
                .any(|(key, value)| key == "COYOTE_SESSION_ID" && value == "sess")
        );
        assert!(
            envs.iter()
                .any(|(key, value)| key == "COYOTE_AGENT_NAME" && value == "bot")
        );
    }

    fn run_agent_for_graph_error(
        agent_name: &str,
        graph_inputs: Option<HashMap<String, Value>>,
    ) -> String {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let err = run_async(run_agent_for_graph(
            &mut ctx,
            agent_name,
            "hi",
            graph_inputs,
        ))
        .expect_err("run_agent_for_graph should reject the inputs");
        format!("{err:#}")
    }

    #[test]
    #[serial]
    fn run_agent_for_graph_rejects_inputs_for_config_only_agent() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_inputs_plain_agent");
        write_config_only_agent(&agent_name);

        let chain = run_agent_for_graph_error(&agent_name, Some(HashMap::new()));

        assert_eq!(
            chain,
            format!(
                "agent '{agent_name}' has no graph.yaml; `inputs:` is only valid on agent nodes that target a graph agent"
            )
        );
    }

    #[test]
    #[serial]
    fn run_agent_for_graph_rejects_initial_prompt_input_on_graph_agent() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_inputs_graph_agent");
        write_graph_agent(&agent_name);
        let inputs = HashMap::from([("initial_prompt".to_string(), json!("x"))]);

        let chain = run_agent_for_graph_error(&agent_name, Some(inputs));

        assert_eq!(
            chain,
            format!(
                "agent '{agent_name}': `inputs.initial_prompt` is reserved (the dispatcher seeds it from `prompt:`)"
            )
        );
    }

    #[test]
    #[serial]
    fn run_agent_for_graph_rejects_previous_turns_input_on_graph_agent() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_inputs_graph_agent_turns");
        write_graph_agent(&agent_name);
        let inputs = HashMap::from([("previous_turns".to_string(), json!("x"))]);

        let chain = run_agent_for_graph_error(&agent_name, Some(inputs));

        assert_eq!(
            chain,
            format!(
                "agent '{agent_name}': `inputs.previous_turns` is reserved (the dispatcher seeds it)"
            )
        );
    }

    /// The child ctx must carry the parent's session flag: a session already
    /// aborted before the child starts trips the child graph's own pre-check,
    /// so it never reaches its (otherwise trivially successful) end node.
    /// Without the inheritance the child sees `None` and returns "done".
    #[test]
    #[serial]
    fn run_agent_for_graph_child_inherits_session_abort() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_session_abort_graph_agent");
        write_graph_agent(&agent_name);
        let session = create_abort_signal();
        session.set_ctrlc();
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.session_abort = Some(Arc::clone(&session));

        let err = run_async(run_agent_for_graph(&mut ctx, &agent_name, "hi", None))
            .expect_err("the inherited session abort must end the child graph");

        let chain = format!("{err:#}");
        assert!(
            chain.contains(&format!("Graph execution failed for agent '{agent_name}'")),
            "{chain}"
        );
        assert!(chain.contains("aborted before super-step"), "{chain}");
        assert!(
            Arc::ptr_eq(ctx.session_abort.as_ref().unwrap(), &session),
            "the parent's own session signal is untouched"
        );
    }

    /// The other half of the inheritance rule: `handle_spawn` builds its
    /// child with `new_for_child` and never copies the session signal, so a
    /// latched root interrupt is invisible to the background child.
    #[test]
    fn spawned_child_ctx_carries_no_session_abort() {
        let session = create_abort_signal();
        session.set_ctrlc();
        let mut parent = ctx_with_supervisor(4, 3);
        parent.session_abort = Some(session);
        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            1,
            Arc::new(Inbox::new()),
            "c1".into(),
        );
        assert!(child.session_abort.is_none());
    }

    /// The child ctx `build_child_spawn` builds is moved into the spawn
    /// task, so the seeding is pinned by source scrape: both child builders
    /// hand the ctx the same `child_abort` the supervisor's cancel path
    /// sets, which is what `work_abort` returns to a child's tool-scope
    /// refresh.
    #[test]
    fn both_child_builders_seed_the_task_abort_with_the_child_signal() {
        let source = include_str!("agents.rs").replace("\r\n", "\n");
        let seed = "child_ctx.task_abort = Some(child_abort.clone());";
        let created = "let child_abort = create_abort_signal();";
        let builder = "RequestContext::new_for_child(";
        let fn_body = |head: &str| {
            let start = source.find(head).unwrap();
            let body = &source[start..];
            let end = body.find("\n}\n").unwrap();
            &source[start..start + end]
        };
        for head in [
            "\npub async fn run_agent_for_graph(",
            "\nasync fn build_child_spawn(",
        ] {
            let body = fn_body(head);
            assert_eq!(body.matches(seed).count(), 1, "{head}");
            assert_eq!(body.matches(created).count(), 1, "{head}");
            let created_at = body.find(created).unwrap();
            let built_at = body.find(builder).unwrap();
            let seeded_at = body.find(seed).unwrap();
            assert!(created_at < built_at && built_at < seeded_at, "{head}");
        }
        // The seeded signal crosses the build/run split inside `ChildSpawn`
        // and is the one the supervisor handle registers.
        let registered = "abort_signal: child_abort,";
        let build_body = fn_body("\nasync fn build_child_spawn(");
        assert_eq!(build_body.matches(seed).count(), 1);
        assert!(!build_body.contains(registered));
        assert!(fn_body("\nfn launch_child_spawn(").contains(registered));
        let production = &source[..source.find("\n#[cfg(test)]\nmod tests").unwrap()];
        assert_eq!(production.matches(seed).count(), 2);
        assert_eq!(production.matches(builder).count(), 2);
    }

    /// Drives the child builder on `ctx` and unwraps the ready spawn.
    fn build_ready_spawn(ctx: &mut RequestContext, args: Value) -> Box<ChildSpawn> {
        match run_async(build_child_spawn(ctx, &args)).unwrap() {
            SpawnBuild::Ready(spawn) => spawn,
            SpawnBuild::Refused(payload) => panic!("spawn refused: {payload}"),
        }
    }

    fn spawner_with_session(name: &str) -> RequestContext {
        let mut ctx = ctx_with_supervisor(4, 3);
        let app = Arc::clone(&ctx.app.config);
        ctx.session = Some(Session::new_from_ctx(&ctx, app.as_ref(), name).unwrap());
        ctx
    }

    fn read_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> T {
        serde_yaml::from_str(&read_to_string(path).unwrap())
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
    }

    fn files_named(dir: &Path, name: &str) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(files_named(&path, name));
            } else if path.file_name().is_some_and(|file| file == name) {
                found.push(path);
            }
        }
        found
    }

    #[test]
    #[serial]
    fn build_child_spawn_seats_the_child_on_an_owner_only_session_under_the_spawner_root() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_child");
        write_config_agent_with_variables(&agent_name);
        let prompt = "describe the seating ".repeat(20);
        assert!(prompt.chars().count() > child_sessions::PROMPT_HEAD_CHARS);
        let mut ctx = ctx_with_supervisor(4, 3);

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": prompt, "variables": {"tone": "loud"}}),
        );

        let sid = spawn
            .session_id
            .clone()
            .expect("a seated child has a session id");
        let session = spawn
            .child_ctx
            .session
            .as_ref()
            .expect("the child context carries the seated session");
        assert_eq!(session.name(), sid);
        assert_eq!(session.save_session(), Some(true));
        let root = spawn
            .child_ctx
            .child_session_root
            .clone()
            .expect("the child carries the spawner's root");
        assert!(
            root.dir()
                .starts_with(_guard.cache_root.join("sessions").join("_proc")),
            "{}",
            root.dir().display()
        );
        let path = root.child_session_file(&sid);
        assert!(path.is_file(), "{}", path.display());
        let on_disk: Session = read_yaml(&path);
        let instructions = spawn
            .child_ctx
            .agent
            .as_ref()
            .unwrap()
            .interpolated_instructions();
        assert_eq!(on_disk.agent_instructions(), instructions);
        assert!(instructions.contains("Say loud to them"), "{instructions}");
        assert!(!instructions.contains("{{"), "{instructions}");
        let variables = on_disk.agent_variables();
        assert_eq!(variables.get("tone").map(String::as_str), Some("loud"));
        assert_eq!(variables.get("who").map(String::as_str), Some("them"));
        assert!(spawn.input.with_session());
        let root_file: RootFile = read_yaml(&root.root_file());
        assert!(root_file.parent_paths.is_empty());
        let meta: ChildMeta = read_yaml(&root.child_meta_file(&sid));
        assert_eq!(meta.status, ChildStatus::Running);
        let head: String = prompt
            .chars()
            .take(child_sessions::PROMPT_HEAD_CHARS)
            .collect();
        assert_eq!(meta.prompt_head, head);
        assert_eq!(meta.spawner_session_id, None);
        assert_eq!(meta.spawner_agent_id, None);
        assert_eq!(meta.agent_id, spawn.agent_id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
    }

    #[test]
    #[serial]
    fn build_child_spawn_records_the_root_spawner_saved_session_as_the_root_parent() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_parent_path");
        write_config_only_agent(&agent_name);
        let mut ctx = spawner_with_session("root-saved");
        let saved = ctx.session_file_for("root-saved", SessionScope::Global);
        create_dir_all(saved.parent().unwrap()).unwrap();
        write(&saved, "model: test-seeded:test-chat\nmessages: []\n").unwrap();
        ctx.session.as_mut().unwrap().set_path(saved.clone());

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let uid = ctx
            .session
            .as_ref()
            .unwrap()
            .uid()
            .expect("the build mints the root session's uid");
        let root = spawn.child_ctx.child_session_root.clone().unwrap();
        assert_eq!(*root, ChildSessionRoot::Uid(uid));
        assert_eq!(
            root.dir(),
            _guard
                .cache_root
                .join("sessions")
                .join(uid.simple().to_string())
        );
        let root_file: RootFile = read_yaml(&root.root_file());
        assert_eq!(
            root_file.parent_paths,
            vec![std::fs::canonicalize(&saved).unwrap()]
        );
    }

    /// The root session's own file is what the root records, not a path
    /// re-derived from the session's name: a spawner mid-way through a
    /// delegated chat turn would derive the delegate's directory instead.
    #[test]
    #[serial]
    fn build_child_spawn_records_the_root_session_file_rather_than_a_rederived_path() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_parent_file");
        write_config_only_agent(&agent_name);
        let mut ctx = spawner_with_session("root-saved");
        let derived = ctx.session_file_for("root-saved", SessionScope::Global);
        let saved = _guard
            .path
            .join("elsewhere")
            .join("under-another-name.yaml");
        assert_ne!(saved, derived);
        create_dir_all(saved.parent().unwrap()).unwrap();
        write(&saved, "model: test-seeded:test-chat\nmessages: []\n").unwrap();
        ctx.session.as_mut().unwrap().set_path(saved.clone());

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let root = spawn.child_ctx.child_session_root.clone().unwrap();
        let root_file: RootFile = read_yaml(&root.root_file());
        assert_eq!(
            root_file.parent_paths,
            vec![std::fs::canonicalize(&saved).unwrap()]
        );
        assert!(!derived.exists());
    }

    #[test]
    #[serial]
    fn build_child_spawn_keys_an_unsaved_root_session_on_its_uid_without_a_parent_path() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_unsaved_parent");
        write_config_only_agent(&agent_name);
        let mut ctx = spawner_with_session("root-unsaved");

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let uid = ctx.session.as_ref().unwrap().uid().unwrap();
        let root = spawn.child_ctx.child_session_root.clone().unwrap();
        assert_eq!(*root, ChildSessionRoot::Uid(uid));
        let root_file: RootFile = read_yaml(&root.root_file());
        assert!(root_file.parent_paths.is_empty(), "{root_file:?}");
    }

    #[test]
    #[serial]
    fn build_child_spawn_from_a_child_shares_the_root_and_names_the_child_as_spawner() {
        let _guard = TestConfigDirGuard::new();
        let child_agent = unique_agent_name("test_seat_child_spawner");
        let grandchild_agent = unique_agent_name("test_seat_grandchild");
        write_config_only_agent(&child_agent);
        write_config_only_agent(&grandchild_agent);
        let mut ctx = ctx_with_supervisor(4, 3);
        let child = build_ready_spawn(&mut ctx, json!({"agent": child_agent, "prompt": "hi"}));
        let child_sid = child.session_id.clone().unwrap();
        let child_root = child.child_ctx.child_session_root.clone().unwrap();
        let ChildSpawn {
            mut child_ctx,
            agent_id: child_agent_id,
            ..
        } = *child;
        child_ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));

        let grandchild = build_ready_spawn(
            &mut child_ctx,
            json!({"agent": grandchild_agent, "prompt": "hi"}),
        );

        let grandchild_root = grandchild.child_ctx.child_session_root.clone().unwrap();
        assert_eq!(grandchild_root.dir(), child_root.dir());
        let sid = grandchild.session_id.clone().unwrap();
        let meta: ChildMeta = read_yaml(&grandchild_root.child_meta_file(&sid));
        assert_eq!(meta.spawner_session_id, Some(child_sid));
        assert_eq!(meta.spawner_agent_id, Some(child_agent_id));
    }

    #[test]
    #[serial]
    fn handle_spawn_with_the_agent_knob_off_mints_the_root_but_seats_no_session() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_agent_knob_off");
        write_config_agent_with_setting(&agent_name, "child_sessions: false");
        let mut ctx = spawner_with_session("knob-spawner");

        let payload = run_async(spawn_and_reclaim(&mut ctx, &agent_name));

        assert_eq!(payload.get("session_id"), Some(&Value::Null), "{payload}");
        assert!(
            ctx.child_session_root.is_some(),
            "the agent knob decides seating, not whether the spawner has a root"
        );
        assert!(ctx.session.as_ref().unwrap().uid().is_some());
        assert!(!_guard.cache_root.join("sessions").exists());
    }

    #[test]
    #[serial]
    fn build_child_spawn_with_the_app_knob_off_mints_nothing() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_app_knob_off");
        write_config_only_agent(&agent_name);
        let app = AppState {
            config: Arc::new(AppConfig {
                child_sessions: Some(false),
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));
        let app_config = Arc::clone(&ctx.app.config);
        ctx.session =
            Some(Session::new_from_ctx(&ctx, app_config.as_ref(), "knob-spawner").unwrap());

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        assert!(spawn.session_id.is_none());
        assert!(spawn.child_ctx.session.is_none());
        assert!(ctx.child_session_root.is_none());
        assert!(ctx.session.as_ref().unwrap().uid().is_none());
        assert!(!_guard.cache_root.join("sessions").exists());
    }

    #[test]
    #[serial]
    fn build_child_spawn_ignores_the_agent_session_setting_of_a_spawned_agent() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_agent_session");
        write_config_agent_with_setting(&agent_name, "agent_session: shared");
        let mut ctx = ctx_with_supervisor(4, 3);

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let session = spawn.child_ctx.session.as_ref().unwrap();
        assert_eq!(Some(session.name()), spawn.session_id.as_deref());
        assert!(session.name().starts_with("sess_"), "{}", session.name());
        assert!(files_named(&_guard.path, "shared.yaml").is_empty());
        assert!(files_named(&_guard.cache_root, "shared.yaml").is_empty());
    }

    #[test]
    #[serial]
    fn a_seated_child_turn_lands_in_its_session_file_and_a_tool_round_does_not() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_turn");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));
        let ChildSpawn {
            mut child_ctx,
            input,
            session_id,
            ..
        } = *spawn;
        let path = child_ctx
            .child_session_root
            .as_ref()
            .unwrap()
            .child_session_file(&session_id.unwrap());
        let app = Arc::clone(&child_ctx.app.config);

        child_ctx
            .after_chat_completion(app.as_ref(), &input, "child reply", &[])
            .unwrap();

        let on_disk: Session = read_yaml(&path);
        let turn: Vec<_> = on_disk
            .messages()
            .iter()
            .filter(|message| !message.role.is_system())
            .collect();
        assert_eq!(turn.len(), 2, "{turn:?}");
        assert!(turn[0].role.is_user());
        assert!(matches!(&turn[0].content, MessageContent::Text(text) if text == "hi"));
        assert!(turn[1].role.is_assistant());
        assert!(matches!(&turn[1].content, MessageContent::Text(text) if text == "child reply"));

        let before = std::fs::read(&path).unwrap();
        let tool_results = vec![ToolResult::new(
            ToolCall::new("t".into(), json!({}), Some("id-0".into())),
            json!("tiny"),
        )];
        child_ctx
            .after_chat_completion(app.as_ref(), &input, "mid-loop", &tool_results)
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    #[serial]
    fn handle_spawn_reports_the_child_session_id_and_a_completed_child_ends_its_sidecar_completed()
    {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_graph_child");
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);

        let payload = run_async(spawn_and_reclaim(&mut ctx, &agent_name));

        let sid = payload["session_id"]
            .as_str()
            .expect("a seated child reports its session id");
        assert!(sid.starts_with("sess_"), "{sid}");
        let root = ctx
            .child_session_root
            .clone()
            .expect("the spawner keeps the root its children live under");
        assert!(root.child_session_file(sid).is_file());
        let meta: ChildMeta = read_yaml(&root.child_meta_file(sid));
        assert_eq!(meta.status, ChildStatus::Completed);
        assert!(meta.error.is_none(), "{meta:?}");
        let on_disk: Session = read_yaml(&root.child_session_file(sid));
        assert_eq!(
            last_exchange_of(&on_disk),
            ("hi".to_string(), "done".to_string())
        );
    }

    /// The last user/assistant pair of a stored session as text.
    fn last_exchange_of(session: &Session) -> (String, String) {
        let messages = session.messages();
        let [.., user, assistant] = messages else {
            panic!("expected an exchange, got {messages:?}");
        };
        assert!(user.role.is_user());
        assert!(assistant.role.is_assistant());
        (user.content.to_text(), assistant.content.to_text())
    }

    /// A seated graph child records its run into its own session file: the
    /// exchange as messages and the run as `last_run`, trace included.
    #[test]
    #[serial]
    fn a_seated_graph_child_records_its_run_into_its_session_file() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_graph_child_records");
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);

        let spawned = run_async(spawn_and_reclaim(&mut ctx, &agent_name));

        let sid = spawned["session_id"].as_str().unwrap().to_string();
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Completed);
        let on_disk: Session = read_yaml(&spawner_root(&ctx).child_session_file(&sid));
        assert_eq!(
            last_exchange_of(&on_disk),
            ("hi".to_string(), "done".to_string())
        );
        let last_run = on_disk.last_run().expect("the child run is recorded");
        assert_eq!(last_run.prompt, "hi");
        assert_eq!(last_run.output, "done");
        assert!(last_run.exposed.is_empty());
        assert!(!last_run.trace.is_empty());
        assert_eq!(last_run.recorded_at_message_count, on_disk.messages().len());
    }

    /// Recording a graph run is the caller's job: the dispatcher hands the
    /// record back and never touches the session itself, and exactly two
    /// callers store it: the child runner, on the child's own session, and
    /// the one-shot graph arm in main.rs, on the engaged session.
    #[test]
    fn only_the_child_runner_and_the_one_shot_arm_record_a_graph_run() {
        let dispatch = include_str!("../graph/dispatch.rs").replace("\r\n", "\n");
        let dispatch = &dispatch[..dispatch.find("\n#[cfg(test)]").unwrap()];
        assert_eq!(dispatch.matches("record_graph_run").count(), 0);
        assert_eq!(dispatch.matches("record_chat_completion").count(), 0);
        let child_entry_at = dispatch
            .find("\npub async fn run_active_agent_graph_with_inputs(")
            .unwrap();
        let child_entry = &dispatch[child_entry_at..];
        let child_entry = &child_entry[..child_entry.find("\n}\n").unwrap()];
        for needle in ["route::", "routing_line", "classify"] {
            assert!(!child_entry.contains(needle), "{needle}");
        }

        let agents = include_str!("agents.rs").replace("\r\n", "\n");
        let agents = &agents[..agents.find("\n#[cfg(test)]\nmod tests").unwrap()];
        assert_eq!(agents.matches("record_graph_run(").count(), 1);

        let main = include_str!("../main.rs").replace("\r\n", "\n");
        assert_eq!(main.matches("record_graph_run(").count(), 1);
    }

    #[test]
    #[serial]
    fn a_failing_child_ends_its_sidecar_failed_with_its_error() {
        let _guard = TestConfigDirGuard::new();
        let _sink = hooks::test_sink::install();
        let agent_name = unique_agent_name("test_sidecar_failed");
        write_graph_agent_with_hooks(&agent_name, "sidecar_run_failed", true);
        let mut ctx = ctx_with_supervisor(4, 3);

        let payload = run_async(spawn_and_reclaim(&mut ctx, &agent_name));

        let meta = child_meta_of(&ctx, payload["session_id"].as_str().unwrap());
        assert_eq!(meta.status, ChildStatus::Failed);
        assert!(
            meta.error.as_deref().is_some_and(|error| !error.is_empty()),
            "{meta:?}"
        );
        let failed = hook_captures_named("sidecar_run_failed_failed");
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(
            !failed[0].envs["COYOTE_AGENT_ERROR"].is_empty(),
            "{failed:?}"
        );
    }

    fn spawn_declaration() -> FunctionDeclaration {
        agent_function_declarations()
            .into_iter()
            .find(|declaration| declaration.name == format!("{AGENT_FUNCTION_PREFIX}spawn"))
            .expect("agent__spawn is declared")
    }

    #[test]
    fn spawn_declaration_describes_session_id_resume_and_the_variables_rule() {
        let declaration = spawn_declaration();
        let properties = declaration.parameters.properties.as_ref().unwrap();
        let session_id = &properties["session_id"];
        assert_eq!(session_id.type_value.as_deref(), Some("string"));
        let description = session_id.description.as_deref().unwrap();
        assert!(
            description.contains("`null` on a spawn result means that child cannot be resumed"),
            "{description}"
        );
        assert!(
            description.contains("Omit it to spawn fresh."),
            "{description}"
        );
        let variables = properties["variables"].description.as_deref().unwrap();
        assert!(
            variables.contains("must equal the stored ones"),
            "{variables}"
        );
        assert!(!variables.contains("take precedence"), "{variables}");
        assert_eq!(
            declaration.parameters.required.as_deref(),
            Some(&["agent".to_string(), "prompt".to_string()][..])
        );
    }

    #[test]
    fn dispatch_spawn_args_never_passes_a_session_id() {
        let mut variables = AgentVariables::new();
        variables.insert("pr".into(), "1".into());
        let args = dispatch_spawn_args("coder", "do it", Some(&variables), Some("7"));
        assert!(args.get("session_id").is_none(), "{args}");
        let source = include_str!("agents.rs").replace("\r\n", "\n");
        let start = source.find("\nfn dispatch_spawn_args(").unwrap();
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(!body.contains("session_id"));
    }

    /// Resumes `sid` for `agent_name` on `ctx` and awaits the child, the
    /// way `spawn_and_reclaim` does for a fresh spawn.
    async fn resume_and_reclaim(ctx: &mut RequestContext, agent_name: &str, sid: &str) -> Value {
        let resumed = handle_spawn(
            ctx,
            &json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        )
        .await
        .unwrap();
        assert_eq!(resumed["status"], "ok", "{resumed}");
        let id = resumed["id"].as_str().unwrap().to_string();
        let handle = ctx
            .supervisor
            .as_ref()
            .unwrap()
            .write()
            .take(&id)
            .expect("resumed agent must be registered");
        handle
            .join_handle
            .await
            .expect("spawn task must not panic")
            .expect("spawn task must return an AgentResult");
        resumed
    }

    #[test]
    #[serial]
    fn resume_of_a_completed_graph_child_keeps_the_session_id_under_a_fresh_agent_id() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_graph");
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let first = run_async(spawn_and_reclaim(&mut ctx, &agent_name));
        let sid = first["session_id"].as_str().unwrap().to_string();
        let first_meta = child_meta_of(&ctx, &sid);
        assert_eq!(first_meta.status, ChildStatus::Completed);

        let resumed = run_async(resume_and_reclaim(&mut ctx, &agent_name, &sid));

        assert_eq!(resumed["session_id"], sid.as_str());
        assert_ne!(resumed["id"], first["id"]);
        let meta = child_meta_of(&ctx, &sid);
        assert_eq!(meta.status, ChildStatus::Completed);
        assert_eq!(Some(meta.agent_id.as_str()), resumed["id"].as_str());
        assert_eq!(meta.created_at, first_meta.created_at);
        assert_eq!(meta.prompt_head, "hi");
        assert!(meta.error.is_none(), "{meta:?}");
    }

    /// A graph agent that carries `said`: its script node folds the prompt
    /// onto the carried value, and the end node renders that value next to
    /// the transcript the dispatcher seeded, so a resumed run shows both
    /// what the earlier run carried and what it said.
    #[cfg(unix)]
    fn write_graph_agent_that_remembers(agent_name: &str) {
        let agent_dir = paths::agent_data_dir(agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(agent_dir.join("remember.sh"), "#!/bin/bash\necho '{}'\n").unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\n\
                 carry: [said]\n\
                 initial_state:\n\
                 \x20 said: \"\"\n\
                 \x20 initial_prompt: \"\"\n\
                 start: remember\n\
                 nodes:\n\
                 \x20 remember:\n\
                 \x20   type: script\n\
                 \x20   script: remember.sh\n\
                 \x20   state_updates:\n\
                 \x20     said: \"{{{{said}}}}|{{{{initial_prompt}}}}\"\n\
                 \x20   next: done\n\
                 \x20 done:\n\
                 \x20   type: end\n\
                 \x20   output: \"turns=<{{{{previous_turns}}}}> said=<{{{{said}}}}>\"\n"
            ),
        )
        .unwrap();
    }

    /// A resumed graph child starts from the session its first run
    /// recorded: the first exchange is in `previous_turns` and the carried
    /// value is seeded back, and the second run's record replaces the first.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn resume_of_a_graph_child_seeds_the_transcript_and_the_carried_values_of_its_first_run() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_graph_remembers");
        write_graph_agent_that_remembers(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let first = run_async(spawn_and_reclaim(&mut ctx, &agent_name));
        let sid = first["session_id"].as_str().unwrap().to_string();
        let path = spawner_root(&ctx).child_session_file(&sid);
        let after_first: Session = read_yaml(&path);
        let first_run = after_first.last_run().expect("the first run is recorded");
        assert_eq!(first_run.output, "turns=<> said=<|hi>");
        assert_eq!(first_run.carried.get("said"), Some(&json!("|hi")));
        assert_eq!(
            last_exchange_of(&after_first),
            ("hi".to_string(), "turns=<> said=<|hi>".to_string())
        );

        let resumed = run_async(resume_and_reclaim(&mut ctx, &agent_name, &sid));

        assert_eq!(resumed["session_id"], sid.as_str());
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Completed);
        let after_second: Session = read_yaml(&path);
        assert_eq!(
            after_second.messages().len(),
            after_first.messages().len() + 2
        );
        let (user, assistant) = last_exchange_of(&after_second);
        assert_eq!(user, "again");
        assert!(
            assistant.contains("said=<|hi|again>"),
            "the carried value is seeded back: {assistant}"
        );
        assert!(
            assistant.contains("user: hi\nassistant: turns=<> said=<|hi>"),
            "the first exchange is in previous_turns: {assistant}"
        );
        let second_run = after_second.last_run().unwrap();
        assert_eq!(second_run.prompt, "again");
        assert_eq!(second_run.output, assistant);
        assert_eq!(second_run.carried.get("said"), Some(&json!("|hi|again")));
        assert_eq!(
            second_run.recorded_at_message_count,
            after_second.messages().len()
        );
    }

    /// A graph agent with neither a `session:` block nor a `carry:` list
    /// whose end node renders the transcript the dispatcher seeded.
    fn write_graph_agent_that_echoes_the_turns(agent_name: &str) {
        let agent_dir = paths::agent_data_dir(agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\nstart: done\nnodes:\n  done:\n    type: end\n    output: \"turns=<{{{{previous_turns}}}}>\"\n"
            ),
        )
        .unwrap();
    }

    /// A child records whatever its graph declares: a resumed child of a
    /// graph with no block and nothing to carry still sees its own first
    /// exchange in `previous_turns`, carries no values and exposes none,
    /// and the second record replaces the first.
    #[test]
    #[serial]
    fn usage_probe_resume_of_a_plain_graph_child_seeds_the_transcript_of_its_first_run() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_plain_graph_turns");
        write_graph_agent_that_echoes_the_turns(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let first = run_async(spawn_and_reclaim(&mut ctx, &agent_name));
        let sid = first["session_id"].as_str().unwrap().to_string();
        let path = spawner_root(&ctx).child_session_file(&sid);
        let after_first: Session = read_yaml(&path);
        let first_run = after_first.last_run().expect("the first run is recorded");
        assert_eq!(first_run.output, "turns=<>");
        assert!(first_run.carried.is_empty());
        assert!(first_run.exposed.is_empty());
        assert_eq!(
            last_exchange_of(&after_first),
            ("hi".to_string(), "turns=<>".to_string())
        );

        let resumed = run_async(resume_and_reclaim(&mut ctx, &agent_name, &sid));

        assert_eq!(resumed["session_id"], sid.as_str());
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Completed);
        let after_second: Session = read_yaml(&path);
        assert_eq!(
            after_second.messages().len(),
            after_first.messages().len() + 2
        );
        let (user, assistant) = last_exchange_of(&after_second);
        assert_eq!(user, "again");
        assert_eq!(assistant, "turns=<user: hi\nassistant: turns=<>>");
        let second_run = after_second.last_run().unwrap();
        assert_eq!(second_run.prompt, "again");
        assert_eq!(second_run.output, assistant);
        assert!(second_run.carried.is_empty());
        assert!(second_run.exposed.is_empty());
        assert!(!second_run.trace.is_empty());
    }

    /// A graph agent whose start node touches `started` and then blocks
    /// until `release` exists, so a test can hold the child mid-run for
    /// exactly as long as it needs. Returns `(started, release)`.
    #[cfg(unix)]
    fn write_graph_agent_gated_by_markers(agent_name: &str) -> (PathBuf, PathBuf) {
        let agent_dir = paths::agent_data_dir(agent_name);
        let started = agent_dir.join("started");
        let release = agent_dir.join("release");
        let script = format!(
            "#!/bin/bash\ntouch \"{}\"\nuntil [ -e \"{}\" ]; do sleep 0.02; done\necho '{{}}'\n",
            started.display(),
            release.display()
        );
        create_dir_all(&agent_dir).unwrap();
        write(agent_dir.join("wait.sh"), script).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\n\
                 start: wait\n\
                 nodes:\n\
                 \x20 wait:\n\
                 \x20   type: script\n\
                 \x20   script: wait.sh\n\
                 \x20   state_updates: {{}}\n\
                 \x20   next: done\n\
                 \x20 done:\n\
                 \x20   type: end\n\
                 \x20   output: done\n"
            ),
        )
        .unwrap();
        (started, release)
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn resume_marks_the_sidecar_running_under_the_new_agent_id_while_the_child_runs() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_graph_running");
        let (started, release) = write_graph_agent_gated_by_markers(&agent_name);
        write(&release, "").unwrap();
        let mut ctx = ctx_with_supervisor(4, 3);
        let first = run_async(spawn_and_reclaim(&mut ctx, &agent_name));
        let sid = first["session_id"].as_str().unwrap().to_string();
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Completed);
        std::fs::remove_file(&release).unwrap();
        std::fs::remove_file(&started).unwrap();

        run_async(async {
            let resumed = handle_spawn(
                &mut ctx,
                &json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
            )
            .await
            .unwrap();
            assert_eq!(resumed["status"], "ok", "{resumed}");
            let id = resumed["id"].as_str().unwrap().to_string();
            assert_ne!(id, first["id"].as_str().unwrap());

            let meta = child_meta_of(&ctx, &sid);
            assert_eq!(meta.status, ChildStatus::Running);
            assert_eq!(meta.agent_id, id);
            assert_eq!(meta.pid, std::process::id());

            // Once the script has touched `started` the child is provably
            // mid-run, and it stays there until `release` appears.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !started.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the resumed child never started its script"
                );
                time::sleep(Duration::from_millis(10)).await;
            }
            let meta = child_meta_of(&ctx, &sid);
            assert_eq!(meta.status, ChildStatus::Running);
            assert_eq!(meta.agent_id, id);

            write(&release, "").unwrap();
            let handle = ctx.supervisor.as_ref().unwrap().write().take(&id).unwrap();
            let result = handle.join_handle.await.unwrap().unwrap();
            assert_eq!(result.exit_status, AgentExitStatus::Completed);
            let meta = child_meta_of(&ctx, &sid);
            assert_eq!(meta.status, ChildStatus::Completed);
            assert_eq!(meta.agent_id, id);
        });
    }

    /// The resume arm re-stamps the sidecar running before the child's MCP
    /// servers connect; a connect failure must hand it back failed, or the
    /// next resume of the same id is refused as still running.
    #[test]
    #[serial]
    fn resume_marks_the_sidecar_failed_when_the_runtime_cannot_be_prepared() {
        use crate::config::StubSpawnsGuard;

        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_runtime_fails");
        write_config_agent_with_setting(&agent_name, "mcp_servers:\n  - fixture");
        let mut ctx = RequestContext::new(app_state_with_fixture_mcp_config(), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));
        let _skipped = StubSpawnsGuard::push(Err(anyhow!("Auth required: no stored token")));
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({}));
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Completed);

        let _broken = StubSpawnsGuard::push(Err(anyhow!("boom")));
        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );
        assert!(error.contains("boom"), "{error}");
        let meta = child_meta_of(&ctx, &sid);
        assert_eq!(meta.status, ChildStatus::Failed);
        assert!(
            meta.error
                .as_deref()
                .is_some_and(|error| error.contains("boom")),
            "{meta:?}"
        );

        let _skipped_again = StubSpawnsGuard::push(Err(anyhow!("Auth required: no stored token")));
        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );
        assert_eq!(spawn.session_id.as_deref(), Some(sid.as_str()));
        let meta = child_meta_of(&ctx, &sid);
        assert_eq!(meta.status, ChildStatus::Running);
        assert_eq!(meta.agent_id, spawn.agent_id);
        assert_eq!(meta.error, None);
    }

    /// Spawns a config child with `variables` on `ctx`, drives one chat
    /// turn so its session file holds a transcript, and records it
    /// completed. Returns the session id and its message count.
    fn seed_completed_config_child(
        ctx: &mut RequestContext,
        agent_name: &str,
        variables: Value,
    ) -> (String, usize) {
        let spawn = build_ready_spawn(
            ctx,
            json!({"agent": agent_name, "prompt": "hi", "variables": variables}),
        );
        let ChildSpawn {
            mut child_ctx,
            input,
            session_id,
            ..
        } = *spawn;
        let sid = session_id.unwrap();
        let app = Arc::clone(&child_ctx.app.config);
        child_ctx
            .after_chat_completion(
                app.as_ref(),
                &input,
                "first reply, long enough that the transcript holds a dozen tokens",
                &[],
            )
            .unwrap();
        let count = child_ctx.session.as_ref().unwrap().messages().len();
        finish_child_meta(
            child_ctx.child_session_root.as_deref(),
            Some(&sid),
            ChildStatus::Completed,
            None,
        );
        (sid, count)
    }

    fn spawner_root(ctx: &RequestContext) -> Arc<ChildSessionRoot> {
        ctx.child_session_root
            .clone()
            .expect("the spawner keeps the root its children live under")
    }

    /// Rewrites the `model:` line of a stored session so it names a model
    /// no client resolves.
    fn store_unresolvable_model(path: &Path) {
        let rewritten: Vec<String> = read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| {
                if line.starts_with("model: ") {
                    "model: no-such-client:ghost".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect();
        write(path, rewritten.join("\n") + "\n").unwrap();
    }

    #[test]
    #[serial]
    fn resumed_config_child_carries_the_stored_transcript_instructions_and_variables() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_config");
        write_config_agent_with_variables(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, stored_count) =
            seed_completed_config_child(&mut ctx, &agent_name, json!({"tone": "loud"}));
        assert!(stored_count >= 2, "{stored_count}");
        let root = spawner_root(&ctx);
        let path = root.child_session_file(&sid);
        let stored: Session = read_yaml(&path);
        let stored_instructions = stored.agent_instructions().to_string();
        assert!(
            stored_instructions.starts_with("Say loud to them"),
            "{stored_instructions}"
        );
        store_unresolvable_model(&path);
        write(
            paths::agent_data_dir(&agent_name).join("config.yaml"),
            format!(
                "name: {agent_name}\n\
                 instructions: Shout {{{{tone}}}} at {{{{who}}}}\n\
                 variables:\n\
                 \x20 - name: tone\n\
                 \x20   description: t\n\
                 \x20   default: calm\n\
                 \x20 - name: who\n\
                 \x20   description: w\n\
                 \x20   default: them\n"
            ),
        )
        .unwrap();

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        assert_eq!(spawn.session_id.as_deref(), Some(sid.as_str()));
        let ChildSpawn {
            mut child_ctx,
            input,
            agent_id,
            ..
        } = *spawn;
        let session = child_ctx.session.as_ref().unwrap();
        assert_eq!(session.messages().len(), stored_count);
        assert_eq!(session.agent_instructions(), stored_instructions);
        assert!(!session.agent_instructions().contains("Shout"));
        assert_eq!(session.agent_variables().get("tone").unwrap(), "loud");
        assert_eq!(session.agent_variables().get("who").unwrap(), "them");
        assert_eq!(
            child_ctx.agent_variables.as_ref(),
            Some(session.agent_variables())
        );
        assert_eq!(session.model().id(), ctx.current_model().id());
        assert_eq!(session.name(), sid);
        assert_eq!(session.path(), Some(path.as_path()));
        let meta = child_meta_of(&ctx, &sid);
        assert_eq!(meta.status, ChildStatus::Running);
        assert_eq!(meta.agent_id, agent_id);
        assert_eq!(meta.pid, std::process::id());
        assert_eq!(meta.pid_start, ProcessStamp::current().start);

        let app = Arc::clone(&child_ctx.app.config);
        child_ctx
            .after_chat_completion(app.as_ref(), &input, "second reply", &[])
            .unwrap();

        let session = child_ctx.session.as_ref().unwrap();
        assert_eq!(session.messages().len(), stored_count + 2);
        let on_disk: Session = read_yaml(&path);
        assert_eq!(on_disk.messages().len(), stored_count + 2);
        let last = on_disk.messages().last().unwrap();
        assert!(matches!(&last.content, MessageContent::Text(text) if text == "second reply"));
        assert!(
            read_to_string(&path)
                .unwrap()
                .contains(&format!("model: {}", ctx.current_model().id()))
        );
    }

    /// Without a `model:` of its own the agent resolves to the spawner's
    /// current model, so that is what the resumed session is seated on,
    /// never the id the session file stored.
    #[test]
    #[serial]
    fn resumed_session_runs_on_the_agents_resolved_model_not_the_stored_one() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_model");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({}));
        let path = spawner_root(&ctx).child_session_file(&sid);
        let stored_model = ctx.current_model().id();
        assert!(
            read_to_string(&path)
                .unwrap()
                .contains(&format!("model: {stored_model}")),
            "the seeded child stored the spawner's model at the time"
        );
        ctx.model = Model::new("test-seeded", "other");
        assert_ne!(ctx.current_model().id(), stored_model);

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        let session = spawn.child_ctx.session.as_ref().unwrap();
        assert_eq!(session.model().id(), "test-seeded:other");
        assert_eq!(spawn.child_ctx.current_model().id(), "test-seeded:other");
        assert_eq!(
            spawn.child_ctx.agent.as_ref().unwrap().model().id(),
            "test-seeded:other"
        );
    }

    /// An agent whose config pins a `model:` resumes on it, as a fresh
    /// seat would, whatever the spawner currently runs.
    #[test]
    #[serial]
    fn resumed_session_keeps_the_agents_pinned_model_over_the_spawners() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_pinned_model");
        write_config_agent_with_setting(&agent_name, "model: test-seeded:pinned");
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({}));
        let path = spawner_root(&ctx).child_session_file(&sid);
        assert!(
            read_to_string(&path)
                .unwrap()
                .contains("model: test-seeded:pinned")
        );
        ctx.model = Model::new("test-seeded", "other");

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        let session = spawn.child_ctx.session.as_ref().unwrap();
        assert_eq!(session.model().id(), "test-seeded:pinned");
        assert_eq!(spawn.child_ctx.current_model().id(), "test-seeded:pinned");
        let app = Arc::clone(&spawn.child_ctx.app.config);
        let role = spawn.child_ctx.extract_role(app.as_ref()).unwrap();
        assert_eq!(role.model().id(), "test-seeded:pinned");
    }

    fn resume_error(ctx: &mut RequestContext, args: Value) -> String {
        match run_async(build_child_spawn(ctx, &args)) {
            Ok(SpawnBuild::Ready(_)) => panic!("resume must be refused: {args}"),
            Ok(SpawnBuild::Refused(payload)) => panic!("refused as a payload: {payload}"),
            Err(error) => format!("{error:#}"),
        }
    }

    fn fixture_time(minute: u32) -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000 + i64::from(minute) * 60, 0).unwrap()
    }

    #[test]
    #[serial]
    fn resume_of_an_unknown_id_lists_the_ten_newest_ids_and_marks_crashed_ones() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_unknown");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let root = ctx.ensure_child_session_root().unwrap();
        root.ensure_dir(&RootFile::seed(ProcessStamp::current(), None))
            .unwrap();
        let dead = ProcessStamp {
            pid: u32::MAX - 1,
            start: 0,
        };
        let mut ids = Vec::new();
        for minute in 0..12 {
            let sid = child_sessions::new_session_id(&agent_name);
            let mut meta = ChildMeta::new_running_at(
                fixture_time(minute),
                &sid,
                &agent_name,
                "agent_x",
                dead,
                None,
                None,
                "hi",
                None,
            );
            if minute != 8 {
                meta.finish_at(fixture_time(minute), ChildStatus::Completed, None);
            }
            root.save_child_meta(&meta).unwrap();
            ids.push(sid);
        }
        let expected: Vec<String> = (2..12)
            .rev()
            .map(|minute| {
                let sid = &ids[minute];
                if minute == 8 {
                    format!("{sid} (crashed)")
                } else {
                    sid.clone()
                }
            })
            .collect();

        let unknown = child_sessions::new_session_id(&agent_name);
        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": unknown}),
        );

        assert!(
            error.contains(&format!(
                "no child session '{unknown}' under this session; recent ids: {} (+2 more)",
                expected.join(", ")
            )),
            "{error}"
        );
    }

    #[test]
    #[serial]
    fn resume_of_an_unknown_id_under_an_empty_root_says_so() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_unknown_empty");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let unknown = child_sessions::new_session_id(&agent_name);

        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": unknown}),
        );

        assert!(
            error.contains(&format!(
                "no child session '{unknown}' under this session; recent ids: none"
            )),
            "{error}"
        );
    }

    fn paths_under(dir: &Path) -> std::collections::BTreeSet<PathBuf> {
        let mut found = std::collections::BTreeSet::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(paths_under(&path));
            }
            found.insert(path);
        }
        found
    }

    #[test]
    #[serial]
    fn resume_refuses_malformed_ids_before_building_any_path() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_malformed");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        seed_completed_config_child(&mut ctx, &agent_name, json!({}));
        let before = paths_under(&_guard.cache_root);
        assert!(!before.is_empty());

        for bad in [
            "sess_../x_0123456789ab",
            "sess_a/b_0123456789ab",
            "sess_a\\b_0123456789ab",
            "nonsense",
        ] {
            let error = resume_error(
                &mut ctx,
                json!({"agent": agent_name, "prompt": "again", "session_id": bad}),
            );
            assert!(
                error.contains(&format!(
                    "'{bad}' is not a child session id (expected sess_<agent>_<12 hex>)"
                )),
                "{error}"
            );
        }

        assert_eq!(paths_under(&_guard.cache_root), before);
    }

    #[test]
    #[serial]
    fn resume_refuses_a_session_that_belongs_to_another_agent() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_owner");
        let other_agent = unique_agent_name("test_resume_other");
        write_config_only_agent(&agent_name);
        write_config_only_agent(&other_agent);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({}));

        let error = resume_error(
            &mut ctx,
            json!({"agent": other_agent, "prompt": "again", "session_id": sid}),
        );

        assert!(
            error.contains(&format!(
                "'{sid}' belongs to agent '{agent_name}', not '{other_agent}'"
            )),
            "{error}"
        );
    }

    #[test]
    #[serial]
    fn resume_refuses_a_running_child_and_accepts_a_crashed_one() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_running");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({}));
        let root = spawner_root(&ctx);
        let mut meta = root.load_child_meta(&sid).unwrap().unwrap();
        meta.status = ChildStatus::Running;
        let live = ProcessStamp::current();
        meta.pid = live.pid;
        meta.pid_start = live.start;
        root.save_child_meta(&meta).unwrap();

        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );
        assert!(
            error.contains(&format!(
                "'{sid}' is still running; collect or cancel it first"
            )),
            "{error}"
        );

        meta.pid = u32::MAX - 1;
        meta.pid_start = 0;
        root.save_child_meta(&meta).unwrap();
        assert_eq!(meta.effective_status(), EffectiveStatus::Crashed);

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        assert_eq!(spawn.session_id.as_deref(), Some(sid.as_str()));
        let meta = child_meta_of(&ctx, &sid);
        assert_eq!(meta.status, ChildStatus::Running);
        assert_eq!(meta.agent_id, spawn.agent_id);
        assert_eq!(meta.pid, live.pid);
    }

    #[test]
    #[serial]
    fn resume_refuses_differing_variables_and_accepts_equal_or_omitted_ones() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_variables");
        write_config_agent_with_variables(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({"tone": "loud"}));

        let error = resume_error(
            &mut ctx,
            json!({
                "agent": agent_name,
                "prompt": "again",
                "session_id": sid,
                "variables": {"tone": "quiet", "who": "them"},
            }),
        );
        assert!(
            error.contains(&format!(
                "'{sid}' was spawned with different variables (tone); resume with the same values or spawn fresh"
            )),
            "{error}"
        );
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Completed);

        let same = build_ready_spawn(
            &mut ctx,
            json!({
                "agent": agent_name,
                "prompt": "again",
                "session_id": sid,
                "variables": {"tone": "loud"},
            }),
        );
        assert_eq!(same.session_id.as_deref(), Some(sid.as_str()));
        finish_child_meta(
            Some(&spawner_root(&ctx)),
            Some(&sid),
            ChildStatus::Completed,
            None,
        );

        let omitted = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );
        assert_eq!(omitted.session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(
            omitted
                .child_ctx
                .session
                .as_ref()
                .unwrap()
                .agent_variables()
                .get("tone")
                .unwrap(),
            "loud"
        );
    }

    /// A variable the agent declares without a default is satisfied from
    /// the stored session on a resume that omits `variables`, so the
    /// session is loaded before the agent's shared variables are
    /// initialised; the inherited ones alone would not carry it.
    #[test]
    #[serial]
    fn resume_with_variables_omitted_satisfies_a_required_variable_from_the_stored_ones() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_required_variable");
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("config.yaml"),
            format!(
                "name: {agent_name}\n\
                 instructions: Say {{{{tone}}}}\n\
                 variables:\n\
                 \x20 - name: tone\n\
                 \x20   description: t\n"
            ),
        )
        .unwrap();
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({"tone": "loud"}));

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        assert_eq!(spawn.session_id.as_deref(), Some(sid.as_str()));
        let instructions = spawn
            .child_ctx
            .agent
            .as_ref()
            .unwrap()
            .interpolated_instructions();
        assert!(instructions.contains("Say loud"), "{instructions}");
        let session = spawn.child_ctx.session.as_ref().unwrap();
        assert_eq!(session.agent_variables().get("tone").unwrap(), "loud");
    }

    /// A child that crashed before its first turn left a session file with
    /// no messages, so the system prompt is not in the transcript; the
    /// resumed child rebuilds it from the stored instructions.
    #[test]
    #[serial]
    fn resume_of_a_child_that_crashed_before_its_first_turn_speaks_with_the_stored_instructions() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_before_first_turn");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let fresh = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));
        let sid = fresh.session_id.clone().unwrap();
        drop(fresh);
        let root = spawner_root(&ctx);
        let stored: Session = read_yaml(&root.child_session_file(&sid));
        assert!(stored.messages().is_empty());
        let stored_instructions = stored.agent_instructions().to_string();
        assert!(
            stored_instructions.starts_with("hi"),
            "{stored_instructions}"
        );
        let mut meta = root.load_child_meta(&sid).unwrap().unwrap();
        meta.status = ChildStatus::Running;
        meta.pid = u32::MAX - 1;
        meta.pid_start = 0;
        root.save_child_meta(&meta).unwrap();
        assert_eq!(meta.effective_status(), EffectiveStatus::Crashed);

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        let ChildSpawn {
            child_ctx, input, ..
        } = *spawn;
        let session = child_ctx.session.as_ref().unwrap();
        assert_eq!(session.agent_instructions(), stored_instructions);
        assert_eq!(session.to_role().prompt(), stored_instructions);
        let app = Arc::clone(&child_ctx.app.config);
        let role = child_ctx.extract_role(app.as_ref()).unwrap();
        assert!(
            role.prompt().starts_with(&stored_instructions),
            "{}",
            role.prompt()
        );
        let messages = session.build_messages(&input);
        assert_eq!(messages[0].role, crate::client::MessageRole::System);
        let MessageContent::Text(system) = &messages[0].content else {
            panic!("{:?}", messages[0].content);
        };
        assert!(system.starts_with(&stored_instructions), "{system}");
        assert!(matches!(
            &messages[1].content,
            MessageContent::Text(text) if text == "again"
        ));
    }

    #[test]
    fn differing_variables_names_missing_and_changed_keys_sorted() {
        let mut stored = AgentVariables::new();
        stored.insert("tone".into(), "loud".into());
        let mut provided = AgentVariables::new();
        provided.insert("who".into(), "them".into());
        provided.insert("tone".into(), "quiet".into());
        assert_eq!(differing_variables(&provided, &stored), ["tone", "who"]);
        provided.insert("tone".into(), "loud".into());
        assert_eq!(differing_variables(&provided, &stored), ["who"]);
        assert!(differing_variables(&AgentVariables::new(), &stored).is_empty());
    }

    #[test]
    #[serial]
    fn knob_source_names_the_app_file_then_the_agent_file_then_dry_run() {
        let _guard = TestConfigDirGuard::new();
        let agent_off = Agent::test_new(AgentConfig {
            name: "knob_probe".into(),
            child_sessions: Some(false),
            ..AgentConfig::default()
        });
        let everything_off = AppConfig {
            child_sessions: Some(false),
            dry_run: true,
            ..Default::default()
        };
        assert_eq!(
            knob_source(Some(&agent_off), &everything_off),
            format!(
                "child_sessions: false in {}",
                paths::config_file().display()
            )
        );
        assert_eq!(
            paths::config_file(),
            _guard.path.join("config.yaml"),
            "the app knob names the config file under the config dir"
        );

        let agent_and_dry_run = AppConfig {
            dry_run: true,
            ..Default::default()
        };
        assert_eq!(
            knob_source(Some(&agent_off), &agent_and_dry_run),
            format!(
                "child_sessions: false in {}",
                paths::agent_data_dir("knob_probe")
                    .join("config.yaml")
                    .display()
            )
        );

        let agent_on = Agent::test_new(AgentConfig {
            name: "knob_probe".into(),
            ..AgentConfig::default()
        });
        assert_eq!(
            knob_source(Some(&agent_on), &agent_and_dry_run),
            "dry_run: true"
        );
        assert_eq!(knob_source(None, &agent_and_dry_run), "dry_run: true");
    }

    #[test]
    #[serial]
    fn resume_with_the_app_knob_off_names_the_config_file() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_app_knob");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_child_sessions_off();
        let sid = child_sessions::new_session_id(&agent_name);

        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        assert!(
            error.contains(&format!(
                "child sessions are off (child_sessions: false in {}); spawn fresh with the context in the prompt",
                paths::config_file().display()
            )),
            "{error}"
        );
        assert!(!_guard.cache_root.join("sessions").exists());
    }

    #[test]
    #[serial]
    fn resume_with_the_agent_knob_off_names_the_agent_config_file() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_agent_knob");
        write_config_agent_with_setting(&agent_name, "child_sessions: false");
        let mut ctx = ctx_with_supervisor(4, 3);
        let sid = child_sessions::new_session_id(&agent_name);

        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        assert!(
            error.contains(&format!(
                "child sessions are off (child_sessions: false in {}); spawn fresh",
                paths::agent_data_dir(&agent_name)
                    .join("config.yaml")
                    .display()
            )),
            "{error}"
        );
    }

    #[test]
    #[serial]
    fn resume_under_dry_run_names_the_flag_before_missing_the_root() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_dry_run");
        write_config_only_agent(&agent_name);
        let app = AppState {
            config: Arc::new(AppConfig {
                dry_run: true,
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));
        let sid = child_sessions::new_session_id(&agent_name);

        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        assert!(
            error.contains("child sessions are off (dry_run: true); spawn fresh"),
            "{error}"
        );
        assert!(ctx.child_session_root.is_none());
    }

    /// A child spawner whose own parent never resolved a root has nowhere
    /// to look a session id up; the knobs are on, so the refusal says that
    /// rather than blaming a setting.
    #[test]
    #[serial]
    fn resume_from_a_spawner_without_a_root_is_refused() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_no_root");
        write_config_only_agent(&agent_name);
        let parent = ctx_with_supervisor(4, 3);
        let mut ctx = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            1,
            Arc::new(Inbox::new()),
            "agent_parent_1".into(),
        );
        ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));
        assert!(ctx.child_session_root.is_none());
        let sid = child_sessions::new_session_id(&agent_name);

        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        assert!(
            error.contains(&format!(
                "cannot resume '{sid}': this spawner has no child session root"
            )),
            "{error}"
        );
    }

    #[test]
    #[serial]
    fn resume_refuses_a_transcript_over_the_model_window() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_window");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let tiny = Model::from_config(
            "test-seeded",
            &[ModelData {
                max_input_tokens: Some(10),
                ..ModelData::new("tiny")
            }],
        )
        .remove(0);
        ctx.model = tiny.clone();
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({}));
        let root = spawner_root(&ctx);
        let path = root.child_session_file(&sid);
        let app = Arc::clone(&ctx.app.config);
        let tokens = Session::load_from_path(&path, tiny.clone(), &ctx, app.as_ref())
            .unwrap()
            .tokens();
        assert!(tokens > 10, "{tokens}");

        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        assert!(
            error.contains(&format!(
                "'{sid}' holds ~{tokens} tokens, over the test-seeded:tiny window (10); child compression is not available yet - spawn fresh"
            )),
            "{error}"
        );
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Completed);
    }

    /// A sidecar whose stamp recorded `start` 0 (sysinfo could not read the
    /// own start time at spawn) under a pid that is still alive reads as
    /// dead: the recent-ids list labels it crashed and the id resumes,
    /// re-stamped with the real start time.
    #[test]
    #[serial]
    fn usage_probe_a_running_child_stamped_with_start_zero_reads_crashed_and_resumes() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_start_zero");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, stored_count) = seed_completed_config_child(&mut ctx, &agent_name, json!({}));
        let live = ProcessStamp::current();
        assert_ne!(
            live.start, 0,
            "this host reports no start time for its own process; the pin cannot discriminate"
        );
        let root = spawner_root(&ctx);
        let mut meta = root.load_child_meta(&sid).unwrap().unwrap();
        meta.status = ChildStatus::Running;
        meta.pid = live.pid;
        meta.pid_start = 0;
        root.save_child_meta(&meta).unwrap();
        assert_eq!(meta.effective_status(), EffectiveStatus::Crashed);

        let unknown = child_sessions::new_session_id(&agent_name);
        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": unknown}),
        );
        assert!(
            error.contains(&format!("recent ids: {sid} (crashed)")),
            "{error}"
        );

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        assert_eq!(spawn.session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(
            spawn.child_ctx.session.as_ref().unwrap().messages().len(),
            stored_count
        );
        let meta = child_meta_of(&ctx, &sid);
        assert_eq!(meta.status, ChildStatus::Running);
        assert_eq!(meta.agent_id, spawn.agent_id);
        assert_eq!(meta.pid, live.pid);
        assert_eq!(meta.pid_start, live.start);
    }

    /// Resuming is not a one-shot: the transcript a resumed child adds is
    /// what the next resume carries, the id stays the same across every
    /// hop, each hop gets its own agent id, and a hop that ended `failed`
    /// resumes like a completed one.
    #[test]
    #[serial]
    fn usage_probe_a_resumed_child_resumes_again_carrying_its_own_additions() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_chain");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, stored_count) = seed_completed_config_child(&mut ctx, &agent_name, json!({}));
        let first_meta = child_meta_of(&ctx, &sid);

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "second", "session_id": sid}),
        );
        let second_agent_id = spawn.agent_id.clone();
        assert_ne!(second_agent_id, first_meta.agent_id);
        let ChildSpawn {
            mut child_ctx,
            input,
            ..
        } = *spawn;
        let app = Arc::clone(&child_ctx.app.config);
        child_ctx
            .after_chat_completion(app.as_ref(), &input, "second reply", &[])
            .unwrap();
        assert_eq!(
            child_ctx.session.as_ref().unwrap().messages().len(),
            stored_count + 2
        );
        finish_child_meta(
            child_ctx.child_session_root.as_deref(),
            Some(&sid),
            ChildStatus::Failed,
            Some("boom".to_string()),
        );
        let failed = child_meta_of(&ctx, &sid);
        assert_eq!(failed.status, ChildStatus::Failed);
        assert_eq!(failed.agent_id, second_agent_id);

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "third", "session_id": sid}),
        );

        assert_eq!(spawn.session_id.as_deref(), Some(sid.as_str()));
        assert_ne!(spawn.agent_id, second_agent_id);
        assert_ne!(spawn.agent_id, first_meta.agent_id);
        let session = spawn.child_ctx.session.as_ref().unwrap();
        assert_eq!(session.messages().len(), stored_count + 2);
        let last = session.messages().last().unwrap();
        assert!(matches!(&last.content, MessageContent::Text(text) if text == "second reply"));
        let meta = child_meta_of(&ctx, &sid);
        assert_eq!(meta.status, ChildStatus::Running);
        assert_eq!(meta.agent_id, spawn.agent_id);
        assert_eq!(meta.created_at, first_meta.created_at);
        assert_eq!(meta.prompt_head, "hi");
        assert!(meta.error.is_none(), "{meta:?}");
    }

    /// A corrupt sidecar, a missing session file or a corrupt session file
    /// each degrade to an error naming the file; none of them panics, and
    /// none re-stamps the sidecar running. The good bytes put back resume.
    #[test]
    #[serial]
    fn usage_probe_resume_degrades_to_an_error_on_a_corrupt_sidecar_or_session_file() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_corrupt");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({}));
        let root = spawner_root(&ctx);
        let meta_path = root.child_meta_file(&sid);
        let session_path = root.child_session_file(&sid);
        let good_meta = read_to_string(&meta_path).unwrap();
        let good_session = read_to_string(&session_path).unwrap();
        let args = json!({"agent": agent_name, "prompt": "again", "session_id": sid});

        write(&meta_path, "status: [running\n").unwrap();
        let error = resume_error(&mut ctx, args.clone());
        assert!(
            error.contains(&format!(
                "Failed to read the sidecar of child session '{sid}'"
            )),
            "{error}"
        );
        assert_eq!(read_to_string(&meta_path).unwrap(), "status: [running\n");
        assert_eq!(read_to_string(&session_path).unwrap(), good_session);
        write(&meta_path, &good_meta).unwrap();

        std::fs::remove_file(&session_path).unwrap();
        let error = resume_error(&mut ctx, args.clone());
        assert!(
            error.contains(&format!(
                "Failed to load session at {}",
                session_path.display()
            )),
            "{error}"
        );
        assert_eq!(read_to_string(&meta_path).unwrap(), good_meta);

        write(&session_path, "messages: [not\n").unwrap();
        let error = resume_error(&mut ctx, args.clone());
        assert!(
            error.contains(&format!("Invalid session at {}", session_path.display())),
            "{error}"
        );
        assert_eq!(read_to_string(&meta_path).unwrap(), good_meta);
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Completed);

        write(&session_path, &good_session).unwrap();
        let spawn = build_ready_spawn(&mut ctx, args);
        assert_eq!(spawn.session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Running);
    }

    /// Ids are looked up under the spawner's own root only: a child another
    /// root spawned is unknown here, its sidecar is left alone, and the
    /// recent-ids list is this root's (empty).
    #[test]
    #[serial]
    fn usage_probe_a_child_of_another_root_is_unknown_under_this_one() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_sibling_root");
        write_config_only_agent(&agent_name);
        let mut owner = spawner_with_session("owner-root");
        let (sid, _) = seed_completed_config_child(&mut owner, &agent_name, json!({}));
        let owner_root = spawner_root(&owner);
        let owner_meta = read_to_string(owner_root.child_meta_file(&sid)).unwrap();

        let mut other = spawner_with_session("other-root");
        let error = resume_error(
            &mut other,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        assert!(
            error.contains(&format!(
                "no child session '{sid}' under this session; recent ids: none"
            )),
            "{error}"
        );
        assert_ne!(*spawner_root(&other), *owner_root);
        assert_eq!(
            read_to_string(owner_root.child_meta_file(&sid)).unwrap(),
            owner_meta
        );
        assert_eq!(child_meta_of(&owner, &sid).status, ChildStatus::Completed);
    }

    /// `session_id: null` is the omitted form: the spawn seats a fresh
    /// child session instead of refusing the id.
    #[test]
    #[serial]
    fn usage_probe_a_null_session_id_spawns_fresh_like_an_omitted_one() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_null_id");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "hi", "session_id": null}),
        );

        let sid = spawn.session_id.clone().expect("a fresh child is seated");
        assert_eq!(
            child_sessions::parse_session_id(&sid).map(|(agent, _)| agent),
            Some(agent_name.as_str())
        );
        let meta = child_meta_of(&ctx, &sid);
        assert_eq!(meta.status, ChildStatus::Running);
        assert_eq!(meta.agent_id, spawn.agent_id);
        assert_eq!(meta.prompt_head, "hi");
    }

    /// What the resumed child answers under is the stored prompt and the
    /// agent's resolved model (the spawner's current one, as this agent
    /// pins none), read the way the run reads them (through
    /// `extract_role`), not the agent's rewritten `config.yaml`.
    #[test]
    #[serial]
    fn usage_probe_a_resumed_child_answers_under_the_stored_prompt_and_the_spawner_model() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_role");
        write_config_agent_with_variables(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, stored_count) =
            seed_completed_config_child(&mut ctx, &agent_name, json!({"tone": "loud"}));
        let root = spawner_root(&ctx);
        store_unresolvable_model(&root.child_session_file(&sid));
        write(
            paths::agent_data_dir(&agent_name).join("config.yaml"),
            format!(
                "name: {agent_name}\n\
                 instructions: Shout {{{{tone}}}} at {{{{who}}}}\n\
                 variables:\n\
                 \x20 - name: tone\n\
                 \x20   description: t\n\
                 \x20   default: calm\n\
                 \x20 - name: who\n\
                 \x20   description: w\n\
                 \x20   default: them\n"
            ),
        )
        .unwrap();
        ctx.model = Model::new("test-seeded", "other");

        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );

        let ChildSpawn {
            child_ctx, input, ..
        } = *spawn;
        // A non-empty session carries its system prompt as the first stored
        // message, so what the model sees is the stored one plus the new
        // prompt; the rebuilt role prompt is only seated into an empty one.
        let session = child_ctx.session.as_ref().unwrap();
        let messages = session.build_messages(&input);
        assert_eq!(messages.len(), stored_count + 1);
        assert_eq!(messages[0].role, crate::client::MessageRole::System);
        let MessageContent::Text(system) = &messages[0].content else {
            panic!("{:?}", messages[0].content);
        };
        assert!(system.contains("Say loud to them"), "{system}");
        assert!(!system.contains("Shout"), "{system}");
        let app = Arc::clone(&child_ctx.app.config);
        let role = child_ctx.extract_role(app.as_ref()).unwrap();
        assert_eq!(role.model().id(), "test-seeded:other");
        assert_eq!(
            child_ctx
                .agent_variables
                .as_ref()
                .unwrap()
                .get("tone")
                .unwrap(),
            "loud"
        );
    }

    /// The window gate is strict: a transcript exactly at the model's
    /// `max_input_tokens` resumes, one token less of room refuses it.
    #[test]
    #[serial]
    fn usage_probe_a_transcript_exactly_at_the_model_window_resumes() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_window_edge");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({}));
        let root = spawner_root(&ctx);
        let path = root.child_session_file(&sid);
        let app = Arc::clone(&ctx.app.config);
        let tokens =
            Session::load_from_path(&path, ctx.current_model().clone(), &ctx, app.as_ref())
                .unwrap()
                .tokens();
        assert!(tokens > 1, "{tokens}");
        let window = |name: &str, max: usize| {
            Model::from_config(
                "test-seeded",
                &[ModelData {
                    max_input_tokens: Some(max),
                    ..ModelData::new(name)
                }],
            )
            .remove(0)
        };

        ctx.model = window("exact", tokens);
        let spawn = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );
        assert_eq!(spawn.session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(spawn.child_ctx.session.as_ref().unwrap().tokens(), tokens);
        finish_child_meta(
            spawn.child_ctx.child_session_root.as_deref(),
            Some(&sid),
            ChildStatus::Completed,
            None,
        );

        ctx.model = window("small", tokens - 1);
        let error = resume_error(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "again", "session_id": sid}),
        );
        assert!(
            error.contains(&format!(
                "'{sid}' holds ~{tokens} tokens, over the test-seeded:small window ({})",
                tokens - 1
            )),
            "{error}"
        );
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Completed);
    }

    /// A double submit through the real tool surface: resuming an id whose
    /// previous resume is still running is refused without disturbing the
    /// running child, whose list/check/cancel payloads carry the same
    /// session id under the new agent id. A cancelled hop resumes like a
    /// completed one and its collect payload carries the id as well.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn usage_probe_a_second_resume_of_a_live_child_is_refused_and_the_cancelled_hop_resumes() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_double_submit");
        let (started, release) = write_graph_agent_gated_by_markers(&agent_name);
        write(&release, "").unwrap();
        let mut ctx = ctx_with_supervisor(4, 3);
        let first = run_async(spawn_and_reclaim(&mut ctx, &agent_name));
        let sid = first["session_id"].as_str().unwrap().to_string();
        assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Completed);
        std::fs::remove_file(&release).unwrap();
        std::fs::remove_file(&started).unwrap();

        run_async(async {
            let args = json!({"agent": agent_name, "prompt": "again", "session_id": sid});
            let resumed = handle_spawn(&mut ctx, &args).await.unwrap();
            assert_eq!(resumed["status"], "ok", "{resumed}");
            let resumed_id = resumed["id"].as_str().unwrap().to_string();
            assert_eq!(resumed["session_id"], sid.as_str());

            let error = format!("{:#}", handle_spawn(&mut ctx, &args).await.unwrap_err());
            assert!(
                error.contains(&format!(
                    "'{sid}' is still running; collect or cancel it first"
                )),
                "{error}"
            );
            let meta = child_meta_of(&ctx, &sid);
            assert_eq!(meta.status, ChildStatus::Running);
            assert_eq!(
                meta.agent_id, resumed_id,
                "the refusal leaves the live hop alone"
            );

            let listed = handle_list_running(&mut ctx).unwrap();
            let rows = listed["agents"].as_array().unwrap();
            assert_eq!(rows.len(), 1, "{listed}");
            assert_eq!(rows[0]["id"], resumed_id.as_str());
            assert_eq!(rows[0]["session_id"], sid.as_str());
            let pending = handle_check(&mut ctx, &json!({"id": resumed_id}))
                .await
                .unwrap();
            assert_eq!(pending["status"], "pending", "{pending}");
            assert_eq!(pending["session_id"], sid.as_str());

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !started.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the resumed child never started its script"
                );
                time::sleep(Duration::from_millis(10)).await;
            }
            let cancelled = handle_cancel(&mut ctx, &json!({"id": resumed_id}))
                .await
                .unwrap();
            assert_eq!(cancelled["status"], "ok", "{cancelled}");
            assert_eq!(cancelled["session_id"], sid.as_str());
            let meta = child_meta_of(&ctx, &sid);
            assert_eq!(meta.status, ChildStatus::Cancelled);
            assert_eq!(meta.agent_id, resumed_id);

            write(&release, "").unwrap();
            std::fs::remove_file(&started).unwrap();
            let third = handle_spawn(&mut ctx, &args).await.unwrap();
            assert_eq!(third["status"], "ok", "{third}");
            assert_eq!(third["session_id"], sid.as_str());
            let third_id = third["id"].as_str().unwrap().to_string();
            assert_ne!(third_id, resumed_id);
            assert_ne!(third_id, first["id"].as_str().unwrap());
            assert_eq!(child_meta_of(&ctx, &sid).agent_id, third_id);

            let collected = handle_collect(&mut ctx, &json!({"id": third_id}))
                .await
                .unwrap();
            assert_eq!(collected["status"], "completed", "{collected}");
            assert_eq!(collected["session_id"], sid.as_str());
            let meta = child_meta_of(&ctx, &sid);
            assert_eq!(meta.status, ChildStatus::Completed);
            assert_eq!(meta.agent_id, third_id);
            assert_eq!(meta.prompt_head, "hi");
        });
    }

    /// The refusals come in the documented order, so a caller who gets
    /// several things wrong at once is told about the earliest one: an off
    /// knob wins over a malformed id, a malformed id over the owner check,
    /// the owner check over the variables, and a live hop over the
    /// variables; none of the refusals re-stamps the sidecar.
    #[test]
    #[serial]
    fn usage_probe_resume_refusals_report_the_earliest_failing_step() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_order");
        let other_agent = unique_agent_name("test_resume_order_other");
        write_config_agent_with_variables(&agent_name);
        write_config_agent_with_variables(&other_agent);
        let mut ctx = ctx_with_supervisor(4, 3);
        let (sid, _) = seed_completed_config_child(&mut ctx, &agent_name, json!({"tone": "loud"}));
        let meta_before = read_to_string(spawner_root(&ctx).child_meta_file(&sid)).unwrap();

        let knob_off_message = format!(
            "child sessions are off (child_sessions: false in {})",
            paths::config_file().display()
        );
        let mut off = ctx_with_child_sessions_off();
        for bad in ["nonsense", sid.as_str()] {
            let error = resume_error(
                &mut off,
                json!({"agent": other_agent, "prompt": "again", "session_id": bad}),
            );
            assert!(error.contains(&knob_off_message), "{error}");
            assert!(!error.contains("is not a child session id"), "{error}");
            assert!(!error.contains("belongs to agent"), "{error}");
        }

        let error = resume_error(
            &mut ctx,
            json!({
                "agent": other_agent,
                "prompt": "again",
                "session_id": "nonsense",
                "variables": {"tone": "quiet"},
            }),
        );
        assert!(
            error.contains("'nonsense' is not a child session id"),
            "{error}"
        );
        assert!(!error.contains("belongs to agent"), "{error}");
        assert!(!error.contains("different variables"), "{error}");

        let error = resume_error(
            &mut ctx,
            json!({
                "agent": other_agent,
                "prompt": "again",
                "session_id": sid,
                "variables": {"tone": "quiet"},
            }),
        );
        assert!(
            error.contains(&format!(
                "'{sid}' belongs to agent '{agent_name}', not '{other_agent}'"
            )),
            "{error}"
        );
        assert!(!error.contains("different variables"), "{error}");

        let root = spawner_root(&ctx);
        let mut meta = root.load_child_meta(&sid).unwrap().unwrap();
        meta.status = ChildStatus::Running;
        let live = ProcessStamp::current();
        meta.pid = live.pid;
        meta.pid_start = live.start;
        root.save_child_meta(&meta).unwrap();
        let live_meta = read_to_string(root.child_meta_file(&sid)).unwrap();
        let error = resume_error(
            &mut ctx,
            json!({
                "agent": agent_name,
                "prompt": "again",
                "session_id": sid,
                "variables": {"tone": "quiet"},
            }),
        );
        assert!(
            error.contains(&format!(
                "'{sid}' is still running; collect or cancel it first"
            )),
            "{error}"
        );
        assert!(!error.contains("different variables"), "{error}");
        assert_eq!(
            read_to_string(root.child_meta_file(&sid)).unwrap(),
            live_meta,
            "a refusal never re-stamps the sidecar"
        );

        write(root.child_meta_file(&sid), &meta_before).unwrap();
        let error = resume_error(
            &mut ctx,
            json!({
                "agent": agent_name,
                "prompt": "again",
                "session_id": sid,
                "variables": {"tone": "quiet"},
            }),
        );
        assert!(error.contains("different variables (tone)"), "{error}");
        assert_eq!(
            read_to_string(root.child_meta_file(&sid)).unwrap(),
            meta_before
        );
    }

    /// The recent-ids hint caps at ten by creation time, not by the order
    /// the sidecars landed on disk: exactly ten ids list in full with no
    /// `+N more`, the eleventh (oldest-created) one falls off the end and
    /// is counted, and re-stamping an old child running does not move it.
    #[test]
    #[serial]
    fn usage_probe_the_recent_ids_hint_orders_by_creation_and_caps_at_ten() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_resume_recent_cap");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let root = ctx.ensure_child_session_root().unwrap();
        root.ensure_dir(&RootFile::seed(ProcessStamp::current(), None))
            .unwrap();
        let live = ProcessStamp::current();
        let save_at = |root: &ChildSessionRoot, minute: u32| -> String {
            let sid = child_sessions::new_session_id(&agent_name);
            let mut meta = ChildMeta::new_running_at(
                fixture_time(minute),
                &sid,
                &agent_name,
                "agent_x",
                live,
                None,
                None,
                "hi",
                None,
            );
            meta.finish_at(fixture_time(minute), ChildStatus::Completed, None);
            root.save_child_meta(&meta).unwrap();
            sid
        };
        // Land the sidecars in a scrambled order of creation times.
        let minutes = [7u32, 2, 9, 4, 1, 8, 3, 6, 5, 10];
        let mut by_minute: Vec<(u32, String)> = minutes
            .iter()
            .map(|&minute| (minute, save_at(&root, minute)))
            .collect();
        by_minute.sort_by_key(|(minute, _)| std::cmp::Reverse(*minute));
        let newest_first: Vec<&str> = by_minute.iter().map(|(_, sid)| sid.as_str()).collect();
        let unknown = child_sessions::new_session_id(&agent_name);
        let args = json!({"agent": agent_name, "prompt": "again", "session_id": unknown});

        let error = resume_error(&mut ctx, args.clone());
        let expected_ten = format!(
            "no child session '{unknown}' under this session; recent ids: {}",
            newest_first.join(", ")
        );
        assert!(error.contains(&expected_ten), "{error}");
        assert!(
            !error.contains("(+"),
            "exactly ten ids carry no tail: {error}"
        );
        assert!(!error.contains("(crashed)"), "{error}");

        // An eleventh child created before all the others is the one that
        // falls off the end, whatever its position on disk.
        let oldest = save_at(&root, 0);
        let error = resume_error(&mut ctx, args.clone());
        assert!(
            error.contains(&format!("{expected_ten} (+1 more)")),
            "{error}"
        );
        assert!(!error.contains(&oldest), "{error}");

        // Re-stamping the minute-1 child running under a dead pid marks it
        // crashed in place; its creation time still decides its slot.
        let (_, dead_sid) = by_minute
            .iter()
            .find(|(minute, _)| *minute == 1)
            .unwrap()
            .clone();
        let mut meta = root.load_child_meta(&dead_sid).unwrap().unwrap();
        meta.status = ChildStatus::Running;
        meta.pid = u32::MAX - 1;
        meta.pid_start = 0;
        meta.updated_at = fixture_time(60);
        root.save_child_meta(&meta).unwrap();
        let error = resume_error(&mut ctx, args);
        let expected_tail = format!("{dead_sid} (crashed) (+1 more)");
        assert!(error.contains(&expected_tail), "{error}");
        assert!(!error.contains(&oldest), "{error}");
    }

    #[test]
    #[serial]
    fn handle_cancel_marks_the_sidecar_cancelled_and_reports_the_child_ids() {
        let _guard = TestConfigDirGuard::new();
        let _sink = hooks::test_sink::install();
        let agent_name = unique_agent_name("test_sidecar_cancel");
        write_graph_agent_with_hooks(&agent_name, "sidecar_cancel", true);
        let mut ctx = ctx_with_supervisor(4, 3);

        let (spawned, cancelled) = run_async(async {
            let spawned = handle_spawn(&mut ctx, &json!({"agent": agent_name, "prompt": "hi"}))
                .await
                .unwrap();
            assert_eq!(spawned["status"], "ok", "{spawned}");
            let cancelled = handle_cancel(&mut ctx, &json!({"id": spawned["id"]}))
                .await
                .unwrap();
            (spawned, cancelled)
        });

        assert_eq!(cancelled["status"], "ok", "{cancelled}");
        assert_eq!(cancelled["id"], spawned["id"]);
        assert_eq!(cancelled["session_id"], spawned["session_id"]);
        let sid = spawned["session_id"]
            .as_str()
            .expect("a seated child reports its session id");
        let meta = child_meta_of(&ctx, sid);
        assert_eq!(meta.status, ChildStatus::Cancelled);
        assert!(meta.error.is_none(), "{meta:?}");
    }

    /// Cancelling a child that already finished but was never collected
    /// is a reclaim, not a verdict: the sidecar keeps what its exit wrote.
    #[test]
    #[serial]
    fn handle_cancel_of_a_finished_child_keeps_its_own_terminal_status() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_sidecar_cancel_finished");
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);

        let (spawned, cancelled) = run_async(async {
            let spawned = handle_spawn(&mut ctx, &json!({"agent": agent_name, "prompt": "hi"}))
                .await
                .unwrap();
            assert_eq!(spawned["status"], "ok", "{spawned}");
            let id = spawned["id"].as_str().unwrap().to_string();
            let supervisor = ctx.supervisor.clone().unwrap();
            while !supervisor.read().is_finished(&id).unwrap() {
                tokio::task::yield_now().await;
            }
            let cancelled = handle_cancel(&mut ctx, &json!({"id": id})).await.unwrap();
            (spawned, cancelled)
        });

        assert_eq!(cancelled["status"], "ok", "{cancelled}");
        assert_eq!(cancelled["id"], spawned["id"]);
        assert_eq!(cancelled["session_id"], spawned["session_id"]);
        let meta = child_meta_of(&ctx, spawned["session_id"].as_str().unwrap());
        assert_eq!(meta.status, ChildStatus::Completed);
        assert!(meta.error.is_none(), "{meta:?}");
    }

    /// Spawns `agent_name`, latches a user interrupt on the spawner and
    /// collects the child before its spawn task has run, so the collect
    /// loop's interrupt check fires first. The child is then reclaimed so
    /// nothing is left running.
    async fn spawn_and_collect_interrupted(
        ctx: &mut RequestContext,
        agent_name: &str,
    ) -> (Value, Value) {
        let spawned = handle_spawn(ctx, &json!({"agent": agent_name, "prompt": "hi"}))
            .await
            .unwrap();
        assert_eq!(spawned["status"], "ok", "{spawned}");
        let id = spawned["id"].as_str().unwrap().to_string();
        let session = create_abort_signal();
        session.set_ctrlc();
        ctx.session_abort = Some(session);

        let interrupted = handle_collect(ctx, &json!({"id": id})).await.unwrap();

        ctx.session_abort = None;
        let handle = ctx
            .supervisor
            .as_ref()
            .unwrap()
            .write()
            .take(&id)
            .expect("an interrupted collect leaves the child registered");
        handle
            .join_handle
            .await
            .expect("spawn task must not panic")
            .expect("spawn task must return an AgentResult");
        (spawned, interrupted)
    }

    #[test]
    #[serial]
    fn interrupted_collect_carries_the_child_session_id() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_collect_interrupted_sid");
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);

        let (spawned, interrupted) =
            run_async(spawn_and_collect_interrupted(&mut ctx, &agent_name));

        assert_eq!(interrupted["status"], "interrupted", "{interrupted}");
        assert_eq!(interrupted["id"], spawned["id"]);
        assert!(spawned["session_id"].is_string(), "{spawned}");
        assert_eq!(interrupted["session_id"], spawned["session_id"]);
    }

    #[test]
    #[serial]
    fn interrupted_collect_of_an_unseated_child_carries_a_null_session_id_key() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_collect_interrupted_null_sid");
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_child_sessions_off();

        let (spawned, interrupted) =
            run_async(spawn_and_collect_interrupted(&mut ctx, &agent_name));

        assert_eq!(interrupted["status"], "interrupted", "{interrupted}");
        assert_eq!(interrupted["id"], spawned["id"]);
        assert_null_session_id(&interrupted);
    }

    /// The ctrl-c verdict wins over the run's own outcome: the child's run
    /// fails once aborted, but a cancelled child is not a failed one.
    #[test]
    #[serial]
    fn a_ctrl_c_aborted_child_ends_its_sidecar_cancelled() {
        let _guard = TestConfigDirGuard::new();
        let _sink = hooks::test_sink::install();
        let agent_name = unique_agent_name("test_sidecar_ctrlc");
        write_graph_agent_with_hooks(&agent_name, "sidecar_ctrlc", true);
        let mut ctx = ctx_with_supervisor(4, 3);

        let spawned = run_async(spawn_ctrlc_and_reclaim(&mut ctx, &agent_name));

        let meta = child_meta_of(&ctx, spawned["session_id"].as_str().unwrap());
        assert_eq!(meta.status, ChildStatus::Cancelled);
        assert!(meta.error.is_none(), "{meta:?}");
    }

    #[test]
    #[serial]
    fn check_collect_and_list_running_carry_the_child_session_id() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_payload_sid");
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);

        run_async(async {
            let spawned = handle_spawn(&mut ctx, &json!({"agent": agent_name, "prompt": "hi"}))
                .await
                .unwrap();
            assert_eq!(spawned["status"], "ok", "{spawned}");
            let id = spawned["id"].as_str().unwrap().to_string();
            let sid = spawned["session_id"].clone();
            assert!(
                sid.as_str().is_some_and(|sid| sid.starts_with("sess_")),
                "{spawned}"
            );
            let supervisor = ctx.supervisor.clone().unwrap();

            let listed = handle_list_running(&mut ctx).unwrap();
            let rows = listed["agents"].as_array().unwrap();
            assert_eq!(rows.len(), 1, "{listed}");
            assert_eq!(rows[0]["id"], id.as_str());
            assert_eq!(rows[0]["session_id"], sid);

            let pending = handle_check(&mut ctx, &json!({"id": id})).await.unwrap();
            assert_eq!(pending["status"], "pending", "{pending}");
            assert_eq!(pending["session_id"], sid);

            while !supervisor.read().is_finished(&id).unwrap() {
                tokio::task::yield_now().await;
            }
            let finished = handle_check(&mut ctx, &json!({"id": id})).await.unwrap();
            assert_eq!(finished["status"], "finished", "{finished}");
            assert_eq!(finished["session_id"], sid);

            let collected = handle_collect(&mut ctx, &json!({"id": id})).await.unwrap();
            assert_eq!(collected["status"], "completed", "{collected}");
            assert_eq!(collected["session_id"], sid);
        });
    }

    fn assert_null_session_id(payload: &Value) {
        let fields = payload.as_object().expect("payload is an object");
        assert!(fields.contains_key("session_id"), "{payload}");
        assert!(fields["session_id"].is_null(), "{payload}");
    }

    #[test]
    #[serial]
    fn unseated_spawn_payloads_all_carry_a_null_session_id_key() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_payload_null_sid");
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_child_sessions_off();

        run_async(async {
            let args = json!({"agent": agent_name, "prompt": "hi"});
            let first = handle_spawn(&mut ctx, &args).await.unwrap();
            let second = handle_spawn(&mut ctx, &args).await.unwrap();
            assert_null_session_id(&first);
            assert_null_session_id(&second);

            let listed = handle_list_running(&mut ctx).unwrap();
            let rows = listed["agents"].as_array().unwrap();
            assert_eq!(rows.len(), 2, "{listed}");
            rows.iter().for_each(assert_null_session_id);

            let checked = handle_check(&mut ctx, &json!({"id": first["id"]}))
                .await
                .unwrap();
            assert_null_session_id(&checked);

            let collected = handle_collect(&mut ctx, &json!({"id": first["id"]}))
                .await
                .unwrap();
            assert_eq!(collected["status"], "completed", "{collected}");
            assert_null_session_id(&collected);

            let cancelled = handle_cancel(&mut ctx, &json!({"id": second["id"]}))
                .await
                .unwrap();
            assert_eq!(cancelled["status"], "ok", "{cancelled}");
            assert_eq!(cancelled["id"], second["id"]);
            assert_null_session_id(&cancelled);
        });
        assert!(!_guard.cache_root.join("sessions").exists());
    }

    /// The terminal sidecar write is bookkeeping: when it cannot land, the
    /// child's result still reaches the collector untouched.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn a_failed_terminal_sidecar_write_leaves_the_collect_result_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        struct RestoreWritable(PathBuf);
        impl Drop for RestoreWritable {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
        }

        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_sidecar_unwritable");
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);

        let (spawned, collected) = run_async(async {
            let spawned = handle_spawn(&mut ctx, &json!({"agent": agent_name, "prompt": "hi"}))
                .await
                .unwrap();
            assert_eq!(spawned["status"], "ok", "{spawned}");
            let children = ctx.child_session_root.as_ref().unwrap().children_dir();
            std::fs::set_permissions(&children, std::fs::Permissions::from_mode(0o500)).unwrap();
            let _restore = RestoreWritable(children);
            let collected = handle_collect(&mut ctx, &json!({"id": spawned["id"]})).await;
            (spawned, collected.unwrap())
        });

        assert_eq!(collected["status"], "completed", "{collected}");
        assert_eq!(collected["exit_status"], "Completed");
        assert_eq!(collected["session_id"], spawned["session_id"]);
        let meta = child_meta_of(&ctx, spawned["session_id"].as_str().unwrap());
        assert_eq!(
            meta.status,
            ChildStatus::Running,
            "the write could not land"
        );
    }

    #[test]
    #[serial]
    fn auto_dispatched_children_carry_the_queue_task_id_into_the_sidecar() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_dispatch_task_id");
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "Step 1"})).unwrap();
        let created = handle_task_create(
            &mut ctx,
            &json!({"subject": "Step 2", "blocked_by": ["1"], "agent": agent_name, "prompt": "hi"}),
        )
        .unwrap();
        let queue_task_id = created["task_id"].as_str().unwrap().to_string();

        let result = run_async(async {
            let result = handle_task_complete(&mut ctx, &json!({"task_id": "1"}))
                .await
                .unwrap();
            let id = result["auto_dispatched"][0]["id"]
                .as_str()
                .unwrap_or_else(|| panic!("{result}"))
                .to_string();
            let handle = ctx
                .supervisor
                .as_ref()
                .unwrap()
                .write()
                .take(&id)
                .expect("the dispatched agent must be registered");
            handle
                .join_handle
                .await
                .expect("spawn task must not panic")
                .expect("spawn task must return an AgentResult");
            result
        });

        let dispatched = &result["auto_dispatched"][0];
        assert_eq!(dispatched["status"], "ok", "{result}");
        let sid = dispatched["session_id"]
            .as_str()
            .expect("a dispatched child reports its session id");
        assert!(sid.starts_with("sess_"), "{sid}");
        let meta = child_meta_of(&ctx, sid);
        assert_eq!(meta.task_id.as_deref(), Some(queue_task_id.as_str()));
        assert_eq!(meta.status, ChildStatus::Completed);
    }

    /// The capacity pre-check in `build_child_spawn` reads `active_count`
    /// while `register` reads `effective_active_count`, so a natural
    /// registration failure is out of reach; swapping in a zero-capacity
    /// supervisor between build and launch is the seam. The spawn task is
    /// already queued when registration refuses it; the runtime stays
    /// alive afterwards so the test can prove the orphan never gets to
    /// run its own exit (which would overwrite the sidecar and fire the
    /// result hooks), while the refusal itself fires the one `agent.failed`
    /// that closes the `agent.started` bracket.
    #[test]
    #[serial]
    fn launch_child_spawn_marks_the_sidecar_failed_when_registration_fails() {
        let _guard = TestConfigDirGuard::new();
        let _sink = hooks::test_sink::install();
        let agent_name = unique_agent_name("test_register_fails");
        write_graph_agent_with_hooks(&agent_name, "sidecar_register_fails", false);
        let mut ctx = ctx_with_supervisor(4, 3);

        let (sid, error) = run_async(async {
            let args = json!({"agent": agent_name, "prompt": "hi"});
            let spawn = match build_child_spawn(&mut ctx, &args).await.unwrap() {
                SpawnBuild::Ready(spawn) => spawn,
                SpawnBuild::Refused(payload) => panic!("spawn refused: {payload}"),
            };
            let sid = spawn.session_id.clone().unwrap();
            assert_eq!(child_meta_of(&ctx, &sid).status, ChildStatus::Running);
            ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(0, 3))));

            let error = launch_child_spawn(&mut ctx, spawn).unwrap_err();

            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            (sid, error)
        });

        assert!(error.to_string().contains("at capacity"), "{error}");
        let meta = child_meta_of(&ctx, &sid);
        assert_eq!(meta.status, ChildStatus::Failed);
        assert_eq!(meta.error.as_deref(), Some(format!("{error:#}").as_str()));
        assert_eq!(
            hook_captures_named("sidecar_register_fails_started").len(),
            1
        );
        assert!(hook_captures_named("sidecar_register_fails_completed").is_empty());
        let failed = hook_captures_named("sidecar_register_fails_failed");
        assert_eq!(failed.len(), 1, "{failed:?}");
        let hook_error = failed[0]
            .envs
            .get("COYOTE_AGENT_ERROR")
            .expect("a failed registration reports its error");
        assert!(hook_error.contains("at capacity"), "{hook_error}");
        assert_session_envs(&failed[0], &sid);
    }

    #[test]
    #[serial]
    fn finish_child_meta_turns_a_running_sidecar_failed_and_keeps_the_error() {
        let _guard = TestConfigDirGuard::new();
        let mut ctx = ctx_with_supervisor(4, 3);
        let root = ctx.ensure_child_session_root().unwrap();
        let sid = child_sessions::new_session_id("test_finish_direct");
        let meta = ChildMeta::new_running(
            &sid,
            "test_finish_direct",
            "test_finish_direct_1",
            ProcessStamp::current(),
            None,
            None,
            "hi",
            None,
        );
        root.ensure_dir(&RootFile::seed(meta.stamp(), None))
            .unwrap();
        root.save_child_meta(&meta).unwrap();

        finish_child_meta(
            Some(&root),
            Some(&sid),
            ChildStatus::Failed,
            Some("boom".into()),
        );

        let finished: ChildMeta = read_yaml(&root.child_meta_file(&sid));
        assert_eq!(finished.status, ChildStatus::Failed);
        assert_eq!(finished.error.as_deref(), Some("boom"));
        assert_eq!(finished.created_at, meta.created_at);
    }

    /// Spawns a seated child, lets `damage` touch its sidecar before the
    /// spawn task has run, then collects it. The exit write cannot land,
    /// which is logged and must leave the collect result untouched.
    fn collect_after_damaging_the_sidecar(
        prefix: &str,
        damage: impl FnOnce(&Path),
    ) -> (Value, Value) {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name(prefix);
        write_graph_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);

        run_async(async {
            let spawned = handle_spawn(&mut ctx, &json!({"agent": agent_name, "prompt": "hi"}))
                .await
                .unwrap();
            assert_eq!(spawned["status"], "ok", "{spawned}");
            let sid = spawned["session_id"].as_str().unwrap();
            let root = ctx.child_session_root.clone().unwrap();
            damage(&root.child_meta_file(sid));

            let collected = handle_collect(&mut ctx, &json!({"id": spawned["id"]}))
                .await
                .unwrap();
            (spawned, collected)
        })
    }

    fn assert_warned_about_the_sidecar(spawned: &Value) {
        let sid = spawned["session_id"].as_str().unwrap();
        let marker = format!("Failed to record child session {sid} as Completed");
        let warns: Vec<String> = crate::testing::warn_snapshot()
            .into_iter()
            .filter(|m| m.contains(&marker))
            .collect();
        assert_eq!(warns.len(), 1, "{warns:?}");
    }

    #[test]
    #[serial]
    fn a_malformed_sidecar_is_logged_and_the_child_still_collects() {
        crate::testing::install_log_collector();

        let (spawned, collected) =
            collect_after_damaging_the_sidecar("test_sidecar_malformed", |path| {
                write(path, "status: [unclosed").unwrap();
            });

        assert_eq!(collected["status"], "completed", "{collected}");
        assert_eq!(collected["session_id"], spawned["session_id"]);
        assert_warned_about_the_sidecar(&spawned);
    }

    #[test]
    #[serial]
    fn a_missing_sidecar_is_logged_and_the_child_still_collects() {
        crate::testing::install_log_collector();

        let (spawned, collected) =
            collect_after_damaging_the_sidecar("test_sidecar_missing", |path| {
                std::fs::remove_file(path).unwrap();
            });

        assert_eq!(collected["status"], "completed", "{collected}");
        assert_eq!(collected["session_id"], spawned["session_id"]);
        assert_warned_about_the_sidecar(&spawned);
    }

    /// Neither the seat-time flush nor `Input::from_str` can be made to
    /// fail from here without a seam elsewhere, so both arms are pinned by
    /// source scrape: each marks the sidecar failed after it was written
    /// and before handing the error back. The runtime preparation between
    /// the resume arm and the fresh seat is pinned the same way, since a
    /// resume has re-stamped the sidecar running by then.
    #[test]
    fn failures_after_the_sidecar_is_written_mark_it_failed() {
        let source = include_str!("agents.rs").replace("\r\n", "\n");
        let fn_body = |head: &str| {
            let start = source.find(head).unwrap();
            let body = &source[start..];
            let end = body.find("\n}\n").unwrap();
            &source[start..start + end]
        };
        let marker = "finish_child_meta(";
        let failed = "ChildStatus::Failed,";

        let seat = fn_body("\nfn seat_child_session(");
        let saved_at = seat.find("root.save_child_meta(meta)?;").unwrap();
        let flushed_at = seat.find(".flush()").unwrap();
        let marked_at = seat.find(marker).unwrap();
        assert!(saved_at < flushed_at && flushed_at < marked_at);
        assert_eq!(seat.matches(marker).count(), 1);
        assert_eq!(seat.matches(failed).count(), 1);
        assert!(seat[marked_at..].contains("return Err(error);"));

        let build = fn_body("\nasync fn build_child_spawn(");
        let resumed_at = build.find("resume_child_session(").unwrap();
        let prepared_at = build
            .find("prepare_child_runtime(&mut child_ctx, &agent_mcp_servers).await")
            .unwrap();
        let prepare_marked_at = build.find(marker).unwrap();
        let seated_at = build
            .find("seat_child_session(ctx, &mut child_ctx, &root, &meta)?;")
            .unwrap();
        let input_at = build
            .find("Input::from_str(&child_ctx, &prompt, None)")
            .unwrap();
        let input_marked_at = build[input_at..].find(marker).unwrap() + input_at;
        assert!(resumed_at < prepared_at && prepared_at < prepare_marked_at);
        assert!(prepare_marked_at < seated_at && seated_at < input_at);
        assert!(input_at < input_marked_at);
        assert_eq!(build.matches(marker).count(), 2);
        assert_eq!(build.matches(failed).count(), 2);
        assert!(build[prepare_marked_at..seated_at].contains("return Err(error);"));
        assert!(build[input_marked_at..].contains("return Err(error);"));
        assert!(build[prepare_marked_at..seated_at].contains("resumed_session_id.as_deref(),"));
    }

    fn read_yaml_value(path: &Path) -> serde_yaml::Value {
        read_yaml(path)
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The spawn payload names the session slot on every spawn: a caller
    /// reading `session_id` must find the key, holding `null`, when the
    /// spawned agent's knob turned seating off.
    #[test]
    #[serial]
    fn usage_probe_unseated_spawn_payload_still_carries_a_null_session_id_key() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_probe_null_key");
        write_config_agent_with_setting(&agent_name, "child_sessions: false");
        let mut ctx = ctx_with_supervisor(4, 3);

        let payload = run_async(spawn_and_reclaim(&mut ctx, &agent_name));

        let fields = payload.as_object().expect("payload is an object");
        assert!(fields.contains_key("session_id"), "{payload}");
        assert!(fields["session_id"].is_null(), "{payload}");
        assert!(!_guard.cache_root.join("sessions").exists());
    }

    /// A dry run persists nothing: no uid is minted on the spawner's
    /// session, no root is resolved, and the child runs sessionless even
    /// though both knobs are on.
    #[test]
    #[serial]
    fn usage_probe_dry_run_spawner_mints_no_root_and_seats_no_session() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_probe_dry_run");
        write_config_only_agent(&agent_name);
        let app = AppState {
            config: Arc::new(AppConfig {
                dry_run: true,
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));
        let app_config = Arc::clone(&ctx.app.config);
        ctx.session =
            Some(Session::new_from_ctx(&ctx, app_config.as_ref(), "dry-spawner").unwrap());

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        assert!(spawn.session_id.is_none());
        assert!(spawn.child_ctx.session.is_none());
        assert!(spawn.child_ctx.child_session_root.is_none());
        assert!(ctx.child_session_root.is_none());
        assert!(ctx.session.as_ref().unwrap().uid().is_none());
        assert!(!_guard.cache_root.join("sessions").exists());
    }

    /// The seated session carries the model the agent's own config pins,
    /// not the one the spawner was using: `current_model()` reads the
    /// session first once one is attached, so a session seated on the
    /// spawner's model would run a pinned child on the wrong model.
    #[test]
    #[serial]
    fn usage_probe_seated_child_session_keeps_the_agent_pinned_model() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_probe_model");
        write_config_agent_with_setting(&agent_name, "model: test-seeded:agent-own");
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.model = Model::new("test-seeded", "parent-pick");
        assert_eq!(ctx.current_model().id(), "test-seeded:parent-pick");

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let agent_model = spawn.child_ctx.agent.as_ref().unwrap().model().id();
        assert_eq!(agent_model, "test-seeded:agent-own");
        let session = spawn.child_ctx.session.as_ref().unwrap();
        assert_eq!(session.model().id(), "test-seeded:agent-own");
        assert_eq!(
            spawn.child_ctx.current_model().id(),
            "test-seeded:agent-own"
        );
        let root = spawn.child_ctx.child_session_root.as_ref().unwrap();
        let on_disk = read_yaml_value(&root.child_session_file(spawn.session_id.as_ref().unwrap()));
        assert_eq!(on_disk["model"].as_str(), Some("test-seeded:agent-own"));
    }

    /// Without a `model:` of its own the agent resolves to the spawner's
    /// current model, and the seated session carries that one.
    #[test]
    #[serial]
    fn seated_child_session_without_a_pinned_model_runs_on_the_spawner_current_model() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_fallback_model");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.model = Model::new("test-seeded", "parent-pick");

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let agent_model = spawn.child_ctx.agent.as_ref().unwrap().model().id();
        assert_eq!(agent_model, "test-seeded:parent-pick");
        let session = spawn.child_ctx.session.as_ref().unwrap();
        assert_eq!(session.model().id(), "test-seeded:parent-pick");
        assert_eq!(
            spawn.child_ctx.current_model().id(),
            "test-seeded:parent-pick"
        );
        let root = spawn.child_ctx.child_session_root.as_ref().unwrap();
        let on_disk = read_yaml_value(&root.child_session_file(spawn.session_id.as_ref().unwrap()));
        assert_eq!(on_disk["model"].as_str(), Some("test-seeded:parent-pick"));
    }

    /// With `save_session: true` already in the app config nothing in the
    /// seating dirties the session, so the seat-time checkpoint has to force
    /// the write for the file to exist before the child's first turn.
    #[test]
    #[serial]
    fn seating_writes_the_session_file_when_the_app_config_already_saves_sessions() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_seat_app_saves");
        write_config_only_agent(&agent_name);
        let app = AppState {
            config: Arc::new(AppConfig {
                save_session: Some(true),
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let sid = spawn.session_id.as_ref().unwrap();
        let root = spawn.child_ctx.child_session_root.as_ref().unwrap();
        let path = root.child_session_file(sid);
        assert!(path.is_file(), "{}", path.display());
        let on_disk: Session = read_yaml(&path);
        assert!(on_disk.messages().is_empty(), "{:?}", on_disk.messages());
    }

    /// `spawner_session_id` names the spawner's own child session and
    /// nothing else: a child spawner holding some other session still
    /// records itself as the spawning agent but leaves the session empty.
    #[test]
    #[serial]
    fn a_child_spawner_on_a_non_child_session_records_no_spawner_session_id() {
        let _guard = TestConfigDirGuard::new();
        let child_agent = unique_agent_name("test_seat_swapped_spawner");
        let grandchild_agent = unique_agent_name("test_seat_swapped_grandchild");
        write_config_only_agent(&child_agent);
        write_config_only_agent(&grandchild_agent);
        let mut ctx = ctx_with_supervisor(4, 3);
        let child = build_ready_spawn(&mut ctx, json!({"agent": child_agent, "prompt": "hi"}));
        let ChildSpawn {
            mut child_ctx,
            agent_id: child_agent_id,
            ..
        } = *child;
        child_ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));
        let app = Arc::clone(&child_ctx.app.config);
        child_ctx.session = Some(Session::new_from_ctx(&child_ctx, app.as_ref(), "temp").unwrap());

        let grandchild = build_ready_spawn(
            &mut child_ctx,
            json!({"agent": grandchild_agent, "prompt": "hi"}),
        );

        let root = grandchild.child_ctx.child_session_root.as_ref().unwrap();
        let sid = grandchild.session_id.as_ref().unwrap();
        let meta: ChildMeta = read_yaml(&root.child_meta_file(sid));
        assert_eq!(meta.spawner_session_id, None);
        assert_eq!(meta.spawner_agent_id, Some(child_agent_id));
    }

    /// Two children of one saved root session share the uid directory: the
    /// second seating neither disturbs the first child's files nor loses the
    /// parent path, every sidecar names the spawning process the root file
    /// names, and everything under the cache dir stays owner-only.
    #[test]
    #[serial]
    fn usage_probe_two_children_of_one_root_share_the_dir_and_keep_the_parent_path() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_probe_two_children");
        write_config_only_agent(&agent_name);
        let mut ctx = spawner_with_session("root-twice");
        let saved = ctx.session_file_for("root-twice", SessionScope::Global);
        create_dir_all(saved.parent().unwrap()).unwrap();
        write(&saved, "model: test-seeded:test-chat\nmessages: []\n").unwrap();
        ctx.session.as_mut().unwrap().set_path(saved.clone());

        let first = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));
        let first_sid = first.session_id.clone().unwrap();
        let first_root = first.child_ctx.child_session_root.clone().unwrap();
        let first_session_bytes = std::fs::read(first_root.child_session_file(&first_sid)).unwrap();
        let second = build_ready_spawn(
            &mut ctx,
            json!({"agent": agent_name, "prompt": "hi", "task_id": "ticket-7"}),
        );
        let second_sid = second.session_id.clone().unwrap();
        let second_root = second.child_ctx.child_session_root.clone().unwrap();

        assert_ne!(first_sid, second_sid);
        assert_eq!(first_root.dir(), second_root.dir());
        assert!(Arc::ptr_eq(&first_root, &second_root));
        let uid = ctx.session.as_ref().unwrap().uid().unwrap();
        assert_eq!(*second_root, ChildSessionRoot::Uid(uid));
        assert_eq!(
            std::fs::read(first_root.child_session_file(&first_sid)).unwrap(),
            first_session_bytes,
            "the second seating must not touch the first child's session file"
        );
        for sid in [&first_sid, &second_sid] {
            assert!(second_root.child_session_file(sid).is_file(), "{sid}");
            assert!(second_root.child_meta_file(sid).is_file(), "{sid}");
        }
        let root_file: RootFile = read_yaml(&second_root.root_file());
        assert_eq!(
            root_file.parent_paths,
            vec![std::fs::canonicalize(&saved).unwrap()]
        );
        let first_meta: ChildMeta = read_yaml(&second_root.child_meta_file(&first_sid));
        let second_meta: ChildMeta = read_yaml(&second_root.child_meta_file(&second_sid));
        for meta in [&first_meta, &second_meta] {
            assert_eq!(meta.agent, agent_name);
            assert_eq!(meta.status, ChildStatus::Running);
            assert_eq!(meta.prompt_head, "hi", "a short prompt is kept whole");
            assert_eq!(meta.pid, root_file.last_pid);
            assert_eq!(meta.pid_start, root_file.last_pid_start);
            assert_eq!(meta.spawner_session_id, None);
            assert_eq!(meta.spawner_agent_id, None);
        }
        assert_eq!(first_meta.task_id, None);
        assert_eq!(second_meta.task_id.as_deref(), Some("ticket-7"));
        assert_eq!(first_meta.agent_id, first.agent_id);
        assert_eq!(second_meta.agent_id, second.agent_id);
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&second_root.dir()), 0o700);
            assert_eq!(mode_of(&second_root.children_dir()), 0o700);
            assert_eq!(mode_of(&second_root.root_file()), 0o600);
            for sid in [&first_sid, &second_sid] {
                assert_eq!(mode_of(&second_root.child_meta_file(sid)), 0o600, "{sid}");
                assert_eq!(
                    mode_of(&second_root.child_session_file(sid)),
                    0o600,
                    "{sid}"
                );
            }
        }
    }

    /// A child whose own knob is off still carries the spawner's root, so a
    /// grandchild it spawns is seated under the same directory; with no
    /// session of its own the child is named as spawner by agent id only.
    #[test]
    #[serial]
    fn usage_probe_unseated_child_still_hands_the_root_to_its_grandchild() {
        let _guard = TestConfigDirGuard::new();
        let child_agent = unique_agent_name("test_probe_unseated_child");
        let grandchild_agent = unique_agent_name("test_probe_seated_grandchild");
        write_config_agent_with_setting(&child_agent, "child_sessions: false");
        write_config_only_agent(&grandchild_agent);
        let mut ctx = ctx_with_supervisor(4, 3);

        let child = build_ready_spawn(&mut ctx, json!({"agent": child_agent, "prompt": "hi"}));
        assert!(child.session_id.is_none());
        assert!(child.child_ctx.session.is_none());
        let child_root = child
            .child_ctx
            .child_session_root
            .clone()
            .expect("the root rides along regardless of the child's own knob");
        assert!(
            !child_root.dir().exists(),
            "an unseated child creates nothing under the cache dir"
        );
        let ChildSpawn {
            mut child_ctx,
            agent_id: child_agent_id,
            ..
        } = *child;
        child_ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));

        let grandchild = build_ready_spawn(
            &mut child_ctx,
            json!({"agent": grandchild_agent, "prompt": "hi"}),
        );

        let sid = grandchild
            .session_id
            .clone()
            .expect("the grandchild's own knob is on");
        let grandchild_root = grandchild.child_ctx.child_session_root.clone().unwrap();
        assert_eq!(grandchild_root.dir(), child_root.dir());
        assert!(grandchild_root.child_session_file(&sid).is_file());
        let meta: ChildMeta = read_yaml(&grandchild_root.child_meta_file(&sid));
        assert_eq!(meta.spawner_session_id, None);
        assert_eq!(meta.spawner_agent_id, Some(child_agent_id));
    }

    /// A file sitting where `children/` must go makes the seating fail
    /// closed: the builder returns an error naming the path instead of
    /// panicking, and the spawner is left usable with its root resolved.
    #[test]
    #[serial]
    fn usage_probe_a_file_in_the_way_of_the_children_dir_is_an_error_not_a_panic() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_probe_children_blocked");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let expected_root = ChildSessionRoot::from_stamp(ProcessStamp::current());
        let blocker = expected_root.children_dir();
        create_dir_all(blocker.parent().unwrap()).unwrap();
        write(&blocker, "not a directory").unwrap();

        let error = run_async(build_child_spawn(
            &mut ctx,
            &json!({"agent": agent_name, "prompt": "hi"}),
        ))
        .err()
        .expect("a blocked children dir fails the spawn");

        let chain = format!("{error:#}");
        assert!(chain.contains(&blocker.display().to_string()), "{chain}");
        assert!(chain.is_ascii(), "{chain}");
        assert_eq!(
            ctx.child_session_root.as_deref(),
            Some(&expected_root),
            "the root was resolved before the seating failed"
        );
        assert!(blocker.is_file(), "the blocker is left as found");
        assert!(expected_root.root_file().is_file());
        assert!(
            files_named(&_guard.cache_root, "root.yaml").len() == 1,
            "{:?}",
            files_named(&_guard.cache_root, "root.yaml")
        );
        // The spawner still answers once the way is clear.
        std::fs::remove_file(&blocker).unwrap();
        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));
        assert!(spawn.session_id.is_some());
        assert!(
            expected_root
                .child_session_file(spawn.session_id.as_ref().unwrap())
                .is_file()
        );
    }

    /// Only a child spawner names its session in the sidecar: a root
    /// spawner whose named session merely looks like a child session id
    /// still records no `spawner_session_id`, since the root's session is
    /// never a child session.
    #[test]
    #[serial]
    fn usage_probe_a_root_spawner_named_like_a_child_session_records_no_spawner_session_id() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_probe_root_lookalike");
        write_config_only_agent(&agent_name);
        let lookalike = child_sessions::new_session_id("rooter");
        assert!(child_sessions::parse_session_id(&lookalike).is_some());
        let mut ctx = spawner_with_session(&lookalike);
        assert!(ctx.self_agent_id.is_none(), "a REPL root is not a child");

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let root = spawn.child_ctx.child_session_root.as_ref().unwrap();
        let sid = spawn.session_id.as_ref().unwrap();
        let meta: ChildMeta = read_yaml(&root.child_meta_file(sid));
        assert_eq!(meta.spawner_agent_id, None);
        assert_eq!(meta.spawner_session_id, None, "{meta:?}");
    }

    /// The seating turns saving on for the child whatever the app config
    /// says: with `save_session: false` app-wide the child's session still
    /// reports `Some(true)`, exists before the first turn, and records that
    /// turn; a multibyte prompt reaches the sidecar cut by characters.
    #[test]
    #[serial]
    fn usage_probe_seating_saves_the_child_even_when_the_app_config_turns_saving_off() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_probe_app_saves_off");
        write_config_only_agent(&agent_name);
        let app = AppState {
            config: Arc::new(AppConfig {
                save_session: Some(false),
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));
        let prompt = format!("{}{}", "a".repeat(195), "\u{e9}".repeat(10));
        assert_eq!(prompt.chars().count(), 205);

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": prompt}));

        let ChildSpawn {
            mut child_ctx,
            input,
            session_id,
            ..
        } = *spawn;
        let sid = session_id.expect("the app knob is on, only saving is off");
        let root = child_ctx.child_session_root.clone().unwrap();
        let path = root.child_session_file(&sid);
        assert_eq!(
            child_ctx.session.as_ref().unwrap().save_session(),
            Some(true)
        );
        assert!(path.is_file(), "{}", path.display());
        let meta: ChildMeta = read_yaml(&root.child_meta_file(&sid));
        assert_eq!(meta.prompt_head.chars().count(), 200);
        assert!(meta.prompt_head.ends_with("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"));
        assert!(prompt.starts_with(&meta.prompt_head));

        let app = Arc::clone(&child_ctx.app.config);
        child_ctx
            .after_chat_completion(app.as_ref(), &input, "child reply", &[])
            .unwrap();

        let on_disk: Session = read_yaml(&path);
        let turn: Vec<_> = on_disk
            .messages()
            .iter()
            .filter(|message| !message.role.is_system())
            .collect();
        assert_eq!(turn.len(), 2, "{turn:?}");
        assert!(matches!(&turn[0].content, MessageContent::Text(text) if *text == prompt));
    }

    /// A root spawner on the temporary session records no parent path even
    /// when a file happens to sit where that session would be saved: the
    /// temporary session is not a saved transcript the root can be matched
    /// back to.
    #[test]
    #[serial]
    fn usage_probe_a_root_spawner_on_the_temp_session_records_no_parent_path() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_probe_temp_root");
        write_config_only_agent(&agent_name);
        let mut ctx = spawner_with_session(TEMP_SESSION_NAME);
        let would_be = ctx.session_file_for(TEMP_SESSION_NAME, SessionScope::Global);
        create_dir_all(would_be.parent().unwrap()).unwrap();
        write(&would_be, "model: test-seeded:test-chat\nmessages: []\n").unwrap();

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let root = spawn.child_ctx.child_session_root.as_ref().unwrap();
        let root_file: RootFile = read_yaml(&root.root_file());
        assert!(root_file.parent_paths.is_empty(), "{root_file:?}");
        assert_eq!(root_file.last_saved_at, None);
        assert!(
            root.child_session_file(spawn.session_id.as_ref().unwrap())
                .is_file()
        );
    }

    /// Whatever an earlier process left at `root.yaml` cannot take the
    /// spawner down: unreadable bytes are replaced by a fresh identity, and
    /// a directory in its place is a plain error naming the path.
    #[test]
    #[serial]
    fn usage_probe_a_malformed_root_file_is_replaced_and_a_directory_there_is_an_error() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_probe_root_file_garbage");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);
        let expected_root = ChildSessionRoot::from_stamp(ProcessStamp::current());
        let root_file = expected_root.root_file();
        create_dir_all(expected_root.children_dir()).unwrap();
        write(&root_file, b"\x00\xff: [not: yaml").unwrap();

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let sid = spawn.session_id.clone().unwrap();
        let root = spawn.child_ctx.child_session_root.clone().unwrap();
        assert_eq!(*root, expected_root);
        let parsed: RootFile = read_yaml(&root_file);
        assert_eq!(parsed.last_pid, ProcessStamp::current().pid);
        assert!(parsed.parent_paths.is_empty());
        assert!(root.child_session_file(&sid).is_file());
        assert!(root.child_meta_file(&sid).is_file());

        std::fs::remove_file(&root_file).unwrap();
        create_dir_all(&root_file).unwrap();
        let error = run_async(build_child_spawn(
            &mut ctx,
            &json!({"agent": agent_name, "prompt": "hi"}),
        ))
        .err()
        .expect("a directory where root.yaml goes fails the spawn");
        let chain = format!("{error:#}");
        assert!(chain.contains(&root_file.display().to_string()), "{chain}");
        assert!(chain.is_ascii(), "{chain}");
        assert!(root_file.is_dir(), "the directory is left as found");
        assert!(
            root.child_session_file(&sid).is_file(),
            "the earlier child's file survives the failed seating"
        );
        let leftovers: Vec<_> = std::fs::read_dir(root.dir())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// A hook author watching `agent.*` events sees the child's session id
    /// as `COYOTE_SESSION_ID` on `agent.started` and `agent.completed` once
    /// the child is seated, equal to the `session_id` the spawn payload
    /// reports; with the app knob off the same agent's events carry no
    /// `COYOTE_SESSION_ID` at all, as before seating existed.
    #[test]
    #[serial]
    fn usage_probe_agent_hooks_of_a_seated_child_carry_its_session_id_and_none_when_off() {
        let _guard = TestConfigDirGuard::new();
        let _sink = hooks::test_sink::install();
        let seated_agent = unique_agent_name("test_probe_hook_sid_on");
        write_graph_agent_with_hooks(&seated_agent, "seated_hook_sid_on", false);
        let unseated_agent = unique_agent_name("test_probe_hook_sid_off");
        write_graph_agent_with_hooks(&unseated_agent, "seated_hook_sid_off", false);

        let seated = run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            spawn_and_reclaim(&mut ctx, &seated_agent).await
        });
        let sid = seated["session_id"]
            .as_str()
            .expect("a seated child reports its session id")
            .to_string();
        for marker in ["seated_hook_sid_on_started", "seated_hook_sid_on_completed"] {
            let captures = hook_captures_named(marker);
            assert_eq!(captures.len(), 1, "{marker}: {captures:?}");
            assert_eq!(
                captures[0]
                    .envs
                    .get("COYOTE_SESSION_ID")
                    .map(String::as_str),
                Some(sid.as_str()),
                "{marker}: {:?}",
                captures[0].envs
            );
            assert_eq!(captures[0].envs["COYOTE_AGENT_ID"], seated["id"]);
        }

        let unseated = run_async(async {
            let app = AppState {
                config: Arc::new(AppConfig {
                    child_sessions: Some(false),
                    ..Default::default()
                }),
                ..AppState::test_default()
            };
            let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
            ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));
            spawn_and_reclaim(&mut ctx, &unseated_agent).await
        });
        assert!(unseated["session_id"].is_null(), "{unseated}");
        for marker in [
            "seated_hook_sid_off_started",
            "seated_hook_sid_off_completed",
        ] {
            let captures = hook_captures_named(marker);
            assert_eq!(captures.len(), 1, "{marker}: {captures:?}");
            assert!(
                !captures[0].envs.contains_key("COYOTE_SESSION_ID"),
                "{marker}: {:?}",
                captures[0].envs
            );
        }
    }

    /// A root spawner on a workspace-scoped session is matched back to the
    /// file that scope saves to, not to a same-named global session that
    /// also happens to exist.
    #[test]
    #[serial]
    fn usage_probe_a_workspace_scoped_root_session_is_the_parent_path_not_its_global_namesake() {
        let _guard = TestConfigDirGuard::new();
        let workspace = env::temp_dir().join(format!(
            "coyote-agents-tests-workspace-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        create_dir_all(&workspace).unwrap();
        let _workspace_env =
            crate::testing::EnvVarGuard::set(get_env_name("workspace_config_dir"), &workspace);
        let agent_name = unique_agent_name("test_probe_workspace_parent");
        write_config_only_agent(&agent_name);
        let mut ctx = spawner_with_session("shared-name");
        ctx.session
            .as_mut()
            .unwrap()
            .set_scope(SessionScope::Workspace);
        let in_workspace = ctx.session_file_for("shared-name", SessionScope::Workspace);
        let in_global = ctx.session_file_for("shared-name", SessionScope::Global);
        assert_ne!(in_workspace, in_global);
        assert!(
            in_workspace.starts_with(&workspace),
            "{}",
            in_workspace.display()
        );
        for file in [&in_workspace, &in_global] {
            create_dir_all(file.parent().unwrap()).unwrap();
            write(file, "model: test-seeded:test-chat\nmessages: []\n").unwrap();
        }
        ctx.session.as_mut().unwrap().set_path(in_workspace.clone());

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let root = spawn.child_ctx.child_session_root.clone().unwrap();
        let root_file: RootFile = read_yaml(&root.root_file());
        assert_eq!(
            root_file.parent_paths,
            vec![std::fs::canonicalize(&in_workspace).unwrap()],
            "{root_file:?}"
        );
        assert!(
            root.child_session_file(spawn.session_id.as_ref().unwrap())
                .is_file()
        );
        let _ = remove_dir_all(&workspace);
    }

    /// When the `children/` directory an earlier process left behind cannot
    /// be written, the spawn is refused as a plain error naming the path it
    /// could not write: no child is registered, no `agent.started` fires,
    /// no half-written file or temp file is left behind, and the directory
    /// is left as found.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn usage_probe_an_unwritable_children_dir_refuses_the_spawn_without_a_trace() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = TestConfigDirGuard::new();
        let _sink = hooks::test_sink::install();
        let agent_name = unique_agent_name("test_probe_children_readonly");
        write_graph_agent_with_hooks(&agent_name, "children_ro_hook", false);
        let mut ctx = ctx_with_supervisor(4, 3);
        let root = ChildSessionRoot::from_stamp(ProcessStamp::current());
        let children = root.children_dir();
        create_dir_all(&children).unwrap();
        std::fs::set_permissions(&children, std::fs::Permissions::from_mode(0o500)).unwrap();
        let restore = || {
            let _ = std::fs::set_permissions(&children, std::fs::Permissions::from_mode(0o700));
        };
        if write(children.join("probe"), "").is_ok() {
            // Running with privileges that ignore the mode bits; the
            // scenario cannot be staged here.
            let _ = std::fs::remove_file(children.join("probe"));
            restore();
            return;
        }

        let outcome = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": agent_name, "prompt": "hi"}),
        ));
        let listing: Vec<_> = std::fs::read_dir(&children)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        let mode = mode_of(&children);
        restore();

        let error = outcome.expect_err("an unwritable children dir refuses the spawn");
        let chain = format!("{error:#}");
        assert!(chain.contains(&children.display().to_string()), "{chain}");
        assert!(chain.is_ascii(), "{chain}");
        assert!(listing.is_empty(), "{listing:?}");
        assert_eq!(mode, 0o500, "the directory is left as found");
        assert!(
            root.root_file().is_file(),
            "root.yaml lands before the sidecar"
        );
        assert_eq!(ctx.supervisor.as_ref().unwrap().read().active_count(), 0);
        assert!(hook_captures_named("children_ro_hook_started").is_empty());
        assert!(hook_captures_named("children_ro_hook_completed").is_empty());
        assert!(hook_captures_named("children_ro_hook_failed").is_empty());
    }

    /// The seat-time session file is a real session: read back through the
    /// session loader it carries the spawned agent's name, the child's
    /// session id as its name, saving on, no exchange yet, and is private
    /// to the owner.
    #[test]
    #[serial]
    fn usage_probe_the_seat_time_session_file_loads_as_the_child_session() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_probe_seat_file_loads");
        write_config_only_agent(&agent_name);
        let mut ctx = ctx_with_supervisor(4, 3);

        let spawn = build_ready_spawn(&mut ctx, json!({"agent": agent_name, "prompt": "hi"}));

        let sid = spawn.session_id.clone().unwrap();
        let root = spawn.child_ctx.child_session_root.clone().unwrap();
        let path = root.child_session_file(&sid);
        let on_disk: Session = read_yaml(&path);
        let rendered = spawn
            .child_ctx
            .agent
            .as_ref()
            .unwrap()
            .interpolated_instructions();
        assert!(on_disk.agent_instructions().starts_with("hi"));
        assert_eq!(on_disk.agent_instructions(), rendered);
        assert_eq!(on_disk.save_session(), Some(true));
        assert!(on_disk.messages().is_empty(), "{:?}", on_disk.messages());
        let seated = spawn.child_ctx.session.as_ref().unwrap();
        assert_eq!(seated.name(), sid);
        assert!(!seated.dirty(), "the seat-time write left nothing pending");
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&path), 0o600);
            assert_eq!(mode_of(&root.child_meta_file(&sid)), 0o600);
            assert_eq!(mode_of(&root.children_dir()), 0o700);
            assert_eq!(mode_of(&root.dir()), 0o700);
        }
    }

    #[test]
    fn cancel_on_drop_guard_cancels_when_armed_and_not_when_disarmed() {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let armed_abort = register_running_agent(&mut ctx, "a1", "explore");
            drop(CancelOnDrop(ctx.supervisor.clone()));
            assert!(
                armed_abort.aborted(),
                "an armed guard cancels the supervisor's agents when dropped"
            );

            let mut ctx = ctx_with_supervisor(4, 3);
            let disarmed_abort = register_running_agent(&mut ctx, "a2", "explore");
            let mut guard = CancelOnDrop(ctx.supervisor.clone());
            guard.disarm();
            drop(guard);
            assert!(
                !disarmed_abort.aborted(),
                "a disarmed guard leaves the supervisor's agents running"
            );
        });
    }

    fn auto_continue_ctx() -> RequestContext {
        let app = AppState {
            config: Arc::new(AppConfig {
                auto_continue: true,
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        RequestContext::new(Arc::new(app), WorkingMode::Cmd)
    }

    #[test]
    fn todo_continuation_fires_for_child_with_incomplete_todos() {
        let mut ctx = auto_continue_ctx();
        ctx.init_todo_list("ship feature");
        ctx.add_todo("build the thing");

        let prompt = todo_continuation_prompt(&mut ctx).expect("child should auto-continue");

        assert!(prompt.contains("TODO CONTINUATION"), "{prompt}");
        assert!(prompt.contains("build the thing"), "{prompt}");
        assert_eq!(ctx.auto_continue_count, 1);
    }

    #[test]
    fn todo_continuation_stops_when_done_paused_capped_or_disabled() {
        // All todos complete → run may end.
        let mut ctx = auto_continue_ctx();
        ctx.init_todo_list("ship feature");
        let id = ctx.add_todo("build the thing");
        ctx.mark_todo_done(id);
        assert!(todo_continuation_prompt(&mut ctx).is_none());

        // Defense-in-depth: children cannot call todo__pause, but a set flag
        // must still stop the continuation loop.
        ctx.add_todo("deploy the thing");
        ctx.pause_auto_continue("blocked on credentials");
        assert!(todo_continuation_prompt(&mut ctx).is_none());
        ctx.resume_auto_continue();
        assert!(todo_continuation_prompt(&mut ctx).is_some());

        // Continuation cap reached → run may end.
        ctx.auto_continue_count = ctx.auto_continue_config().max_continues;
        assert!(todo_continuation_prompt(&mut ctx).is_none());

        // Auto-continue disabled → run may end even with incomplete todos.
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.init_todo_list("ship feature");
        ctx.add_todo("build the thing");
        assert!(todo_continuation_prompt(&mut ctx).is_none());
    }

    fn ctx_with_job_capable_supervisor() -> RequestContext {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(
            Supervisor::new(4, 3).with_max_concurrent_jobs(4),
        )));
        ctx
    }

    fn make_fake_job(id: &str) -> JobHandle {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let join_handle = rt.spawn(async {
            Ok(JobResult {
                output: json!(null),
                exit_code: Some(0),
                output_bytes_captured: 0,
            })
        });
        mem::forget(rt);
        JobHandle {
            id: id.to_string(),
            tool: "execute_command".to_string(),
            started_at: std::time::Instant::now(),
            join_handle,
            abort_signal: create_abort_signal(),
            state: Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            })),
            output_buf: Arc::new(Mutex::new(RingBuf::default())),
            no_change_checks: 0,
            last_check_state: None,
        }
    }

    fn register_fake_job(ctx: &mut RequestContext, id: &str) {
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(make_fake_job(id))
            .unwrap();
    }

    fn assert_job_teaching_error(result: &Value, id: &str) {
        assert_eq!(result["status"], "error");
        let message = result["message"].as_str().unwrap();
        assert_eq!(
            message,
            format!(
                "'{id}' is a background job, not an agent — use job__check / job__collect / job__cancel"
            )
        );
    }

    fn register_fake_agent(ctx: &mut RequestContext, id: &str, name: &str) {
        register_fake_agent_with_output(ctx, id, name, "fake output");
    }

    fn register_fake_agent_with_output(
        ctx: &mut RequestContext,
        id: &str,
        name: &str,
        output: &str,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let id_owned = id.to_string();
        let name_owned = name.to_string();
        let output_owned = output.to_string();
        let join_handle = rt.spawn(async move {
            Ok(AgentResult {
                id: id_owned,
                agent_name: name_owned,
                output: output_owned,
                exit_status: AgentExitStatus::Completed,
                session_id: None,
            })
        });
        mem::forget(rt);

        let handle = AgentHandle {
            id: id.to_string(),
            agent_name: name.to_string(),
            depth: 1,
            inbox: Arc::new(Inbox::new()),
            abort_signal: create_abort_signal(),
            join_handle,
            child_supervisor: None,
            session_id: None,
        };
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(handle)
            .unwrap();
    }

    fn run_async<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn register_running_agent(ctx: &mut RequestContext, id: &str, name: &str) -> AbortSignal {
        let abort = create_abort_signal();
        let id_owned = id.to_string();
        let name_owned = name.to_string();
        let join_handle = tokio::spawn(async move {
            time::sleep(Duration::from_secs(60)).await;
            Ok(AgentResult {
                id: id_owned,
                agent_name: name_owned,
                output: String::new(),
                exit_status: AgentExitStatus::Completed,
                session_id: None,
            })
        });
        let handle = AgentHandle {
            id: id.to_string(),
            agent_name: name.to_string(),
            depth: 1,
            inbox: Arc::new(Inbox::new()),
            abort_signal: abort.clone(),
            join_handle,
            child_supervisor: None,
            session_id: None,
        };
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(handle)
            .unwrap();
        abort
    }

    fn wait_until_finished(ctx: &RequestContext, id: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while ctx.supervisor.as_ref().unwrap().read().is_finished(id) != Some(true) {
            assert!(
                std::time::Instant::now() < deadline,
                "agent '{id}' never finished"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn sync_agent_functions_gates_meta_functions_on_live_capabilities() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(AgentConfig::default()));
        let (runtime, _server) = fixture_runtime(FixtureServer {
            tools_capability: false,
            resources_capability: true,
            ..FixtureServer::default()
        })
        .await;
        ctx.tool_scope.mcp_runtime = runtime;

        sync_agent_functions_to_ctx(&mut ctx).unwrap();

        let functions = &ctx.tool_scope.functions;
        assert_eq!(functions.declarations().len(), 3);
        assert!(functions.contains("mcp_search_fixture"));
        assert!(functions.contains("mcp_describe_fixture"));
        assert!(functions.contains("mcp_read_fixture"));
        assert!(!functions.contains("mcp_invoke_fixture"));
    }

    /// `Agent::init` registers the full todo tool set (incl. `todo__pause`)
    /// whenever the agent config enables auto_continue, because the same
    /// function set serves top-level `--agent` sessions. Spawned children
    /// must never advertise `todo__pause` (they are driven to completion
    /// autonomously), so the child-only sync strips that one declaration
    /// while leaving the rest of the todo tools intact.
    #[tokio::test]
    async fn sync_strips_todo_pause_declaration_for_child_contexts() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let mut agent = Agent::test_new(AgentConfig::default());
        agent.functions_mut().append_todo_functions();
        ctx.agent = Some(agent);
        ctx.self_agent_id = Some("agent_sisyphus_abc123".to_string());

        sync_agent_functions_to_ctx(&mut ctx).unwrap();

        let functions = &ctx.tool_scope.functions;
        assert!(!functions.contains("todo__pause"));
        assert!(functions.contains("todo__init"));
        assert!(functions.contains("todo__add"));
        assert!(functions.contains("todo__done"));
        assert!(functions.contains("todo__list"));
        assert!(functions.contains("todo__clear"));
    }

    fn app_state_with_fixture_mcp_config() -> Arc<AppState> {
        let mut state = AppState::test_default();
        state.mcp_config = Some(McpServersConfig {
            mcp_servers: [(
                "fixture".to_string(),
                McpServer {
                    transport_type: McpTransportType::Stdio,
                    command: Some("echo".to_string()),
                    args: None,
                    env: None,
                    cwd: None,
                    url: None,
                    headers: None,
                    oauth: None,
                    allowed_tools: None,
                    initialize_timeout: None,
                },
            )]
            .into_iter()
            .collect(),
        });
        Arc::new(state)
    }

    #[tokio::test]
    async fn spawned_child_runtime_enforces_child_agent_filters() {
        use std::sync::atomic::Ordering;

        let config = AgentConfig {
            mcp_tools: Some(IndexMap::from([(
                "fixture".to_string(),
                vec!["get_*".to_string()],
            )])),
            ..Default::default()
        };
        let mut ctx = RequestContext::new(app_state_with_fixture_mcp_config(), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(config));
        let fixture = FixtureServer::default();
        let call_tool_calls = Arc::clone(&fixture.call_tool_calls);
        let (runtime, _server) = fixture_runtime(fixture).await;
        ctx.tool_scope.mcp_runtime = runtime;

        populate_agent_mcp_runtime(&mut ctx, &[]).await.unwrap();
        ctx.refresh_mcp_tool_filters();

        let err = ctx
            .tool_scope
            .mcp_runtime
            .invoke("fixture", "dup", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "dup not found in fixture MCP server catalog");
        assert_eq!(call_tool_calls.load(Ordering::SeqCst), 0);
    }

    /// Spawning a child whose agent binds an OAuth MCP server the user has
    /// not authenticated must not fail the spawn; the server is skipped the
    /// same way the REPL's agent-load path skips it.
    #[tokio::test]
    #[serial]
    async fn populate_agent_mcp_runtime_skips_auth_required_servers() {
        use crate::config::StubSpawnsGuard;

        let mut ctx = RequestContext::new(app_state_with_fixture_mcp_config(), WorkingMode::Cmd);
        let _stub = StubSpawnsGuard::push(Err(anyhow!("Auth required: no stored token")));

        populate_agent_mcp_runtime(&mut ctx, &["fixture".into()])
            .await
            .unwrap();

        assert!(ctx.tool_scope.mcp_runtime.is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn populate_agent_mcp_runtime_propagates_non_auth_failures() {
        use crate::config::StubSpawnsGuard;

        let mut ctx = RequestContext::new(app_state_with_fixture_mcp_config(), WorkingMode::Cmd);
        let _stub = StubSpawnsGuard::push(Err(anyhow!("boom")));

        let err = populate_agent_mcp_runtime(&mut ctx, &["fixture".into()])
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("boom"), "{err:#}");
        assert!(ctx.tool_scope.mcp_runtime.is_empty());
    }

    /// Two declared servers; the spawn path tries them in declaration order
    /// and the test stub pops LIFO, so outcomes are pushed in reverse.
    fn app_state_with_two_fixture_servers(first: &str, second: &str) -> Arc<AppState> {
        let template = app_state_with_fixture_mcp_config()
            .mcp_config
            .as_ref()
            .unwrap()
            .mcp_servers
            .get("fixture")
            .cloned()
            .unwrap();
        let mut state = AppState::test_default();
        state.mcp_config = Some(McpServersConfig {
            mcp_servers: [
                (first.to_string(), template.clone()),
                (second.to_string(), template),
            ]
            .into_iter()
            .collect(),
        });
        Arc::new(state)
    }

    /// The real-world shape: the librarian declares `github` (OAuth not yet
    /// completed) next to a server that connects fine. The spawn must keep
    /// the healthy one, drop only the one needing auth, and say which.
    #[tokio::test]
    #[serial]
    async fn usage_probe_spawn_keeps_the_healthy_server_when_an_earlier_one_needs_auth() {
        use crate::config::StubSpawnsGuard;
        use crate::testing::{install_log_collector, warn_snapshot};

        install_log_collector();
        let mut ctx = RequestContext::new(
            app_state_with_two_fixture_servers("github", "docs"),
            WorkingMode::Cmd,
        );
        let (mut healthy, _server) = fixture_runtime(FixtureServer::default()).await;
        let handle = healthy.servers.remove("fixture").unwrap();
        // Popped second -> serves "docs".
        let _ok = StubSpawnsGuard::push(Ok(handle));
        // Popped first -> serves "github"; the exact rmcp wording.
        let _auth =
            StubSpawnsGuard::push(Err(anyhow!("Auth required, when send initialize request")));

        populate_agent_mcp_runtime(&mut ctx, &["github".into(), "docs".into()])
            .await
            .expect("an unauthenticated server must not fail the spawn");

        assert!(ctx.tool_scope.mcp_runtime.get("github").is_none());
        assert!(ctx.tool_scope.mcp_runtime.get("docs").is_some());
        let warns: Vec<String> = warn_snapshot()
            .into_iter()
            .filter(|m| m.contains("skipped MCP server"))
            .collect();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("spawned agent skipped MCP server 'github'")
                    && m.contains("Auth required")),
            "{warns:?}"
        );
        assert!(
            warns.iter().all(|m| !m.contains("'docs'")),
            "the healthy server must not be reported as skipped: {warns:?}"
        );
    }

    /// Same pair the other way round: a healthy server already in the
    /// runtime stays there when a LATER declared server needs auth.
    #[tokio::test]
    #[serial]
    async fn usage_probe_spawn_keeps_an_earlier_healthy_server_when_a_later_one_needs_auth() {
        use crate::config::StubSpawnsGuard;

        let mut ctx = RequestContext::new(
            app_state_with_two_fixture_servers("docs", "github"),
            WorkingMode::Cmd,
        );
        let (mut healthy, _server) = fixture_runtime(FixtureServer::default()).await;
        let handle = healthy.servers.remove("fixture").unwrap();
        let _auth = StubSpawnsGuard::push(Err(anyhow!("Auth required: no stored token")));
        let _ok = StubSpawnsGuard::push(Ok(handle));

        populate_agent_mcp_runtime(&mut ctx, &["docs".into(), "github".into()])
            .await
            .unwrap();

        assert!(ctx.tool_scope.mcp_runtime.get("docs").is_some());
        assert!(ctx.tool_scope.mcp_runtime.get("github").is_none());
        assert_eq!(ctx.tool_scope.mcp_runtime.servers.len(), 1);
    }

    /// Skipping an unauthenticated server is not a licence to swallow the
    /// next failure: a non-auth error on a later server still fails the spawn.
    #[tokio::test]
    #[serial]
    async fn usage_probe_spawn_auth_skip_does_not_mask_a_later_non_auth_failure() {
        use crate::config::StubSpawnsGuard;

        let mut ctx = RequestContext::new(
            app_state_with_two_fixture_servers("github", "broken"),
            WorkingMode::Cmd,
        );
        let _boom = StubSpawnsGuard::push(Err(anyhow!("connection refused")));
        let _auth = StubSpawnsGuard::push(Err(anyhow!("Auth required: no stored token")));

        let err = populate_agent_mcp_runtime(&mut ctx, &["github".into(), "broken".into()])
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("connection refused"), "{err:#}");
        assert!(ctx.tool_scope.mcp_runtime.is_empty());
    }

    /// Cold start: a server id the agent declares but mcp.json does not
    /// define is silently absent (pre-existing), and an auth-required server
    /// alongside it still leaves the spawn Ok with an empty runtime.
    #[tokio::test]
    #[serial]
    async fn usage_probe_spawn_with_only_undefined_and_unauthenticated_servers_is_ok_and_empty() {
        use crate::config::StubSpawnsGuard;

        let mut ctx = RequestContext::new(app_state_with_fixture_mcp_config(), WorkingMode::Cmd);
        let _auth = StubSpawnsGuard::push(Err(anyhow!("Auth required: no stored token")));

        populate_agent_mcp_runtime(&mut ctx, &["not-in-mcp-json".into(), "fixture".into()])
            .await
            .unwrap();

        assert!(ctx.tool_scope.mcp_runtime.is_empty());
    }

    #[test]
    fn handle_list_running_empty_supervisor() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_list_running(&mut ctx).unwrap();
        assert_eq!(result["active_count"], 0);
        assert_eq!(result["max_concurrent"], 4);
        assert!(result["agents"].as_array().unwrap().is_empty());
    }

    #[test]
    fn handle_list_running_with_agents() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        register_fake_agent(&mut ctx, "a2", "coder");
        let result = handle_list_running(&mut ctx).unwrap();
        assert_eq!(result["active_count"], 2);
        let agents = result["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn handle_list_running_no_supervisor_errors() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let result = handle_list_running(&mut ctx);
        assert!(result.is_err());
    }

    #[test]
    fn handle_list_running_includes_peers_and_excludes_self() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.self_agent_id = Some("p_self".to_string());
        let registry = Arc::new(PeerRegistry::new());
        registry.insert("p_self".into(), "shards[0]".into(), Arc::new(Inbox::new()));
        registry.insert("p_other".into(), "shards[1]".into(), Arc::new(Inbox::new()));
        ctx.peer_registry = Some(registry);

        let result = handle_list_running(&mut ctx).unwrap();

        assert_eq!(result["active_count"], 0);
        let peers = result["peers"].as_array().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0]["id"], "p_other");
        assert_eq!(peers[0]["label"], "shards[1]");
        assert_eq!(peers[0]["kind"], "peer");
    }

    #[test]
    fn handle_list_running_peer_status_reflects_finished() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.self_agent_id = Some("p1".to_string());
        let registry = Arc::new(PeerRegistry::new());
        registry.insert("p1".into(), "shards[0]".into(), Arc::new(Inbox::new()));
        registry.insert("p2".into(), "shards[1]".into(), Arc::new(Inbox::new()));
        registry.insert("p3".into(), "shards[2]".into(), Arc::new(Inbox::new()));
        registry.mark_finished("p3");
        ctx.peer_registry = Some(registry);

        let result = handle_list_running(&mut ctx).unwrap();

        let peers = result["peers"].as_array().unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0]["id"], "p2");
        assert_eq!(peers[0]["status"], "running");
        assert_eq!(peers[1]["id"], "p3");
        assert_eq!(peers[1]["status"], "finished");
    }

    #[test]
    fn handle_list_running_peers_without_supervisor() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.self_agent_id = Some("p_self".to_string());
        let registry = Arc::new(PeerRegistry::new());
        registry.insert("p_self".into(), "a".into(), Arc::new(Inbox::new()));
        registry.insert("p_other".into(), "b".into(), Arc::new(Inbox::new()));
        ctx.peer_registry = Some(registry);

        let result = handle_list_running(&mut ctx).unwrap();

        assert_eq!(result["peers"].as_array().unwrap().len(), 1);
        assert!(result.get("agents").is_none());
    }

    #[test]
    fn handle_list_running_no_peers_key_without_registry() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_list_running(&mut ctx).unwrap();
        assert!(result.get("peers").is_none());
    }

    #[test]
    fn handle_list_available_returns_shape() {
        let ctx = ctx_with_supervisor(4, 3);

        let result = handle_list_available(&ctx).unwrap();

        assert!(result["count"].is_number());
        assert!(result["agents"].is_array());
    }

    #[test]
    #[serial]
    fn handle_list_available_unrestricted_when_no_whitelist() {
        let ctx = ctx_with_supervisor(4, 3);
        let result = handle_list_available(&ctx).unwrap();

        let full_count = result["count"].as_u64().unwrap();

        assert_eq!(full_count as usize, list_agents_with_descriptions().len());
    }

    #[test]
    fn variables_json_maps_required_and_default() {
        let vars = vec![
            AgentVariable {
                name: "pr".to_string(),
                description: "PR number".to_string(),
                default: None,
                value: String::new(),
            },
            AgentVariable {
                name: "branch".to_string(),
                description: "Target branch".to_string(),
                default: Some("x".to_string()),
                value: String::new(),
            },
        ];

        let result = variables_json(&vars).unwrap();

        assert_eq!(result[0]["name"], "pr");
        assert_eq!(result[0]["description"], "PR number");
        assert_eq!(result[0]["required"], true);
        assert!(result[0].get("default").is_none());
        assert_eq!(result[1]["name"], "branch");
        assert_eq!(result[1]["required"], false);
        assert_eq!(result[1]["default"], "x");
    }

    #[test]
    fn variables_json_empty_slice_is_none() {
        assert!(variables_json(&[]).is_none());
    }

    #[test]
    fn variables_json_never_emits_value() {
        let vars = vec![AgentVariable {
            name: "pr".to_string(),
            description: "PR number".to_string(),
            default: None,
            value: "resolved".to_string(),
        }];

        let result = variables_json(&vars).unwrap();

        assert!(result[0].get("value").is_none());
    }

    #[test]
    fn agent_permitted_none_whitelist_allows_all() {
        assert!(agent_permitted(None, "explore"));
        assert!(agent_permitted(None, "anything"));
    }

    #[test]
    fn agent_permitted_empty_whitelist_denies_all() {
        let empty: Vec<String> = vec![];

        assert!(!agent_permitted(Some(&empty), "explore"));
    }

    #[test]
    fn agent_permitted_named_whitelist_matches_exact() {
        let allowed = vec!["explore".to_string(), "coder".to_string()];

        assert!(agent_permitted(Some(&allowed), "explore"));
        assert!(agent_permitted(Some(&allowed), "coder"));
        assert!(!agent_permitted(Some(&allowed), "oracle"));
        assert!(!agent_permitted(Some(&allowed), "Explore"));
    }

    #[test]
    fn handle_check_unknown_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_check(&mut ctx, &json!({"id": "nonexistent"})));
        let val = result.unwrap();
        assert_eq!(val["status"], "error");
    }

    #[test]
    fn handle_check_pending_agent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let inbox = Arc::new(Inbox::new());
            let abort = create_abort_signal();
            let join_handle = tokio::spawn(async {
                time::sleep(Duration::from_secs(60)).await;
                Ok(AgentResult {
                    id: "slow".into(),
                    agent_name: "test".into(),
                    output: String::new(),
                    exit_status: AgentExitStatus::Completed,
                    session_id: None,
                })
            });
            let handle = AgentHandle {
                id: "slow".into(),
                agent_name: "test".into(),
                depth: 1,
                inbox,
                abort_signal: abort,
                join_handle,
                child_supervisor: None,
                session_id: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            let result = handle_check(&mut ctx, &json!({"id": "slow"}))
                .await
                .unwrap();
            assert_eq!(result["status"], "pending");
        });
    }

    #[test]
    fn handle_cancel_registered_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        let result = run_async(handle_cancel(&mut ctx, &json!({"id": "a1"}))).unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(ctx.supervisor.as_ref().unwrap().read().active_count(), 0);
    }

    #[test]
    fn handle_cancel_unknown_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_cancel(&mut ctx, &json!({"id": "missing"}))).unwrap();
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn handle_cancel_no_supervisor_errors() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let result = run_async(handle_cancel(&mut ctx, &json!({"id": "x"})));
        assert!(result.is_err());
    }

    #[test]
    fn handle_send_message_to_registered_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        let result = handle_send_message(
            &mut ctx,
            &json!({"id": "a1", "message": "hello from parent"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");

        let inbox = ctx
            .supervisor
            .as_ref()
            .unwrap()
            .read()
            .inbox("a1")
            .unwrap()
            .clone();
        let msgs = inbox.drain();
        assert_eq!(msgs.len(), 1);
        match &msgs[0].payload {
            EnvelopePayload::Text { content } => assert_eq!(content, "hello from parent"),
            _ => panic!("expected text payload"),
        }
        // A child can only route replies through the reserved alias, so
        // messages from the spawning agent must be labeled with it.
        assert_eq!(msgs[0].from, PARENT_ALIAS);
    }

    #[test]
    fn handle_send_message_to_unknown_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result =
            handle_send_message(&mut ctx, &json!({"id": "missing", "message": "hi"})).unwrap();
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn handle_send_message_parent_alias_delivers_to_parent_inbox() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let parent_inbox = Arc::new(Inbox::new());
        ctx.parent_inbox = Some(Arc::clone(&parent_inbox));
        ctx.self_agent_id = Some("agent_explore_abc123".to_string());

        let result = handle_send_message(
            &mut ctx,
            &json!({"id": "parent", "message": "hello from child"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");

        let msgs = parent_inbox.drain();
        assert_eq!(msgs.len(), 1);
        // The parent can reply through its own supervisor, so the sender must
        // be the child's routable agent id.
        assert_eq!(msgs[0].from, "agent_explore_abc123");
        match &msgs[0].payload {
            EnvelopePayload::Text { content } => assert_eq!(content, "hello from child"),
            _ => panic!("expected text payload"),
        }
    }

    #[test]
    fn handle_send_message_parent_alias_without_parent_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result =
            handle_send_message(&mut ctx, &json!({"id": "parent", "message": "hi"})).unwrap();
        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("no parent agent")
        );
    }

    #[test]
    fn handle_send_message_to_sibling_labels_sender_with_own_id() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.self_agent_id = Some("agent_explore_sender".to_string());

        let mut sibling_ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut sibling_ctx, "agent_explore_receiver", "explore");
        ctx.parent_supervisor = sibling_ctx.supervisor.clone();

        let result = handle_send_message(
            &mut ctx,
            &json!({"id": "agent_explore_receiver", "message": "hi sibling"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");

        let inbox = ctx
            .parent_supervisor
            .as_ref()
            .unwrap()
            .read()
            .inbox("agent_explore_receiver")
            .unwrap()
            .clone();
        let msgs = inbox.drain();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, "agent_explore_sender");
    }

    #[test]
    fn handle_send_message_resolves_teammate_peer_id() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.self_agent_id = Some("graph_agent_worker_aaaa1111".to_string());
        let peer_inbox = Arc::new(Inbox::new());
        let registry = Arc::new(PeerRegistry::new());
        registry.insert(
            "graph_agent_worker_bbbb2222".into(),
            "shards[1]".into(),
            Arc::clone(&peer_inbox),
        );
        ctx.peer_registry = Some(registry);

        let result = handle_send_message(
            &mut ctx,
            &json!({"id": "graph_agent_worker_bbbb2222", "message": "hi peer"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");

        let msgs = peer_inbox.drain();
        assert_eq!(msgs.len(), 1);
        // The sender's self_agent_id IS its peer id, so the recipient can
        // reply symmetrically through the same registry.
        assert_eq!(msgs[0].from, "graph_agent_worker_aaaa1111");
        match &msgs[0].payload {
            EnvelopePayload::Text { content } => assert_eq!(content, "hi peer"),
            _ => panic!("expected text payload"),
        }
    }

    #[test]
    fn handle_send_message_to_finished_teammate_errors() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.self_agent_id = Some("graph_agent_worker_aaaa1111".to_string());
        let peer_inbox = Arc::new(Inbox::new());
        let registry = Arc::new(PeerRegistry::new());
        registry.insert(
            "graph_agent_worker_bbbb2222".into(),
            "shards[1]".into(),
            Arc::clone(&peer_inbox),
        );
        registry.mark_finished("graph_agent_worker_bbbb2222");
        ctx.peer_registry = Some(registry);

        let result = handle_send_message(
            &mut ctx,
            &json!({"id": "graph_agent_worker_bbbb2222", "message": "hi peer"}),
        )
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("has already finished; message not delivered"),
            "{result}"
        );
        assert!(peer_inbox.drain().is_empty());
    }

    #[test]
    fn handle_send_message_unknown_id_with_peer_registry_errors() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.peer_registry = Some(Arc::new(PeerRegistry::new()));
        let result =
            handle_send_message(&mut ctx, &json!({"id": "missing", "message": "hi"})).unwrap();
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn teammate_roster_excludes_self_and_includes_labels() {
        let registry = PeerRegistry::new();
        registry.insert("p1".into(), "shards[0]".into(), Arc::new(Inbox::new()));
        registry.insert("p2".into(), "shards[1]".into(), Arc::new(Inbox::new()));
        registry.insert("p3".into(), "shards[2]".into(), Arc::new(Inbox::new()));

        let roster = render_teammate_roster("p2", &registry);

        assert!(roster.contains("You are `p2` (shards[1])"), "{roster}");
        assert!(roster.contains("- `p1` — shards[0]"), "{roster}");
        assert!(roster.contains("- `p3` — shards[2]"), "{roster}");
        assert!(!roster.contains("- `p2`"), "{roster}");
    }

    /// A parent mid-way through a teammate fan-out: the next
    /// `run_agent_for_graph` call takes the seat `p1` in a two-peer roster.
    fn parent_seating_a_teammate(app: Arc<AppState>) -> RequestContext {
        let mut ctx = RequestContext::new(app, WorkingMode::Cmd);
        let registry = Arc::new(PeerRegistry::new());
        registry.insert("p1".into(), "shards[0]".into(), Arc::new(Inbox::new()));
        registry.insert("p2".into(), "shards[1]".into(), Arc::new(Inbox::new()));
        ctx.peer_registry = Some(registry);
        ctx.peer_assignment = Some(("p1".into(), Arc::new(Inbox::new())));
        ctx
    }

    /// An app whose model calls echo the request instead of sending it, and
    /// an `openai` client so the model a teammate inherits from its parent
    /// has a client to echo through.
    fn echoing_app_state() -> Arc<AppState> {
        Arc::new(AppState {
            config: Arc::new(AppConfig {
                dry_run: true,
                clients: vec![crate::client::ClientConfig::default()],
                ..AppConfig::default()
            }),
            ..AppState::test_default()
        })
    }

    /// A teammate's roster rides the request as a preamble, so the prompt
    /// the child is built from is the bare spawn prompt: a graph teammate
    /// sees only that prompt as `initial_prompt`.
    #[test]
    #[serial]
    fn a_graph_teammate_gets_the_bare_prompt_as_its_initial_prompt() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_teammate_graph_prompt");
        let agent_dir = paths::agent_data_dir(&agent_name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {agent_name}\ninitial_state:\n  initial_prompt: \"\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: \"<{{{{initial_prompt}}}}>\"\n"
            ),
        )
        .unwrap();
        let mut ctx = parent_seating_a_teammate(default_app_state());

        let output = run_async(run_agent_for_graph(&mut ctx, &agent_name, "hi", None)).unwrap();

        assert_eq!(output, "<hi>");
    }

    /// Under `dry_run` the model call echoes the request it would have sent,
    /// so a config teammate's echoed User message shows the roster ahead of
    /// the prompt; the request is the only place the roster appears.
    #[test]
    #[serial]
    fn a_config_teammate_is_sent_the_roster_as_a_request_preamble() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_teammate_config_roster");
        write_config_agent_with_setting(&agent_name, "agent_session: teammate");
        let mut ctx = parent_seating_a_teammate(echoing_app_state());
        ctx.model = Model::new("openai", "test-chat");

        let output = run_async(run_agent_for_graph(&mut ctx, &agent_name, "hi", None)).unwrap();

        let request: Vec<crate::client::Message> = serde_yaml::from_str(&output).unwrap();
        let user = request
            .iter()
            .rev()
            .find(|message| message.role.is_user())
            .expect("the echoed request carries the user turn");
        let content = user.content.to_text();
        assert!(content.starts_with("## Teammate roster\n"), "{content}");
        assert!(content.contains("You are `p1` (shards[0])"), "{content}");
        assert!(content.contains("`p2`"), "{content}");
        assert!(content.ends_with("\n\nhi"), "{content}");
        assert_eq!(output.matches("## Teammate roster").count(), 1, "{output}");
    }

    #[test]
    #[serial]
    fn a_teammate_without_a_registry_is_sent_no_roster() {
        let _guard = TestConfigDirGuard::new();
        let agent_name = unique_agent_name("test_teammate_no_roster");
        write_config_agent_with_setting(&agent_name, "agent_session: teammate");
        let mut ctx = RequestContext::new(echoing_app_state(), WorkingMode::Cmd);
        ctx.model = Model::new("openai", "test-chat");

        let output = run_async(run_agent_for_graph(&mut ctx, &agent_name, "hi", None)).unwrap();

        assert!(!output.contains("Teammate roster"), "{output}");
        assert!(output.contains("hi"), "{output}");
    }

    #[test]
    fn handle_check_inbox_with_messages() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let inbox = Arc::new(Inbox::new());
        inbox.deliver(Envelope {
            from: "sibling".into(),
            to: "me".into(),
            payload: EnvelopePayload::Text {
                content: "hey".into(),
            },
            timestamp: Utc::now(),
        });
        ctx.inbox = Some(inbox);

        let result = handle_check_inbox(&mut ctx).unwrap();
        assert_eq!(result["count"], 1);
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["from"], "sibling");
    }

    #[test]
    fn handle_check_inbox_no_inbox() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_check_inbox(&mut ctx).unwrap();
        assert_eq!(result["count"], 0);
    }

    #[test]
    fn handle_check_inbox_empty_inbox() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.inbox = Some(Arc::new(Inbox::new()));
        let result = handle_check_inbox(&mut ctx).unwrap();
        assert_eq!(result["count"], 0);
    }

    #[test]
    fn handle_reply_escalation_success() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let queue = Arc::new(EscalationQueue::new());
        let (tx, rx) = tokio::sync::oneshot::channel();
        queue.submit(EscalationRequest {
            id: "esc_1".into(),
            from_agent_id: "a1".into(),
            from_agent_name: "explore".into(),
            question: "What do?".into(),
            options: None,
            reply_tx: tx,
        });
        ctx.escalation_queue = Some(queue);

        let result = handle_reply_escalation(
            &mut ctx,
            &json!({"escalation_id": "esc_1", "reply": "do X"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(rx.blocking_recv().unwrap(), "do X");
    }

    #[test]
    fn handle_reply_escalation_missing_id() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.escalation_queue = Some(Arc::new(EscalationQueue::new()));
        let result = handle_reply_escalation(
            &mut ctx,
            &json!({"escalation_id": "missing", "reply": "whatever"}),
        )
        .unwrap();
        assert_eq!(result["status"], "error");
        assert_eq!(
            result["message"],
            "No pending escalation found with id 'missing'. It may have already been replied to."
        );
    }

    #[test]
    fn handle_reply_escalation_no_queue_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result =
            handle_reply_escalation(&mut ctx, &json!({"escalation_id": "x", "reply": "y"}));
        assert!(result.is_err());
    }

    /// `escalation.answered` belongs to the successful take -> reply arm
    /// alone: exactly once per escalation, and never from the failure arms
    /// (unknown id, already-taken id).
    #[test]
    #[serial]
    fn reply_escalation_fires_answered_once_and_never_on_failure_arms() {
        let _sink = hooks::test_sink::install();
        let marker = "t049_answered_once";
        let mut hooks_map = hooks::HooksMap::default();
        hooks_map.insert(
            "escalation.answered".to_string(),
            vec![hooks::HookDef {
                name: marker.to_string(),
                command: "true".to_string(),
            }],
        );
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            hooks: hooks_map,
            ..Default::default()
        });
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        let queue = Arc::new(EscalationQueue::new());
        let (tx, rx) = tokio::sync::oneshot::channel();
        queue.submit(EscalationRequest {
            id: "esc_once".into(),
            from_agent_id: "a1".into(),
            from_agent_name: "explore".into(),
            question: "[input] What do?".into(),
            options: None,
            reply_tx: tx,
        });
        ctx.escalation_queue = Some(queue);

        // Unknown id: an error result, and no event.
        let miss = handle_reply_escalation(
            &mut ctx,
            &json!({"escalation_id": "missing", "reply": "nope"}),
        )
        .unwrap();
        assert_eq!(miss["status"], "error");
        assert!(
            hook_captures_named(marker).is_empty(),
            "a failed reply must not fire escalation.answered"
        );

        // The successful take -> reply arm fires it exactly once.
        let ok = handle_reply_escalation(
            &mut ctx,
            &json!({"escalation_id": "esc_once", "reply": "do X"}),
        )
        .unwrap();
        assert_eq!(ok["status"], "ok");
        assert_eq!(rx.blocking_recv().unwrap(), "do X");
        let answered = hook_captures_named(marker);
        assert_eq!(answered.len(), 1, "{answered:?}");
        assert_eq!(answered[0].envs["COYOTE_ESCALATION_ID"], "esc_once");
        assert_eq!(answered[0].envs["COYOTE_ESCALATION_REPLY"], "do X");

        // Replying again to the same id hits the already-taken arm: still one.
        let again = handle_reply_escalation(
            &mut ctx,
            &json!({"escalation_id": "esc_once", "reply": "again"}),
        )
        .unwrap();
        assert_eq!(again["status"], "error");
        assert_eq!(
            hook_captures_named(marker).len(),
            1,
            "a second reply must not re-fire escalation.answered"
        );
    }

    #[test]
    fn handle_task_create_simple() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_create(&mut ctx, &json!({"subject": "Do research"})).unwrap();
        assert_eq!(result["status"], "ok");
        assert!(result["task_id"].as_str().is_some());
    }

    #[test]
    fn handle_task_create_with_dependencies() {
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "Step 1"})).unwrap();
        let result =
            handle_task_create(&mut ctx, &json!({"subject": "Step 2", "blocked_by": ["1"]}))
                .unwrap();
        assert_eq!(result["status"], "ok");
    }

    #[test]
    fn handle_task_create_with_dispatch_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_create(
            &mut ctx,
            &json!({"subject": "Auto task", "agent": "coder", "prompt": "do it"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["auto_dispatch"], true);
    }

    #[test]
    fn handle_task_create_stores_variables() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_create(
            &mut ctx,
            &json!({"subject": "Auto task", "agent": "coder", "prompt": "do it", "variables": {"pr": "1234"}}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");

        let list = handle_task_list(&mut ctx).unwrap();
        let tasks = list["tasks"].as_array().unwrap();
        assert_eq!(tasks[0]["variables"]["pr"], "1234");
    }

    #[test]
    fn handle_task_create_rejects_non_object_variables() {
        let mut ctx = ctx_with_supervisor(4, 3);

        let result =
            handle_task_create(&mut ctx, &json!({"subject": "Bad", "variables": ["pr"]})).unwrap();

        assert_eq!(result["status"], "error");
        assert!(result["message"].as_str().unwrap().contains("JSON object"));
    }

    #[test]
    fn handle_task_create_agent_without_prompt_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_create(&mut ctx, &json!({"subject": "Bad", "agent": "coder"}));
        assert!(result.is_err());
    }

    #[test]
    fn handle_task_list_empty() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_list(&mut ctx).unwrap();
        assert!(result["tasks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn handle_task_list_with_tasks() {
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "A"})).unwrap();
        handle_task_create(&mut ctx, &json!({"subject": "B"})).unwrap();
        let result = handle_task_list(&mut ctx).unwrap();
        assert_eq!(result["tasks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn handle_task_complete_unblocks_dependents() {
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "Step 1"})).unwrap();
        handle_task_create(&mut ctx, &json!({"subject": "Step 2", "blocked_by": ["1"]})).unwrap();

        let result = run_async(handle_task_complete(&mut ctx, &json!({"task_id": "1"}))).unwrap();
        assert_eq!(result["status"], "ok");
        let newly_runnable = result["newly_runnable"].as_array().unwrap();
        assert_eq!(newly_runnable.len(), 1);
        assert_eq!(newly_runnable[0]["id"], "2");
    }

    #[test]
    fn handle_task_fail_marks_failed() {
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "Doomed"})).unwrap();
        let result = handle_task_fail(&mut ctx, &json!({"task_id": "1"})).unwrap();
        assert_eq!(result["status"], "ok");
    }

    #[test]
    fn handle_task_fail_reports_blocked_dependents() {
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "A"})).unwrap();
        handle_task_create(&mut ctx, &json!({"subject": "B", "blocked_by": ["1"]})).unwrap();
        let result = handle_task_fail(&mut ctx, &json!({"task_id": "1"})).unwrap();
        let deps = result["blocked_dependents"].as_array().unwrap();
        assert_eq!(deps.len(), 1);
    }

    #[test]
    fn handle_task_fail_missing_task() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_fail(&mut ctx, &json!({"task_id": "nonexistent"})).unwrap();
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn dispatch_unknown_action_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_agent_tool(&mut ctx, "agent__bogus", &json!({})));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unknown agent action")
        );
    }

    #[test]
    fn dispatch_routes_list_running() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_agent_tool(
            &mut ctx,
            "agent__list_running",
            &json!({}),
        ))
        .unwrap();
        assert!(result["active_count"].is_number());
    }

    #[test]
    fn dispatch_routes_list_available() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_agent_tool(
            &mut ctx,
            "agent__list_available",
            &json!({}),
        ))
        .unwrap();
        assert!(result["count"].is_number());
        assert!(result["agents"].is_array());
    }

    #[test]
    fn dispatch_routes_task_list() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result =
            run_async(handle_agent_tool(&mut ctx, "agent__task_list", &json!({}))).unwrap();
        assert!(result["tasks"].is_array());
    }

    #[test]
    fn new_for_child_inherits_escalation_queue() {
        let mut parent = ctx_with_supervisor(4, 3);
        let queue = parent.ensure_root_escalation_queue();

        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            2,
            Arc::new(Inbox::new()),
            "child_1".into(),
        );

        assert!(child.escalation_queue.is_some());
        assert!(Arc::ptr_eq(
            child.escalation_queue.as_ref().unwrap(),
            &queue
        ));
    }

    #[test]
    fn new_for_child_sets_depth_and_id() {
        let parent = ctx_with_supervisor(4, 3);
        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            3,
            Arc::new(Inbox::new()),
            "child_xyz".into(),
        );
        assert_eq!(child.current_depth, 3);
        assert_eq!(child.self_agent_id, Some("child_xyz".to_string()));
    }

    #[test]
    fn new_for_child_has_inbox() {
        let parent = ctx_with_supervisor(4, 3);
        let inbox = Arc::new(Inbox::new());
        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            1,
            Arc::clone(&inbox),
            "c1".into(),
        );
        assert!(child.inbox.is_some());
        assert!(Arc::ptr_eq(child.inbox.as_ref().unwrap(), &inbox));
    }

    #[test]
    fn new_for_child_inherits_parent_supervisor() {
        let parent = ctx_with_supervisor(4, 3);
        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            1,
            Arc::new(Inbox::new()),
            "c1".into(),
        );
        assert!(child.parent_supervisor.is_some());
        assert!(child.supervisor.is_none());
    }

    #[test]
    fn new_for_child_starts_with_empty_scope() {
        let parent = ctx_with_supervisor(4, 3);
        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            1,
            Arc::new(Inbox::new()),
            "c1".into(),
        );
        assert!(child.tool_scope.functions.is_empty());
        assert!(child.tool_scope.mcp_runtime.is_empty());
        assert!(child.role.is_none());
        assert!(child.session.is_none());
        assert!(child.agent.is_none());
    }

    #[test]
    fn ensure_root_escalation_queue_creates_on_first_call() {
        let mut ctx = ctx_with_supervisor(4, 3);
        assert!(ctx.escalation_queue.is_none());
        let q = ctx.ensure_root_escalation_queue();
        assert!(!q.has_pending());
        assert!(ctx.escalation_queue.is_some());
    }

    #[test]
    fn ensure_root_escalation_queue_returns_same_on_second_call() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let q1 = ctx.ensure_root_escalation_queue();
        let q2 = ctx.ensure_root_escalation_queue();
        assert!(Arc::ptr_eq(&q1, &q2));
    }

    #[test]
    fn guardrail_prompt_mentions_pending_escalations() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let join_handle = tokio::spawn(async {
                time::sleep(Duration::from_secs(60)).await;
                Ok(AgentResult {
                    id: "slow".into(),
                    agent_name: "test".into(),
                    output: String::new(),
                    exit_status: AgentExitStatus::Completed,
                    session_id: None,
                })
            });
            let handle = AgentHandle {
                id: "slow".into(),
                agent_name: "test".into(),
                depth: 1,
                inbox: Arc::new(Inbox::new()),
                abort_signal: create_abort_signal(),
                join_handle,
                child_supervisor: None,
                session_id: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            let queue = ctx.ensure_root_escalation_queue();
            let (tx, _rx) = tokio::sync::oneshot::channel();
            queue.submit(EscalationRequest {
                id: "esc_9".into(),
                from_agent_id: "a1".into(),
                from_agent_name: "explore".into(),
                question: "Which option?".into(),
                options: None,
                reply_tx: tx,
            });

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::Inject(prompt) => {
                    assert!(prompt.contains("agent__reply_escalation"));
                    assert!(prompt.contains("esc_9"));
                }
                _ => panic!("expected Inject action"),
            }
        });
    }

    #[test]
    fn guardrail_prompt_omits_escalations_when_none_pending() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let join_handle = tokio::spawn(async {
                time::sleep(Duration::from_secs(60)).await;
                Ok(AgentResult {
                    id: "slow".into(),
                    agent_name: "test".into(),
                    output: String::new(),
                    exit_status: AgentExitStatus::Completed,
                    session_id: None,
                })
            });
            let handle = AgentHandle {
                id: "slow".into(),
                agent_name: "test".into(),
                depth: 1,
                inbox: Arc::new(Inbox::new()),
                abort_signal: create_abort_signal(),
                join_handle,
                child_supervisor: None,
                session_id: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::Inject(prompt) => {
                    assert!(!prompt.contains("agent__reply_escalation"));
                }
                _ => panic!("expected Inject action"),
            }
        });
    }

    #[test]
    fn handle_collect_finished_agent_returns_output_and_consumes_handle() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        ctx.pending_tasks_guardrail_count = 2;

        let result = run_async(handle_collect(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(result["status"], "completed");
        assert_eq!(result["id"], "a1");
        assert_eq!(result["agent"], "explore");
        assert_eq!(result["exit_status"], "Completed");
        assert_eq!(result["output"], "fake output");
        assert_eq!(ctx.pending_tasks_guardrail_count, 0);
        assert_eq!(
            ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
            None
        );
    }

    #[test]
    fn handle_collect_session_interrupt_returns_and_keeps_agent() {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let join_handle = tokio::spawn(async move {
                let _ = rx.await;
                Ok(AgentResult {
                    id: "a1".to_string(),
                    agent_name: "explore".to_string(),
                    output: "late output".to_string(),
                    exit_status: AgentExitStatus::Completed,
                    session_id: None,
                })
            });
            let handle = AgentHandle {
                id: "a1".to_string(),
                agent_name: "explore".to_string(),
                depth: 1,
                inbox: Arc::new(Inbox::new()),
                abort_signal: create_abort_signal(),
                join_handle,
                child_supervisor: None,
                session_id: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            let session = create_abort_signal();
            session.set_ctrlc();
            ctx.session_abort = Some(session.clone());

            let result = time::timeout(
                Duration::from_secs(2),
                handle_collect(&mut ctx, &json!({"id": "a1"})),
            )
            .await
            .expect("interrupted collect must return promptly")
            .unwrap();

            assert_eq!(result["status"], "interrupted");
            assert_eq!(result["id"], "a1");
            assert!(
                result["message"]
                    .as_str()
                    .unwrap()
                    .contains("still running")
            );
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
                Some(false)
            );

            session.reset();
            tx.send(()).unwrap();

            let collected = handle_collect(&mut ctx, &json!({"id": "a1"}))
                .await
                .unwrap();
            assert_eq!(collected["status"], "completed");
            assert_eq!(collected["output"], "late output");
        });
    }

    /// A spawned child carries no session signal, so its collect has no
    /// interrupt arm to fire. The bare-SIGINT arm cannot be driven
    /// hermetically; this pins the select shape: the collect waits for its
    /// target and returns the result.
    #[test]
    fn handle_collect_in_a_spawned_child_waits_for_its_target() {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            ctx.self_agent_id = Some("c1".to_string());
            ctx.session_abort = None;
            let join_handle = tokio::spawn(async move {
                time::sleep(Duration::from_millis(300)).await;
                Ok(AgentResult {
                    id: "g1".to_string(),
                    agent_name: "explore".to_string(),
                    output: "grandchild output".to_string(),
                    exit_status: AgentExitStatus::Completed,
                    session_id: None,
                })
            });
            let handle = AgentHandle {
                id: "g1".to_string(),
                agent_name: "explore".to_string(),
                depth: 2,
                inbox: Arc::new(Inbox::new()),
                abort_signal: create_abort_signal(),
                join_handle,
                child_supervisor: None,
                session_id: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            let result = time::timeout(
                Duration::from_secs(5),
                handle_collect(&mut ctx, &json!({"id": "g1"})),
            )
            .await
            .expect("the spawned child collect must return once its target finishes")
            .unwrap();

            assert_eq!(result["status"], "completed");
            assert_eq!(result["output"], "grandchild output");
        });
    }

    /// An agent node runs inline in a graph turn and carries the turn's
    /// session signal, so that signal is its interrupt even though it has a
    /// `self_agent_id`.
    #[test]
    fn handle_collect_in_an_agent_node_returns_interrupted_on_the_turn_signal() {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            ctx.self_agent_id = Some("n1".to_string());
            let join_handle = tokio::spawn(async move {
                time::sleep(Duration::from_secs(60)).await;
                Ok(AgentResult {
                    id: "g1".to_string(),
                    agent_name: "explore".to_string(),
                    output: "never collected".to_string(),
                    exit_status: AgentExitStatus::Completed,
                    session_id: None,
                })
            });
            let handle = AgentHandle {
                id: "g1".to_string(),
                agent_name: "explore".to_string(),
                depth: 2,
                inbox: Arc::new(Inbox::new()),
                abort_signal: create_abort_signal(),
                join_handle,
                child_supervisor: None,
                session_id: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            let session = create_abort_signal();
            session.set_ctrlc();
            ctx.session_abort = Some(session);

            let result = time::timeout(
                Duration::from_secs(2),
                handle_collect(&mut ctx, &json!({"id": "g1"})),
            )
            .await
            .expect("the agent node collect must return promptly on the turn's interrupt")
            .unwrap();

            assert_eq!(result["status"], "interrupted");
            assert_eq!(result["id"], "g1");
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("g1"),
                Some(false),
                "the interrupted collect leaves the target registered"
            );
        });
    }

    /// Besides completion and a pending escalation, the target's own cancel
    /// is the only exit a spawned child's collect has. The fixture task ends
    /// on its abort signal the way a real agent loop does; the collect's join
    /// arm has no timeout, so an unresponsive task would block it for its
    /// full life.
    #[test]
    fn handle_collect_in_a_child_returns_on_the_target_cancel() {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            ctx.self_agent_id = Some("c1".to_string());
            ctx.session_abort = None;
            let target = create_abort_signal();
            let task_abort = target.clone();
            let join_handle = tokio::spawn(async move {
                tokio::select! {
                    _ = time::sleep(Duration::from_secs(60)) => {}
                    _ = wait_abort_signal(&task_abort) => {}
                }
                Ok(AgentResult {
                    id: "g1".to_string(),
                    agent_name: "explore".to_string(),
                    output: "stopped early".to_string(),
                    exit_status: AgentExitStatus::Completed,
                    session_id: None,
                })
            });
            let handle = AgentHandle {
                id: "g1".to_string(),
                agent_name: "explore".to_string(),
                depth: 2,
                inbox: Arc::new(Inbox::new()),
                abort_signal: target.clone(),
                join_handle,
                child_supervisor: None,
                session_id: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            let cancel = target.clone();
            tokio::spawn(async move {
                time::sleep(Duration::from_millis(300)).await;
                cancel.set_ctrlc();
            });

            let result = time::timeout(
                Duration::from_secs(5),
                handle_collect(&mut ctx, &json!({"id": "g1"})),
            )
            .await
            .expect("the target cancel must end the child collect within the grace")
            .unwrap();

            assert_eq!(result["status"], "completed");
            assert_eq!(result["output"], "stopped early");
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("g1"),
                None,
                "the collected handle is consumed"
            );
        });
    }

    #[test]
    fn handle_collect_pending_escalations_early_out_keeps_handle() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let _abort = register_running_agent(&mut ctx, "slow", "test");
            let queue = ctx.ensure_root_escalation_queue();
            let (tx, _rx) = tokio::sync::oneshot::channel();
            queue.submit(EscalationRequest {
                id: "esc_1".into(),
                from_agent_id: "a1".into(),
                from_agent_name: "explore".into(),
                question: "What do?".into(),
                options: None,
                reply_tx: tx,
            });

            let result = handle_collect(&mut ctx, &json!({"id": "slow"}))
                .await
                .unwrap();

            assert_eq!(result["status"], "pending");
            assert!(result["pending_escalations"].is_array());
            assert_null_session_id(&result);
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("slow"),
                Some(false)
            );
        });
    }

    #[test]
    fn handle_collect_unknown_agent_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);

        let result = run_async(handle_collect(&mut ctx, &json!({"id": "missing"}))).unwrap();

        assert_eq!(result["status"], "error");
        assert!(result["message"].as_str().unwrap().contains("not found"));
    }

    #[test]
    fn handle_collect_without_agent_passes_long_output_through_verbatim() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let long_output = "x".repeat(10_000);
        register_fake_agent_with_output(&mut ctx, "a1", "explore", &long_output);

        let result = run_async(handle_collect(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(result["output"], long_output);
    }

    #[test]
    fn handle_collect_output_below_agent_threshold_passes_through() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.agent = Some(Agent::test_new(AgentConfig {
            summarization_threshold: 1_000_000,
            ..Default::default()
        }));
        register_fake_agent(&mut ctx, "a1", "explore");

        let result = run_async(handle_collect(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(result["status"], "completed");
        assert_eq!(result["output"], "fake output");
    }

    #[test]
    fn handle_collect_over_threshold_with_unknown_summarization_model_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.agent = Some(Agent::test_new(AgentConfig {
            summarization_threshold: 1,
            summarization_model: Some("nonexistent_client:model".into()),
            ..Default::default()
        }));
        register_fake_agent(&mut ctx, "a1", "explore");

        let err = run_async(handle_collect(&mut ctx, &json!({"id": "a1"}))).unwrap_err();

        assert!(err.to_string().contains("nonexistent_client"));
    }

    #[test]
    fn guardrail_no_supervisor_is_no_action_and_resets_counter() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.pending_tasks_guardrail_count = 2;

        assert!(matches!(
            check_pending_tasks_guardrail(&mut ctx),
            GuardrailAction::NoAction
        ));
        assert_eq!(ctx.pending_tasks_guardrail_count, 0);
    }

    /// A finished-but-uncollected agent counts as pending: the turn-end
    /// guardrail tells the model to collect it instead of letting the result
    /// be silently dropped, and the handle stays registered.
    #[test]
    fn guardrail_surfaces_finished_but_uncollected_agents() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        wait_until_finished(&ctx, "a1");
        ctx.pending_tasks_guardrail_count = 2;

        match check_pending_tasks_guardrail(&mut ctx) {
            GuardrailAction::Inject(prompt) => {
                assert!(prompt.contains("a1"));
                assert!(prompt.contains("agent__collect --id a1"));
                assert!(prompt.contains("Completed but UNCOLLECTED"));
            }
            _ => panic!("expected Inject action"),
        }
        assert_eq!(ctx.pending_tasks_guardrail_count, 3);
        assert_eq!(
            ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
            Some(true)
        );
    }

    #[test]
    fn guardrail_force_terminate_discards_finished_uncollected_handles() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        wait_until_finished(&ctx, "a1");
        ctx.pending_tasks_guardrail_count = PENDING_TASKS_GUARDRAIL_MAX;

        match check_pending_tasks_guardrail(&mut ctx) {
            GuardrailAction::ForceTerminate(ids) => {
                assert_eq!(ids, vec!["a1".to_string()]);
            }
            _ => panic!("expected ForceTerminate action"),
        }
        assert_eq!(ctx.pending_tasks_guardrail_count, 0);
        assert_eq!(
            ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
            None
        );
    }

    #[test]
    fn guardrail_prompt_renders_running_and_finished_sections() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let _abort = register_running_agent(&mut ctx, "slow", "test");
            register_fake_agent(&mut ctx, "a1", "explore");
            wait_until_finished(&ctx, "a1");

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::Inject(prompt) => {
                    assert!(prompt.contains("Still running"));
                    assert!(prompt.contains("slow (agent)"));
                    assert!(prompt.contains("Completed but UNCOLLECTED"));
                    assert!(prompt.contains("agent__collect --id a1"));
                }
                _ => panic!("expected Inject action"),
            }
        });
    }

    #[test]
    fn guardrail_prompt_is_kind_aware_for_jobs() {
        let tasks = vec![
            PendingTask {
                id: "job_1".into(),
                kind: TaskKind::Job,
                finished: false,
            },
            PendingTask {
                id: "job_2".into(),
                kind: TaskKind::Job,
                finished: true,
            },
        ];

        let prompt = build_pending_tasks_guardrail_prompt(&tasks);

        assert!(prompt.contains("job_1 (job)"));
        assert!(prompt.contains("job__cancel"));
        assert!(prompt.contains("job__collect --id job_2"));
    }

    #[test]
    fn pending_tasks_includes_registered_jobs() {
        let mut ctx = ctx_with_job_capable_supervisor();
        register_fake_job(&mut ctx, "job_1");

        let tasks = pending_tasks(&ctx);

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "job_1");
        assert_eq!(tasks[0].kind, TaskKind::Job);
    }

    #[test]
    fn pending_tasks_scopes_jobs_to_node_scope() {
        let mut ctx = ctx_with_job_capable_supervisor();
        register_fake_job(&mut ctx, "job_mine");
        register_fake_job(&mut ctx, "job_other");

        ctx.node_job_scope = Some(vec!["job_mine".to_string()]);
        let tasks = pending_tasks(&ctx);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "job_mine");
        assert_eq!(tasks[0].kind, TaskKind::Job);

        ctx.node_job_scope = None;
        assert_eq!(pending_tasks(&ctx).len(), 2);
    }

    #[test]
    fn guardrail_force_terminates_at_max_and_cancels_agents() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let abort = register_running_agent(&mut ctx, "slow", "test");
            ctx.pending_tasks_guardrail_count = PENDING_TASKS_GUARDRAIL_MAX;

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::ForceTerminate(ids) => {
                    assert_eq!(ids, vec!["slow".to_string()]);
                }
                _ => panic!("expected ForceTerminate action"),
            }
            assert_eq!(ctx.pending_tasks_guardrail_count, 0);
            assert!(abort.aborted());
        });
    }

    #[test]
    fn guardrail_injects_prompt_and_increments_counter_below_max() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let _abort = register_running_agent(&mut ctx, "slow", "test");
            ctx.pending_tasks_guardrail_count = 1;

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::Inject(prompt) => {
                    assert!(prompt.contains("slow"));
                    assert!(prompt.contains("agent__collect"));
                }
                _ => panic!("expected Inject action"),
            }
            assert_eq!(ctx.pending_tasks_guardrail_count, 2);
        });
    }

    #[test]
    fn handle_cancel_resets_guardrail_counter() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        ctx.pending_tasks_guardrail_count = 2;

        let result = run_async(handle_cancel(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(result["status"], "ok");
        assert_eq!(ctx.pending_tasks_guardrail_count, 0);
    }

    #[test]
    fn handle_spawn_missing_agent_arg_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let err = run_async(handle_spawn(&mut ctx, &json!({}))).unwrap_err();
        assert!(err.to_string().contains("'agent' is required"));
    }

    #[test]
    fn handle_spawn_missing_prompt_arg_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let err = run_async(handle_spawn(&mut ctx, &json!({"agent": "explore"}))).unwrap_err();
        assert!(err.to_string().contains("'prompt' is required"));
    }

    #[test]
    fn handle_spawn_rejects_agent_outside_whitelist() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.agent = Some(Agent::test_new(AgentConfig {
            spawnable_agents: Some(vec!["allowed".into()]),
            ..Default::default()
        }));

        let result = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": "notallowed", "prompt": "p"}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("spawnable_agents")
        );
    }

    #[test]
    fn handle_spawn_non_object_variables_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);

        let result = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": "x", "prompt": "p", "variables": "pr=1234"}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(result["message"].as_str().unwrap().contains("JSON object"));
    }

    #[test]
    fn handle_spawn_non_string_variable_value_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);

        let result = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": "x", "prompt": "p", "variables": {"pr": 1234}}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("'variables.pr'")
        );
    }

    #[test]
    fn parse_variables_arg_absent_returns_none() {
        assert_eq!(parse_variables_arg(&json!({})).unwrap(), None);
    }

    #[test]
    fn parse_variables_arg_accepts_object_of_strings() {
        let parsed = parse_variables_arg(&json!({"variables": {"pr": "1234", "repo": "coyote"}}))
            .unwrap()
            .unwrap();

        assert_eq!(parsed.get("pr").map(String::as_str), Some("1234"));
        assert_eq!(parsed.get("repo").map(String::as_str), Some("coyote"));
    }

    #[test]
    fn validate_spawn_variables_unknown_name_lists_declared() {
        let declared = vec![AgentVariable {
            name: "pr".into(),
            description: "PR number".into(),
            ..Default::default()
        }];

        let provided = AgentVariables::from([("typo".to_string(), "x".to_string())]);

        let err = validate_spawn_variables("coder", &declared, &provided).unwrap_err();
        assert!(err.contains("typo"));
        assert!(err.contains("pr (PR number)"));
    }

    #[test]
    fn validate_spawn_variables_none_declared_errors() {
        let provided = AgentVariables::from([("pr".to_string(), "1".to_string())]);

        let err = validate_spawn_variables("coder", &[], &provided).unwrap_err();

        assert!(err.contains("declares no variables"));
    }

    #[test]
    fn validate_spawn_variables_known_names_ok() {
        let declared = vec![AgentVariable {
            name: "pr".into(),
            description: "PR number".into(),
            ..Default::default()
        }];

        let provided = AgentVariables::from([("pr".to_string(), "1".to_string())]);

        assert!(validate_spawn_variables("coder", &declared, &provided).is_ok());
    }

    #[test]
    fn merge_spawn_variables_overrides_inherited_per_key() {
        let mut inherited = Some(AgentVariables::from([
            ("pr".to_string(), "1".to_string()),
            ("repo".to_string(), "coyote".to_string()),
        ]));

        merge_spawn_variables(
            &mut inherited,
            AgentVariables::from([("pr".to_string(), "2".to_string())]),
        );

        let merged = inherited.unwrap();
        assert_eq!(merged.get("pr").map(String::as_str), Some("2"));
        assert_eq!(merged.get("repo").map(String::as_str), Some("coyote"));
    }

    #[test]
    fn merge_spawn_variables_seeds_when_nothing_inherited() {
        let mut inherited = None;

        merge_spawn_variables(
            &mut inherited,
            AgentVariables::from([("pr".to_string(), "2".to_string())]),
        );

        assert_eq!(inherited.unwrap().get("pr").map(String::as_str), Some("2"));
    }

    #[test]
    fn dispatch_spawn_args_includes_variables() {
        let variables = AgentVariables::from([("pr".to_string(), "1234".to_string())]);

        let args = dispatch_spawn_args("coder", "do it", Some(&variables), None);

        assert_eq!(args["agent"], "coder");
        assert_eq!(args["prompt"], "do it");
        assert_eq!(args["variables"]["pr"], "1234");
        assert!(args.get("task_id").is_none());
    }

    #[test]
    fn dispatch_spawn_args_omits_absent_variables() {
        let args = dispatch_spawn_args("coder", "do it", None, None);

        assert!(args.get("variables").is_none());
    }

    #[test]
    fn dispatch_spawn_args_carries_the_queue_task_id() {
        let args = dispatch_spawn_args("coder", "do it", None, Some("7"));

        assert_eq!(args["task_id"], "7");
    }

    #[test]
    fn handle_spawn_at_capacity_errors() {
        let mut ctx = ctx_with_supervisor(1, 3);
        register_fake_agent(&mut ctx, "a1", "explore");

        let result = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": "x", "prompt": "p"}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert_eq!(
            result["message"],
            "At capacity: 1/1 agents running. Wait for one to finish or cancel one."
        );
    }

    #[test]
    fn handle_spawn_exceeding_depth_errors() {
        let mut ctx = ctx_with_supervisor(4, 0);

        let result = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": "x", "prompt": "p"}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("Max agent depth exceeded")
        );
    }

    #[test]
    fn handle_spawn_no_supervisor_errors() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let err = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": "x", "prompt": "p"}),
        ))
        .unwrap_err();
        assert!(err.to_string().contains("No supervisor active"));
    }

    /// Checking a finished agent is a pure status probe: it reports the
    /// agent as finished, points at agent__collect, and leaves the handle
    /// registered so a subsequent collect still returns the result.
    #[test]
    fn handle_check_finished_agent_reports_status_and_keeps_handle() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        wait_until_finished(&ctx, "a1");

        let result = run_async(handle_check(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(result["status"], "finished");
        assert_eq!(result["id"], "a1");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("agent__collect")
        );
        assert_eq!(
            ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
            Some(true)
        );

        let collected = run_async(handle_collect(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(collected["status"], "completed");
        assert_eq!(collected["output"], "fake output");
    }

    #[test]
    fn handle_cancel_running_agent_aborts_and_waits_for_cleanup() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let sig = create_abort_signal();
            let sig2 = sig.clone();
            let join_handle = tokio::spawn(async move {
                loop {
                    if sig2.aborted() {
                        return Ok(AgentResult {
                            id: "a1".into(),
                            agent_name: "explore".into(),
                            output: String::new(),
                            exit_status: AgentExitStatus::Completed,
                            session_id: None,
                        });
                    }
                    time::sleep(Duration::from_millis(10)).await;
                }
            });
            let handle = AgentHandle {
                id: "a1".into(),
                agent_name: "explore".into(),
                depth: 1,
                inbox: Arc::new(Inbox::new()),
                abort_signal: sig.clone(),
                join_handle,
                child_supervisor: None,
                session_id: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();
            ctx.pending_tasks_guardrail_count = 2;

            let result = handle_cancel(&mut ctx, &json!({"id": "a1"})).await.unwrap();

            assert_eq!(result["status"], "ok");
            let message = result["message"].as_str().unwrap();
            assert!(message.contains("Cancelled agent 'explore'"));
            assert!(message.contains("waited for cleanup"));
            assert!(sig.aborted());
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
                None
            );
            assert_eq!(ctx.pending_tasks_guardrail_count, 0);
        });
    }

    #[test]
    fn handle_check_registered_job_id_teaches_job_tools() {
        run_async(async {
            let mut ctx = ctx_with_job_capable_supervisor();
            register_fake_job(&mut ctx, "bg_1");

            let result = handle_check(&mut ctx, &json!({"id": "bg_1"}))
                .await
                .unwrap();

            assert_job_teaching_error(&result, "bg_1");
        });
    }

    #[test]
    fn handle_check_job_prefixed_id_teaches_job_tools() {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);

            let result = handle_check(&mut ctx, &json!({"id": "job_deadbeef"}))
                .await
                .unwrap();

            assert_job_teaching_error(&result, "job_deadbeef");
        });
    }

    #[test]
    fn handle_collect_registered_job_id_teaches_job_tools() {
        run_async(async {
            let mut ctx = ctx_with_job_capable_supervisor();
            register_fake_job(&mut ctx, "bg_1");

            let result = handle_collect(&mut ctx, &json!({"id": "bg_1"}))
                .await
                .unwrap();

            assert_job_teaching_error(&result, "bg_1");
        });
    }

    #[test]
    fn handle_collect_job_prefixed_id_teaches_job_tools() {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);

            let result = handle_collect(&mut ctx, &json!({"id": "job_deadbeef"}))
                .await
                .unwrap();

            assert_job_teaching_error(&result, "job_deadbeef");
        });
    }

    #[test]
    fn handle_cancel_registered_job_id_teaches_job_tools_and_keeps_job() {
        run_async(async {
            let mut ctx = ctx_with_job_capable_supervisor();
            register_fake_job(&mut ctx, "bg_1");

            let result = handle_cancel(&mut ctx, &json!({"id": "bg_1"}))
                .await
                .unwrap();

            assert_job_teaching_error(&result, "bg_1");
            assert!(ctx.supervisor.as_ref().unwrap().read().has_job("bg_1"));
        });
    }

    #[test]
    fn handle_send_message_registered_job_id_teaches_job_tools() {
        run_async(async {
            let mut ctx = ctx_with_job_capable_supervisor();
            register_fake_job(&mut ctx, "bg_1");

            let result =
                handle_send_message(&mut ctx, &json!({"id": "bg_1", "message": "hi"})).unwrap();

            assert_job_teaching_error(&result, "bg_1");
        });
    }

    #[test]
    fn handle_send_message_job_in_parent_supervisor_teaches_job_tools() {
        run_async(async {
            let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
            let mut parent_sup = Supervisor::new(4, 3).with_max_concurrent_jobs(4);
            parent_sup.register(make_fake_job("bg_p")).unwrap();
            ctx.parent_supervisor = Some(Arc::new(RwLock::new(parent_sup)));

            let result =
                handle_send_message(&mut ctx, &json!({"id": "bg_p", "message": "hi"})).unwrap();

            assert_job_teaching_error(&result, "bg_p");
        });
    }

    #[test]
    fn guardrail_burns_bounded_injects_then_force_terminates_running_job() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_job_capable_supervisor();
            let abort = create_abort_signal();
            let join_handle = tokio::spawn(async {
                time::sleep(Duration::from_secs(60)).await;
                Ok(JobResult {
                    output: json!(null),
                    exit_code: Some(0),
                    output_bytes_captured: 0,
                })
            });
            let handle = JobHandle {
                id: "job_1".to_string(),
                tool: "execute_command".to_string(),
                started_at: std::time::Instant::now(),
                join_handle,
                abort_signal: abort.clone(),
                state: Arc::new(Mutex::new(JobState {
                    status: JobStatus::Running,
                    pgid: None,
                })),
                output_buf: Arc::new(Mutex::new(RingBuf::default())),
                no_change_checks: 0,
                last_check_state: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            for expected_count in 1..=PENDING_TASKS_GUARDRAIL_MAX {
                match check_pending_tasks_guardrail(&mut ctx) {
                    GuardrailAction::Inject(prompt) => assert!(prompt.contains("job_1")),
                    _ => panic!("expected Inject below max"),
                }
                assert_eq!(ctx.pending_tasks_guardrail_count, expected_count);
            }

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::ForceTerminate(ids) => {
                    assert_eq!(ids, vec!["job_1".to_string()]);
                }
                _ => panic!("expected ForceTerminate at max"),
            }
            assert_eq!(ctx.pending_tasks_guardrail_count, 0);
            assert!(abort.aborted());
        });
    }

    #[test]
    fn guardrail_force_terminate_discards_finished_uncollected_job() {
        let mut ctx = ctx_with_job_capable_supervisor();
        register_fake_job(&mut ctx, "job_1");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pending_tasks(&ctx).iter().any(|t| t.finished) {
            assert!(
                std::time::Instant::now() < deadline,
                "job 'job_1' never finished"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        ctx.pending_tasks_guardrail_count = PENDING_TASKS_GUARDRAIL_MAX;

        match check_pending_tasks_guardrail(&mut ctx) {
            GuardrailAction::ForceTerminate(ids) => {
                assert_eq!(ids, vec!["job_1".to_string()]);
            }
            _ => panic!("expected ForceTerminate action"),
        }
        assert_eq!(ctx.pending_tasks_guardrail_count, 0);
        assert!(!ctx.supervisor.as_ref().unwrap().read().has_job("job_1"));
    }
}
