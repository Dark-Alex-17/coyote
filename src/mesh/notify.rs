//! One human-facing line for the terminal. This is the channel peers reach the
//! person at the keyboard through; it is separate from
//! `crate::supervisor::notification`, whose `SystemNotification` queue is
//! merged into tool results for the model to read. Nothing here goes to the model.

use crate::config::sanitize_display_text;
use crate::mesh::announce::is_control_or_invisible;

/// Peer-supplied text reaches the terminal through this path, so both dimensions of a
/// notification are bounded: characters per line and lines per notification.
pub(crate) const NOTIFICATION_LINE_MAX_CHARS: usize = 512;
pub(crate) const NOTIFICATION_MAX_LINES: usize = 16;

// Constructed by the idle-time driver and the mesh request handlers once they land.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Node lifecycle: the mesh coming up, going down, or losing its relay.
    Mesh,
    /// Knock requests from peers asking to be trusted.
    Knock,
    /// Messages a trusted peer addressed to this node.
    Message,
    /// Propagation fetch and post activity against a propagation node.
    Propagation,
}

impl Source {
    // Iterated by the tests only until a caller needs every source at once.
    #[allow(dead_code)]
    pub(crate) const ALL: [Source; 4] = [
        Source::Mesh,
        Source::Knock,
        Source::Message,
        Source::Propagation,
    ];

    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Source::Mesh => "[mesh]",
            Source::Knock => "[mesh:knock]",
            Source::Message => "[mesh:message]",
            Source::Propagation => "[mesh:propagation]",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notification {
    pub(crate) source: Source,
    pub(crate) text: String,
}

impl Notification {
    // Constructed by the idle-time driver and the mesh request handlers once they land.
    #[allow(dead_code)]
    pub(crate) fn new(source: Source, text: impl Into<String>) -> Self {
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
    pub(crate) fn render_lines(&self) -> Vec<String> {
        let prefix = self.source.prefix();
        // Split before sanitising: `sanitize_display_text` maps `\n` to a space.
        let mut lines: Vec<String> = self
            .text
            .split('\n')
            .map(|piece| piece.strip_suffix('\r').unwrap_or(piece))
            .filter_map(|piece| clean_line(piece, NOTIFICATION_LINE_MAX_CHARS))
            .map(|line| format!("{prefix} {line}"))
            .collect();
        if lines.is_empty() {
            return vec![prefix.to_string()];
        }
        if lines.len() > NOTIFICATION_MAX_LINES {
            let more = lines.len() - NOTIFICATION_MAX_LINES;
            lines.truncate(NOTIFICATION_MAX_LINES);
            lines.push(format!("{prefix} (+{more} more lines)"));
        }
        lines
    }
}

fn clean_line(text: &str, max_chars: usize) -> Option<String> {
    let cleaned: String = sanitize_display_text(text)
        .chars()
        .filter(|c| !is_control_or_invisible(*c))
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    let capped = match trimmed.char_indices().nth(max_chars) {
        Some((cut, _)) => &trimmed[..cut],
        None => trimmed,
    };
    Some(capped.trim_end().to_string())
}

/// Where rendered lines go. Implementations must not block: `notify` is called from the
/// node's async tasks and from request handlers. `text` is untrusted peer input, so an
/// implementation renders through `Notification::render_lines`, which owns the escape
/// stripping, the invisible-character filter and the line and character caps, and never
/// prints `text` directly.
pub(crate) trait NotificationSink: Send + Sync {
    fn notify(&self, note: Notification);
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
            Source::Propagation,
        ] {
            match source {
                Source::Mesh | Source::Knock | Source::Message | Source::Propagation => {}
            }
            assert!(Source::ALL.contains(&source), "{source:?} missing from ALL");
        }
        assert_eq!(Source::ALL.len(), 4);
        for source in Source::ALL {
            assert!(source.prefix().starts_with("[mesh"), "{source:?}");
        }
        assert_eq!(Source::Mesh.prefix(), "[mesh]");
        assert_eq!(Source::Knock.prefix(), "[mesh:knock]");
        assert_eq!(Source::Message.prefix(), "[mesh:message]");
        assert_eq!(Source::Propagation.prefix(), "[mesh:propagation]");
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
}
