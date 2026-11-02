mod inquire;
mod markdown;
mod stream;

pub use inquire::prompt_theme;

pub use self::markdown::{MarkdownRender, RenderOptions};
use self::stream::{markdown_stream, raw_stream};

use crate::utils::{AbortSignal, IS_STDOUT_TERMINAL, pretty_error, stderr_error_text};
use crate::{
    client::SseEvent,
    config::{AppConfig, CommandPrefix},
};

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
    eprintln!("{}", stderr_error_text(&pretty_error(&err)));
}

/// `render_error` for an error a REPL command raised: its command hints are shown in
/// `prefix`, the way the human types them.
pub fn render_command_error(err: anyhow::Error, prefix: CommandPrefix) {
    eprintln!("{}", stderr_error_text(&command_error_text(&err, prefix)));
}

pub(crate) fn command_error_text(err: &anyhow::Error, prefix: CommandPrefix) -> String {
    prefix.render_hints(&pretty_error(err)).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    mod command_prefix_sinks {
        use super::*;

        #[test]
        fn command_error_hints_follow_the_prefix() {
            let err = anyhow::anyhow!(
                "No path to 1a2b3c4d is known; nothing was sent. `.mesh peers` lists what this node has heard from."
            )
            .context("`.mesh message` failed");
            for (prefix, sigil) in [
                (CommandPrefix::Dot, '.'),
                (CommandPrefix::Slash, '/'),
                (CommandPrefix::Both, '.'),
            ] {
                let text = command_error_text(&err, prefix);
                assert_eq!(
                    text,
                    format!(
                        "Error: `{sigil}mesh message` failed\n\nCaused by:\n    No path to 1a2b3c4d is known; nothing was sent. `{sigil}mesh peers` lists what this node has heard from."
                    ),
                    "{prefix}"
                );
            }
        }
    }
}
