//! LaTeX math in a reply's prose, found before the markdown renderer sees
//! it. Display math on lines of its own is a block the segmenter splits
//! out (see [`super::segment`]); this finds the inline formulas of a
//! markdown run and swaps each one that draws on a single row for
//! placeholder characters of the same width, which survive the markdown
//! renderer's wrapping and are swapped back for the drawing afterwards.
//!
//! Code, links, autolinks and HTML tags are never read for math, and a
//! single `$` must look like the start of a formula rather than a price or
//! a shell variable.

use super::math_render::{MathRenderer, render_math};
use super::{CopyRegion, Delimiter, block_delimiter};
use ratatui::text::{Line, Span};
use std::ops::Range;
use std::sync::Arc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The delimiters a formula is written between, in the order they are
/// tried at an offset: the opener, the closer and whether it is display
/// math.
pub(crate) const DELIMITERS: [(&str, &str, bool); 4] = [
    ("$$", "$$", true),
    ("\\[", "\\]", true),
    ("\\(", "\\)", false),
    ("$", "$", false),
];

/// The placeholder for the first column of a formula; every column after
/// it is [`PLACEHOLDER_REST`]. Both are private-use characters one column
/// wide, so the renderer wraps and sizes the placeholder as it would the
/// drawing.
const PLACEHOLDER_START: char = '\u{E000}';
const PLACEHOLDER_REST: char = '\u{E001}';

/// Whether `next`, the character after a single `$`, can start a formula:
/// a blank cannot (`$ 5`), nor can the bracket of a shell substitution
/// (`$(cmd)`, `${VAR}`).
fn opens_a_dollar_formula(next: char) -> bool {
    !(next.is_whitespace() || next == '(' || next == '{')
}

const OPERATORS: [char; 10] = ['\\', '^', '_', '=', '+', '-', '*', '/', '<', '>'];

/// Whether the TeX between single dollars reads as math: not empty, not
/// ending in a blank, not a bare number (`$5 and $10`), not an all-caps
/// word (`$HOME`, `$PATH`).
fn plausible_dollar_formula(tex: &str) -> bool {
    if tex.is_empty() || tex.ends_with(char::is_whitespace) {
        return false;
    }
    if tex.starts_with(|c: char| c.is_ascii_digit()) && !tex.contains(OPERATORS) && tex != "0" {
        return false;
    }
    let shouty = tex.starts_with(|c: char| c.is_ascii_uppercase())
        && tex
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    !(shouty && tex.len() > 1)
}

/// Whether the byte at `at` follows an odd run of backslashes, which
/// escapes it.
fn escaped(bytes: &[u8], at: usize) -> bool {
    bytes[..at]
        .iter()
        .rev()
        .take_while(|b| **b == b'\\')
        .count()
        % 2
        == 1
}

/// The offset of the first unescaped `closer` in `from..to` of `source`.
fn find_closer(source: &str, from: usize, to: usize, closer: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    (from..to.saturating_sub(closer.len() - 1))
        .find(|&at| bytes[at..].starts_with(closer.as_bytes()) && !escaped(bytes, at))
}

/// The offset just past the closer of display math in `text`, and whether
/// nothing but blanks follow it; None when `text` holds no closer.
pub(crate) fn display_closer(text: &str, closer: &str) -> Option<(usize, bool)> {
    let at = find_closer(text, 0, text.len(), closer)?;
    let end = at + closer.len();
    Some((end, text[end..].trim().is_empty()))
}

/// The end of the paragraph `from` is in: the start of the next blank
/// line, or the end of `source`.
fn paragraph_end(source: &str, from: usize) -> usize {
    let mut offset = from;
    for line in source[from..].split_inclusive('\n') {
        if offset > from && line.trim().is_empty() {
            return offset;
        }
        offset += line.len();
    }
    source.len()
}

