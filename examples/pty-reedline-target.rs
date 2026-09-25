//! Process driven by `tests/pty_repl.rs` through a pseudo-terminal.
//!
//! It is the smallest program that exercises the same reedline machinery the REPL
//! relies on: a `Reedline` in raw mode with an `ExternalPrinter` attached, reading
//! exactly one line. Everything the tests need to observe is written as a plain
//! `KEY:value` line to stdout while the terminal is in cooked mode, so the tests
//! never have to parse the escape sequences reedline paints in between.
//!
//! Injection is driven by what the user types, not by time: when the buffer first
//! equals the marker text (`PTY_TARGET_INJECT_WHEN_BUFFER`), the line in
//! `PTY_TARGET_INJECT_LINE` is pushed through the external printer, exactly once.
//! Reedline calls the highlighter on every repaint, so the marker is seen on the
//! keystroke that completes it. Leaving the marker unset disables injection.
//!
//! This is a test fixture, not part of the shipped binary.

use std::borrow::Cow;
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};

use nu_ansi_term::Style;
use reedline::{
    ExternalPrinter, Highlighter, Prompt, PromptEditMode, PromptHistorySearch, Reedline, Signal,
    StyledText,
};

const INJECT_WHEN_BUFFER_ENV: &str = "PTY_TARGET_INJECT_WHEN_BUFFER";
const INJECT_LINE_ENV: &str = "PTY_TARGET_INJECT_LINE";

/// Bound on queued printer lines; one injection never comes near it.
const PRINTER_CAPACITY: usize = 8;

const PROMPT_LEFT: &str = "pty";
const PROMPT_INDICATOR: &str = "> ";

struct FixedPrompt;

impl Prompt for FixedPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(PROMPT_LEFT)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed(PROMPT_INDICATOR)
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("... ")
    }

    fn render_prompt_history_search_indicator(&self, _search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}

/// Highlighter that leaves the text unstyled and fires the injection when the
/// buffer matches the marker. The sender is held behind a closure so this file
/// never names the channel type reedline uses.
struct InjectOnMarker {
    marker: String,
    inject: Box<dyn Fn() + Send + Sync>,
    fired: AtomicBool,
}

impl Highlighter for InjectOnMarker {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        if line == self.marker && !self.fired.swap(true, Ordering::SeqCst) {
            (self.inject)();
        }
        let mut text = StyledText::new();
        text.push((Style::new(), line.to_string()));
        text
    }
}

fn main() {
    let printer = ExternalPrinter::<String>::new(PRINTER_CAPACITY);
    let mut editor = Reedline::create().with_external_printer(printer.clone());

    if let Ok(marker) = env::var(INJECT_WHEN_BUFFER_ENV) {
        let line = env::var(INJECT_LINE_ENV).unwrap_or_else(|_| "[mesh] injected".to_string());
        let sender = printer.sender();
        editor = editor.with_highlighter(Box::new(InjectOnMarker {
            marker,
            inject: Box::new(move || {
                // A full queue is a fixture bug, not something to block on.
                let _ = sender.try_send(line.clone());
            }),
            fired: AtomicBool::new(false),
        }));
    }

    println!("TARGET-READY");

    match editor.read_line(&FixedPrompt) {
        Ok(Signal::Success(buffer)) => println!("READ:{buffer}"),
        Ok(Signal::CtrlC) => println!("SIGNAL:ctrl-c"),
        Ok(Signal::CtrlD) => println!("SIGNAL:ctrl-d"),
        Ok(other) => println!("SIGNAL:{other:?}"),
        Err(err) => println!("ERROR:{err}"),
    }
}
