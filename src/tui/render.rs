//! Turns a cell's source into ratatui lines at a given width. Markdown
//! goes through the REPL's renderer so both surfaces paint the same way,
//! then gets wrapped, scrubbed of every escape that is not a colour or
//! attribute, and parsed back into styled spans. A streaming reply is split
//! into a stable part, rendered once per width, and a live tail.

use crate::render::{MarkdownRender, RenderOptions};
use ansi_to_tui::IntoText;
use anyhow::Result;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};

const ESC: char = '\x1b';

/// Keeps SGR sequences and drops everything else a terminal would act on:
/// OSC strings (hyperlinks, titles), non-SGR CSI (cursor moves, erases),
/// other escape pairs and C0 controls other than newline and tab.
pub(crate) fn sanitize_ansi(text: &str) -> String {
    scrub(text, true)
}

fn scrub(text: &str, keep_sgr: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != ESC {
            if c == '\n' || c == '\t' || (!c.is_control() && c != '\u{7f}') {
                out.push(c);
            }
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('[') => {
                let start = i;
                i += 2;
                while matches!(chars.get(i), Some(c) if ('\x30'..='\x3f').contains(c)) {
                    i += 1;
                }
                while matches!(chars.get(i), Some(c) if ('\x20'..='\x2f').contains(c)) {
                    i += 1;
                }
                let final_byte = chars.get(i).copied();
                i += 1;
                if keep_sgr && final_byte == Some('m') {
                    out.extend(&chars[start..i.min(chars.len())]);
                }
            }
            Some(']') => {
                i += 2;
                loop {
                    match chars.get(i) {
                        None => break,
                        Some('\x07') => {
                            i += 1;
                            break;
                        }
                        Some(&ESC) if chars.get(i + 1) == Some(&'\\') => {
                            i += 2;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
            }
            Some(c) if ('\x20'..='\x2f').contains(c) => i += 3,
            Some(_) => i += 2,
            None => i += 1,
        }
    }
    out
}

/// Parses SGR-styled text into lines; falls back to the bare characters
/// when the parser rejects the input.
fn styled_lines(ansi: &str) -> Vec<Line<'static>> {
    if ansi.is_empty() {
        return Vec::new();
    }
    match ansi.into_text() {
        Ok(text) => text.lines,
        Err(err) => {
            debug!("Falling back to plain text for a cell: {err}");
            Text::raw(scrub(ansi, false)).lines
        }
    }
}

/// Wraps each line of `text` to `width` columns; escape sequences do not
/// count towards the width.
fn wrap_lines(text: &str, width: u16) -> String {
    let width = usize::from(width.max(1));
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let pieces = textwrap::wrap(line, width);
        for (j, piece) in pieces.iter().enumerate() {
            if j > 0 {
                out.push('\n');
            }
            out.push_str(piece);
        }
    }
    out
}

/// Plain text wrapped to `width`, every span in `style`.
pub(crate) fn plain_lines(text: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    wrap_lines(text, width)
        .split('\n')
        .map(|line| Line::from(Span::styled(line.to_string(), style)))
        .collect()
}

/// Already-painted output (a command's table) parsed as-is: no wrapping,
/// so a table keeps its columns and the view clips what does not fit.
pub(crate) fn preformatted_lines(output: &str) -> Vec<Line<'static>> {
    styled_lines(&sanitize_ansi(output.trim_end_matches(['\n', '\r'])))
}

/// The first line of already-painted text; the footer's usage readout
/// comes from the REPL's right prompt with its colours still in.
pub(crate) fn styled_line(ansi: &str) -> Line<'static> {
    preformatted_lines(ansi)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// The REPL's markdown renderer kept warm between frames.
pub(crate) struct Renderer {
    markdown: MarkdownRender,
}

impl Renderer {
    /// Wrapping is done here per width, so the renderer runs unwrapped.
    pub(crate) fn new(options: RenderOptions) -> Result<Self> {
        let markdown = MarkdownRender::init(RenderOptions {
            wrap: None,
            ..options
        })?;
        Ok(Self { markdown })
    }

    pub(crate) fn markdown(&mut self, source: &str, width: u16) -> Vec<Line<'static>> {
        if source.is_empty() {
            return Vec::new();
        }
        let mut ansi = self.markdown.render(source);
        let pending_table = self.markdown.finalize();
        if !pending_table.is_empty() {
            if !ansi.is_empty() {
                ansi.push('\n');
            }
            ansi.push_str(&pending_table);
        }
        // The renderer remembers an open fence across calls; closing it
        // keeps the next call from highlighting prose as code.
        if fence_open(source) {
            self.markdown.render("```");
        }
        styled_lines(&sanitize_ansi(&wrap_lines(&ansi, width)))
    }
}

fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

fn fence_open(source: &str) -> bool {
    source.split('\n').filter(|line| is_fence(line)).count() % 2 == 1
}

/// Byte offset where the live tail of a streaming reply begins: the last
/// blank line outside a code fence. Everything before it is complete
/// paragraphs that render the same on every frame.
pub(crate) fn split_stable_tail(source: &str) -> usize {
    let mut split = 0;
    let mut offset = 0;
    let mut in_fence = false;
    for line in source.split_inclusive('\n') {
        if is_fence(line) {
            in_fence = !in_fence;
        } else if !in_fence && line.trim().is_empty() && line.ends_with('\n') {
            split = offset;
        }
        offset += line.len();
    }
    split
}

struct StableLines {
    width: u16,
    stable_len: usize,
    lines: Vec<Line<'static>>,
}

/// Rendered lines of one reply, cached per width for the stable part. The
/// two most recently used widths stay cached so a resize and back costs
/// nothing.
#[derive(Default)]
pub(crate) struct RenderCache {
    entries: Vec<StableLines>,
}

