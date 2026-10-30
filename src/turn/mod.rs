//! Where a turn's output goes: a `UiSink` renders what the engine says
//! (assistant text, notices, tool narration and output) so the same turn
//! loop can drive a terminal, a silent run, or a later full-screen UI.

mod silent;
mod stdout;

pub use silent::SilentSink;
pub use stdout::StdoutSink;

use crate::client::SseEvent;
use crate::config::AppConfig;
use crate::events::{NoticeKind, OutputStream};
use crate::utils::AbortSignal;
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc::UnboundedReceiver;

pub trait UiSink: Send + Sync {
    /// The final text of a round, rendered as markdown where the sink can.
    #[allow(dead_code)] // TASK-307
    fn assistant_text(&self, app: &AppConfig, text: &str) -> Result<()>;
    /// Consumes a streaming response; the sink owns the receiver.
    #[allow(dead_code)] // TASK-307
    fn stream<'a>(
        &'a self,
        app: &'a AppConfig,
        rx: UnboundedReceiver<SseEvent>,
        abort: AbortSignal,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
    /// An engine notice (checkpoint, auto-continue, tool warning, ...).
    fn notice(&self, kind: NoticeKind, text: &str);
    /// The `Call <tool> <args>` line announcing a tool call.
    fn tool_narration(&self, call_id: &str, line: &str);
    /// A chunk a running tool wrote to its stdout or stderr.
    fn tool_output(&self, call_id: Option<&str>, stream: OutputStream, depth: usize, chunk: &[u8]);
    /// Shows (`Some(message)`) or clears (`None`) the busy indicator.
    #[allow(dead_code)] // TASK-307
    fn busy(&self, message: Option<&str>);
    /// The rule drawn between two turns.
    #[allow(dead_code)] // TASK-307
    fn turn_divider(&self);
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

    fn tool_narration(&self, _call_id: &str, _line: &str) {}

    fn tool_output(&self, _: Option<&str>, _: OutputStream, _: usize, _: &[u8]) {}

    fn busy(&self, _message: Option<&str>) {}

    fn turn_divider(&self) {}
}
