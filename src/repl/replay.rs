use anyhow::Result;

use crate::client::{Message, MessageRole};
use crate::config::{AppConfig, Session};
use crate::utils::dimmed_text;

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
                    println!("{}", dimmed_text("You:"));
                    println!("{text}");
                    println!();
                }
            }
            MessageRole::Assistant => {
                if let Some(text) = message.content.as_text() {
                    app.print_markdown(text)?;
                    println!();
                }
            }
            _ => {}
        }
    }

    Ok(())
}
