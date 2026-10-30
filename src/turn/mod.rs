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
