use super::UiSink;
use crate::client::SseEvent;
use crate::config::AppConfig;
use crate::events::{NoticeKind, OutputStream};
use crate::utils::AbortSignal;
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc::UnboundedReceiver;

/// A sink for runs nobody is watching: shows nothing, but still drains the
/// stream so the producer never blocks and keeps notices in the debug log
/// so a failed unattended tool run leaves a trace.
pub struct SilentSink;

impl UiSink for SilentSink {
    fn assistant_text(&self, _app: &AppConfig, _text: &str) -> Result<()> {
        Ok(())
    }

    fn stream<'a>(
        &'a self,
        app: &'a AppConfig,
        rx: UnboundedReceiver<SseEvent>,
        abort: AbortSignal,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(crate::render::render_stream(rx, app, abort, true))
    }

    fn notice(&self, _kind: NoticeKind, text: &str) {
        debug!("{text}");
    }

    fn tool_narration(&self, _call_id: &str, _line: &str) {}

    fn tool_output(
        &self,
        _call_id: Option<&str>,
        _stream: OutputStream,
        _depth: usize,
        _chunk: &[u8],
    ) {
    }

    fn busy(&self, _message: Option<&str>) {}

    fn turn_divider(&self) {}
}
