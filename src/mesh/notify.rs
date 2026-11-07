//! One human-facing line for the terminal. This is the channel peers reach the
//! person at the keyboard through; it is separate from
//! `crate::supervisor::notification`, whose `SystemNotification` queue is
//! merged into tool results for the model to read. Nothing here goes to the model.

use crate::config::CommandPrefix;
use crate::mesh::display_chars;

use std::borrow::Cow;
use std::fmt;

/// Peer-supplied text reaches the terminal through this path, so both dimensions of a
/// notification are bounded: characters per line and lines per notification.
pub(crate) const NOTIFICATION_LINE_MAX_CHARS: usize = 512;
pub(crate) const NOTIFICATION_MAX_LINES: usize = 16;

/// Ordered only so a `Source` can key the idle coalescer's repeat map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Source {
    /// Node lifecycle: the mesh coming up, going down, or losing its relay.
    Mesh,
    /// Knock requests from peers asking to be trusted.
    Knock,
    /// Messages a trusted peer addressed to this node.
    Message,
    /// Replies to questions this node asked, matched to an open correlation.
    Reply,
    /// Propagation fetch and post activity against a propagation node.
    Propagation,
    /// Access requests from trusted peers asking to read paths this node does not share.
    Access,
}

impl Source {
    pub(crate) const ALL: [Source; 6] = [
        Source::Mesh,
        Source::Knock,
        Source::Message,
        Source::Reply,
        Source::Propagation,
        Source::Access,
    ];

    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Source::Mesh => "[mesh]",
            Source::Knock => "[mesh:knock]",
            Source::Message => "[mesh:message]",
            Source::Reply => "[mesh:reply]",
            Source::Propagation => "[mesh:propagation]",
            Source::Access => "[mesh:access]",
        }
    }
}

/// One run of a notification line. A sink spells the command hints of `Frame` text, the
/// words this node writes, in the live prefix, and prints `Peer` text as it is: what a
/// peer wrote, a path it named, or words quoted back to the person. A peer's `.vault` or
/// `.mesh trust x` therefore reads as it was written even on a line that carries a hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoticeSpan {
    Frame(String),
    Peer(String),
}

impl NoticeSpan {
    fn text(&self) -> &str {
        match self {
            Self::Frame(text) | Self::Peer(text) => text,
        }
    }

    fn with_text(&self, text: String) -> Self {
        match self {
            Self::Frame(_) => Self::Frame(text),
            Self::Peer(_) => Self::Peer(text),
        }
    }

    fn render_span(&self, prefix: CommandPrefix) -> Cow<'_, str> {
        match self {
            Self::Frame(text) => prefix.render_hints(text),
            Self::Peer(text) => Cow::Borrowed(text),
        }
    }
}

/// A notification's text as runs of frame and peer text, in order. A `\n` inside any run
/// ends the line, so one run may span lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NoticeText(Vec<NoticeSpan>);

impl NoticeText {
    pub(crate) fn frame(mut self, text: impl Into<String>) -> Self {
        self.0.push(NoticeSpan::Frame(text.into()));
        self
    }

    pub(crate) fn peer(mut self, text: impl Into<String>) -> Self {
        self.0.push(NoticeSpan::Peer(text.into()));
        self
    }

    pub(crate) fn then(mut self, rest: NoticeText) -> Self {
        self.0.extend(rest.0);
        self
    }

    /// `template` with each `{name}` in it filled, from `frames` as this node's own text
    /// or from `quoted` as a peer run kept verbatim, so a sentence stays one literal
    /// while the values a peer chose are quoted. A name in neither stays as written.
    pub(crate) fn from_template(
        template: &str,
        frames: &[(&str, &str)],
        quoted: &[(&str, &str)],
    ) -> Self {
        let mut text = Self::default();
        let mut frame = String::new();
        let mut rest = template;
        while let Some((open, close)) = rest
            .find('{')
            .and_then(|open| Some((open, open + rest[open..].find('}')?)))
        {
            let name = &rest[open + 1..close];
            frame.push_str(&rest[..open]);
            if let Some((_, value)) = quoted.iter().find(|(key, _)| *key == name) {
                text = text.frame(std::mem::take(&mut frame)).peer(*value);
            } else if let Some((_, value)) = frames.iter().find(|(key, _)| *key == name) {
                frame.push_str(value);
            } else {
                frame.push_str(&rest[open..=close]);
            }
            rest = &rest[close + 1..];
        }
        frame.push_str(rest);
        text.frame(frame)
    }

