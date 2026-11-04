//! How a turn reaches the screen: the work task talks to `TuiSink` through
//! the `UiSink` trait and the bus, and every call becomes a `UiMsg` in the
//! UI loop's inbox. A question for the human goes the same way and blocks
//! the asking thread on a std channel until the modal answers.

use crate::client::SseEvent;
use crate::config::AppConfig;
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
    Prompt {
        req: Box<PromptRequest>,
        reply: std::sync::mpsc::Sender<PromptAnswer>,
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
                .trim_start_matches('\n')
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
        _depth: usize,
        chunk: &[u8],
    ) {
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

fn hook_tool_name(envs: &[(String, String)]) -> Option<String> {
    // The name is drawn on the status row; a tool's env is its own text.
    envs.iter()
        .find(|(key, _)| key == "COYOTE_TOOL_NAME")
        .map(|(_, value)| super::render::plain_text(value))
}

/// Only the root turn's own tools drive the status row; a child agent's
/// tool starting or finishing says nothing about what the root is doing.
fn is_root(origin: &Origin) -> bool {
    origin.agent_id.is_none() && origin.depth == 0
}

impl EventSink for TuiSink {
    fn on_event(&self, event: &Event) {
        match event {
            Event::ThinkingStarted => self.send(UiMsg::ThinkingStarted),
            Event::ThinkingStopped => self.send(UiMsg::ThinkingStopped),
            Event::UsageUpdated { right_prompt } => {
                self.send(UiMsg::UsageUpdated(right_prompt.clone()))
            }
            Event::Hook {
                event: HookEvent::ToolStarted,
                envs,
                origin,
                ..
            } if is_root(origin) => self.send(UiMsg::ToolRunning(hook_tool_name(envs))),
            Event::Hook {
                event: HookEvent::ToolCompleted | HookEvent::ToolFailed,
                origin,
                ..
            } if is_root(origin) => self.send(UiMsg::ToolRunning(None)),
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
