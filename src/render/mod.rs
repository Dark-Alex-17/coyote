mod inquire;
mod markdown;
mod stream;

pub use inquire::prompt_theme;

pub use self::markdown::{MarkdownRender, RenderOptions};
use self::stream::{markdown_stream, raw_stream};

use crate::utils::{AbortSignal, IS_STDOUT_TERMINAL, error_text, pretty_error};
use crate::{client::SseEvent, config::AppConfig};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedReceiver;

pub async fn render_stream(
    rx: UnboundedReceiver<SseEvent>,
    app: &AppConfig,
    abort_signal: AbortSignal,
    silent: bool,
) -> Result<()> {
    if silent {
        return drain_silently(rx, &abort_signal).await;
    }
    let ret = if *IS_STDOUT_TERMINAL && app.highlight {
        let render_options = app.render_options()?;
        let mut render = MarkdownRender::init(render_options)?;
        markdown_stream(rx, &mut render, &abort_signal).await
    } else {
        raw_stream(rx, &abort_signal).await
    };
    ret.map_err(|err| err.context("Failed to reader stream"))
}

async fn drain_silently(
    mut rx: UnboundedReceiver<SseEvent>,
    abort_signal: &AbortSignal,
) -> Result<()> {
    loop {
        if abort_signal.aborted() {
            break;
        }
        match rx.recv().await {
            Some(SseEvent::Done) | None => break,
            Some(SseEvent::Text(_)) => {}
        }
    }
    Ok(())
}

pub fn render_error(err: anyhow::Error) {
    eprintln!("{}", error_text(&pretty_error(&err)));
}