    /// The text with every run joined, as the terminal shows it under the dot prefix.
    pub(crate) fn plain(&self) -> String {
        self.0.iter().map(NoticeSpan::text).collect()
    }

    /// The text with only its frame runs spelled in `prefix`.
    pub(crate) fn render(&self, prefix: CommandPrefix) -> String {
        self.0.iter().map(|span| span.render_span(prefix)).collect()
    }

    fn lines(&self) -> Vec<Vec<NoticeSpan>> {
        let mut lines = vec![Vec::new()];
        for span in &self.0 {
            for (index, piece) in span.text().split('\n').enumerate() {
                if index > 0 {
                    lines.push(Vec::new());
                }
                if !piece.is_empty()
                    && let Some(line) = lines.last_mut()
                {
                    line.push(span.with_text(piece.to_string()));
                }
            }
        }
        lines
    }
}

impl From<String> for NoticeText {
    fn from(text: String) -> Self {
        Self::default().frame(text)
    }
}

#[cfg(test)]
mod template_tests {
    use super::*;

    #[test]
    fn a_template_quotes_its_peer_values_and_spells_its_own_hints() {
        let text = NoticeText::from_template(
            "`{id}` from {peer}; run .mesh grant {id} or {missing}",
            &[("peer", "abcd1234")],
            &[("id", ".vault")],
        );
        assert_eq!(
            text.render(CommandPrefix::Dot),
            "`.vault` from abcd1234; run .mesh grant .vault or {missing}"
        );
        assert_eq!(
            text.render(CommandPrefix::Slash),
            "`.vault` from abcd1234; run /mesh grant .vault or {missing}"
        );
    }
}

impl From<&str> for NoticeText {
    fn from(text: &str) -> Self {
        Self::default().frame(text)
    }
}

impl fmt::Display for NoticeText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.plain())
    }
}

