use std::fmt;

/// The surface that answers this run's prompts, exported to tools as
/// `COYOTE_UI` so a tool can tell whether a human can be asked at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiKind {
    Repl,
    Headless,
}

impl UiKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Repl => "repl",
            Self::Headless => "headless",
        }
    }
}

impl fmt::Display for UiKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
