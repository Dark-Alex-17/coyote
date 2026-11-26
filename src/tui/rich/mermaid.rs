//! Mermaid diagrams drawn in box-drawing characters. The drawing is done
//! by a backend behind [`MermaidRenderer`]; [`render_mermaid`] guards
//! every backend alike: it caps the source, survives a panic, scrubs the
//! output and refuses a drawing wider than the room it is given.

use crate::tui::render::plain_text;
use crate::tui::terminal::catch_quietly;
use unicode_width::UnicodeWidthStr;

/// The most source a diagram may have before it is shown as source
/// without trying to draw it.
const MAX_SOURCE_BYTES: usize = 16 * 1024;
const MAX_SOURCE_LINES: usize = 400;

/// Draws a mermaid diagram as text in at most `max_width` columns.
pub(crate) trait MermaidRenderer: Send + Sync {
    fn render(&self, source: &str, max_width: u16) -> Result<String, MermaidError>;
}

/// Why a diagram is shown as source instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MermaidError {
    Empty,
    Unsupported(String),
    Parse(String),
    TooWide { needed: usize, available: u16 },
    Panicked(String),
    TooLarge,
}

impl MermaidError {
    /// The one-line note shown under the source.
    pub(crate) fn note(&self) -> String {
        let why = match self {
            MermaidError::Empty => "empty diagram".to_string(),
            MermaidError::Unsupported(kind) => format!("unsupported diagram \"{kind}\""),
            MermaidError::Parse(message) => format!("parse error: {}", first_line(message)),
            MermaidError::TooWide { needed, available } => {
                format!("diagram needs {needed} columns, {available} available")
            }
            MermaidError::Panicked(message) => {
                format!("renderer failed: {}", first_line(message))
            }
            MermaidError::TooLarge => format!(
                "diagram longer than {MAX_SOURCE_LINES} lines or {} KiB",
                MAX_SOURCE_BYTES / 1024
            ),
        };
        format!("mermaid: {why} (showing source)")
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default().trim()
}

/// The rows of `source` drawn by `backend` in at most `width` columns,
/// without control characters or trailing blank rows. A backend that
/// panics, or returns rows wider than `width` (some diagram kinds ignore
/// the width they are given), fails like one that refused.
pub(crate) fn render_mermaid(
    backend: &dyn MermaidRenderer,
    source: &str,
    width: u16,
) -> Result<Vec<String>, MermaidError> {
    if source.len() > MAX_SOURCE_BYTES || source.lines().count() > MAX_SOURCE_LINES {
        return Err(MermaidError::TooLarge);
    }
    let drawn =
        catch_quietly(|| backend.render(source, width)).map_err(MermaidError::Panicked)??;
    let mut rows: Vec<String> = plain_text(&drawn)
        .split('\n')
        .map(|row| row.chars().filter(|c| !c.is_control()).collect())
        .collect();
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }
    if rows.is_empty() {
        return Err(MermaidError::Empty);
    }
    let needed = rows.iter().map(|row| row.width()).max().unwrap_or(0);
    if needed > usize::from(width) {
        return Err(MermaidError::TooWide {
            needed,
            available: width,
        });
    }
    Ok(rows)
}

/// The `mermaid-text` crate, asked for plain box drawing with the width as
/// a hard limit.
pub(crate) struct MermaidTextBackend;

impl MermaidRenderer for MermaidTextBackend {
    fn render(&self, source: &str, max_width: u16) -> Result<String, MermaidError> {
        let options = mermaid_text::RenderOptions {
            max_width: Some(usize::from(max_width)),
            max_width_strict: true,
            color: false,
            ascii: false,
            ..Default::default()
        };
        mermaid_text::render_with_options(source, &options).map_err(|err| match err {
            mermaid_text::Error::EmptyInput => MermaidError::Empty,
            mermaid_text::Error::UnsupportedDiagram(kind) => MermaidError::Unsupported(kind),
            mermaid_text::Error::ParseError(message) => MermaidError::Parse(message),
            mermaid_text::Error::TooWide { actual, .. } => MermaidError::TooWide {
                needed: actual,
                available: max_width,
            },
        })
    }
}

#[cfg(test)]
type Respond = dyn Fn(&str, u16) -> Result<String, MermaidError> + Send + Sync;