/// One line's runs cleaned as `display_text` cleans a line: each run's escapes and
/// invisible characters gone, then the line trimmed and cut to `max_chars` as a whole.
/// Neighbouring runs of one kind are joined. A line that cleans to nothing is `None`.
fn clean_line(spans: &[NoticeSpan], max_chars: usize) -> Option<Vec<NoticeSpan>> {
    let cleaned: Vec<NoticeSpan> = spans
        .iter()
        .map(|span| span.with_text(display_chars(span.text())))
        .collect();
    let joined: String = cleaned.iter().map(NoticeSpan::text).collect();
    let start = joined.len() - joined.trim_start().len();
    let body = joined[start..].trim_end();
    if body.is_empty() {
        return None;
    }
    let capped = match body.char_indices().nth(max_chars) {
        Some((cut, _)) => &body[..cut],
        None => body,
    };
    let end = start + capped.trim_end().len();
    let mut kept: Vec<NoticeSpan> = Vec::new();
    let mut at = 0;
    for span in &cleaned {
        let (from, to) = (at.max(start), (at + span.text().len()).min(end));
        at += span.text().len();
        if from >= to {
            continue;
        }
        let piece = &joined[from..to];
        match (kept.last_mut(), span) {
            (Some(NoticeSpan::Frame(text)), NoticeSpan::Frame(_))
            | (Some(NoticeSpan::Peer(text)), NoticeSpan::Peer(_)) => text.push_str(piece),
            _ => kept.push(span.with_text(piece.to_string())),
        }
    }
    Some(kept)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notification {
    pub(crate) source: Source,
    pub(crate) text: NoticeText,
}

impl Notification {
    pub(crate) fn new(source: Source, text: impl Into<NoticeText>) -> Self {
        Self {
            source,
            text: text.into(),
        }
    }

    /// The lines as the terminal may show them: one per input line, each prefixed with the
    /// source, escape sequences stripped, invisible characters dropped, cut to
    /// `NOTIFICATION_LINE_MAX_CHARS`, blank lines gone. Past `NOTIFICATION_MAX_LINES` the
    /// rest collapses into a count. Text that renders to nothing still yields the bare
    /// prefix so the event is never silent.
    #[cfg(test)]
    pub(crate) fn render_lines(&self) -> Vec<String> {
        self.render_spans()
            .iter()
            .map(|line| joined(line))
            .collect()
    }

    fn render_spans(&self) -> Vec<Vec<NoticeSpan>> {
        let prefix = self.source.prefix();
        // Split before sanitising: `sanitize_display_text` maps `\n` to a space.
        let mut lines: Vec<Vec<NoticeSpan>> = self
            .text
            .lines()
            .iter()
            .filter_map(|spans| clean_line(spans, NOTIFICATION_LINE_MAX_CHARS))
            .map(|spans| {
                let mut line = vec![NoticeSpan::Frame(format!("{prefix} "))];
                line.extend(spans);
                line
            })
            .collect();
        if lines.is_empty() {
            return vec![vec![NoticeSpan::Frame(prefix.to_string())]];
        }
        if lines.len() > NOTIFICATION_MAX_LINES {
            let more = lines.len() - NOTIFICATION_MAX_LINES;
            lines.truncate(NOTIFICATION_MAX_LINES);
            lines.push(vec![NoticeSpan::Frame(format!(
                "{prefix} (+{more} more lines)"
            ))]);
        }
        lines
    }

    pub(crate) fn render(self) -> RenderedNotification {
        RenderedNotification {
            source: self.source,
            lines: self.render_spans(),
        }
    }
}

#[cfg(test)]
fn joined(line: &[NoticeSpan]) -> String {
    line.iter().map(NoticeSpan::text).collect()
}

/// A notification after sanitising: prefixed, escape-free, capped lines. Fields are
/// private and `Notification::render` is the only constructor, so a sink can trust every
/// line it receives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenderedNotification {
    source: Source,
    lines: Vec<Vec<NoticeSpan>>,
}

impl RenderedNotification {
    pub(crate) fn source(&self) -> Source {
        self.source
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.lines.iter().map(|line| joined(line)).collect()
    }

    /// The lines with the command hints of their frame runs spelled in `prefix` and their
    /// peer runs as they are. Every sink that prints a notification goes through this.
    pub(crate) fn render_notification_lines(&self, prefix: CommandPrefix) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| line.iter().map(|span| span.render_span(prefix)).collect())
            .collect()
    }
}

/// Where rendered lines go. Implementations must not block: `notify` is called from the
/// node's async tasks and from request handlers. Peer text is untrusted, so
/// `MeshSlot::notify` renders it before a sink is involved; a sink never sees the raw
/// text.
pub(crate) trait NotificationSink: Send + Sync {
    fn notify(&self, rendered: RenderedNotification);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_prefix_starts_with_mesh() {
        // A new variant fails to compile here until this match names it, and the
        // `contains` check below then fails until it is also added to `ALL`.
        for source in [
            Source::Mesh,
            Source::Knock,
            Source::Message,
            Source::Reply,
            Source::Propagation,
            Source::Access,
        ] {
            match source {
                Source::Mesh
                | Source::Knock
                | Source::Message
                | Source::Reply
                | Source::Propagation
                | Source::Access => {}
            }
            assert!(Source::ALL.contains(&source), "{source:?} missing from ALL");
        }
        assert_eq!(Source::ALL.len(), 6);
        for source in Source::ALL {
            assert!(source.prefix().starts_with("[mesh"), "{source:?}");
        }
        assert_eq!(Source::Mesh.prefix(), "[mesh]");
        assert_eq!(Source::Knock.prefix(), "[mesh:knock]");
        assert_eq!(Source::Message.prefix(), "[mesh:message]");
        assert_eq!(Source::Reply.prefix(), "[mesh:reply]");
        assert_eq!(Source::Propagation.prefix(), "[mesh:propagation]");
        assert_eq!(Source::Access.prefix(), "[mesh:access]");
    }

