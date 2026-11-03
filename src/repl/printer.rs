use crate::config::LivePrefix;
use crate::mesh::notify::{NotificationSink, RenderedNotification, Source};
use crate::utils::{NO_COLOR, tool_name_text_forced};

use parking_lot::Mutex;
use reedline::ExternalPrinter;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use unicode_width::UnicodeWidthChar;

/// Lines queued but not yet painted. Sized for a burst of peer events between two
/// repaints; anything past it is counted and reported, never waited on.
pub const PROMPT_PRINTER_CAPACITY: usize = 64;

const FALLBACK_TERMINAL_COLUMNS: usize = 80;

/// Narrowest width rows are wrapped at, so a prefix wider than the terminal still
/// leaves room for at least one body character per row.
const MIN_WRAP_WIDTH: usize = 16;

/// Terminal sink for the interactive REPL: lines handed to reedline's external
/// printer land above the prompt, which is then repainted with the half-typed
/// buffer intact. Lines are painted only while a prompt is active, so anything
/// notified during a turn waits in the queue until the next prompt, and past the
/// queue's capacity is counted as dropped. Holds the receiving end only until the
/// editor claims it, so the queue's channel type never leaves `src/repl/`.
pub struct PromptPrinter {
    sender: SyncSender<String>,
    /// Reedline owns the receiving end; `None` once `attach` has handed it over.
    unattached: Mutex<Option<ExternalPrinter<String>>>,
    dropped: AtomicUsize,
    /// Settled once at construction: the prefixes are painted in the tool-call name
    /// colour, or every row is plain text.
    colour: bool,
    /// Read per notification, so a `.set command_prefix` shows on the next line.
    command_prefix: LivePrefix,
}

impl PromptPrinter {
    pub fn new(command_prefix: LivePrefix) -> Self {
        Self::with_colour(PROMPT_PRINTER_CAPACITY, !*NO_COLOR, command_prefix)
    }

    pub(crate) fn with_colour(cap: usize, colour: bool, command_prefix: LivePrefix) -> Self {
        let printer = ExternalPrinter::new(cap);
        Self {
            sender: printer.sender(),
            unattached: Mutex::new(Some(printer)),
            dropped: AtomicUsize::new(0),
            colour,
            command_prefix,
        }
    }

    /// The receiving end of the queue for `Reedline::with_external_printer`. The
    /// queue has one receiver, so this hands it over exactly once.
    pub fn attach(&self) -> ExternalPrinter<String> {
        self.unattached
            .lock()
            .take()
            .expect("the prompt printer is attached to one editor")
    }

    #[cfg(test)]
    fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Acquire)
    }

    /// `row` with its bracketed prefix in the tool-call name colour when colour is on;
    /// the body keeps its bytes either way. Only a notification's first row carries
    /// the prefix, so continuation rows pass through as they are.
    fn paint_prefix(&self, row: String, prefix: &str) -> String {
        if !self.colour {
            return row;
        }
        match row.strip_prefix(prefix) {
            Some(body) => format!("{}{body}", tool_name_text_forced(prefix)),
            None => row,
        }
    }
}

impl NotificationSink for PromptPrinter {
    /// Never blocks: the queue is drained on the editor thread, and a caller waiting on it
    /// from inside a turn would wait until the next prompt. A full queue drops the
    /// notification and the next one that fits carries the count. Command hints are
    /// rewritten before the rows are wrapped, so the widths measured are the ones printed.
    fn notify(&self, rendered: RenderedNotification) {
        let dropped = self.dropped.swap(0, Ordering::AcqRel);
        let mut rows = Vec::new();
        if dropped > 0 {
            let noun = if dropped == 1 {
                "notification"
            } else {
                "notifications"
            };
            let prefix = Source::Mesh.prefix();
            rows.push(self.paint_prefix(format!("{prefix} ({dropped} {noun} dropped)"), prefix));
        }
        let prefix = rendered.source().prefix();
        let lines = rendered.render_notification_lines(self.command_prefix.load());
        rows.extend(
            wrap_rows(&lines, prefix, terminal_width())
                .into_iter()
                .map(|row| self.paint_prefix(row, prefix)),
        );
        let payload = rows.join("\n");
        // A full queue and a dropped receiver (no editor attached any more) both
        // count as a dropped notification; neither is waited on.
        if self.sender.try_send(payload).is_err() {
            self.dropped.fetch_add(dropped + 1, Ordering::AcqRel);
        }
    }
}

