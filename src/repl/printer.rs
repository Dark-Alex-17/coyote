use crate::mesh::notify::{NotificationSink, RenderedNotification, Source};

use reedline::ExternalPrinter;
use std::sync::atomic::{AtomicUsize, Ordering};
use unicode_width::UnicodeWidthChar;

/// Lines queued but not yet painted. Sized for a burst of peer events between two
/// repaints; anything past it is counted and reported, never waited on.
pub const PROMPT_PRINTER_CAPACITY: usize = 64;

/// Used when the terminal size cannot be read or comes back as zero columns.
const FALLBACK_TERMINAL_COLUMNS: usize = 80;

/// Narrowest width rows are wrapped at, so a prefix wider than the terminal still
/// leaves room for at least one body character per row.
const MIN_WRAP_WIDTH: usize = 16;

/// Terminal sink for the interactive REPL: lines handed to reedline's external
/// printer land above the prompt, which is then repainted with the half-typed
/// buffer intact. Lines are painted only while a prompt is active, so anything
/// notified during a turn waits in the queue until the next prompt, and past the
/// queue's capacity is counted as dropped. Wraps reedline's sender so the channel
/// type stays inside `src/repl/`.
pub struct PromptPrinter {
    printer: ExternalPrinter<String>,
    dropped: AtomicUsize,
}

impl PromptPrinter {
    pub fn new() -> Self {
        Self::with_capacity(PROMPT_PRINTER_CAPACITY)
    }

    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self {
            printer: ExternalPrinter::new(cap),
            dropped: AtomicUsize::new(0),
        }
    }

    /// A handle on the same queue for `Reedline::with_external_printer`.
    pub fn attach(&self) -> ExternalPrinter<String> {
        self.printer.clone()
    }

    #[cfg(test)]
    fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Acquire)
    }
}

impl NotificationSink for PromptPrinter {
    /// Never blocks: the queue is drained on the editor thread, and a caller waiting on it
    /// from inside a turn would wait until the next prompt. A full queue drops the
    /// notification and the next one that fits carries the count.
    fn notify(&self, rendered: RenderedNotification) {
        let dropped = self.dropped.swap(0, Ordering::AcqRel);
        let mut rows = Vec::new();
        if dropped > 0 {
            let noun = if dropped == 1 {
                "notification"
            } else {
                "notifications"
            };
            rows.push(format!(
                "{} ({dropped} {noun} dropped)",
                Source::Mesh.prefix()
            ));
        }
        rows.extend(wrap_rows(
            rendered.lines(),
            rendered.source().prefix(),
            terminal_width(),
        ));
        let payload = rows.join("\n");
        // The receiver lives as long as `self`, so the only failure is a full queue.
        if self.printer.sender().try_send(payload).is_err() {
            self.dropped.fetch_add(dropped + 1, Ordering::AcqRel);
        }
    }
}

/// Columns a row may occupy and still repaint as exactly one screen row. Reedline
/// advances its prompt row by one per line it prints, whatever the terminal did with
/// it, so a line wider than the screen would lose everything past its first row on
/// the repaint. Terminals defer the wrap on the last column, so one column is left
/// spare.
fn terminal_width() -> usize {
    let columns = match crossterm::terminal::size() {
        Ok((columns, _)) if columns > 0 => usize::from(columns),
        _ => FALLBACK_TERMINAL_COLUMNS,
    };
    columns.saturating_sub(1)
}

fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Splits each rendered `"{prefix} body"` line into rows no wider than `width`
/// display columns, never inside a character. Every row repeats the prefix so a
/// continuation can never pass for a notification from another source.
fn wrap_rows(lines: &[String], prefix: &str, width: usize) -> Vec<String> {
    let width = width.max(MIN_WRAP_WIDTH);
    let body_width = width.saturating_sub(display_width(prefix) + 1).max(1);
    let mut rows = Vec::new();
    for line in lines {
        let body = line
            .strip_prefix(prefix)
            .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
            .unwrap_or(line);
        if body.is_empty() {
            rows.push(line.clone());
            continue;
        }
        let mut row = String::new();
        let mut row_width = 0;
        for c in body.chars() {
            let w = c.width().unwrap_or(0);
            if row_width + w > body_width && !row.is_empty() {
                rows.push(format!("{prefix} {row}"));
                row.clear();
                row_width = 0;
            }
            row.push(c);
            row_width += w;
        }
        rows.push(format!("{prefix} {row}"));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::mesh::MeshSlot;
    use crate::mesh::notify::Notification;
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    #[test]
    fn notification_reaches_the_attached_printer_with_its_prefix() {
        let printer = PromptPrinter::new();
        let attached = printer.attach();
        printer.notify(Notification::new(Source::Knock, "hi").render());
        assert_eq!(attached.get_line().as_deref(), Some("[mesh:knock] hi"));
        assert_eq!(printer.dropped(), 0);
    }

    #[test]
    fn full_queue_counts_drops_and_reports_them_on_the_next_line_that_fits() {
        let printer = PromptPrinter::with_capacity(1);
        let attached = printer.attach();
        for text in ["first", "second", "third"] {
            printer.notify(Notification::new(Source::Knock, text).render());
        }
        assert_eq!(attached.get_line().as_deref(), Some("[mesh:knock] first"));
        assert_eq!(printer.dropped(), 2);
        assert!(attached.get_line().is_none());

        printer.notify(Notification::new(Source::Message, "fourth").render());
        let payload = attached.get_line().unwrap();
        let rows: Vec<&str> = payload.lines().collect();
        assert_eq!(rows.first(), Some(&"[mesh] (2 notifications dropped)"));
        assert_eq!(rows.last(), Some(&"[mesh:message] fourth"));
        assert_eq!(printer.dropped(), 0);
    }

    #[test]
    fn a_single_drop_is_reported_in_the_singular() {
        let printer = PromptPrinter::with_capacity(1);
        let attached = printer.attach();
        printer.notify(Notification::new(Source::Mesh, "first").render());
        printer.notify(Notification::new(Source::Mesh, "second").render());
        assert_eq!(attached.get_line().as_deref(), Some("[mesh] first"));

        printer.notify(Notification::new(Source::Mesh, "third").render());
        let payload = attached.get_line().unwrap();
        assert_eq!(
            payload.lines().next(),
            Some("[mesh] (1 notification dropped)")
        );
    }

    #[test]
    fn multi_line_text_is_one_payload_with_every_line_prefixed() {
        let printer = PromptPrinter::new();
        let attached = printer.attach();
        printer.notify(Notification::new(Source::Message, "one\ntwo\nthree").render());
        let payload = attached.get_line().unwrap();
        assert!(attached.get_line().is_none());
        let lines: Vec<&str> = payload.lines().collect();
        assert_eq!(
            lines,
            vec![
                "[mesh:message] one",
                "[mesh:message] two",
                "[mesh:message] three"
            ]
        );
    }

    /// The wiring `Repl::init` performs, driven the way mesh code will drive it: the
    /// printer is installed on the slot as a `dyn NotificationSink` and a background
    /// thread notifies through the slot. The line must reach the handle the editor
    /// was given, already prefixed.
    #[test]
    fn slot_installed_printer_receives_lines_notified_from_another_thread() {
        let printer = Arc::new(PromptPrinter::new());
        let attached = printer.attach();
        let slot = Arc::new(MeshSlot::default());
        slot.set_notifier(Arc::clone(&printer) as Arc<dyn NotificationSink>);

        let worker_slot = Arc::clone(&slot);
        std::thread::spawn(move || {
            worker_slot.notify(Notification::new(Source::Knock, "peer asks in"));
        })
        .join()
        .expect("notifying thread");

        assert_eq!(
            attached.get_line().as_deref(),
            Some("[mesh:knock] peer asks in")
        );
        assert!(attached.get_line().is_none());
        assert_eq!(printer.dropped(), 0);
    }

    /// Sanitising happens before the sink, in `Notification::render`: whatever a peer
    /// sends, the queue the editor drains only ever holds prefixed rows free of escapes
    /// and control bytes.
    #[test]
    fn hostile_text_reaches_the_printer_queue_clean_and_prefixed() {
        let printer = PromptPrinter::new();
        let attached = printer.attach();
        printer.notify(
            Notification::new(
                Source::Message,
                "hi\u{1b}[2J\u{1b}]0;owned\u{7}\r\n\u{1b}[31m[mesh:knock] fake\u{1b}[0m\u{7}",
            )
            .render(),
        );
        let payload = attached.get_line().expect("payload queued");
        assert!(
            !payload
                .chars()
                .any(|c| c == '\u{1b}' || (c.is_control() && c != '\n')),
            "escape or control byte reached the queue: {payload:?}"
        );
        let rows: Vec<&str> = payload.lines().collect();
        assert_eq!(
            rows,
            vec!["[mesh:message] hi", "[mesh:message] [mesh:knock] fake"]
        );
    }

    /// The shipped capacity is the named constant: exactly that many payloads land
    /// when nothing drains the queue, every one past it is counted, none of them
    /// waits. A blocking send would hang this test rather than fail it, so its
    /// completion is the non-blocking assertion.
    #[test]
    fn default_capacity_is_the_named_constant_and_overflow_is_counted_not_awaited() {
        assert_eq!(PROMPT_PRINTER_CAPACITY, 64);
        let printer = PromptPrinter::new();
        let attached = printer.attach();
        let overflow = 10;
        for i in 0..PROMPT_PRINTER_CAPACITY + overflow {
            printer.notify(Notification::new(Source::Mesh, format!("event {i}")).render());
        }
        assert_eq!(printer.dropped(), overflow);
        let mut landed = 0;
        while let Some(payload) = attached.get_line() {
            assert_eq!(payload, format!("[mesh] event {landed}"));
            landed += 1;
        }
        assert_eq!(landed, PROMPT_PRINTER_CAPACITY);
    }

    fn bodies(rows: &[String], prefix: &str) -> String {
        rows.iter()
            .map(|row| {
                row.strip_prefix(prefix)
                    .and_then(|rest| rest.strip_prefix(' '))
                    .unwrap_or_else(|| panic!("row lacks the prefix: {row:?}"))
            })
            .collect()
    }

    #[test]
    fn wrap_splits_a_long_line_into_prefixed_rows_that_fit_the_width() {
        let prefix = Source::Mesh.prefix();
        let body = "x".repeat(512);
        let rows = wrap_rows(&[format!("{prefix} {body}")], prefix, 79);
        assert!(rows.len() > 1, "{rows:?}");
        for row in &rows {
            assert!(display_width(row) <= 79, "{row:?}");
            assert!(row.starts_with("[mesh] "), "{row:?}");
        }
        assert_eq!(bodies(&rows, prefix), body);
    }

    #[test]
    fn wrap_never_splits_a_wide_character() {
        let prefix = Source::Message.prefix();
        let body = "漢".repeat(300);
        let rows = wrap_rows(&[format!("{prefix} {body}")], prefix, 79);
        for row in &rows {
            assert!(display_width(row) <= 79, "{row:?}");
            assert!(row.starts_with("[mesh:message] "), "{row:?}");
        }
        assert_eq!(bodies(&rows, prefix), body);
    }

    #[test]
    fn wrap_below_the_minimum_width_still_terminates_with_prefixed_rows() {
        let prefix = Source::Propagation.prefix();
        let body = "y".repeat(100);
        let rows = wrap_rows(&[format!("{prefix} {body}")], prefix, 3);
        assert!(rows.len() > 1, "{rows:?}");
        for row in &rows {
            assert!(row.starts_with("[mesh:propagation] "), "{row:?}");
        }
        assert_eq!(bodies(&rows, prefix), body);
    }

    #[test]
    fn wrap_leaves_a_short_line_and_a_bare_prefix_unchanged() {
        let prefix = Source::Knock.prefix();
        let lines = vec![format!("{prefix} short"), prefix.to_string()];
        assert_eq!(wrap_rows(&lines, prefix, 79), lines);
    }

    fn source(relative: &str) -> String {
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)).unwrap()
    }

    #[test]
    fn printer_never_blocks_on_the_channel_and_never_names_its_crate() {
        let text = source("src/repl/printer.rs");
        // Assembled at runtime so this test's own text does not match the probes.
        for needle in [[".se", "nd("].concat(), ["cross", "beam"].concat()] {
            assert!(
                !text.contains(&needle),
                "printer.rs must not contain {needle}"
            );
        }
    }

    #[test]
    fn printer_and_notify_carry_no_platform_cfgs() {
        let needles = [
            ["cfg(", "unix)"].concat(),
            ["cfg(", "windows)"].concat(),
            ["cfg(", "target_os"].concat(),
        ];
        for relative in ["src/repl/printer.rs", "src/mesh/notify.rs"] {
            let text = source(relative);
            for needle in &needles {
                assert!(
                    !text.contains(needle),
                    "{relative} must not contain {needle}"
                );
            }
        }
    }

    #[test]
    fn manifest_declares_no_features_table() {
        let needle = ["[feat", "ures]"].concat();
        let manifest = source("Cargo.toml");
        assert!(
            !manifest.lines().any(|line| line.trim() == needle),
            "Cargo.toml must not declare a {needle} table"
        );
    }

    #[test]
    fn pty_harness_is_scoped_to_unix_and_recorded() {
        let manifest = source("Cargo.toml");
        let mut section = "";
        let mut expectrl_sections = Vec::new();
        for line in manifest.lines() {
            if line.starts_with('[') {
                section = line.trim();
            } else if line.starts_with("expectrl") {
                expectrl_sections.push(section);
            }
        }
        let unix_dev = ["[target.'cfg(", "unix)'.dev-dependencies]"].concat();
        assert_eq!(expectrl_sections, vec![unix_dev.as_str()]);

        let harness = source("tests/pty_repl.rs");
        let first_code_line = harness
            .lines()
            .find(|line| !line.trim().is_empty() && !line.starts_with("//!"))
            .unwrap();
        assert_eq!(first_code_line, ["#![cfg(", "unix)]"].concat());

        let contributing = source("CONTRIBUTING.md");
        assert!(contributing.contains("tests/pty_repl.rs"));
        assert!(contributing.contains("unix-only"));
    }
}