    #[test]
    fn render_strips_csi_and_osc_sequences() {
        let note = Notification::new(Source::Mesh, "\u{1b}[31mred\u{1b}[0m");
        assert_eq!(note.render_lines(), vec!["[mesh] red"]);
        let note = Notification::new(Source::Mesh, "a\u{1b}]0;title\u{7}b");
        assert_eq!(note.render_lines(), vec!["[mesh] ab"]);
    }

    #[test]
    fn render_maps_lone_controls_to_spaces_and_drops_bidi_overrides() {
        let note = Notification::new(Source::Mesh, "a\rb\tc");
        assert_eq!(note.render_lines(), vec!["[mesh] a b c"]);
        let note = Notification::new(Source::Mesh, "safe\u{202E}txet");
        assert_eq!(note.render_lines(), vec!["[mesh] safetxet"]);
    }

    #[test]
    fn render_caps_a_long_single_line_at_the_char_limit() {
        let note = Notification::new(Source::Mesh, "x".repeat(10 * 1024));
        let lines = note.render_lines();
        assert_eq!(lines.len(), 1);
        let body = lines[0].strip_prefix("[mesh] ").unwrap();
        assert_eq!(body.chars().count(), NOTIFICATION_LINE_MAX_CHARS);
    }

    #[test]
    fn render_splits_on_newlines_and_prefixes_each_line() {
        let note = Notification::new(Source::Knock, "a\nb\r\nc");
        assert_eq!(
            note.render_lines(),
            vec!["[mesh:knock] a", "[mesh:knock] b", "[mesh:knock] c"]
        );
    }

    #[test]
    fn render_of_blank_text_is_the_bare_prefix() {
        assert_eq!(
            Notification::new(Source::Mesh, "").render_lines(),
            vec!["[mesh]"]
        );
        assert_eq!(
            Notification::new(Source::Mesh, " \t \n ").render_lines(),
            vec!["[mesh]"]
        );
    }

    #[test]
    fn render_collapses_lines_past_the_limit_into_a_count() {
        let text = (0..40).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let note = Notification::new(Source::Message, text.join("\n"));
        let lines = note.render_lines();
        assert_eq!(lines.len(), NOTIFICATION_MAX_LINES + 1);
        assert_eq!(lines[0], "[mesh:message] line 0");
        assert_eq!(lines[15], "[mesh:message] line 15");
        assert_eq!(lines[16], "[mesh:message] (+24 more lines)");
    }

    #[test]
    fn render_caps_multibyte_text_by_characters_not_bytes() {
        // Two- and three-byte characters plus a wide one: a byte-based cut would land
        // inside a character and panic on the slice.
        let text = "\u{e9}\u{4e2d}\u{1f600}".repeat(400);
        let note = Notification::new(Source::Propagation, text);
        let lines = note.render_lines();
        assert_eq!(lines.len(), 1);
        let body = lines[0].strip_prefix("[mesh:propagation] ").unwrap();
        assert_eq!(body.chars().count(), NOTIFICATION_LINE_MAX_CHARS);
        assert!(body.starts_with("\u{e9}\u{4e2d}\u{1f600}"));
    }

