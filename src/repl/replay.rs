use anyhow::Result;

use crate::client::{Message, MessageContent, MessageContentToolCalls, MessageRole};
use crate::config::{AppConfig, Session};
use crate::utils::{dimmed_text, replay_label_text};

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

fn filter_for_display(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter(|m| !m.role.is_system())
        .cloned()
        .collect()
}

fn render_messages(app: &AppConfig, messages: &[Message]) -> Result<()> {
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
