//! How a turn reaches the screen: the work task talks to `TuiSink` through
//! the `UiSink` trait and the bus, and every call becomes a `UiMsg` in the
//! UI loop's inbox. A question for the human goes the same way and blocks
//! the asking thread on a std channel until the modal answers.

use crate::client::SseEvent;
use crate::config::AppConfig;
use crate::config::todo::TodoItem;
use crate::events::{Event, EventSink, NoticeKind, Origin, OutputStream};
use crate::hooks::HookEvent;
use crate::prompt::{PromptAction, PromptAnswer, PromptRequest, Prompter};
use crate::turn::UiSink;
use crate::utils::AbortSignal;
use anyhow::Result;
use serde_json::Value;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Everything the UI loop reacts to besides terminal input.
#[derive(Debug)]
pub(crate) enum UiMsg {
    TextDelta(String),
    StreamDone,
    AssistantText(String),
    Notice {
        kind: NoticeKind,
        text: String,
    },
    ToolNarration {
        call_id: String,
        line: String,
    },
    ToolOutput {
        call_id: Option<String>,
        bytes: Vec<u8>,
    },
    Busy(Option<String>),
    TurnDivider,
    ThinkingStarted,
    ThinkingStopped,
    UsageUpdated(String),
    /// The tool a `tool.*` hook named, or `None` once it finished.
    ToolRunning(Option<String>),
    /// The root turn's `tool.started` hook: names the call the narration
    /// line opened and carries its arguments.
    ToolStarted {
        call_id: String,
        name: String,
        args: Option<Value>,
    },
    ToolFinished {
        call_id: String,
        ok: bool,
        error: Option<String>,
        duration_ms: Option<u64>,
    },
    JobStarted {
        job_id: String,
        tool: String,
    },
    JobFinished {
        job_id: String,
        ok: bool,
        error: Option<String>,
    },
    JobCancelled {
        job_id: String,
    },
    AgentStarted {
        agent_id: String,
        name: String,
    },
    AgentFinished {
        agent_id: String,
        outcome: AgentOutcome,
    },
    TodoUpdated(Vec<TodoItem>),
    /// A queued line the model has just been handed; the transcript
    /// records it where it was read.
    QueuedInputSent(String),
    Prompt {
        req: Box<PromptRequest>,
        reply: std::sync::mpsc::Sender<PromptAnswer>,
    },
    /// Work that needs the terminal in its normal modes (a shell
    /// passthrough, an editor on a file): the loop releases the screen,
    /// runs it, and takes the screen back.
    LendTerminal(LendJob),
}

/// The job behind [`UiMsg::LendTerminal`]; its result travels back to
/// the asker inside the closure.
pub(crate) struct LendJob(pub(crate) Box<dyn FnOnce(&super::TtyHandles) + Send>);

impl fmt::Debug for LendJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LendJob")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentOutcome {
    Completed,
    Failed(String),
    Interrupted,
}

/// The UI's sink: forwards every call to the UI loop's inbox. Also a bus
/// subscriber for the events that reach the UI only through the bus.
pub(crate) struct TuiSink {
    tx: UnboundedSender<UiMsg>,
}

impl TuiSink {
    pub(crate) fn new(tx: UnboundedSender<UiMsg>) -> Self {
        Self { tx }
    }

    fn send(&self, msg: UiMsg) {
        // The loop has gone away only during teardown, when nothing is
        // left to draw the message.
        let _ = self.tx.send(msg);
    }
}

impl UiSink for TuiSink {
    fn assistant_text(&self, _app: &AppConfig, text: &str) -> Result<()> {
        self.send(UiMsg::AssistantText(text.to_string()));
        Ok(())
    }