/// The byte ranges of `source` never read for math, in order: fenced code
/// (an open fence runs to the end), code spans, links and images, and
/// autolinks and HTML tags.
fn protected(source: &str) -> Vec<Range<usize>> {
    let mut fences = Vec::new();
    let mut open = None;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        if matches!(block_delimiter(line), Some(Delimiter::Fence { .. })) {
            match open.take() {
                None => open = Some(start),
                Some(at) => fences.push(at..offset),
            }
        }
    }
    if let Some(at) = open {
        fences.push(at..source.len());
    }
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut fences = fences.into_iter().peekable();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(fence) = fences.next_if(|fence| fence.start <= i) {
            i = i.max(fence.end);
            ranges.push(fence);
            continue;
        }
        let limit = fences.peek().map_or(bytes.len(), |fence| fence.start);
        let span = match bytes[i] {
            b'`' => code_span(bytes, i, limit),
            b'[' | b'!' if !escaped(bytes, i) => link(bytes, i, limit),
            b'<' => tag(bytes, i, limit),
            _ => None,
        };
        match span {
            Some(end) => {
                ranges.push(i..end);
                i = end;
            }
            None if bytes[i] == b'`' => {
                i += bytes[i..].iter().take_while(|b| **b == b'`').count();
            }
            None => i += 1,
        }
    }
    ranges
}

/// The end of the code span opened by the backtick run at `at`: the next
/// run of the same length before `limit`.
fn code_span(bytes: &[u8], at: usize, limit: usize) -> Option<usize> {
    let run = bytes[at..limit].iter().take_while(|b| **b == b'`').count();
    let mut i = at + run;
    while i < limit {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let closing = bytes[i..limit].iter().take_while(|b| **b == b'`').count();
        if closing == run {
            return Some(i + run);
        }
        i += closing;
    }
    None
}

/// The end of a link `[text](target)` or image `![text](target)` at `at`,
/// all on one line.
fn link(bytes: &[u8], at: usize, limit: usize) -> Option<usize> {
    let open = if bytes[at] == b'!' { at + 1 } else { at };
    if bytes.get(open) != Some(&b'[') {
        return None;
    }
    let line_end = bytes[open..limit]
        .iter()
        .position(|b| *b == b'\n')
        .map_or(limit, |n| open + n);
    let close = open + bytes[open..line_end].iter().position(|b| *b == b']')?;
    if bytes.get(close + 1) != Some(&b'(') {
        return None;
    }
    let paren = close + 1 + bytes[close + 1..line_end].iter().position(|b| *b == b')')?;
    Some(paren + 1)
}

/// The end of an autolink (`<https://...>`, `<a@b.c>`), an HTML tag or an
/// HTML comment at `at`, all on one line.
fn tag(bytes: &[u8], at: usize, limit: usize) -> Option<usize> {
    let rest = &bytes[at + 1..limit];
    if rest.starts_with(b"!--") {
        let end = rest.windows(3).position(|w| w == b"-->")?;
        return Some(at + 1 + end + 3);
    }
    let close = rest
        .iter()
        .take_while(|b| **b != b'\n' && **b != b'<')
        .position(|b| *b == b'>')?;
    let inner = &rest[..close];
    let name = inner.strip_prefix(b"/").unwrap_or(inner);
    let name_len = name
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'-')
        .count();
    let is_tag = name.first().is_some_and(u8::is_ascii_alphabetic)
        && name
            .get(name_len)
            .is_none_or(|b| b.is_ascii_whitespace() || *b == b'/');
    let is_autolink = !inner.contains(&b' ')
        && (inner.contains(&b'@')
            || inner.iter().position(|b| *b == b':').is_some_and(|colon| {
                colon > 1 && inner[..colon].iter().all(u8::is_ascii_alphanumeric)
            }));
    (is_tag || is_autolink).then_some(at + 1 + close + 1)
}

/// An inline formula in a run: the bytes it takes with its delimiters,
/// and the TeX between them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) whole: Range<usize>,
    pub(crate) tex: Range<usize>,
}