/// Columns a row may occupy and still repaint as exactly one screen row. Reedline
/// advances its prompt row by one per line it prints, whatever the terminal did with
/// it, so a line wider than the screen would lose everything past its first row on
/// the repaint. Terminals defer the wrap on the last column, so one column is left
/// spare.
pub(super) fn terminal_width() -> usize {
    let columns = match crossterm::terminal::size() {
        Ok((columns, _)) if columns > 0 => usize::from(columns),
        _ => FALLBACK_TERMINAL_COLUMNS,
    };
    columns.saturating_sub(1)
}

fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Lays one notification's rendered `"{prefix} body"` lines out as rows no wider than
/// `width` display columns. The prefix appears once, on the first row; every later
/// row, whether it continues a wrapped line or starts the next line, is indented by
/// the prefix width plus one. Column 0 of every row is therefore the prefix or a
/// space, never peer text, so a body cannot pass for a notification from another
/// source. Rows break at spaces; only a word wider than a whole row is split, and
/// never inside a character.
fn wrap_rows(lines: &[String], prefix: &str, width: usize) -> Vec<String> {
    let width = width.max(MIN_WRAP_WIDTH);
    let indent = " ".repeat(display_width(prefix) + 1);
    let body_width = width.saturating_sub(indent.len()).max(1);
    lines
        .iter()
        .flat_map(|line| {
            let body = line
                .strip_prefix(prefix)
                .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
                .unwrap_or(line);
            wrap_body(body, body_width)
        })
        .enumerate()
        .map(|(index, body)| match (index, body.is_empty()) {
            (0, true) => prefix.to_string(),
            (0, false) => format!("{prefix} {body}"),
            (_, true) => body,
            (_, false) => format!("{indent}{body}"),
        })
        .collect()
}