    fn stream<'a>(
        &'a self,
        _app: &'a AppConfig,
        mut rx: UnboundedReceiver<SseEvent>,
        abort: AbortSignal,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            loop {
                if abort.aborted() {
                    break;
                }
                match rx.recv().await {
                    Some(SseEvent::Text(text)) => self.send(UiMsg::TextDelta(text)),
                    Some(SseEvent::Done) | None => break,
                }
            }
            self.send(UiMsg::StreamDone);
            Ok(())
        })
    }

    fn notice(&self, kind: NoticeKind, text: &str) {
        // Emitters paint notices for the line-oriented REPL (red error
        // text, a leading newline for spacing); the cell renders them as
        // plain styled lines.
        self.send(UiMsg::Notice {
            kind,
            text: super::render::plain_text(text)
                .trim_matches('\n')
                .to_string(),
        });
    }

    fn tool_narration(
        &self,
        call_id: &str,
        cmd_name: &str,
        cmd_args: &[String],
        json_data: &Value,
    ) {
        self.send(UiMsg::ToolNarration {
            call_id: call_id.to_string(),
            line: crate::function::format_call_log(cmd_name, cmd_args, json_data),
        });
    }

    fn tool_output(
        &self,
        call_id: Option<&str>,
        _stream: OutputStream,
        depth: usize,
        chunk: &[u8],
    ) {
        // A child agent's tool output stays out of the root transcript, as
        // it stays off the REPL's terminal; its cell would otherwise split
        // the live reply.
        if depth > 0 {
            return;
        }
        self.send(UiMsg::ToolOutput {
            call_id: call_id.map(str::to_string),
            bytes: chunk.to_vec(),
        });
    }

    fn busy(&self, message: Option<&str>) {
        self.send(UiMsg::Busy(message.map(str::to_string)));
    }

    fn turn_divider(&self) {
        self.send(UiMsg::TurnDivider);
    }
}

/// One value of a hook's environment. It is drawn on the status row or a
/// transcript row, and a tool's env is its own text.
fn hook_env(envs: &[(String, String)], key: &str) -> Option<String> {
    envs.iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| super::render::plain_text(value))
}

fn hook_env_or_empty(envs: &[(String, String)], key: &str) -> String {
    hook_env(envs, key).unwrap_or_default()
}

/// Only the root turn's own tools and thinking drive the status row; a
/// child agent's tool or reasoning starting or finishing says nothing
/// about what the root is doing. A detached origin (no agent, depth 0)
/// counts as root: it is what a publisher without a request context has.
fn is_root(origin: &Origin) -> bool {
    origin.agent_id.is_none() && origin.depth == 0
}