/// The inline formulas of `source`, in order. Display math met in prose
/// (not on lines of its own) is left as text, with everything up to its
/// closer, or to the end of its paragraph while the closer has not come.
pub(crate) fn find_inline(source: &str) -> Vec<Found> {
    let bytes = source.as_bytes();
    let protected = protected(source);
    let mut guarded = protected.iter().peekable();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while guarded.next_if(|range| range.end <= i).is_some() {}
        if let Some(range) = guarded.peek().filter(|range| range.start <= i) {
            i = range.end;
            continue;
        }
        let limit = guarded.peek().map_or(bytes.len(), |range| range.start);
        let Some(&(opener, closer, display)) = DELIMITERS
            .iter()
            .find(|(opener, ..)| bytes[i..].starts_with(opener.as_bytes()))
        else {
            i += 1;
            continue;
        };
        let body = i + opener.len();
        if escaped(bytes, i) {
            i = body;
            continue;
        }
        if display {
            let to = paragraph_end(source, i).min(limit);
            i = find_closer(source, body, to, closer).map_or(to.max(body), |at| at + closer.len());
            continue;
        }
        let line_end = source[body..limit].find('\n').map_or(limit, |n| body + n);
        let dollar = opener == "$";
        let opens = !dollar
            || source[body..]
                .chars()
                .next()
                .is_some_and(opens_a_dollar_formula);
        let Some(at) = find_closer(source, body, line_end, closer).filter(|_| opens) else {
            i = body;
            continue;
        };
        let end = at + closer.len();
        let tex = &source[body..at];
        let accepted = if dollar {
            plausible_dollar_formula(tex)
                && !source[end..].starts_with(|c: char| c.is_alphanumeric())
        } else {
            !tex.trim().is_empty()
        };
        if accepted {
            found.push(Found {
                whole: i..end,
                tex: body..at,
            });
            i = end;
        } else {
            i = body;
        }
    }
    found
}

/// An inline formula swapped for placeholders: its drawing and its
/// source, delimiters included, which a copy takes in its place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InlineMath {
    drawn: String,
    source: Arc<str>,
}

/// `run` with each inline formula that draws on one row within `width`
/// swapped for a placeholder as wide as the drawing, and those formulas
/// in order. A formula that cannot be drawn on one row stays as its
/// source. When any is swapped, the placeholder characters the reply
/// itself held are dropped, so that every placeholder is one of ours.
pub(crate) fn mask_inline(
    run: &str,
    width: u16,
    backend: &dyn MathRenderer,
) -> (String, Vec<InlineMath>) {
    let own = |text: &str| text.replace([PLACEHOLDER_START, PLACEHOLDER_REST], "");
    let mut masked = String::with_capacity(run.len());
    let mut spans = Vec::new();
    let mut copied = 0;
    for found in find_inline(run) {
        let Some(drawn) = render_math(backend, &run[found.tex.clone()], width)
            .filter(|rows| rows.len() == 1)
            .and_then(|mut rows| rows.pop())
        else {
            continue;
        };
        let columns = drawn.width();
        if columns == 0 {
            continue;
        }
        masked.push_str(&own(&run[copied..found.whole.start]));
        masked.push(PLACEHOLDER_START);
        masked.extend(std::iter::repeat_n(PLACEHOLDER_REST, columns - 1));
        spans.push(InlineMath {
            drawn,
            source: Arc::from(&run[found.whole.clone()]),
        });
        copied = found.whole.end;
    }
    if spans.is_empty() {
        return (run.to_string(), spans);
    }
    masked.push_str(&own(&run[copied..]));
    (masked, spans)
}

/// One column of a drawing: the character that starts in it, with any
/// zero-width ones after, and its width; or the second column of a wide
/// character.
enum Column {
    Starts(String, usize),
    Covered,
}

fn drawn_columns(drawn: &str) -> Vec<Column> {
    let mut columns = Vec::new();
    for c in drawn.chars() {
        match c.width().unwrap_or(0) {
            0 => {
                if let Some(Column::Starts(text, _)) = columns
                    .iter_mut()
                    .rev()
                    .find(|column| matches!(column, Column::Starts(..)))
                {
                    text.push(c);
                }
            }
            width => {
                columns.push(Column::Starts(c.to_string(), width));
                columns.extend(std::iter::repeat_with(|| Column::Covered).take(width - 1));
            }
        }
    }
    columns
}

