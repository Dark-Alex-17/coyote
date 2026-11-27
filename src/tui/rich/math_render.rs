//! LaTeX formulas drawn as rows of text. The drawing is done by a backend
//! behind [`MathRenderer`]; [`math_rows`] guards every backend alike: it
//! caps the input and its nesting, turns away what the backend is known to
//! draw wrongly, survives a panic, scrubs the output and refuses a drawing
//! that still holds a command the backend did not know or is too wide.

use crate::tui::render::plain_text;
use crate::tui::terminal::catch_quietly_at;
use unicode_width::UnicodeWidthStr;

/// The most TeX a formula may have before it is shown as source without
/// trying to draw it. The backend's parser recurses, and a stack overflow
/// cannot be caught the way a panic can.
const MAX_TEX_BYTES: usize = 4096;

/// The deepest braces may nest before a formula is shown as source. The
/// backend's layout time grows steeply with depth (a thousand nested
/// superscripts take seconds), and it runs on the draw path.
const MAX_NESTING: usize = 32;

/// Constructs the backend lays out wrongly, or draws as their source.
const UNSUPPORTED: [&str; 7] = [
    "\\begin{align*}",
    "\\begin{align}",
    "\\begin{gather*}",
    "\\begin{gather}",
    "\\begin{array}",
    "\\begin{aligned}",
    "\\sqrt[",
];

/// The columns a drawing leaves free of the room it is given.
const MARGIN: u16 = 4;

/// Draws a formula as text, one row per line. A formula has no width to
/// fit: a two-dimensional layout cannot be wrapped.
pub(crate) trait MathRenderer: Send + Sync {
    fn render(&self, tex: &str) -> String;
}

/// Why a formula is shown as source instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MathError {
    TooLarge,
    TooDeep,
    Unsupported(&'static str),
    Panicked(String),
    Empty,
    TooWide { needed: usize, available: u16 },
    UnknownCommand,
}

impl MathError {
    /// The one-line note shown under the source.
    pub(crate) fn note(&self) -> String {
        let why = match self {
            MathError::TooLarge => format!("formula longer than {} KiB", MAX_TEX_BYTES / 1024),
            MathError::TooDeep => "nested too deeply".to_string(),
            MathError::Unsupported(construct) => format!("{construct} is not supported"),
            MathError::Panicked(message) => format!(
                "renderer failed: {}",
                message.lines().next().unwrap_or_default().trim()
            ),
            MathError::Empty => "empty formula".to_string(),
            MathError::TooWide { needed, available } => {
                format!("formula needs {needed} columns, {available} available")
            }
            MathError::UnknownCommand => "unknown command".to_string(),
        };
        format!("math: {why} (showing source)")
    }
}

/// The one row `tex` draws as in `max_width` columns, or None when it
/// cannot be drawn there or takes more than one row. A panic is logged at
/// debug: an inline formula in the streaming tail is drawn again on every
/// new width and every reply, and its source is shown either way.
pub(crate) fn one_row(backend: &dyn MathRenderer, tex: &str, max_width: u16) -> Option<String> {
    rows_logging_at(log::Level::Debug, backend, tex, max_width)
        .ok()
        .filter(|rows| rows.len() == 1)
        .and_then(|mut rows| rows.pop())
}