impl EventSink for TuiSink {
    fn on_event(&self, event: &Event) {
        match event {
            Event::ThinkingStarted { origin } if is_root(origin) => {
                self.send(UiMsg::ThinkingStarted)
            }
            Event::ThinkingStopped { origin } if is_root(origin) => {
                self.send(UiMsg::ThinkingStopped)
            }
            Event::UsageUpdated { right_prompt } => {
                self.send(UiMsg::UsageUpdated(right_prompt.clone()))
            }
            Event::Hook {
                event: HookEvent::ToolStarted,
                envs,
                payload,
                origin,
            } if is_root(origin) => {
                self.send(UiMsg::ToolRunning(hook_env(envs, "COYOTE_TOOL_NAME")));
                self.send(UiMsg::ToolStarted {
                    call_id: hook_env_or_empty(envs, "COYOTE_TOOL_CALL_ID"),
                    name: hook_env_or_empty(envs, "COYOTE_TOOL_NAME"),
                    args: payload
                        .as_deref()
                        .and_then(|payload| serde_json::from_str(payload).ok()),
                });
            }
            Event::Hook {
                event: event @ (HookEvent::ToolCompleted | HookEvent::ToolFailed),
                envs,
                origin,
                ..
            } if is_root(origin) => {
                self.send(UiMsg::ToolRunning(None));
                self.send(UiMsg::ToolFinished {
                    call_id: hook_env_or_empty(envs, "COYOTE_TOOL_CALL_ID"),
                    ok: *event == HookEvent::ToolCompleted,
                    error: hook_env(envs, "COYOTE_TOOL_ERROR"),
                    duration_ms: hook_env(envs, "COYOTE_TOOL_DURATION_MS")
                        .and_then(|ms| ms.parse().ok()),
                });
            }
            Event::Hook {
                event: HookEvent::JobStarted,
                envs,
                origin,
                ..
            } if is_root(origin) => self.send(UiMsg::JobStarted {
                job_id: hook_env_or_empty(envs, "COYOTE_JOB_ID"),
                tool: hook_env_or_empty(envs, "COYOTE_TOOL_NAME"),
            }),
            // A job's end is published from its own task with a detached
            // origin; the id says which row it belongs to.
            Event::Hook {
                event: event @ (HookEvent::JobCompleted | HookEvent::JobFailed),
                envs,
                ..
            } => self.send(UiMsg::JobFinished {
                job_id: hook_env_or_empty(envs, "COYOTE_JOB_ID"),
                ok: *event == HookEvent::JobCompleted,
                error: hook_env(envs, "COYOTE_JOB_ERROR"),
            }),
            Event::JobCancelled { job_id } => self.send(UiMsg::JobCancelled {
                job_id: job_id.clone(),
            }),
            // `agent.started` is fired with the child's own context, so a
            // direct child of the root turn arrives at depth one.
            Event::Hook {
                event: HookEvent::AgentStarted,
                envs,
                origin,
                ..
            } if origin.depth == 1 => self.send(UiMsg::AgentStarted {
                agent_id: hook_env_or_empty(envs, "COYOTE_AGENT_ID"),
                name: hook_env_or_empty(envs, "COYOTE_AGENT_NAME"),
            }),
            Event::Hook {
                event:
                    event @ (HookEvent::AgentCompleted
                    | HookEvent::AgentFailed
                    | HookEvent::AgentInterrupted),
                envs,
                ..
            } => self.send(UiMsg::AgentFinished {
                agent_id: hook_env_or_empty(envs, "COYOTE_AGENT_ID"),
                outcome: match event {
                    HookEvent::AgentFailed => {
                        AgentOutcome::Failed(hook_env_or_empty(envs, "COYOTE_AGENT_ERROR"))
                    }
                    HookEvent::AgentInterrupted => AgentOutcome::Interrupted,
                    _ => AgentOutcome::Completed,
                },
            }),
            Event::TodoUpdated { items } => self.send(UiMsg::TodoUpdated(items.clone())),
            Event::QueuedInputSent { text } => self.send(UiMsg::QueuedInputSent(text.clone())),
            _ => {}
        }
    }
}

/// What a question gets when nobody is left to answer it. Every yes/no
/// question is answered no, whatever default the asker supplied: a
/// tool's default is the tool's choice, not the human's, and even
/// coyote's own opt-in confirms must not be granted by an absence. The
/// save-on-exit question defaults to no anyway, so a teardown with a
/// dirty session still completes, the way the REPL's default signal
/// disposition would have dropped it. Every other kind is cancelled.
pub(crate) fn unattended_answer(req: &PromptRequest) -> PromptAnswer {
    match req.action {
        PromptAction::Confirm => PromptAnswer::Confirmed(false),
        _ => PromptAnswer::Cancelled,
    }
}

/// Asks the human through the UI loop and blocks until the modal answers.
/// Synchronous by contract: the asking code sits on a work task or inside
/// a blocking tool, never on the UI loop itself.
pub(crate) struct TuiPrompter {
    tx: UnboundedSender<UiMsg>,
    /// Set once the UI is going away without a human to ask: every prompt
    /// gets its [`unattended_answer`] at once.
    auto_cancel: Arc<AtomicBool>,
}

impl TuiPrompter {
    pub(crate) fn new(tx: UnboundedSender<UiMsg>, auto_cancel: Arc<AtomicBool>) -> Self {
        Self { tx, auto_cancel }
    }
}

impl fmt::Debug for TuiPrompter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TuiPrompter")
            .field("auto_cancel", &self.auto_cancel.load(Ordering::SeqCst))
            .finish()
    }
}

