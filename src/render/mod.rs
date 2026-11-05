mod inquire;
mod markdown;
mod stream;

pub use inquire::prompt_theme;

pub use self::markdown::{MarkdownRender, RenderOptions};
use self::stream::{markdown_stream, raw_stream};

use crate::mesh::AlreadyClosed;
use crate::mesh::notify::NoticeText;
use crate::utils::{
    AbortSignal, IS_STDOUT_TERMINAL, pretty_error, pretty_error_with, stderr_error_text,
};
use crate::{
    client::SseEvent,
    config::{AppConfig, CommandPrefix},
};

use anyhow::Result;
use std::fmt;
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

/// `render_error` for an error a REPL command raised. A `.mesh` error, marked by
/// `MeshVerbError`, shows its command hints in `prefix`, the way the human types them;
/// any other error is rendered as `render_error` would.
pub fn render_command_error(err: anyhow::Error, prefix: CommandPrefix) {
    eprintln!("{}", stderr_error_text(&command_error_text(&err, prefix)));
}

pub(crate) fn command_error_text(err: &anyhow::Error, prefix: CommandPrefix) -> String {
    if err.downcast_ref::<MeshVerbError>().is_none() {
        return pretty_error(err);
    }
    pretty_error_with(err, |link| match quoted_text(link) {
        Some(text) => text.render(prefix),
        None => prefix.render_hints(&link.to_string()).into_owned(),
    })
}

fn quoted_text<'a>(link: &'a (dyn std::error::Error + 'static)) -> Option<&'a NoticeText> {
    if let Some(verb) = link.downcast_ref::<MeshVerbError>() {
        return quoted_text(verb.0.as_ref());
    }
    if let Some(closed) = link.downcast_ref::<AlreadyClosed>() {
        return Some(closed.notice());
    }
    link.downcast_ref::<QuotingError>()
        .map(|quoting| &quoting.0)
}

/// Marks an error the `.mesh` verb raised. Display and source pass straight through, so
/// the rendered chain reads exactly as the wrapped error's would.
#[derive(Debug)]
pub struct MeshVerbError(anyhow::Error);

impl MeshVerbError {
    pub fn wrap(err: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(Self(err))
    }
}

impl fmt::Display for MeshVerbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for MeshVerbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.chain().nth(1)
    }
}

/// A `.mesh` error that quotes text this node did not write: a path a peer named, a
/// share rule's pattern, or a share file's own words. Only its frame runs are spelled
/// in the prefix, so a quoted `.vault` keeps its dot while the hints beside it follow
/// the prefix.
#[derive(Debug)]
pub(crate) struct QuotingError(NoticeText);

impl QuotingError {
    pub(crate) fn wrap(text: NoticeText) -> anyhow::Error {
        anyhow::Error::new(Self(text))
    }
}

impl fmt::Display for QuotingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for QuotingError {}

#[cfg(test)]
mod tests {
    use super::*;

    mod command_prefix_sinks {
        use super::*;

        #[test]
        fn command_error_hints_follow_the_prefix() {
            let err = MeshVerbError::wrap(anyhow::anyhow!(
                "No path to 1a2b3c4d is known; nothing was sent. `.mesh peers` lists what this node has heard from."
            )
            .context("`.mesh message` failed"));
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

        fn quoting() -> anyhow::Error {
            QuotingError::wrap(
                NoticeText::default()
                    .frame("No share rule has the pattern `")
                    .peer(".vault")
                    .frame("`; `.mesh shares` lists them."),
            )
        }

        #[test]
        fn a_quoting_error_raised_bare_keeps_its_quote_and_slashes_its_hint() {
            let text = command_error_text(&MeshVerbError::wrap(quoting()), CommandPrefix::Slash);
            assert!(
                text.ends_with(
                    "No share rule has the pattern `.vault`; `/mesh shares` lists them."
                ),
                "{text}"
            );
            assert!(!text.contains("/vault"), "{text}");
        }

        #[test]
        fn a_quoting_error_under_context_keeps_its_quote_and_slashes_both_hints() {
            let err = MeshVerbError::wrap(quoting().context("`.mesh unshare` failed"));
            let text = command_error_text(&err, CommandPrefix::Slash);
            assert!(text.starts_with("Error: `/mesh unshare` failed"), "{text}");
            assert!(
                text.ends_with(
                    "No share rule has the pattern `.vault`; `/mesh shares` lists them."
                ),
                "{text}"
            );
            assert!(!text.contains("/vault"), "{text}");
        }

        #[test]
        fn the_mesh_marker_leaves_the_rendered_chain_unchanged() {
            let inner = || {
                anyhow::anyhow!("root cause")
                    .context("middle")
                    .context("`.mesh trust` failed")
            };
            let marked = MeshVerbError::wrap(inner());
            assert_eq!(pretty_error(&marked), pretty_error(&inner()));
            assert_eq!(format!("{marked:#}"), format!("{:#}", inner()));
            assert_eq!(
                command_error_text(&marked, CommandPrefix::Dot),
                pretty_error(&inner())
            );
        }

        /// Only `.mesh` errors are rendered: another command's error keeps a typed
        /// `.session` hint as it was written under every prefix.
        #[test]
        fn an_unmarked_error_is_rendered_as_it_was_raised() {
            let err = anyhow::anyhow!("No session is active; `.session` starts one.");
            for prefix in [
                CommandPrefix::Dot,
                CommandPrefix::Slash,
                CommandPrefix::Both,
            ] {
                assert_eq!(
                    command_error_text(&err, prefix),
                    pretty_error(&err),
                    "{prefix}"
                );
            }
        }
    }
}