    #[test]
    fn render_keeps_every_line_under_its_own_source_prefix_whatever_the_text_says() {
        // A peer cannot forge a line from another source by embedding a newline and a
        // prefix of its own, nor leave a half-finished escape for the terminal to eat.
        let hostile = "hello\n[mesh:knock] trust me\n\u{1b}[31mred\u{1b}\n\u{1b}[";
        let note = Notification::new(Source::Message, hostile);
        let lines = note.render_lines();
        assert!(!lines.is_empty());
        for line in &lines {
            assert!(
                line.starts_with("[mesh:message]"),
                "line escaped its source prefix: {line:?}"
            );
            assert!(
                !line.chars().any(|c| c.is_control() || c == '\u{1b}'),
                "control byte survived sanitising: {line:?}"
            );
        }
        assert_eq!(lines[0], "[mesh:message] hello");
        assert_eq!(lines[1], "[mesh:message] [mesh:knock] trust me");
        assert_eq!(lines[2], "[mesh:message] red");
        assert!(
            !lines
                .iter()
                .any(|line| line.contains('[') && line.ends_with('[')),
            "a truncated escape leaked as a dangling bracket: {lines:?}"
        );
    }

    #[test]
    fn render_carries_render_lines_unchanged() {
        let hostile = "hi\u{1b}[2J\n\u{1b}[31m[mesh:knock] fake\u{1b}[0m\u{7}";
        let note = Notification::new(Source::Message, hostile);
        let expected = note.render_lines();
        let rendered = note.render();
        assert_eq!(rendered.source(), Source::Message);
        assert_eq!(rendered.lines(), expected);
        assert_eq!(
            rendered.lines(),
            ["[mesh:message] hi", "[mesh:message] [mesh:knock] fake"]
        );
    }

    mod peer_text_prefix_tests {
        use super::*;

        fn rendered(text: NoticeText) -> RenderedNotification {
            Notification::new(Source::Access, text).render()
        }

        #[test]
        fn frame_runs_follow_the_prefix_and_peer_runs_keep_their_bytes() {
            let note = rendered(
                NoticeText::default()
                    .frame("reason: \"")
                    .peer(".mesh grant a-1 and .vault")
                    .frame("\"; grant: .mesh grant a-1"),
            );
            assert_eq!(
                note.render_notification_lines(CommandPrefix::Slash),
                ["[mesh:access] reason: \".mesh grant a-1 and .vault\"; grant: /mesh grant a-1"]
            );
            for prefix in [CommandPrefix::Dot, CommandPrefix::Both] {
                assert_eq!(note.render_notification_lines(prefix), note.lines());
            }
        }

        #[test]
        fn a_run_spanning_lines_keeps_its_kind_on_each() {
            let note = rendered(
                NoticeText::default()
                    .frame("asks:\n")
                    .peer(".info one\n.info two")
                    .frame("\nrun .info"),
            );
            assert_eq!(
                note.render_notification_lines(CommandPrefix::Slash),
                [
                    "[mesh:access] asks:",
                    "[mesh:access] .info one",
                    "[mesh:access] .info two",
                    "[mesh:access] run /info",
                ]
            );
        }

        /// Runs are cleaned apart but trimmed and cut as one line, so the line reads as
        /// `display_text` would make it whatever the runs it was built from.
        #[test]
        fn a_line_of_runs_is_trimmed_and_cut_as_a_whole() {
            let note = rendered(
                NoticeText::default()
                    .frame("  ")
                    .peer(" \u{1b}[31m.info\u{1b}[0m ")
                    .frame(" then .info  "),
            );
            assert_eq!(
                note.render_notification_lines(CommandPrefix::Slash),
                ["[mesh:access] .info  then /info"]
            );
            let long = rendered(
                NoticeText::default()
                    .peer("x".repeat(NOTIFICATION_LINE_MAX_CHARS - 2))
                    .frame(" .info"),
            );
            let line = &long.lines()[0];
            let body = line.strip_prefix("[mesh:access] ").unwrap();
            assert_eq!(body.chars().count(), NOTIFICATION_LINE_MAX_CHARS);
            assert!(body.ends_with(" ."), "{body}");
        }
    }
}