/// The piece of a formula a row is being read through: which formula,
/// the row's column it starts in, the last column of the formula read so
/// far, and whether the piece holds the formula's first column.
#[derive(Clone, Copy)]
struct Piece {
    formula: usize,
    from: usize,
    last: usize,
    first: bool,
}

/// The rendered lines of a masked run with each placeholder swapped back
/// for its formula's drawing, in the style the renderer gave the
/// placeholder, and the columns each formula takes on each row it lands
/// on. A placeholder the wrap split across rows is drawn piece by piece;
/// a wide character the split cuts in two becomes a blank on each side.
/// Only the first piece of a formula copies as its source. Rows without a
/// placeholder are returned as they came.
pub(crate) fn unmask(
    lines: Vec<Line<'static>>,
    spans: &[InlineMath],
) -> (Vec<Line<'static>>, Vec<CopyRegion>) {
    if spans.is_empty() {
        return (lines, Vec::new());
    }
    let drawings: Vec<Vec<Column>> = spans
        .iter()
        .map(|span| drawn_columns(&span.drawn))
        .collect();
    let mut started = 0;
    let mut column_in_formula = 0;
    let mut regions = Vec::new();
    let mut out = Vec::with_capacity(lines.len());
    for (row, line) in lines.into_iter().enumerate() {
        let holds = |span: &Span| span.content.contains([PLACEHOLDER_START, PLACEHOLDER_REST]);
        if !line.spans.iter().any(holds) {
            out.push(line);
            continue;
        }
        let mut tags: Vec<Option<(usize, usize)>> = Vec::new();
        for c in line.spans.iter().flat_map(|span| span.content.chars()) {
            tags.push(match c {
                PLACEHOLDER_START => {
                    started += 1;
                    column_in_formula = 1;
                    Some((started - 1, 0))
                }
                PLACEHOLDER_REST => {
                    column_in_formula += 1;
                    started.checked_sub(1).map(|k| (k, column_in_formula - 1))
                }
                _ => None,
            });
        }
        let drawn_at = |t: usize, (k, at): (usize, usize)| -> String {
            let same = |other: Option<&Option<(usize, usize)>>, at: Option<usize>| {
                at.is_some_and(|at| other == Some(&Some((k, at))))
            };
            match drawings.get(k).and_then(|columns| columns.get(at)) {
                Some(Column::Starts(text, 1)) => text.clone(),
                Some(Column::Starts(text, width)) => {
                    let whole = (1..*width).all(|d| same(tags.get(t + d), Some(at + d)));
                    if whole { text.clone() } else { " ".to_string() }
                }
                Some(Column::Covered) => {
                    let joined = t > 0 && same(tags.get(t - 1), at.checked_sub(1));
                    if joined {
                        String::new()
                    } else {
                        " ".to_string()
                    }
                }
                None => " ".to_string(),
            }
        };
        let mut t = 0;
        let mut column = 0;
        let mut open: Option<Piece> = None;
        let mut close = |open: &mut Option<Piece>, end: usize| {
            if let Some(piece) = open.take() {
                regions.push(CopyRegion::Inline {
                    row,
                    cols: piece.from..end,
                    source: if piece.first {
                        Arc::clone(&spans[piece.formula].source)
                    } else {
                        Arc::from("")
                    },
                });
            }
        };
        let Line {
            spans: line_spans,
            style,
            alignment,
        } = line;
        let mut rebuilt = Vec::with_capacity(line_spans.len());
        for span in line_spans {
            let mut content = String::with_capacity(span.content.len());
            for c in span.content.chars() {
                match tags[t] {
                    Some((k, at)) => {
                        match open.as_mut() {
                            Some(piece) if piece.formula == k && piece.last + 1 == at => {
                                piece.last = at;
                            }
                            _ => {
                                close(&mut open, column);
                                open = Some(Piece {
                                    formula: k,
                                    from: column,
                                    last: at,
                                    first: at == 0,
                                });
                            }
                        }
                        content.push_str(&drawn_at(t, (k, at)));
                        column += 1;
                    }
                    None => {
                        close(&mut open, column);
                        content.push(c);
                        column += c.width().unwrap_or(0);
                    }
                }
                t += 1;
            }
            if !content.is_empty() || span.content.is_empty() {
                rebuilt.push(Span::styled(content, span.style));
            }
        }
        close(&mut open, column);
        out.push(Line {
            spans: rebuilt,
            style,
            alignment,
        });
    }
    (out, regions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::rich::math_render::FakeMath;

    fn formulas(source: &str) -> Vec<&str> {
        find_inline(source)
            .into_iter()
            .map(|found| &source[found.whole])
            .collect()
    }

    #[test]
    fn the_placeholders_are_one_column_wide() {
        assert_eq!(PLACEHOLDER_START.width(), Some(1));
        assert_eq!(PLACEHOLDER_REST.width(), Some(1));
    }

    #[test]
    fn each_inline_delimiter_is_found() {
        assert_eq!(formulas("a $x^2$ b \\(y_1\\) c"), ["$x^2$", "\\(y_1\\)"]);
        let found = find_inline("so $a+b$.");
        assert_eq!(
            found,
            [Found {
                whole: 3..8,
                tex: 4..7
            }]
        );
    }

    /// Display math in prose is not inline math, and the formula inside it
    /// is not read as one either.
    #[test]
    fn display_math_in_prose_is_left_as_text() {
        assert!(formulas("see $$x$$ and \\[y\\] here").is_empty());
        assert_eq!(formulas("$$a $b$ c$$ then $d$"), ["$d$"]);
        assert!(formulas("$$ open\nstill $x$").is_empty());
        assert_eq!(formulas("$$ open\n\nlater $x$"), ["$x$"]);
    }

    #[test]
    fn prices_and_shell_variables_are_not_math() {
        for prose in [
            "$5 and $10",
            "costs $3.50 or $4.25 a month",
            "it costs $3.50",
            "echo $HOME",
            "set $HOME and $PATH",
            "add $PATH/bin to $PATH",
            "run $(date) and ${HOME}",
            "the vars $AB$ and $HOME_DIR$",
            "x $5$ y",
            "between $ x$ and $y $",
            "the $x$y joined",
        ] {
            assert!(formulas(prose).is_empty(), "{prose:?}");
        }
        assert_eq!(formulas("a $0$ b $5+1$ c $A$"), ["$0$", "$5+1$", "$A$"]);
    }

    #[test]
    fn code_links_and_html_are_not_read_for_math() {
        for prose in [
            "a `$x$` b",
            "a ``$x$ ` $y$`` b",
            "```\n$x$\n$$\n```\n",
            "```rust\nlet a = \"$x$\";\n",
            "[link](https://e.com/$x$)",
            "![image $x$](a.png)",
            "<https://e.com/$x$>",
            "<a href=\"$x$\">",
            "<!-- $x$ -->",
        ] {
            assert!(formulas(prose).is_empty(), "{prose:?}");
        }
        assert_eq!(formulas("`code` then $x$"), ["$x$"]);
        assert_eq!(formulas("a ` lone tick $x$"), ["$x$"]);
        assert_eq!(formulas("if $a<b$ and c>d"), ["$a<b$"]);
        assert_eq!(formulas("[not a link] $x$"), ["$x$"]);
    }

    #[test]
    fn inline_math_never_spans_a_line() {
        assert!(formulas("$x\n+ y$").is_empty());
        assert!(formulas("\\(x\n\\)").is_empty());
    }

    #[test]
    fn an_escaped_delimiter_is_text() {
        assert!(formulas("\\$x$ and \\\\(y\\)").is_empty());
        assert_eq!(formulas("\\\\$x$"), ["$x$"]);
        assert_eq!(formulas("$a\\$b$"), ["$a\\$b$"]);
        assert!(formulas("\\(x\\\\)").is_empty());
    }

    #[test]
    fn a_display_closer_reports_what_follows_it() {
        assert_eq!(display_closer("x $$", "$$"), Some((4, true)));
        assert_eq!(display_closer("x $$ y", "$$"), Some((4, false)));
        assert_eq!(display_closer("x \\$$", "$$"), None);
        assert_eq!(display_closer("a \\] ", "\\]"), Some((4, true)));
        assert_eq!(display_closer("a \\\\]", "\\]"), None);
    }

    fn drawn_as(drawn: &'static str) -> FakeMath {
        FakeMath::new(move |_| drawn.to_string())
    }

    #[test]
    fn a_formula_is_masked_as_wide_as_its_drawing() {
        let (masked, spans) = mask_inline("a $x^2$ b", 80, &drawn_as("x²"));
        assert_eq!(masked, "a \u{E000}\u{E001} b");
        assert_eq!(spans.len(), 1);
        assert_eq!(&*spans[0].source, "$x^2$");
    }

    /// A formula that draws on more than one row, or that cannot be drawn,
    /// stays as its source, and the run is left untouched.
    #[test]
    fn a_formula_that_is_not_one_row_stays_as_source() {
        for drawn in ["a\n─\nb", "\\foo", ""] {
            let run = "a $x$ b \u{E000}";
            assert_eq!(
                mask_inline(run, 80, &drawn_as(drawn)),
                (run.to_string(), Vec::new())
            );
        }
    }

    #[test]
    fn the_replys_own_placeholder_characters_go_once_a_formula_is_masked() {
        let (masked, _) = mask_inline("\u{E000}a $x$ \u{E001}", 80, &drawn_as("x"));
        assert_eq!(masked, "a \u{E000} ");
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(Line::to_string).collect()
    }

    #[test]
    fn unmasking_swaps_the_drawing_in_and_records_its_columns() {
        let (masked, spans) = mask_inline("one $x^2$ two $y$", 80, &drawn_as("ab"));
        let bold = ratatui::style::Style::new().add_modifier(ratatui::style::Modifier::BOLD);
        let line = Line::from(vec![
            Span::raw(masked[..masked.find(" two").unwrap()].to_string()),
            Span::styled(masked[masked.find(" two").unwrap()..].to_string(), bold),
        ]);
        let (lines, regions) = unmask(vec![Line::from("before"), line], &spans);
        assert_eq!(texts(&lines), ["before", "one ab two ab"]);
        assert_eq!(lines[1].spans[1].style, bold);
        assert_eq!(
            regions,
            [
                CopyRegion::Inline {
                    row: 1,
                    cols: 4..6,
                    source: Arc::from("$x^2$"),
                },
                CopyRegion::Inline {
                    row: 1,
                    cols: 11..13,
                    source: Arc::from("$y$"),
                },
            ]
        );
    }

    /// A placeholder split by a hard wrap is drawn on both rows from where
    /// the split fell, a wide character cut in two turning into blanks,
    /// and only the first piece copies as the source.
    #[test]
    fn a_placeholder_split_across_rows_is_drawn_piece_by_piece() {
        let (masked, spans) = mask_inline("$x$", 80, &drawn_as("a\u{4e2d}b"));
        assert_eq!(masked.chars().count(), 4);
        let split: Vec<char> = masked.chars().collect();
        let first: String = split[..2].iter().collect();
        let second: String = split[2..].iter().collect();
        let lines = vec![
            Line::from(first),
            Line::from(vec![
                crate::tui::render::wrap_mark(false),
                Span::raw(second),
            ]),
        ];
        let (lines, regions) = unmask(lines, &spans);
        assert_eq!(texts(&lines), ["a ", " b"]);
        assert_eq!(
            regions,
            [
                CopyRegion::Inline {
                    row: 0,
                    cols: 0..2,
                    source: Arc::from("$x$"),
                },
                CopyRegion::Inline {
                    row: 1,
                    cols: 0..2,
                    source: Arc::from(""),
                },
            ]
        );
        let whole = vec![Line::from(masked)];
        assert_eq!(texts(&unmask(whole, &spans).0), ["a\u{4e2d}b"]);
    }

    #[test]
    fn rows_without_a_placeholder_come_back_as_they_were() {
        let (_, spans) = mask_inline("$x$", 80, &drawn_as("x"));
        let lines = vec![Line::from(vec![Span::raw("plain"), Span::raw("")])];
        assert_eq!(unmask(lines.clone(), &spans), (lines, Vec::new()));
    }
}