impl RenderCache {
    pub(crate) fn lines(
        &mut self,
        source: &str,
        done: bool,
        width: u16,
        renderer: &mut Renderer,
    ) -> Vec<Line<'static>> {
        let split = if done {
            source.len()
        } else {
            split_stable_tail(source)
        };
        let entry = match self.entries.iter().position(|e| e.width == width) {
            Some(index) => self.entries.remove(index),
            None => {
                if self.entries.len() >= 2 {
                    self.entries.remove(0);
                }
                StableLines {
                    width,
                    stable_len: usize::MAX,
                    lines: Vec::new(),
                }
            }
        };
        self.entries.push(entry);
        let index = self.entries.len() - 1;
        if self.entries[index].stable_len != split {
            self.entries[index].lines = renderer.markdown(&source[..split], width);
            self.entries[index].stable_len = split;
        }
        let mut lines = self.entries[index].lines.clone();
        if split < source.len() {
            lines.extend(renderer.markdown(&source[split..], width));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::decode_bin;
    use ratatui::style::{Color, Modifier};
    use syntect::highlighting::Theme;

    const DARK_THEME: &[u8] = include_bytes!("../../assets/monokai-extended.theme.bin");

    fn every_span<'a>(lines: &'a [Line<'a>]) -> impl Iterator<Item = &'a Span<'a>> {
        lines.iter().flat_map(|line| line.spans.iter())
    }

    #[test]
    fn sanitize_keeps_sgr_and_drops_hyperlinks_and_cursor_moves() {
        let input =
            "\x1b]8;;https://example.com\x07link\x1b]8;;\x07 \x1b[2Aup \x1b[1mbold\x1b[0m\r\x07";
        assert_eq!(sanitize_ansi(input), "link up \x1b[1mbold\x1b[0m");
        assert_eq!(sanitize_ansi("\x1b(Bx\x1b=y\tz\n"), "xy\tz\n");
        assert_eq!(sanitize_ansi("\x1b]0;title\x1b\\after"), "after");
    }

    #[test]
    fn without_a_theme_bold_survives_and_no_colour_is_painted() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let lines = renderer.markdown("plain **bold** text", 80);
        let bold: Vec<&Span<'_>> = every_span(&lines)
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .collect();
        assert_eq!(bold.len(), 1, "{lines:?}");
        assert_eq!(bold[0].content.as_ref(), "bold");
        assert!(
            every_span(&lines).all(|span| matches!(span.style.fg, None | Some(Color::Reset))),
            "{lines:?}"
        );
    }

    #[test]
    fn a_theme_without_truecolor_paints_no_rgb() {
        let theme: Theme = decode_bin(DARK_THEME).unwrap();
        let options = RenderOptions {
            theme: Some(theme),
            truecolor: false,
            ..RenderOptions::default()
        };
        let mut renderer = Renderer::new(options).unwrap();
        let lines = renderer.markdown(
            "# Title\n\nSome `code` and a link [x](https://e.com)\n\n```rust\nfn main() {}\n```\n",
            80,
        );
        let painted = every_span(&lines)
            .filter(|span| span.style.fg.is_some())
            .count();
        assert!(painted > 0, "{lines:?}");
        for span in every_span(&lines) {
            for colour in [span.style.fg, span.style.bg, span.style.underline_color] {
                assert!(!matches!(colour, Some(Color::Rgb(..))), "{span:?}");
            }
        }
    }

    #[test]
    fn wrapping_measures_text_not_escapes() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let lines = renderer.markdown("one two **three** four five six seven", 12);
        assert!(lines.len() >= 3, "{lines:?}");
        for line in &lines {
            assert!(line.width() <= 12, "{line:?}");
        }
    }

    #[test]
    fn stable_tail_split_sits_at_the_last_blank_line_outside_a_fence() {
        assert_eq!(split_stable_tail("one paragraph"), 0);
        let two = "first\n\nsecond";
        assert_eq!(&two[..split_stable_tail(two)], "first\n");
        let fenced = "intro\n\n```\ncode\n\nmore code\n";
        assert_eq!(&fenced[..split_stable_tail(fenced)], "intro\n");
        let closed = "intro\n\n```\ncode\n```\n\ntail";
        assert_eq!(
            &closed[..split_stable_tail(closed)],
            "intro\n\n```\ncode\n```\n"
        );
    }

    #[test]
    fn an_open_fence_does_not_leak_into_the_next_render() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let _ = renderer.markdown("```rust\nfn main() {}", 80);
        let lines = renderer.markdown("plain **bold**", 80);
        assert!(
            every_span(&lines).any(|span| span.style.add_modifier.contains(Modifier::BOLD)),
            "{lines:?}"
        );
    }

    #[test]
    fn the_cache_reuses_the_stable_part_and_keeps_two_widths() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let mut cache = RenderCache::default();
        let source = "first paragraph\n\nsecond paragraph still streaming";
        let wide = cache.lines(source, false, 80, &mut renderer);
        let narrow = cache.lines(source, false, 10, &mut renderer);
        assert_eq!(cache.entries.len(), 2);
        assert!(narrow.len() > wide.len());
        let longer = format!("{source} and more");
        let again = cache.lines(&longer, false, 80, &mut renderer);
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(again[0], wide[0]);
        cache.lines(&longer, false, 40, &mut renderer);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.entries.iter().all(|e| e.width != 10));
        let done = cache.lines(&longer, true, 80, &mut renderer);
        assert!(
            done.iter()
                .any(|line| line.to_string().contains("and more")),
            "{done:?}"
        );
        assert_eq!(cache.entries.last().unwrap().stable_len, longer.len());
    }
}
