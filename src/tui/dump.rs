//! What the UI leaves on the normal screen when it exits: the committed
//! transcript, each reply rendered by the REPL's markdown renderer the
//! way the REPL prints one, everything else as plain text with tool,
//! job and agent rows down to their one-line header.

use super::cells::{Cell, without_think_spans};
use super::render;
use crate::config::AppConfig;
use crate::render::{MarkdownRender, RenderOptions};

/// The transcript text for `cells`, ending in a newline unless empty.
pub(super) fn transcript_text(cells: &[Cell], config: &AppConfig) -> String {
    let options = match config.render_options() {
        Ok(options) => Some(options),
        Err(err) => {
            warn!("Printing the transcript without markdown: {err}");
            None
        }
    };
    let mut out = String::new();
    for cell in cells {
        let block = match cell {
            Cell::User { text, mid_turn, .. } => {
                let marker = if *mid_turn { " (sent mid-turn)" } else { "" };
                format!("> {}{marker}", render::plain_text(text))
            }
            Cell::Assistant { source, .. } => {
                markdown(without_think_spans(source, true), options.as_ref())
            }
            Cell::Notice { text, .. } | Cell::Error { text, .. } => render::plain_text(text),
            Cell::Command { name, output, .. } => {
                let header = format!("> {}", render::plain_text(name));
                let output = render::sanitize_ansi(output.trim_end());
                if output.is_empty() {
                    header
                } else {
                    format!("{header}\n{output}")
                }
            }
            Cell::Tool { .. } | Cell::Job { .. } | Cell::Agent { .. } => {
                cell.summary_line().unwrap_or_default()
            }
            Cell::Text { .. } | Cell::Welcome { .. } => continue,
        };
        // A blank line before each line the user sent, so every exchange
        // reads as its own paragraph.
        if !out.is_empty() && matches!(cell, Cell::User { .. } | Cell::Command { .. }) {
            out.push('\n');
        }
        out.push_str(block.trim_end_matches('\n'));
        out.push('\n');
    }
    out
}

/// `AppConfig::print_markdown`'s rendering, with only the colours and
/// attributes of what it produces let through to the terminal.
fn markdown(text: &str, options: Option<&RenderOptions>) -> String {
    let rendered = options.map(|options| {
        MarkdownRender::init(options.clone()).map(|mut renderer| {
            let body = renderer.render(text);
            let tail = renderer.finalize();
            if tail.is_empty() {
                body
            } else {
                format!("{body}\n{tail}")
            }
        })
    });
    match rendered {
        Some(Ok(rendered)) => render::sanitize_ansi(&rendered),
        Some(Err(err)) => {
            warn!("Printing a reply without markdown: {err}");
            render::plain_text(text)
        }
        None => render::plain_text(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::NoticeKind;
    use crate::tui::cells::ToolStatus;
    use std::time::{Duration, Instant};

    fn no_color_config() -> AppConfig {
        AppConfig {
            highlight: false,
            ..Default::default()
        }
    }

    #[test]
    fn the_dump_prints_prompts_replies_commands_and_one_line_tool_rows() {
        let mut tool = Cell::tool(
            "c1".to_string(),
            "Call fs_read {\"path\":\"a.txt\"}".to_string(),
            Instant::now(),
        );
        let Cell::Tool { status, .. } = &mut tool else {
            unreachable!();
        };
        *status = ToolStatus::Ok {
            elapsed: Duration::from_millis(200),
        };
        let cells = vec![
            Cell::welcome("m".to_string(), crate::config::CommandPrefix::Dot),
            Cell::user("read \x1b[31ma.txt\x1b[0m"),
            tool,
            Cell::assistant(
                "<think>hmm</think>The file says **hello**.".to_string(),
                true,
            ),
            Cell::notice(NoticeKind::Interrupted, "Cancelled job job-1".to_string()),
            {
                let mut command = Cell::command(".info");
                if let Cell::Command { output, .. } = &mut command {
                    *output = "model  m\x1b[2J\n".to_string();
                }
                command
            },
        ];
        let text = transcript_text(&cells, &no_color_config());
        assert_eq!(
            render::plain_text(&text),
            "> read a.txt\n\
             fs_read({\"path\":\"a.txt\"})  0.2s  ok\n\
             The file says hello.\n\
             Cancelled job job-1\n\
             \n\
             > .info\n\
             model  m\n"
        );
        assert!(!text.contains("\x1b[2J"), "{text:?}");
        assert!(!text.contains("\x1b[31m"), "{text:?}");
        assert!(!text.contains("hmm"), "{text:?}");
    }

    #[test]
    fn nothing_committed_dumps_nothing() {
        let cells = vec![Cell::welcome(
            "m".to_string(),
            crate::config::CommandPrefix::Dot,
        )];
        assert_eq!(transcript_text(&cells, &no_color_config()), "");
    }
}
