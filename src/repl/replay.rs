use anyhow::Result;
use crossterm::{
    QueueableCommand, cursor,
    style::{Attribute, Print, SetAttribute},
    terminal,
};
use std::io::{self, Write};

use crate::client::{Message, MessageContent, MessageContentToolCalls, MessageRole};
use crate::config::{AppConfig, LastMessage, Session};
use crate::utils::{IS_STDOUT_TERMINAL, dimmed_text, replay_label_text};

pub fn snapshot(session: &Session) -> (Vec<Message>, Vec<Message>) {
    (
        filter_for_display(session.compressed_messages()),
        filter_for_display(session.messages()),
    )
}

pub fn render(app: &AppConfig, compressed: &[Message], active: &[Message]) -> Result<()> {
    if compressed.is_empty() && active.is_empty() {
        return Ok(());
    }

    render_messages(app, compressed)?;
    if !compressed.is_empty() && !active.is_empty() {
        println!("{}", dimmed_text("─── ↑ pre-compression history ↑ ───"));
        println!();
    }
    render_messages(app, active)?;
    println!("{}", dimmed_text("─── ↑ previous conversation ↑ ───"));
    println!();
    Ok(())
}

pub(crate) fn last_turn(compressed: &[Message], active: &[Message]) -> Option<Vec<Message>> {
    let (preferred, fallback) = if active.is_empty() {
        (compressed, active)
    } else {
        (active, compressed)
    };
    from_last_user(preferred)
        .or_else(|| from_last_user(fallback))
        .map(<[Message]>::to_vec)
}

fn from_last_user(messages: &[Message]) -> Option<&[Message]> {
    let start = messages.iter().rposition(|m| m.role == MessageRole::User)?;
    Some(&messages[start..])
}

pub(crate) fn synthesize(last_message: &LastMessage) -> Vec<Message> {
    vec![
        Message::new(
            MessageRole::User,
            MessageContent::Text(last_message.input.raw()),
        ),
        Message::new(
            MessageRole::Assistant,
            MessageContent::Text(last_message.output.clone()),
        ),
    ]
}

// Scrolls the old viewport into scrollback instead of erasing it: multiplexers
// (Zellij stacked panes) can't recover rows removed by an erase-type clear.
pub(crate) fn scroll_clear<W: Write>(w: &mut W, rows: u16) -> io::Result<()> {
    w.queue(SetAttribute(Attribute::Reset))?
        .queue(cursor::Show)?
        .queue(cursor::MoveTo(0, rows.saturating_sub(1)))?
        .queue(Print("\n".repeat(rows as usize)))?
        .queue(cursor::MoveTo(0, 0))?;
    w.flush()
}

pub(crate) fn clear_viewport() -> Result<()> {
    if !*IS_STDOUT_TERMINAL {
        return Ok(());
    }
    if let Ok((_, rows)) = terminal::size()
        && rows > 1
    {
        scroll_clear(&mut io::stdout(), rows)?;
    }

    Ok(())
}

fn filter_for_display(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter(|m| !m.role.is_system())
        .cloned()
        .collect()
}

pub(crate) fn render_messages(app: &AppConfig, messages: &[Message]) -> Result<()> {
    for message in messages {
        match message.role {
            MessageRole::User => {
                if let Some(text) = message.content.as_text() {
                    println!("{}", replay_label_text("You:"));
                    println!("{text}");
                    println!();
                }
            }
            MessageRole::Assistant => {
                if let Some(text) = message.content.as_text() {
                    println!("{}", replay_label_text("Assistant:"));
                    app.print_markdown(text)?;
                    println!();
                }
            }
            MessageRole::Tool => {
                if let MessageContent::ToolCalls(tool_calls) = &message.content {
                    render_tool_call_rounds(app, tool_calls)?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn render_tool_call_rounds(app: &AppConfig, tool_calls: &MessageContentToolCalls) -> Result<()> {
    let mut needs_gap_after_calls = false;
    if !tool_calls.text.trim().is_empty() {
        println!("{}", replay_label_text("Assistant:"));
        app.print_markdown(&tool_calls.text)?;
        println!();
    }

    for result in &tool_calls.tool_results {
        if let Some(text) = result.text.as_deref().filter(|t| !t.trim().is_empty()) {
            if needs_gap_after_calls {
                println!();
            }

            println!("{}", replay_label_text("Assistant:"));
            app.print_markdown(text)?;
            println!();
        }

        println!("{}", dimmed_text(&format!("⚙ {}", result.call.name)));
        needs_gap_after_calls = true;
    }

    if needs_gap_after_calls {
        println!();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ToolCall;
    use crate::config::{AppState, Input, RequestContext, WorkingMode};
    use crate::function::ToolResult;
    use serde_json::json;
    use std::sync::Arc;

    fn user(text: &str) -> Message {
        Message::new(MessageRole::User, MessageContent::Text(text.to_string()))
    }

    fn assistant(text: &str) -> Message {
        Message::new(
            MessageRole::Assistant,
            MessageContent::Text(text.to_string()),
        )
    }

    fn tool_round() -> Message {
        let call = ToolCall::new("fs_ls".to_string(), json!({}), None);
        Message::new(
            MessageRole::Tool,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![ToolResult::new(call, json!("ok"))],
                String::new(),
            )),
        )
    }

    fn texts(messages: &[Message]) -> Vec<Option<&str>> {
        messages.iter().map(|m| m.content.as_text()).collect()
    }

    #[test]
    fn last_turn_slices_from_the_last_user_message() {
        let active = vec![
            user("first"),
            assistant("one"),
            user("second"),
            tool_round(),
            assistant("two"),
        ];

        let turn = last_turn(&[], &active).unwrap();

        assert_eq!(turn.len(), 3);
        assert_eq!(texts(&turn), vec![Some("second"), None, Some("two")]);
        assert_eq!(turn[1].role, MessageRole::Tool);
    }

    #[test]
    fn last_turn_falls_back_to_compressed_when_active_has_no_user() {
        let compressed = vec![user("old"), assistant("reply")];
        let active = vec![assistant("dangling")];

        let turn = last_turn(&compressed, &active).unwrap();

        assert_eq!(texts(&turn), vec![Some("old"), Some("reply")]);
    }

    #[test]
    fn last_turn_is_none_when_both_lists_are_empty() {
        assert!(last_turn(&[], &[]).is_none());
    }

    #[test]
    fn synthesize_yields_a_user_assistant_pair_from_raw_input() {
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Repl);
        let input = Input::from_str(&ctx, "what time is it", None).unwrap();
        let last_message = LastMessage::new(input, "noon".to_string());

        let messages = synthesize(&last_message);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[0].content.as_text(), Some("what time is it"));
        assert_eq!(messages[1].role, MessageRole::Assistant);
        assert_eq!(messages[1].content.as_text(), Some("noon"));
    }

    #[test]
    fn scroll_clear_scrolls_instead_of_erasing() {
        let mut out = Vec::new();

        scroll_clear(&mut out, 24).unwrap();

        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("\x1b[24;1H"),
            "moves to the bottom row: {out:?}"
        );
        assert_eq!(out.matches('\n').count(), 24);
        assert!(out.ends_with("\x1b[1;1H"), "re-homes the cursor: {out:?}");
        assert!(!out.contains("\x1b[J"));
        assert!(!out.contains("\x1b[2J"));
        assert!(!out.contains("\x1b[3J"));
    }
}
