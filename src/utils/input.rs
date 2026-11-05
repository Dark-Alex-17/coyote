use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{Write, stdout};

/// Reads a single character from stdin without requiring Enter
/// Returns the character if it's one of the valid options, or the default if Enter is pressed
pub fn read_single_key(valid_chars: &[char], default: char, prompt: &str) -> Result<char> {
    if crate::tui::active() {
        // The last line of the prompt is the question itself; what is
        // above it is preamble the transcript already carries.
        let question = crate::tui::plain_text(prompt);
        let question = question
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .trim();
        let truncated = question.chars().count() > 80;
        let mut question: String = question.chars().take(80).collect();
        if truncated {
            question.push_str("...");
        }
        bail!("Keyboard prompts are not available in the TUI yet; unanswered: {question}"); // TASK-315
    }
    print!("{prompt}");
    stdout().flush()?;

    enable_raw_mode()?;

    let result = loop {
        if let Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })) = event::read()
        {
            match code {
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(anyhow::anyhow!("Interrupted"));
                }
                KeyCode::Char(c) if valid_chars.contains(&c) => {
                    // Invalid character, continue loop
                    break Ok(c);
                }
                KeyCode::Enter => {
                    break Ok(default);
                }
                _ => {
                    // Other keys are ignored, continue loop
                }
            }
        }
    };

    disable_raw_mode()?;

    // Print the chosen character and newline for clean output
    if let Ok(chosen) = &result {
        println!("{chosen}");
    }

    result
}