/// `body` broken at spaces into pieces no wider than `width` columns; the space a
/// break lands on is dropped, and so are the spaces that would open the next piece, so
/// a run of spaces never yields a blank row. A word wider than `width` starts its own
/// piece and is split between characters; its last piece may be followed by the next
/// words.
fn wrap_body(body: &str, width: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut piece_width = 0;
    let mut opens_a_row = false;
    for (index, word) in body.split(' ').enumerate() {
        if opens_a_row && word.is_empty() {
            continue;
        }
        let word_width = display_width(word);
        if index > 0 && !opens_a_row {
            if piece_width + 1 + word_width <= width {
                piece.push(' ');
                piece.push_str(word);
                piece_width += 1 + word_width;
                continue;
            }
            let kept = piece.trim_end();
            if !kept.is_empty() {
                pieces.push(kept.to_string());
            }
            piece.clear();
            piece_width = 0;
            if word.is_empty() {
                opens_a_row = true;
                continue;
            }
        }
        opens_a_row = false;
        for c in word.chars() {
            let w = c.width().unwrap_or(0);
            if piece_width + w > width && !piece.is_empty() {
                pieces.push(std::mem::take(&mut piece));
                piece_width = 0;
            }
            piece.push(c);
            piece_width += w;
        }
    }
    if pieces.is_empty() || !piece.is_empty() {
        pieces.push(piece);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::mesh::MeshSlot;
    use crate::mesh::notify::{NOTIFICATION_LINE_MAX_CHARS, Notification};
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    fn plain(cap: usize) -> PromptPrinter {
        PromptPrinter::with_colour(cap, false, LivePrefix::default())
    }

    fn painted(cap: usize) -> PromptPrinter {
        PromptPrinter::with_colour(cap, true, LivePrefix::default())
    }

    mod command_prefix_sinks {
        use super::*;
        use crate::config::CommandPrefix;

        const PREFIXES: [(CommandPrefix, &str); 3] = [
            (CommandPrefix::Dot, ".mesh"),
            (CommandPrefix::Slash, "/mesh"),
            (CommandPrefix::Both, ".mesh"),
        ];

        fn printer_under(prefix: CommandPrefix) -> PromptPrinter {
            let live = LivePrefix::default();
            live.store(prefix);
            PromptPrinter::with_colour(PROMPT_PRINTER_CAPACITY, false, live)
        }

        /// The payload's rows rejoined into one body, so an assertion holds at whatever
        /// width the terminal running the test wraps them to. The bodies here are words,
        /// so every break fell on a space.
        fn unwrapped(payload: &str, prefix: &str) -> String {
            let rows: Vec<String> = payload.lines().map(str::to_string).collect();
            bodies(&rows, prefix).join(" ")
        }

        /// The idle lines a person acts on: a knock, an access request, and the envoy
        /// escalating a question to them.
        #[test]
        fn idle_lines_spell_their_hints_in_the_prefix() {
            for (prefix, mesh) in PREFIXES {
                let printer = printer_under(prefix);
                let attached = printer.attach();
                for (source, text, expected) in [
                    (
                        Source::Knock,
                        "Tia knocked; `.mesh trust 1a2b3c4d` lets it in",
                        format!("Tia knocked; `{mesh} trust 1a2b3c4d` lets it in"),
                    ),
                    (
                        Source::Access,
                        "Tia asks for 2 paths; `.mesh pending` lists it",
                        format!("Tia asks for 2 paths; `{mesh} pending` lists it"),
                    ),
                    (
                        Source::Message,
                        "envoy escalated to the human; answer with `.mesh answer 1a2b3c4d`",
                        format!(
                            "envoy escalated to the human; answer with `{mesh} answer 1a2b3c4d`"
                        ),
                    ),
                ] {
                    printer.notify(Notification::new(source, text).render());
                    let payload = attached.get_line().expect("payload queued");
                    assert_eq!(unwrapped(&payload, source.prefix()), expected, "{prefix}");
                }
            }
        }

        /// The hint is rewritten before the rows are measured, so a wrapped line keeps
        /// the width bound and carries the rendered hint, never the typed one.
        #[test]
        fn a_wrapped_line_is_rendered_before_it_is_measured() {
            let printer = printer_under(CommandPrefix::Slash);
            let attached = printer.attach();
            let body = format!("{}then `.mesh trust 1a2b3c4d`", "word ".repeat(90));
            printer.notify(Notification::new(Source::Knock, body.as_str()).render());
            let payload = attached.get_line().expect("payload queued");
            let rows: Vec<String> = payload.lines().map(str::to_string).collect();
            assert!(rows.len() > 1, "{rows:?}");
            for row in &rows {
                assert!(
                    display_width(row) <= terminal_width().max(MIN_WRAP_WIDTH),
                    "{row:?}"
                );
            }
            assert_eq!(
                bodies(&rows, Source::Knock.prefix()).join(" "),
                body.replace(".mesh", "/mesh")
            );
        }

        #[test]
        fn the_dropped_count_row_is_left_as_it_is() {
            let printer = PromptPrinter::with_colour(1, false, {
                let live = LivePrefix::default();
                live.store(CommandPrefix::Slash);
                live
            });
            let attached = printer.attach();
            printer.notify(Notification::new(Source::Knock, "first").render());
            printer.notify(Notification::new(Source::Knock, "dropped").render());
            assert_eq!(attached.get_line().as_deref(), Some("[mesh:knock] first"));
            printer.notify(Notification::new(Source::Knock, "`.mesh knocks`").render());
            assert_eq!(
                attached.get_line().as_deref(),
                Some("[mesh] (1 notification dropped)\n[mesh:knock] `/mesh knocks`")
            );
        }
    }

    #[test]
    fn notification_reaches_the_attached_printer_with_its_prefix() {
        let printer = plain(PROMPT_PRINTER_CAPACITY);
        let attached = printer.attach();
        printer.notify(Notification::new(Source::Knock, "hi").render());
        assert_eq!(attached.get_line().as_deref(), Some("[mesh:knock] hi"));
        assert_eq!(printer.dropped(), 0);
    }

    /// The shipped constructor settles colour from the process-wide decision, so the
    /// row is the plain snapshot or the painted one by `NO_COLOR` alone.
    #[test]
    fn new_follows_the_process_colour_decision() {
        let printer = PromptPrinter::new(LivePrefix::default());
        let attached = printer.attach();
        printer.notify(Notification::new(Source::Knock, "hi").render());
        let expected = if *NO_COLOR {
            "[mesh:knock] hi".to_string()
        } else {
            format!("{} hi", tool_name_text_forced("[mesh:knock]"))
        };
        assert_eq!(attached.get_line(), Some(expected));
    }

    #[test]
    fn colour_on_paints_only_the_prefix() {
        let printer = painted(PROMPT_PRINTER_CAPACITY);
        let attached = printer.attach();
        printer.notify(Notification::new(Source::Knock, "hi").render());
        let row = attached.get_line().unwrap();
        assert_eq!(row, format!("{} hi", tool_name_text_forced("[mesh:knock]")));
        assert!(row.starts_with("\u{1b}["), "{row:?}");
        assert!(row.contains("[mesh:knock]\u{1b}[0m hi"), "{row:?}");
    }

    /// Only the prefix the source owns is painted: a body that opens with another
    /// source's prefix keeps it as plain text, so peer text cannot pose as a knock.
    #[test]
    fn colour_on_leaves_a_forged_prefix_in_the_body_unpainted() {
        let printer = painted(PROMPT_PRINTER_CAPACITY);
        let attached = printer.attach();
        printer.notify(Notification::new(Source::Message, "[mesh:knock] fake").render());
        let row = attached.get_line().unwrap();
        assert_eq!(
            row,
            format!(
                "{} [mesh:knock] fake",
                tool_name_text_forced("[mesh:message]")
            )
        );
    }

    #[test]
    fn colour_on_paints_the_one_prefix_of_a_wrapped_line_and_leaves_the_body_whole() {
        let printer = painted(PROMPT_PRINTER_CAPACITY);
        let attached = printer.attach();
        let body = "x".repeat(NOTIFICATION_LINE_MAX_CHARS);
        printer.notify(Notification::new(Source::Mesh, body.clone()).render());
        let payload = attached.get_line().unwrap();
        let rows: Vec<String> = payload.lines().map(str::to_string).collect();
        let prefix = tool_name_text_forced("[mesh]");
        assert!(rows.len() > 1, "{rows:?}");
        assert!(rows[0].starts_with(&prefix), "{:?}", rows[0]);
        for row in &rows[1..] {
            assert!(!row.contains('\u{1b}'), "{row:?}");
        }
        assert_eq!(bodies(&rows, &prefix).concat(), body);
    }

    #[test]
    fn colour_on_paints_the_dropped_count_row() {
        let printer = painted(1);
        let attached = printer.attach();
        printer.notify(Notification::new(Source::Mesh, "first").render());
        printer.notify(Notification::new(Source::Mesh, "second").render());
        assert_eq!(
            attached.get_line(),
            Some(format!("{} first", tool_name_text_forced("[mesh]")))
        );

        printer.notify(Notification::new(Source::Mesh, "third").render());
        let payload = attached.get_line().unwrap();
        let rows: Vec<String> = payload.lines().map(str::to_string).collect();
        assert_eq!(
            rows,
            vec![
                format!(
                    "{} (1 notification dropped)",
                    tool_name_text_forced("[mesh]")
                ),
                format!("{} third", tool_name_text_forced("[mesh]")),
            ]
        );
    }

    #[test]
    fn colour_off_rows_carry_no_escape() {
        let printer = plain(1);
        let attached = printer.attach();
        printer.notify(Notification::new(Source::Mesh, "first").render());
        printer.notify(Notification::new(Source::Mesh, "second").render());
        assert_eq!(attached.get_line().as_deref(), Some("[mesh] first"));
        printer.notify(Notification::new(Source::Mesh, "third").render());
        assert_eq!(
            attached.get_line().as_deref(),
            Some("[mesh] (1 notification dropped)\n[mesh] third")
        );
    }

    #[test]
    fn full_queue_counts_drops_and_reports_them_on_the_next_line_that_fits() {
        let printer = plain(1);
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
        let printer = plain(1);
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
    fn multi_line_text_is_one_payload_with_one_prefix() {
        let printer = plain(PROMPT_PRINTER_CAPACITY);
        let attached = printer.attach();
        printer.notify(Notification::new(Source::Message, "one\ntwo\nthree").render());
        let payload = attached.get_line().unwrap();
        assert!(attached.get_line().is_none());
        let lines: Vec<&str> = payload.lines().collect();
        assert_eq!(
            lines,
            vec![
                "[mesh:message] one",
                "               two",
                "               three"
            ]
        );
    }

    /// The wiring `Repl::init` performs, driven the way mesh code drives it: the printer
    /// is installed on the slot as a `dyn NotificationSink` and a background thread
    /// notifies through the slot. The line reaches the handle the editor holds, already
    /// prefixed.
    #[test]
    fn slot_installed_printer_receives_lines_notified_from_another_thread() {
        let printer = Arc::new(plain(PROMPT_PRINTER_CAPACITY));
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
        let printer = plain(PROMPT_PRINTER_CAPACITY);
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
            vec!["[mesh:message] hi", "               [mesh:knock] fake"]
        );
        assert!(
            rows.iter().skip(1).all(|row| !row.starts_with('[')),
            "{rows:?}"
        );
    }

    /// The shipped capacity is the named constant: exactly that many payloads land
    /// when nothing drains the queue, every one past it is counted, none of them
    /// waits. A blocking send would hang this test rather than fail it, so its
    /// completion is the non-blocking assertion.
    #[test]
    fn default_capacity_is_the_named_constant_and_overflow_is_counted_not_awaited() {
        assert_eq!(PROMPT_PRINTER_CAPACITY, 64);
        let printer = plain(PROMPT_PRINTER_CAPACITY);
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

    /// Each row's body: the first row's after the prefix, every later row's after the
    /// indent. Panics on a row that has neither, so a caller also checks the layout.
    /// `prefix` may be painted; the indent follows its visible width.
    fn bodies<'a>(rows: &'a [String], prefix: &str) -> Vec<&'a str> {
        let indent = " ".repeat(display_width(&strip_escapes(prefix)) + 1);
        rows.iter()
            .enumerate()
            .map(|(index, row)| {
                let body = if index == 0 {
                    row.strip_prefix(prefix)
                        .and_then(|rest| rest.strip_prefix(' '))
                } else {
                    row.strip_prefix(indent.as_str())
                };
                body.unwrap_or_else(|| panic!("row {index} is out of layout: {row:?}"))
            })
            .collect()
    }

    fn strip_escapes(painted: &str) -> String {
        let mut plain = String::new();
        let mut chars = painted.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                chars.by_ref().find(|c| c.is_ascii_alphabetic());
            } else {
                plain.push(c);
            }
        }
        plain
    }

    #[test]
    fn wrap_hard_splits_a_word_wider_than_a_row_under_one_prefix() {
        let prefix = Source::Mesh.prefix();
        let body = "x".repeat(512);
        let rows = wrap_rows(&[format!("{prefix} {body}")], prefix, 79);
        assert!(rows.len() > 1, "{rows:?}");
        for row in &rows {
            assert!(display_width(row) <= 79, "{row:?}");
        }
        assert!(rows[0].starts_with("[mesh] "), "{rows:?}");
        assert_eq!(bodies(&rows, prefix).concat(), body);
    }

    #[test]
    fn wrap_never_splits_a_wide_character() {
        let prefix = Source::Message.prefix();
        let body = "漢".repeat(300);
        let rows = wrap_rows(&[format!("{prefix} {body}")], prefix, 79);
        for row in &rows {
            assert!(display_width(row) <= 79, "{row:?}");
        }
        assert_eq!(bodies(&rows, prefix).concat(), body);
    }

    #[test]
    fn wrap_below_the_minimum_width_still_terminates_under_one_prefix() {
        let prefix = Source::Propagation.prefix();
        let body = "y".repeat(100);
        let rows = wrap_rows(&[format!("{prefix} {body}")], prefix, 3);
        assert!(rows.len() > 1, "{rows:?}");
        assert_eq!(bodies(&rows, prefix).concat(), body);
    }

    #[test]
    fn wrap_leaves_a_short_line_and_a_bare_prefix_unchanged() {
        let prefix = Source::Knock.prefix();
        for lines in [vec![format!("{prefix} short")], vec![prefix.to_string()]] {
            assert_eq!(wrap_rows(&lines, prefix, 79), lines);
        }
    }

    #[test]
    fn wrap_turns_no_run_of_spaces_into_a_blank_row_or_a_leading_space() {
        assert_eq!(wrap_body(" abcdefgh", 5), ["abcde", "fgh"]);
        assert_eq!(wrap_body("abc      def", 4), ["abc", "def"]);
        let prefix = Source::Mesh.prefix();
        assert_eq!(
            wrap_rows(
                &[format!("{prefix} one  two   three    four")],
                prefix,
                MIN_WRAP_WIDTH
            ),
            ["[mesh] one  two", "       three", "       four"]
        );
    }

    mod idle_wrap_tests {
        use super::*;
        use crate::config::mesh_envoy::escalation_line;

        const MESSAGE_ID: &str = "0123456789abcdef0123456789abcdef";

        /// A question long enough to be cut, so the line carries the pending pointer
        /// and the whole answer command after the peer's words.
        fn escalated() -> String {
            let question = (0..30)
                .map(|i| format!("word{i:07}"))
                .collect::<Vec<_>>()
                .join(" ");
            escalation_line("Tia", &question, MESSAGE_ID)
        }

        fn rendered_rows(source: Source, text: &str, width: usize) -> Vec<String> {
            let rendered = Notification::new(source, text).render();
            wrap_rows(rendered.lines(), source.prefix(), width)
        }

        #[test]
        fn an_escalation_line_wraps_on_words_under_one_prefix_at_common_widths() {
            let line = escalated();
            assert!(line.contains("(full text: .mesh pending);"), "{line}");
            let prefix = Source::Message.prefix();
            let indent = " ".repeat(display_width(prefix) + 1);
            for width in [80, 120, 200] {
                let rows = rendered_rows(Source::Message, &line, width);
                assert!(rows.len() > 1, "{width}: {rows:?}");
                assert_eq!(rows.concat().matches(prefix).count(), 1, "{rows:?}");
                assert!(rows[0].starts_with(&format!("{prefix} ")), "{rows:?}");
                for row in &rows[1..] {
                    assert!(row.starts_with(&indent), "{row:?}");
                    assert!(!row[indent.len()..].starts_with(' '), "{row:?}");
                }
                for row in &rows {
                    assert!(display_width(row) <= width, "{width}: {row:?}");
                }
                let tokens: Vec<&str> = bodies(&rows, prefix)
                    .iter()
                    .flat_map(|body| body.split_whitespace())
                    .collect();
                assert_eq!(tokens, line.split_whitespace().collect::<Vec<_>>());
                assert!(tokens.contains(&"pending);"), "{tokens:?}");
                assert!(tokens.contains(&MESSAGE_ID), "{tokens:?}");
            }
        }

        #[test]
        fn a_two_line_knock_prints_one_prefix() {
            let identity = "cd".repeat(16);
            let destination = "ab".repeat(16);
            let text = format!(
                "\"Tia\" (identity {identity}) knocks from instance {destination}\ntrust: .mesh trust {destination} | who: .mesh info {destination} | silence: .mesh block --identity {identity}"
            );
            let prefix = Source::Knock.prefix();
            let rows = rendered_rows(Source::Knock, &text, 200);
            assert_eq!(
                rows,
                vec![
                    format!(
                        "{prefix} \"Tia\" (identity {identity}) knocks from instance {destination}"
                    ),
                    format!(
                        "{} trust: .mesh trust {destination} | who: .mesh info {destination} | silence: .mesh block --identity {identity}",
                        " ".repeat(display_width(prefix))
                    ),
                ]
            );
        }
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
    fn pty_harness_is_scoped_to_unix() {
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

        let harness = source("tests/mesh_autostart_repl.rs");
        let first_code_line = harness
            .lines()
            .find(|line| !line.trim().is_empty() && !line.starts_with("//!"))
            .unwrap();
        assert_eq!(first_code_line, ["#![cfg(", "unix)]"].concat());

        let platform_note = harness
            .lines()
            .take_while(|line| line.starts_with("//!"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(platform_note.contains("Unix only"));
        assert!(platform_note.contains("ConPTY"));
    }

    /// The whole idle path, as the REPL wires it: an over-budget ask the envoy
    /// escalated is recorded on the slot, the real idle driver takes it off the slot's
    /// idle sink, renders it, and the painted printer hands it to the editor. The
    /// first row carries the painted prefix and the rest are plain indented rows,
    /// however many the terminal width wraps the line into; the bodies joined back are
    /// the one composed line, whose peer words stop on a word within the budget and
    /// whose tail (`is in the inbox` and the pointer) sits whole after them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cut_peer_ask_reaches_the_editor_painted_with_its_tail_whole() {
        use crate::config::{AppState, RequestContext, WorkingMode};
        use crate::mesh::message::{PeerKind, PeerMessage, PeerVia};
        use crate::repl::idle::IdleDriver;
        use parking_lot::RwLock;
        use std::time::{Duration, Instant};

        let ctx = Arc::new(RwLock::new(RequestContext::new(
            Arc::new(AppState::test_default()),
            WorkingMode::Cmd,
        )));
        let app = Arc::clone(&ctx.try_read().unwrap().app);
        let printer = Arc::new(painted(PROMPT_PRINTER_CAPACITY));
        let attached = printer.attach();
        app.mesh
            .set_notifier(Arc::clone(&printer) as Arc<dyn NotificationSink>);
        let driver = IdleDriver::start(Arc::clone(&ctx), Arc::clone(&app));

        // Eleven-letter words: the 160th character falls inside the fourteenth word,
        // so a cut on the budget alone and a cut on the last word boundary differ.
        let words = (0..30)
            .map(|i| format!("word{i:07}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(words.chars().count(), 359);
        assert_eq!(words.chars().nth(160), Some('0'));
        let instance = "ab".repeat(16);
        let original = PeerMessage {
            source_identity: "cd".repeat(16),
            source_destination: instance.clone(),
            destination: "01".repeat(16),
            title: None,
            content: words,
            fields: None,
            timestamp: 1_700_000_000.0,
            message_id: "a-1".to_string(),
            in_reply_to: None,
            kind: PeerKind::Ask,
            via: PeerVia::Direct,
            thread: None,
            disposition: None,
            retry_after: None,
            parts: Vec::new(),
            dropped_parts: 0,
        };
        app.mesh.record_envoy_escalated(original, "a-1");

        let deadline = Instant::now() + Duration::from_secs(5);
        let payload = loop {
            if let Some(payload) = attached.get_line() {
                break payload;
            }
            assert!(
                Instant::now() < deadline,
                "the idle line never reached the editor"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        driver.stop().await;

        let prefix = tool_name_text_forced(Source::Message.prefix());
        assert!(prefix.contains("\u{1b}["), "{prefix:?}");
        let rows: Vec<String> = payload.lines().map(str::to_string).collect();
        assert!(rows[0].starts_with(&prefix), "{:?}", rows[0]);
        for body in bodies(&rows, &prefix) {
            assert!(
                !body.contains('\u{1b}'),
                "only the prefix is painted: {body:?}"
            );
        }
        let line = bodies(&rows, &prefix).join(" ");
        let kept = (0..13)
            .map(|i| format!("word{i:07}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(kept.chars().count(), 155);
        assert_eq!(
            line,
            format!(
                "envoy escalated to the human; {} asks: {kept}... is in the inbox (full text: .mesh inbox)",
                &instance[..8]
            )
        );
        assert_eq!(line.matches(".mesh inbox").count(), 1);
        assert!(
            line.chars().count() > 160,
            "the tail sits outside the peer-text budget"
        );
        assert_eq!(printer.dropped(), 0);
    }

    /// The same path with colour settled off: the editor gets the same composed line
    /// in plain bytes, no escape anywhere in the payload. The peer's words are one
    /// run longer than a narrow terminal row, so the rows are compared to the line
    /// with the whitespace taken out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cut_peer_ask_reaches_the_editor_plain_when_colour_is_off() {
        use crate::config::{AppState, RequestContext, WorkingMode};
        use crate::mesh::message::{PeerKind, PeerMessage, PeerVia};
        use crate::repl::idle::IdleDriver;
        use parking_lot::RwLock;
        use std::time::{Duration, Instant};

        let ctx = Arc::new(RwLock::new(RequestContext::new(
            Arc::new(AppState::test_default()),
            WorkingMode::Cmd,
        )));
        let app = Arc::clone(&ctx.try_read().unwrap().app);
        let printer = Arc::new(plain(PROMPT_PRINTER_CAPACITY));
        let attached = printer.attach();
        app.mesh
            .set_notifier(Arc::clone(&printer) as Arc<dyn NotificationSink>);
        let driver = IdleDriver::start(Arc::clone(&ctx), Arc::clone(&app));

        let original = PeerMessage {
            source_identity: "cd".repeat(16),
            source_destination: "ab".repeat(16),
            destination: "01".repeat(16),
            title: None,
            content: "q".repeat(300),
            fields: None,
            timestamp: 1_700_000_000.0,
            message_id: "a-2".to_string(),
            in_reply_to: None,
            kind: PeerKind::Ask,
            via: PeerVia::Direct,
            thread: None,
            disposition: None,
            retry_after: None,
            parts: Vec::new(),
            dropped_parts: 0,
        };
        app.mesh.record_envoy_escalated(original, "a-2");

        let deadline = Instant::now() + Duration::from_secs(5);
        let payload = loop {
            if let Some(payload) = attached.get_line() {
                break payload;
            }
            assert!(
                Instant::now() < deadline,
                "the idle line never reached the editor"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        driver.stop().await;

        assert!(!payload.contains('\u{1b}'), "{payload:?}");
        let rows: Vec<String> = payload.lines().map(str::to_string).collect();
        let squeezed = |text: &str| text.split_whitespace().collect::<String>();
        assert_eq!(
            squeezed(&bodies(&rows, Source::Message.prefix()).join(" ")),
            squeezed(&format!(
                "envoy escalated to the human; abababab asks: {}... is in the inbox (full text: .mesh inbox)",
                "q".repeat(160)
            ))
        );
    }
}
