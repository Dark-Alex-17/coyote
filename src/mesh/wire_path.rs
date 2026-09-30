//! The relative-path grammar on the wire. A peer names a file by a `/`-separated, NFC,
//! relative path; `WirePath::parse` is the one gate every such string passes before the
//! filesystem is touched, and the `rule` it refuses with is what the `invalid_path` reply
//! carries. A path that is not already NFC is refused, never normalised, so the name the
//! peer sent and the name on disk are the same bytes.

use std::fmt;
use std::path::PathBuf;
use unicode_normalization::is_nfc;

pub(crate) const WIRE_PATH_MAX_BYTES: usize = 1_024;
pub(crate) const WIRE_PATH_MAX_SEGMENTS: usize = 64;

type Rule = (&'static str, fn(&str) -> bool);

/// The rules in the order they are checked; the first one a string breaks names the
/// refusal. `backslash` comes before `drive_letter` so `C:\x` is refused for the separator
/// it carries, and `nfc` before the segment rules so a decomposed name is never split.
const RULES: [Rule; 9] = [
    ("empty", str::is_empty),
    ("length", |text| text.len() > WIRE_PATH_MAX_BYTES),
    ("control", |text| text.chars().any(char::is_control)),
    ("backslash", |text| text.contains('\\')),
    ("leading_slash", |text| text.starts_with('/')),
    ("drive_letter", |text| {
        let bytes = text.as_bytes();
        bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':')
    }),
    ("nfc", |text| !is_nfc(text)),
    ("segments", |text| {
        text.split('/').count() > WIRE_PATH_MAX_SEGMENTS
    }),
    ("segment", |text| {
        text.split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
    }),
];

/// A path string that has passed every rule in `RULES`, so it is relative, NFC, and free of
/// empty, `.` and `..` segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct WirePath(String);

/// Which rule the string broke; the id is stable wire vocabulary, not prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidPath {
    pub rule: &'static str,
}

impl fmt::Display for InvalidPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "The path breaks the `{}` rule", self.rule)
    }
}

impl std::error::Error for InvalidPath {}

impl WirePath {
    pub(crate) fn parse(text: &str) -> Result<Self, InvalidPath> {
        match RULES.iter().find(|(_, broken)| broken(text)) {
            Some((rule, _)) => Err(InvalidPath { rule }),
            None => Ok(Self(text.to_string())),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The segments joined with the platform separator, for use under a root of the
    /// caller's choosing; it is never absolute.
    pub(crate) fn to_relative_path(&self) -> PathBuf {
        self.segments().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn rule(text: &str) -> &'static str {
        WirePath::parse(text).unwrap_err().rule
    }

    #[test]
    fn an_empty_string_is_refused_with_rule_empty() {
        assert_eq!(rule(""), "empty");
    }

    #[test]
    fn a_path_over_the_byte_limit_is_refused_with_rule_length() {
        assert_eq!(rule(&"a".repeat(WIRE_PATH_MAX_BYTES + 1)), "length");
        assert!(WirePath::parse(&"a".repeat(WIRE_PATH_MAX_BYTES)).is_ok());
    }

    #[test]
    fn a_path_with_a_control_character_is_refused_with_rule_control() {
        for text in ["a\0b", "a\tb", "a\nb", "a\u{7f}b", "a\u{85}b"] {
            assert_eq!(rule(text), "control", "{text:?}");
        }
    }

    #[test]
    fn a_path_with_a_backslash_is_refused_with_rule_backslash() {
        assert_eq!(rule("docs\\a.md"), "backslash");
        assert_eq!(rule("C:\\x"), "backslash");
    }

    #[test]
    fn a_path_with_a_leading_slash_is_refused_with_rule_leading_slash() {
        assert_eq!(rule("/etc/passwd"), "leading_slash");
    }

    #[test]
    fn a_path_with_a_drive_letter_is_refused_with_rule_drive_letter() {
        assert_eq!(rule("C:x"), "drive_letter");
        assert_eq!(rule("c:/x"), "drive_letter");
        assert!(WirePath::parse("1:x").is_ok());
    }

    #[test]
    fn a_decomposed_name_is_refused_with_rule_nfc() {
        assert_eq!(rule("docs/he\u{301}llo/nai\u{308}ve.md"), "nfc");
    }

    #[test]
    fn a_path_over_the_segment_limit_is_refused_with_rule_segments() {
        let over = vec!["a"; WIRE_PATH_MAX_SEGMENTS + 1].join("/");
        assert_eq!(rule(&over), "segments");
        let at = vec!["a"; WIRE_PATH_MAX_SEGMENTS].join("/");
        assert!(WirePath::parse(&at).is_ok());
    }

    #[test]
    fn a_path_with_a_dot_dot_segment_is_refused_with_rule_segment() {
        for text in ["../../.bashrc", "a/../b", "./a", "a/./b", "a//b", "a/", "."] {
            assert_eq!(rule(text), "segment", "{text:?}");
        }
    }

    #[test]
    fn the_first_broken_rule_in_order_names_the_refusal() {
        let long_and_traversing = format!("../{}", "a".repeat(WIRE_PATH_MAX_BYTES));
        assert_eq!(rule(&long_and_traversing), "length");
        assert_eq!(rule("\\.."), "backslash");
    }

    #[test]
    fn a_composed_non_ascii_path_parses_and_round_trips() {
        let text = "docs/h\u{e9}llo/na\u{ef}ve.md";
        let path = WirePath::parse(text).unwrap();
        assert_eq!(path.as_str(), text);
        assert_eq!(
            path.segments().collect::<Vec<_>>(),
            ["docs", "h\u{e9}llo", "na\u{ef}ve.md"]
        );
        assert_eq!(
            path.to_relative_path(),
            Path::new("docs").join("h\u{e9}llo").join("na\u{ef}ve.md")
        );
        assert!(path.to_relative_path().is_relative());
    }
}