/// How deep the braces of `tex` nest; a brace after a backslash is a
/// literal one and does not count.
fn nesting(tex: &str) -> usize {
    let mut depth: usize = 0;
    let mut deepest = 0;
    let mut chars = tex.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '{' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

/// The rows of `tex` drawn by `backend`, without control characters,
/// trailing blanks or blank rows above and below, each at most
/// `max_width` less the margin wide. A tab in the TeX or the drawing is a
/// space.
pub(crate) fn math_rows(
    backend: &dyn MathRenderer,
    tex: &str,
    max_width: u16,
) -> Result<Vec<String>, MathError> {
    rows_logging_at(log::Level::Warn, backend, tex, max_width)
}

fn rows_logging_at(
    level: log::Level,
    backend: &dyn MathRenderer,
    tex: &str,
    max_width: u16,
) -> Result<Vec<String>, MathError> {
    if tex.len() > MAX_TEX_BYTES {
        return Err(MathError::TooLarge);
    }
    if nesting(tex) > MAX_NESTING {
        return Err(MathError::TooDeep);
    }
    if let Some(construct) = UNSUPPORTED
        .iter()
        .find(|construct| tex.contains(*construct))
    {
        return Err(MathError::Unsupported(construct));
    }
    let tex = tex.replace('\t', " ");
    let drawn = catch_quietly_at(level, || backend.render(&tex)).map_err(MathError::Panicked)?;
    let mut rows: Vec<String> = plain_text(&drawn)
        .split('\n')
        .map(|row| {
            let row: String = row
                .chars()
                .map(|c| if c == '\t' { ' ' } else { c })
                .filter(|c| !c.is_control())
                .collect();
            row.trim_end().to_string()
        })
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    let blank_above = rows.iter().take_while(|row| row.is_empty()).count();
    rows.drain(..blank_above);
    if rows.is_empty() {
        return Err(MathError::Empty);
    }
    if rows.iter().any(|row| row.contains('\\')) {
        return Err(MathError::UnknownCommand);
    }
    let available = max_width.saturating_sub(MARGIN);
    let needed = rows.iter().map(|row| row.width()).max().unwrap_or(0);
    if needed > usize::from(available) {
        return Err(MathError::TooWide { needed, available });
    }
    Ok(rows)
}

/// The `term-maths` crate.
pub(crate) struct TermMathsBackend;

impl MathRenderer for TermMathsBackend {
    fn render(&self, tex: &str) -> String {
        term_maths::render(tex).to_string()
    }
}

#[cfg(test)]
type Respond = dyn Fn(&str) -> String + Send + Sync;

/// A backend that answers with whatever `respond` gives and counts its
/// calls.
#[cfg(test)]
pub(crate) struct FakeMath {
    respond: Box<Respond>,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl FakeMath {
    pub(crate) fn new(respond: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        Self {
            respond: Box::new(respond),
            calls: Default::default(),
        }
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl MathRenderer for FakeMath {
    fn render(&self, tex: &str) -> String {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (self.respond)(tex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(drawn: &'static str) -> FakeMath {
        FakeMath::new(move |_| drawn.to_string())
    }

    #[test]
    fn too_much_tex_is_not_handed_to_the_backend() {
        let backend = answers("x");
        let big = "x".repeat(MAX_TEX_BYTES + 1);
        assert_eq!(math_rows(&backend, &big, 80), Err(MathError::TooLarge));
        assert!(math_rows(&backend, &"x".repeat(MAX_TEX_BYTES), 80).is_ok());
        assert_eq!(backend.calls(), 1);
    }

    #[test]
    fn each_unsupported_construct_is_turned_away_up_front() {
        let backend = answers("drawn");
        for construct in UNSUPPORTED {
            let tex = format!("{construct} a &= b \\end");
            assert_eq!(
                math_rows(&backend, &tex, 80),
                Err(MathError::Unsupported(construct)),
                "{tex}"
            );
        }
        assert_eq!(backend.calls(), 0);
        assert!(math_rows(&backend, "\\begin{pmatrix} a \\end{pmatrix}", 80).is_ok());
    }

    #[test]
    fn a_panicking_backend_fails_with_its_message() {
        let backend = FakeMath::new(|_| panic!("parser overflowed"));
        assert_eq!(
            math_rows(&backend, "x", 80),
            Err(MathError::Panicked("parser overflowed".to_string()))
        );
    }

    #[test]
    fn an_empty_drawing_fails() {
        for drawn in ["", "\n  \n", "\x1b[0m\x07"] {
            assert_eq!(
                math_rows(&answers(drawn), "{", 80),
                Err(MathError::Empty),
                "{drawn:?}"
            );
        }
    }

    /// The drawing must leave the margin free: at 20 columns a row of 16
    /// fits and one of 17 does not.
    #[test]
    fn a_drawing_wider_than_the_room_less_the_margin_fails() {
        assert_eq!(
            math_rows(&answers("0123456789abcdef"), "x", 20).map(|rows| rows.len()),
            Ok(1)
        );
        assert_eq!(
            math_rows(&answers("0123456789abcdefg"), "x", 20),
            Err(MathError::TooWide {
                needed: 17,
                available: 16,
            })
        );
        assert!(matches!(
            math_rows(&answers("x"), "x", 3),
            Err(MathError::TooWide { available: 0, .. })
        ));
    }

    #[test]
    fn a_command_left_in_the_drawing_fails() {
        assert_eq!(
            math_rows(&answers("\\foox"), "\\foo{x}", 80),
            Err(MathError::UnknownCommand)
        );
        assert_eq!(
            math_rows(&answers("\\foo{a very long drawing}"), "\\foo{x}", 10),
            Err(MathError::UnknownCommand),
            "an unknown command is named before the width"
        );
    }

    fn nested(depth: usize) -> String {
        format!("{}x{}", "x^{".repeat(depth), "}".repeat(depth))
    }

    #[test]
    fn braces_nested_too_deep_are_not_handed_to_the_backend() {
        let backend = answers("x");
        assert_eq!(
            math_rows(&backend, &nested(MAX_NESTING + 1), 80),
            Err(MathError::TooDeep)
        );
        assert_eq!(backend.calls(), 0);
        assert!(math_rows(&backend, &nested(MAX_NESTING), 80).is_ok());
        let literal = format!("{}x", "\\{".repeat(MAX_NESTING + 1));
        assert!(math_rows(&backend, &literal, 80).is_ok());
        assert_eq!(backend.calls(), 2);
        assert_eq!(nesting("\\\\{a}"), 1);
    }

    #[test]
    fn tabs_are_spaces_and_blank_rows_above_go() {
        let echo = FakeMath::new(|tex| format!("\n \n{tex}"));
        assert_eq!(math_rows(&echo, "a\tb", 80), Ok(vec!["a b".to_string()]));
        assert_eq!(
            math_rows(&answers("a\tb\n"), "x", 80),
            Ok(vec!["a b".to_string()])
        );
    }

    #[test]
    fn control_characters_and_trailing_blanks_are_dropped() {
        let backend = answers("\x1b[1m a \x1b[0m \r\n\x07───\n c  \n\n");
        assert_eq!(
            math_rows(&backend, "\\frac{a}{c}", 80),
            Ok(vec![" a".to_string(), "───".to_string(), " c".to_string()])
        );
        assert_eq!(one_row(&backend, "\\frac{a}{c}", 80), None);
        assert_eq!(one_row(&answers("x²"), "x^2", 80), Some("x²".to_string()));
        assert_eq!(one_row(&answers("x²"), "x^2", 3), None);
    }

    #[test]
    fn every_note_is_one_line_naming_the_reason() {
        let notes = [
            (MathError::TooLarge, "formula longer than 4 KiB"),
            (MathError::TooDeep, "nested too deeply"),
            (
                MathError::Unsupported("\\begin{align*}"),
                "\\begin{align*} is not supported",
            ),
            (
                MathError::Panicked("boom\nbacktrace".to_string()),
                "renderer failed: boom",
            ),
            (MathError::Empty, "empty formula"),
            (
                MathError::TooWide {
                    needed: 90,
                    available: 36,
                },
                "formula needs 90 columns, 36 available",
            ),
            (MathError::UnknownCommand, "unknown command"),
        ];
        for (error, why) in notes {
            assert_eq!(error.note(), format!("math: {why} (showing source)"));
        }
    }

    /// The real crate draws a fraction on three rows, a matrix and cases
    /// with their brackets, and leaves an unknown command in its output,
    /// which is then refused.
    #[test]
    fn the_crate_draws_the_common_constructs() {
        let fraction = math_rows(&TermMathsBackend, "\\frac{a+b}{c}", 80).unwrap();
        assert_eq!(fraction.len(), 3, "{fraction:?}");
        assert!(fraction[1].contains('─'), "{fraction:?}");
        let matrix = math_rows(
            &TermMathsBackend,
            "\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}",
            80,
        )
        .unwrap();
        assert!(matrix[0].starts_with('⎛'), "{matrix:?}");
        let cases = math_rows(
            &TermMathsBackend,
            "f(x) = \\begin{cases} x & x > 0 \\\\ -x & x \\le 0 \\end{cases}",
            80,
        )
        .unwrap();
        assert_eq!(cases.len(), 2, "{cases:?}");
        assert_eq!(
            math_rows(&TermMathsBackend, "x^2 + y^2", 80),
            Ok(vec!["x² + y²".to_string()])
        );
        assert_eq!(
            math_rows(&TermMathsBackend, "\\foo{x}", 80),
            Err(MathError::UnknownCommand)
        );
        assert!(math_rows(&TermMathsBackend, &nested(MAX_NESTING), 400).is_ok());
    }
}
