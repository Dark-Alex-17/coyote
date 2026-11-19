//! Where a turn's output goes: a `UiSink` renders what the engine says
//! (assistant text, notices, tool narration and output) so the same turn
//! loop can drive a terminal, a silent run, or a later full-screen UI.

pub(crate) mod driver;
mod silent;
mod stdout;

pub use driver::{TurnDriver, TurnMode};
pub use silent::SilentSink;
pub use stdout::StdoutSink;

use crate::client::SseEvent;
use crate::config::AppConfig;
use crate::events::{Event, NoticeKind, OutputStream};
use crate::utils::AbortSignal;
use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc::UnboundedReceiver;

pub trait UiSink: Send + Sync {
    /// The final text of a round, rendered as markdown where the sink can.
    fn assistant_text(&self, app: &AppConfig, text: &str) -> Result<()>;
    /// Consumes a streaming response; the sink owns the receiver.
    fn stream<'a>(
        &'a self,
        app: &'a AppConfig,
        rx: UnboundedReceiver<SseEvent>,
        abort: AbortSignal,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
    /// An engine notice (checkpoint, auto-continue, tool warning, ...).
    fn notice(&self, kind: NoticeKind, text: &str);
    /// Announces a tool call; the sink decides how `Call <tool> <args>` is
    /// styled, so producers hand over the parts rather than a painted line.
    fn tool_narration(&self, call_id: &str, cmd_name: &str, cmd_args: &[String], json_data: &Value);
    /// A chunk a running tool wrote to its stdout or stderr.
    fn tool_output(&self, call_id: Option<&str>, stream: OutputStream, depth: usize, chunk: &[u8]);
    /// Shows (`Some(message)`) or clears (`None`) the busy indicator.
    /// Clearing is synchronous: the indicator is gone from the terminal
    /// when `busy(None)` returns, so the caller may print straight after.
    fn busy(&self, message: Option<&str>);
    /// The rule drawn between two turns.
    fn turn_divider(&self);
}

/// Keeps the sink's busy indicator up for as long as it lives, so a future
/// cancelled mid-request clears the indicator on the way out.
pub(crate) struct BusyGuard<'a>(&'a dyn UiSink);

impl<'a> BusyGuard<'a> {
    pub(crate) fn show(ui: &'a dyn UiSink, message: &str) -> Self {
        ui.busy(Some(message));
        Self(ui)
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.busy(None);
    }
}

/// Renders a notice on the sink and publishes it, so a sink stays a renderer
/// and the bus stays the observer.
pub(crate) fn emit_notice(ui: &dyn UiSink, kind: NoticeKind, text: String) {
    ui.notice(kind, &text);
    crate::events::bus().publish(Event::Notice { kind, text });
}

/// Renders a round's final text on the sink and publishes it as markdown.
pub(crate) fn emit_assistant_text(ui: &dyn UiSink, app: &AppConfig, text: &str) -> Result<()> {
    ui.assistant_text(app, text)?;
    crate::events::bus().publish(Event::AssistantText {
        text: text.to_string(),
        markdown: true,
    });
    Ok(())
}

/// A sink that records every notice and ignores everything else, so a test
/// can pin what a producer hands the sink before any terminal paint.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct NoticeProbe(std::sync::Mutex<Vec<(NoticeKind, String)>>);

#[cfg(test)]
impl NoticeProbe {
    pub(crate) fn notices(&self) -> Vec<(NoticeKind, String)> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(test)]
impl UiSink for NoticeProbe {
    fn assistant_text(&self, _app: &AppConfig, _text: &str) -> Result<()> {
        Ok(())
    }

    fn stream<'a>(
        &'a self,
        _app: &'a AppConfig,
        _rx: UnboundedReceiver<SseEvent>,
        _abort: AbortSignal,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn notice(&self, kind: NoticeKind, text: &str) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((kind, text.to_string()));
    }

    fn tool_narration(&self, _: &str, _: &str, _: &[String], _: &Value) {}

    fn tool_output(&self, _: Option<&str>, _: OutputStream, _: usize, _: &[u8]) {}

    fn busy(&self, _message: Option<&str>) {}

    fn turn_divider(&self) {}
}