/// A backend that answers with whatever `respond` gives and counts its
/// calls.
#[cfg(test)]
pub(crate) struct FakeMermaid {
    respond: Box<Respond>,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl FakeMermaid {
    pub(crate) fn new(
        respond: impl Fn(&str, u16) -> Result<String, MermaidError> + Send + Sync + 'static,
    ) -> Self {
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
impl MermaidRenderer for FakeMermaid {
    fn render(&self, source: &str, max_width: u16) -> Result<String, MermaidError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (self.respond)(source, max_width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fails_with(error: MermaidError) -> FakeMermaid {
        FakeMermaid::new(move |_, _| Err(error.clone()))
    }

    #[test]
    fn each_refusal_of_the_backend_comes_back_as_is() {
        for error in [
            MermaidError::Empty,
            MermaidError::Unsupported("gantt".to_string()),
            MermaidError::Parse("line 2: expected arrow".to_string()),
            MermaidError::TooWide {
                needed: 90,
                available: 40,
            },
        ] {
            let backend = fails_with(error.clone());
            assert_eq!(render_mermaid(&backend, "x", 40), Err(error));
        }
    }

    #[test]
    fn a_panicking_backend_fails_with_its_message() {
        let backend = FakeMermaid::new(|_, _| panic!("layout overflowed"));
        assert_eq!(
            render_mermaid(&backend, "flowchart TD", 40),
            Err(MermaidError::Panicked("layout overflowed".to_string()))
        );
    }

    /// A row wider than the room fails even when the backend called it a
    /// success, since some diagram kinds ignore the width.
    #[test]
    fn rows_wider_than_the_room_fail_whatever_the_backend_said() {
        let backend = FakeMermaid::new(|_, _| Ok("x".repeat(41)));
        assert_eq!(
            render_mermaid(&backend, "sequenceDiagram", 40),
            Err(MermaidError::TooWide {
                needed: 41,
                available: 40,
            })
        );
    }

    #[test]
    fn too_much_source_is_not_handed_to_the_backend() {
        let backend = FakeMermaid::new(|_, _| Ok("drawn".to_string()));
        let long = "A-->B\n".repeat(MAX_SOURCE_LINES + 1);
        let big = "x".repeat(MAX_SOURCE_BYTES + 1);
        for source in [long, big] {
            assert_eq!(
                render_mermaid(&backend, &source, 80),
                Err(MermaidError::TooLarge)
            );
        }
        assert_eq!(backend.calls(), 0);
    }

    #[test]
    fn control_characters_and_trailing_blank_rows_are_dropped() {
        let backend =
            FakeMermaid::new(|_, _| Ok("\x1b[31m┌─┐\x1b[0m\r\n│\x07a│\n└─┘\n\n  \n".to_string()));
        assert_eq!(
            render_mermaid(&backend, "flowchart TD", 80),
            Ok(vec![
                "┌─┐".to_string(),
                "│a│".to_string(),
                "└─┘".to_string()
            ])
        );
        let blank = FakeMermaid::new(|_, _| Ok("\n \n".to_string()));
        assert_eq!(
            render_mermaid(&blank, "flowchart TD", 80),
            Err(MermaidError::Empty)
        );
    }

    #[test]
    fn every_note_is_one_line_naming_the_reason() {
        let notes = [
            (MermaidError::Empty, "empty diagram"),
            (
                MermaidError::Unsupported("gantt".to_string()),
                "unsupported diagram \"gantt\"",
            ),
            (
                MermaidError::Parse("bad arrow\nat line 3".to_string()),
                "parse error: bad arrow",
            ),
            (
                MermaidError::TooWide {
                    needed: 90,
                    available: 40,
                },
                "diagram needs 90 columns, 40 available",
            ),
            (
                MermaidError::Panicked("boom".to_string()),
                "renderer failed: boom",
            ),
            (
                MermaidError::TooLarge,
                "diagram longer than 400 lines or 16 KiB",
            ),
        ];
        for (error, why) in notes {
            assert_eq!(error.note(), format!("mermaid: {why} (showing source)"));
        }
    }

    /// The real crate draws a flowchart within the width, and reports one
    /// it cannot fit as too wide rather than overflowing.
    #[test]
    fn the_crate_draws_within_the_width_or_says_it_cannot() {
        let rows = render_mermaid(&MermaidTextBackend, "flowchart LR\nA-->B", 40).unwrap();
        assert!(rows.iter().all(|row| row.width() <= 40), "{rows:?}");
        assert!(rows.concat().contains('A'), "{rows:?}");
        let wide = "flowchart LR\nAlpha-->Bravo-->Charlie-->Delta-->Echo-->Foxtrot";
        assert!(matches!(
            render_mermaid(&MermaidTextBackend, wide, 12),
            Err(MermaidError::TooWide { available: 12, .. })
        ));
        assert!(matches!(
            render_mermaid(&MermaidTextBackend, "zenuml\nA.b()", 80),
            Err(MermaidError::Unsupported(_))
        ));
    }
}
