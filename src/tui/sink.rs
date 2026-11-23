//! How a turn reaches the screen: the work task talks to `TuiSink` through
//! the `UiSink` trait and the bus, and every call becomes a `UiMsg` in the
//! UI loop's inbox. A question for the human goes the same way and blocks
//! the asking thread on a std channel until the modal answers.

use super::pager::ChildEntry;
use super::run_now::CommandOutput;
use crate::client::SseEvent;
use crate::config::AppConfig;
use crate::config::child_sessions::ChildSessionRoot;
use crate::config::todo::TodoItem;
pub(crate) use crate::events::AgentOutcome;
use crate::events::{Event, EventSink, NoticeKind, Origin, OutputStream};
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
        /// The tool and its argument words the line is built from.
        name: String,
        args: Vec<String>,
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
    /// A child agent's `tool.started` hook. `node` is the graph node the
    /// call runs inside, when the child is a graph agent.
    ChildToolStarted {
        agent_id: String,
        node: Option<String>,
        call_id: String,
        name: String,
        args: Option<Value>,
    },
    ChildToolFinished {
        agent_id: String,
        node: Option<String>,
        call_id: String,
        name: String,
        args: Option<Value>,
        ok: bool,
        error: Option<String>,
        duration_ms: Option<u64>,
    },
    ChildToolOutput {
        agent_id: String,
        node: Option<String>,
        call_id: String,
        chunk: Vec<u8>,
    },
    /// A graph node of a child agent started or ended.
    ChildNode {
        agent_id: String,
        node_id: String,
        node_type: String,
        state: ChildNodeState,
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
        session_id: Option<String>,
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
    /// Text `.copy` puts on the clipboard. Fd 1 is a turn's capture pipe,
    /// so the loop copies it over the screen's descriptor as Alt+C does.
    Copy(String),
    /// What the paste key read off the clipboard, read and saved on a
    /// worker thread; its images are numbered when it lands.
    ClipboardPasted(crate::repl::PreparedPaste),
    /// A resumed session's history, which the REPL prints on `.session`
    /// and `.agent`; the transcript shows it as rows instead.
    History(Vec<ChildEntry>),
    /// A child agent wrote its session file.
    ChildTranscriptFlushed {
        session_id: String,
    },
    /// The running turn settled where its child sessions live.
    ChildSessionRootResolved(Arc<ChildSessionRoot>),
    /// What reading a child's session file for the pager gave.
    ChildLoaded {
        session_id: String,
        /// Which read this is; only the newest one asked for is shown.
        generation: u64,
        result: Result<Vec<super::pager::ChildEntry>, String>,
    },
    /// What cancelling a task from the roster gave: the cancel tool's own
    /// result, or the error that kept it from running.
    TaskCancelled {
        id: String,
        result: Result<Value, String>,
    },
    /// What a command run during a turn on the turn's starting context
    /// left to show, or the error it failed with.
    CommandRan {
        line: String,
        result: Result<CommandOutput, String>,
    },
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
pub(crate) enum ChildNodeState {
    Started,
    Completed {
        duration_ms: Option<u64>,
        route: Option<String>,
    },
    Failed {
        error: String,
    },
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
            name: cmd_name.to_string(),
            args: cmd_args.to_vec(),
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

/// A string from an engine event, made safe to draw: it lands on the status
/// row or a transcript row, and a tool's text is its own. It is cut to the
/// length a hook's environment value gets, so a long error stays a bounded
/// row.
fn plain(text: &str) -> String {
    super::render::plain_text(crate::hooks::truncate_env_value(text))
}

fn plain_opt(text: &Option<String>) -> Option<String> {
    text.as_deref().map(plain)
}

/// Only the root turn's own tools and thinking drive the status row; a
/// child agent's tool or reasoning starting or finishing says nothing
/// about what the root is doing. A detached origin (no agent, depth 0)
/// counts as root: it is what a publisher without a request context has.
fn is_root(origin: &Origin) -> bool {
    origin.agent_id.is_none() && origin.depth == 0
}

fn tool_args(args: &str) -> Option<Value> {
    serde_json::from_str(args).ok()
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
            Event::UsageUpdated { right_prompt, .. } => {
                self.send(UiMsg::UsageUpdated(right_prompt.clone()))
            }
            Event::ToolStarted {
                origin,
                name,
                call_id,
                args,
            } if is_root(origin) => {
                self.send(UiMsg::ToolRunning(Some(plain(name))));
                self.send(UiMsg::ToolStarted {
                    call_id: plain(call_id),
                    name: plain(name),
                    args: tool_args(args),
                });
            }
            Event::ToolStarted {
                origin:
                    Origin {
                        agent_id: Some(agent_id),
                        node,
                        ..
                    },
                name,
                call_id,
                args,
            } => self.send(UiMsg::ChildToolStarted {
                agent_id: agent_id.clone(),
                node: node.clone(),
                call_id: plain(call_id),
                name: plain(name),
                args: tool_args(args),
            }),
            Event::ToolFinished {
                origin,
                call_id,
                duration_ms,
                error,
                ..
            } if is_root(origin) => {
                self.send(UiMsg::ToolRunning(None));
                self.send(UiMsg::ToolFinished {
                    call_id: plain(call_id),
                    ok: error.is_none(),
                    error: plain_opt(error),
                    duration_ms: *duration_ms,
                });
            }
            Event::ToolFinished {
                origin:
                    Origin {
                        agent_id: Some(agent_id),
                        node,
                        ..
                    },
                name,
                call_id,
                args,
                duration_ms,
                error,
            } => self.send(UiMsg::ChildToolFinished {
                agent_id: agent_id.clone(),
                node: node.clone(),
                call_id: plain(call_id),
                name: plain(name),
                args: args.as_deref().and_then(tool_args),
                ok: error.is_none(),
                error: plain_opt(error),
                duration_ms: *duration_ms,
            }),
            // The root's own output reaches the transcript through
            // `tool_output`; only a child's is taken from the bus.
            Event::ToolOutputChunk {
                call_id,
                bytes,
                origin:
                    Origin {
                        agent_id: Some(agent_id),
                        node,
                        ..
                    },
                ..
            } => self.send(UiMsg::ChildToolOutput {
                agent_id: agent_id.clone(),
                node: node.clone(),
                call_id: call_id.clone(),
                chunk: bytes.clone(),
            }),
            Event::NodeStarted {
                origin:
                    Origin {
                        agent_id: Some(agent_id),
                        ..
                    },
                node_id,
                node_type,
            } => self.send(UiMsg::ChildNode {
                agent_id: agent_id.clone(),
                node_id: plain(node_id),
                node_type: plain(node_type),
                state: ChildNodeState::Started,
            }),
            Event::NodeFinished {
                origin:
                    Origin {
                        agent_id: Some(agent_id),
                        ..
                    },
                node_id,
                node_type,
                duration_ms,
                route,
                error,
            } => self.send(UiMsg::ChildNode {
                agent_id: agent_id.clone(),
                node_id: plain(node_id),
                node_type: plain(node_type),
                state: match error {
                    Some(error) => ChildNodeState::Failed {
                        error: plain(error),
                    },
                    None => ChildNodeState::Completed {
                        duration_ms: Some(*duration_ms),
                        route: plain_opt(route),
                    },
                },
            }),
            Event::JobStarted {
                origin,
                job_id,
                tool,
            } if is_root(origin) => self.send(UiMsg::JobStarted {
                job_id: plain(job_id),
                tool: plain(tool),
            }),
            // A job's end and cancel are matched by id from any origin: a
            // job a child started can still end on a row the root shows.
            Event::JobFinished {
                job_id, ok, error, ..
            } => self.send(UiMsg::JobFinished {
                job_id: plain(job_id),
                ok: *ok,
                error: plain_opt(error),
            }),
            Event::JobCancelled { job_id, .. } => self.send(UiMsg::JobCancelled {
                job_id: job_id.clone(),
            }),
            // `agent.started` is fired with the child's own context, so a
            // direct child of the root turn arrives at depth one.
            Event::AgentStarted {
                origin,
                agent_id,
                name,
                session_id,
            } if origin.depth == 1 => self.send(UiMsg::AgentStarted {
                agent_id: plain(agent_id),
                name: plain_opt(name).unwrap_or_default(),
                session_id: plain_opt(session_id),
            }),
            Event::AgentFinished {
                agent_id, outcome, ..
            } => self.send(UiMsg::AgentFinished {
                agent_id: plain(agent_id),
                outcome: match outcome {
                    AgentOutcome::Failed(error) => AgentOutcome::Failed(plain(error)),
                    other => other.clone(),
                },
            }),
            Event::TodoUpdated { items } => self.send(UiMsg::TodoUpdated(items.clone())),
            Event::QueuedInputSent { text } => self.send(UiMsg::QueuedInputSent(text.clone())),
            Event::ChildTranscriptFlushed { session_id } => {
                self.send(UiMsg::ChildTranscriptFlushed {
                    session_id: session_id.clone(),
                })
            }
            Event::ChildSessionRootResolved { root } => {
                self.send(UiMsg::ChildSessionRootResolved(Arc::clone(root)))
            }
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

    fn tool_started(name: &str, call_id: &str, args: &str, origin: Origin) -> Event {
        Event::ToolStarted {
            origin,
            name: name.to_string(),
            call_id: call_id.to_string(),
            args: args.to_string(),
        }
    }

    fn tool_finished(
        call_id: &str,
        duration_ms: Option<u64>,
        error: Option<&str>,
        origin: Origin,
    ) -> Event {
        Event::ToolFinished {
            origin,
            name: String::new(),
            call_id: call_id.to_string(),
            args: None,
            duration_ms,
            error: error.map(str::to_string),
        }
    }

    fn node_finished(
        node_id: &str,
        node_type: &str,
        duration_ms: u64,
        route: Option<&str>,
        error: Option<&str>,
    ) -> Event {
        Event::NodeFinished {
            origin: graph_child(Some(node_id)),
            node_id: node_id.to_string(),
            node_type: node_type.to_string(),
            duration_ms,
            route: route.map(str::to_string),
            error: error.map(str::to_string),
        }
    }

    fn job_finished(job_id: &str, error: Option<&str>, origin: Origin) -> Event {
        Event::JobFinished {
            origin,
            job_id: job_id.to_string(),
            tool: String::new(),
            ok: error.is_none(),
            error: error.map(str::to_string),
        }
    }

    fn agent_finished(agent_id: &str, outcome: AgentOutcome, origin: Origin) -> Event {
        Event::AgentFinished {
            origin,
            agent_id: agent_id.to_string(),
            name: None,
            session_id: None,
            outcome,
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
        sink.on_event(&tool_started(
            "job__check",
            "call-7",
            r#"{"id":"job_1a2b"}"#,
            Origin::detached(),
        ));
        let msgs = drain(&mut rx);
        assert!(matches!(&msgs[0], UiMsg::ToolRunning(Some(name)) if name == "job__check"));
        assert!(matches!(
            &msgs[1],
            UiMsg::ToolStarted { call_id, name, args: Some(args) }
                if call_id == "call-7" && name == "job__check" && args["id"] == "job_1a2b"
        ));
        assert_eq!(msgs.len(), 2);

        sink.on_event(&tool_finished(
            "call-7",
            Some(1200),
            None,
            Origin::detached(),
        ));
        sink.on_event(&tool_finished(
            "call-8",
            None,
            Some("exit 2"),
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

        // A child's tool hooks stay off the root transcript and the
        // status row; they reach the child's own thread instead.
        sink.on_event(&tool_started(
            "fs_read",
            "",
            "{}",
            Origin {
                agent_id: Some("agent_1".to_string()),
                depth: 1,
                node: None,
            },
        ));
        let msgs = drain(&mut rx);
        assert!(
            matches!(&msgs[..], [UiMsg::ChildToolStarted { .. }]),
            "{msgs:?}"
        );
    }

    fn graph_child(node: Option<&str>) -> Origin {
        Origin {
            agent_id: Some("agent_graph_9f8e".to_string()),
            depth: 1,
            node: node.map(str::to_string),
        }
    }

    /// A child's tool hooks become the child's own start and end, naming
    /// the agent and the graph node, and never touch the status row.
    #[test]
    fn child_tool_hooks_become_child_tool_messages() {
        let (sink, mut rx) = sink();
        sink.on_event(&tool_started(
            "fs_read",
            "call-c1",
            r#"{"path":"a.rs"}"#,
            graph_child(Some("plan")),
        ));
        sink.on_event(&tool_finished(
            "call-c1",
            Some(40),
            Some("missing"),
            graph_child(Some("plan")),
        ));
        sink.on_event(&tool_finished("call-c2", None, None, graph_child(None)));
        let msgs = drain(&mut rx);
        assert!(
            !msgs.iter().any(|msg| matches!(msg, UiMsg::ToolRunning(_))),
            "{msgs:?}"
        );
        assert!(matches!(
            &msgs[0],
            UiMsg::ChildToolStarted { agent_id, node: Some(node), call_id, name, args: Some(args) }
                if agent_id == "agent_graph_9f8e" && node == "plan" && call_id == "call-c1"
                    && name == "fs_read" && args["path"] == "a.rs"
        ));
        assert!(matches!(
            &msgs[1],
            UiMsg::ChildToolFinished {
                agent_id, node: Some(node), call_id, ok: false, error: Some(error), duration_ms: Some(40), ..
            } if agent_id == "agent_graph_9f8e" && node == "plan" && call_id == "call-c1" && error == "missing"
        ));
        assert!(matches!(
            &msgs[2],
            UiMsg::ChildToolFinished { node: None, call_id, ok: true, error: None, .. }
                if call_id == "call-c2"
        ));
        assert_eq!(msgs.len(), 3);
    }

    /// A child's graph node hooks carry its state; the root's own graph
    /// nodes have no child thread to go to.
    #[test]
    fn child_node_hooks_become_child_node_messages() {
        let (sink, mut rx) = sink();
        sink.on_event(&Event::NodeStarted {
            origin: graph_child(Some("plan")),
            node_id: "plan".to_string(),
            node_type: "llm".to_string(),
        });
        sink.on_event(&node_finished("plan", "llm", 1500, Some("build"), None));
        sink.on_event(&node_finished("build", "script", 20, None, Some("exit 3")));
        sink.on_event(&Event::NodeStarted {
            origin: Origin {
                node: Some("root-node".to_string()),
                ..Origin::detached()
            },
            node_id: "root-node".to_string(),
            node_type: "llm".to_string(),
        });
        let msgs = drain(&mut rx);
        assert!(matches!(
            &msgs[0],
            UiMsg::ChildNode { agent_id, node_id, node_type, state: ChildNodeState::Started }
                if agent_id == "agent_graph_9f8e" && node_id == "plan" && node_type == "llm"
        ));
        assert!(matches!(
            &msgs[1],
            UiMsg::ChildNode {
                node_id,
                state: ChildNodeState::Completed { duration_ms: Some(1500), route: Some(route) },
                ..
            } if node_id == "plan" && route == "build"
        ));
        assert!(matches!(
            &msgs[2],
            UiMsg::ChildNode { node_id, state: ChildNodeState::Failed { error }, .. }
                if node_id == "build" && error == "exit 3"
        ));
        assert_eq!(msgs.len(), 3);
    }

    /// A child's tool output reaches its thread from the bus; the root's
    /// arrives through `tool_output` alone, so the bus copy is dropped.
    #[test]
    fn only_a_childs_output_chunk_is_forwarded_from_the_bus() {
        let (sink, mut rx) = sink();
        let chunk = |origin: Origin| Event::ToolOutputChunk {
            call_id: "call-o1".to_string(),
            stream: OutputStream::Stdout,
            bytes: b"line\n".to_vec(),
            origin,
        };
        sink.on_event(&chunk(Origin::detached()));
        assert!(drain(&mut rx).is_empty());
        sink.on_event(&chunk(graph_child(Some("plan"))));
        let msgs = drain(&mut rx);
        assert!(matches!(
            &msgs[..],
            [UiMsg::ChildToolOutput { agent_id, node, call_id, chunk }]
                if agent_id == "agent_graph_9f8e"
                    && node.as_deref() == Some("plan")
                    && call_id == "call-o1"
                    && chunk == b"line\n"
        ));
    }

    /// A job's start reaches the transcript only from the root; its end
    /// and a cancel reach it whatever their origin.
    #[test]
    fn job_hooks_and_cancellation_reach_the_transcript() {
        let (sink, mut rx) = sink();
        let child = Origin {
            agent_id: Some("agent_1".to_string()),
            depth: 1,
            node: None,
        };
        sink.on_event(&Event::JobStarted {
            origin: Origin::detached(),
            job_id: "job_1a2b".to_string(),
            tool: "execute_command".to_string(),
        });
        sink.on_event(&Event::JobStarted {
            origin: child.clone(),
            job_id: "job_child".to_string(),
            tool: String::new(),
        });
        sink.on_event(&job_finished(
            "job_1a2b",
            Some("exit 1"),
            Origin::detached(),
        ));
        sink.on_event(&job_finished("job_3c4d", None, child.clone()));
        sink.on_event(&Event::JobCancelled {
            job_id: "job_5e6f".to_string(),
            origin: child,
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
    /// an agent's end is matched by id whatever its depth.
    #[test]
    fn agent_hooks_reach_the_transcript_for_direct_children() {
        let (sink, mut rx) = sink();
        let child = |depth: usize| Origin {
            agent_id: Some("agent_coder_1a2b".to_string()),
            depth,
            node: None,
        };
        let started = |origin: Origin| Event::AgentStarted {
            origin,
            agent_id: "agent_coder_1a2b".to_string(),
            name: Some("coder".to_string()),
            session_id: Some("sess_coder_1a2b".to_string()),
        };
        sink.on_event(&started(child(2)));
        sink.on_event(&started(Origin::detached()));
        assert!(drain(&mut rx).is_empty());
        sink.on_event(&started(child(1)));
        sink.on_event(&agent_finished(
            "agent_coder_1a2b",
            AgentOutcome::Failed("boom".to_string()),
            child(1),
        ));
        sink.on_event(&agent_finished(
            "agent_b",
            AgentOutcome::Interrupted,
            child(2),
        ));
        sink.on_event(&agent_finished(
            "agent_c",
            AgentOutcome::Completed,
            Origin::detached(),
        ));
        let msgs = drain(&mut rx);
        assert!(matches!(
            &msgs[0],
            UiMsg::AgentStarted { agent_id, name, session_id: Some(session_id) }
                if agent_id == "agent_coder_1a2b" && name == "coder" && session_id == "sess_coder_1a2b"
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

    /// Queueing a line is the composer's own doing and already on screen;
    /// only the line's hand-over to the model is forwarded.
    #[test]
    fn a_sent_queued_line_reaches_the_transcript_but_a_queued_one_does_not() {
        let (sink, mut rx) = sink();
        sink.on_event(&Event::QueuedInput {
            text: "later".to_string(),
        });
        sink.on_event(&Event::QueuedInputSent {
            text: "later".to_string(),
        });
        assert!(matches!(rx.try_recv(), Ok(UiMsg::QueuedInputSent(text)) if text == "later"));
        assert!(rx.try_recv().is_err());
    }
}