impl Prompter for TuiPrompter {
    fn prompt(&self, req: PromptRequest) -> Result<PromptAnswer> {
        if self.auto_cancel.load(Ordering::SeqCst) {
            return Ok(unattended_answer(&req));
        }
        let unattended = unattended_answer(&req);
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let msg = UiMsg::Prompt {
            req: Box::new(req),
            reply: reply_tx,
        };
        if self.tx.send(msg).is_err() {
            return Ok(unattended);
        }
        Ok(reply_rx.recv().unwrap_or(unattended))
    }

    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn envs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn hook(event: HookEvent, pairs: &[(&str, &str)], origin: Origin) -> Event {
        Event::Hook {
            event,
            envs: envs(pairs),
            payload: None,
            origin,
        }
    }

    fn sink() -> (TuiSink, UnboundedReceiver<UiMsg>) {
        let (tx, rx) = unbounded_channel();
        (TuiSink::new(tx), rx)
    }

    fn drain(rx: &mut UnboundedReceiver<UiMsg>) -> Vec<UiMsg> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
        out
    }

    /// The root's tool hooks reach the transcript as a start carrying the
    /// call id and parsed arguments, and an end carrying the outcome;
    /// the status row's message comes first, as before.
    #[test]
    fn root_tool_hooks_become_tool_started_and_finished() {
        let (sink, mut rx) = sink();
        sink.on_event(&Event::Hook {
            event: HookEvent::ToolStarted,
            envs: envs(&[
                ("COYOTE_TOOL_NAME", "job__check"),
                ("COYOTE_TOOL_CALL_ID", "call-7"),
            ]),
            payload: Some(r#"{"id":"job_1a2b"}"#.to_string()),
            origin: Origin::detached(),
        });
        let msgs = drain(&mut rx);
        assert!(matches!(&msgs[0], UiMsg::ToolRunning(Some(name)) if name == "job__check"));
        assert!(matches!(
            &msgs[1],
            UiMsg::ToolStarted { call_id, name, args: Some(args) }
                if call_id == "call-7" && name == "job__check" && args["id"] == "job_1a2b"
        ));
        assert_eq!(msgs.len(), 2);

        sink.on_event(&hook(
            HookEvent::ToolCompleted,
            &[
                ("COYOTE_TOOL_NAME", "job__check"),
                ("COYOTE_TOOL_CALL_ID", "call-7"),
                ("COYOTE_TOOL_DURATION_MS", "1200"),
            ],
            Origin::detached(),
        ));
        sink.on_event(&hook(
            HookEvent::ToolFailed,
            &[
                ("COYOTE_TOOL_CALL_ID", "call-8"),
                ("COYOTE_TOOL_ERROR", "exit 2"),
            ],
            Origin::detached(),
        ));
        let msgs = drain(&mut rx);
        assert!(matches!(&msgs[0], UiMsg::ToolRunning(None)));
        assert!(matches!(
            &msgs[1],
            UiMsg::ToolFinished { call_id, ok: true, error: None, duration_ms: Some(1200) }
                if call_id == "call-7"
        ));
        assert!(matches!(&msgs[2], UiMsg::ToolRunning(None)));
        assert!(matches!(
            &msgs[3],
            UiMsg::ToolFinished { call_id, ok: false, error: Some(error), duration_ms: None }
                if call_id == "call-8" && error == "exit 2"
        ));
        assert_eq!(msgs.len(), 4);

        // A child's tool hooks stay off the root transcript.
        sink.on_event(&hook(
            HookEvent::ToolStarted,
            &[("COYOTE_TOOL_NAME", "fs_read")],
            Origin {
                agent_id: Some("agent_1".to_string()),
                depth: 1,
            },
        ));
        assert!(drain(&mut rx).is_empty());
    }

    /// A job starts under the root but ends from its own task with a
    /// detached origin; both reach the transcript, and so does a cancel.
    #[test]
    fn job_hooks_and_cancellation_reach_the_transcript() {
        let (sink, mut rx) = sink();
        sink.on_event(&hook(
            HookEvent::JobStarted,
            &[
                ("COYOTE_JOB_ID", "job_1a2b"),
                ("COYOTE_TOOL_NAME", "execute_command"),
            ],
            Origin::detached(),
        ));
        sink.on_event(&hook(
            HookEvent::JobStarted,
            &[("COYOTE_JOB_ID", "job_child")],
            Origin {
                agent_id: Some("agent_1".to_string()),
                depth: 1,
            },
        ));
        sink.on_event(&hook(
            HookEvent::JobFailed,
            &[
                ("COYOTE_JOB_ID", "job_1a2b"),
                ("COYOTE_JOB_ERROR", "exit 1"),
            ],
            Origin::detached(),
        ));
        sink.on_event(&hook(
            HookEvent::JobCompleted,
            &[("COYOTE_JOB_ID", "job_3c4d")],
            Origin::detached(),
        ));
        sink.on_event(&Event::JobCancelled {
            job_id: "job_5e6f".to_string(),
        });
        let msgs = drain(&mut rx);
        assert!(matches!(
            &msgs[0],
            UiMsg::JobStarted { job_id, tool } if job_id == "job_1a2b" && tool == "execute_command"
        ));
        assert!(matches!(
            &msgs[1],
            UiMsg::JobFinished { job_id, ok: false, error: Some(error) }
                if job_id == "job_1a2b" && error == "exit 1"
        ));
        assert!(matches!(
            &msgs[2],
            UiMsg::JobFinished { job_id, ok: true, error: None } if job_id == "job_3c4d"
        ));
        assert!(matches!(&msgs[3], UiMsg::JobCancelled { job_id } if job_id == "job_5e6f"));
        assert_eq!(msgs.len(), 4);
    }

    /// `agent.started` fires with the child's context, so the root's
    /// direct children arrive at depth one and deeper ones are dropped;
    /// an agent's end comes detached and is matched by id.
    #[test]
    fn agent_hooks_reach_the_transcript_for_direct_children() {
        let (sink, mut rx) = sink();
        let child = |depth: usize| Origin {
            agent_id: Some("agent_coder_1a2b".to_string()),
            depth,
        };
        let started = &[
            ("COYOTE_AGENT_ID", "agent_coder_1a2b"),
            ("COYOTE_AGENT_NAME", "coder"),
        ];
        sink.on_event(&hook(HookEvent::AgentStarted, started, child(2)));
        sink.on_event(&hook(HookEvent::AgentStarted, started, Origin::detached()));
        assert!(drain(&mut rx).is_empty());
        sink.on_event(&hook(HookEvent::AgentStarted, started, child(1)));
        sink.on_event(&hook(
            HookEvent::AgentFailed,
            &[
                ("COYOTE_AGENT_ID", "agent_coder_1a2b"),
                ("COYOTE_AGENT_ERROR", "boom"),
            ],
            Origin::detached(),
        ));
        sink.on_event(&hook(
            HookEvent::AgentInterrupted,
            &[("COYOTE_AGENT_ID", "agent_b")],
            Origin::detached(),
        ));
        sink.on_event(&hook(
            HookEvent::AgentCompleted,
            &[("COYOTE_AGENT_ID", "agent_c")],
            Origin::detached(),
        ));
        let msgs = drain(&mut rx);
        assert!(matches!(
            &msgs[0],
            UiMsg::AgentStarted { agent_id, name } if agent_id == "agent_coder_1a2b" && name == "coder"
        ));
        assert!(matches!(
            &msgs[1],
            UiMsg::AgentFinished { agent_id, outcome: AgentOutcome::Failed(error) }
                if agent_id == "agent_coder_1a2b" && error == "boom"
        ));
        assert!(matches!(
            &msgs[2],
            UiMsg::AgentFinished { agent_id, outcome: AgentOutcome::Interrupted } if agent_id == "agent_b"
        ));
        assert!(matches!(
            &msgs[3],
            UiMsg::AgentFinished { agent_id, outcome: AgentOutcome::Completed } if agent_id == "agent_c"
        ));
        assert_eq!(msgs.len(), 4);
    }

    #[test]
    fn todo_updates_reach_the_panel() {
        let (sink, mut rx) = sink();
        let items = vec![TodoItem {
            id: 1,
            desc: "write the test".to_string(),
            done: false,
        }];
        sink.on_event(&Event::TodoUpdated {
            items: items.clone(),
        });
        assert!(matches!(rx.try_recv(), Ok(UiMsg::TodoUpdated(got)) if got == items));
    }
}
