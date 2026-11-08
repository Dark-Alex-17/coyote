//! The composer's line history, kept in the file the line editor of the
//! plain REPL writes so the two share one record. The file holds one
//! entry per line, a newline inside an entry stored as the four
//! characters `<\n>` (that is reedline's `FileBackedHistory` encoding),
//! oldest first, at most `CAPACITY` entries.

use std::fs;
use std::io;
use std::path::Path;

use crate::utils::fs::{create_private_dir_all, write_private_atomic};

const CAPACITY: usize = 1000;
const NEWLINE_ESCAPE: &str = "<\\n>";

fn encode_entry(entry: &str) -> String {
    entry.replace('\n', NEWLINE_ESCAPE)
}

fn decode_entry(line: &str) -> String {
    line.replace(NEWLINE_ESCAPE, "\n")
}

fn read_entries(path: &Path) -> io::Result<Vec<String>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut entries: Vec<String> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(decode_entry)
        .collect();
    cap(&mut entries);
    Ok(entries)
}

/// Drops the oldest entries past the capacity.
fn cap(entries: &mut Vec<String>) {
    let excess = entries.len().saturating_sub(CAPACITY);
    entries.drain(..excess);
}

/// Pushes `line` unless it repeats the newest entry, the way a shell's
/// history skips an immediate repeat.
fn push_entry(entries: &mut Vec<String>, line: &str) {
    if entries.last().is_some_and(|last| last == line) {
        return;
    }
    entries.push(line.to_string());
    cap(entries);
}

/// Records `line` in the file at `path`, creating it (owner-only) and
/// its directory as needed and dropping the oldest entries past the
/// capacity. Prompts can carry secrets, so the file is never world-readable.
pub(crate) fn append(path: &Path, line: &str) -> io::Result<()> {
    let mut entries = read_entries(path)?;
    push_entry(&mut entries, line);
    if let Some(dir) = path.parent() {
        create_private_dir_all(dir)?;
    }
    let mut text = String::new();
    for entry in &entries {
        text.push_str(&encode_entry(entry));
        text.push('\n');
    }
    write_private_atomic(path, &text)
}

/// The entries in memory and the walk through them: `cursor` is the
/// entry the composer shows while a walk is on, `draft` what it showed
/// before the walk started, given back when the walk passes the newest.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ComposerHistory {
    entries: Vec<String>,
    cursor: Option<usize>,
    draft: Option<String>,
}

impl ComposerHistory {
    /// The entries on disk; a missing file is an empty history.
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        Ok(Self {
            entries: read_entries(path)?,
            cursor: None,
            draft: None,
        })
    }

    pub(crate) fn push(&mut self, line: &str) {
        push_entry(&mut self.entries, line);
        self.reset();
    }

    /// Ends a walk; the next `prev` starts from the newest entry again.
    pub(crate) fn reset(&mut self) {
        self.cursor = None;
        self.draft = None;
    }

    /// One step towards the oldest entry. The first step stashes `draft`;
    /// a step from the oldest entry stays there.
    pub(crate) fn prev(&mut self, draft: &str) -> Option<&str> {
        let index = match self.cursor {
            None => {
                if self.entries.is_empty() {
                    return None;
                }
                self.draft = Some(draft.to_string());
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(index) => index - 1,
        };
        self.cursor = Some(index);
        Some(&self.entries[index])
    }

    /// One step towards the newest entry; the step past it ends the walk
    /// and gives the stashed draft back. None while no walk is on.
    pub(crate) fn next(&mut self) -> Option<String> {
        let index = self.cursor? + 1;
        if index < self.entries.len() {
            self.cursor = Some(index);
            Some(self.entries[index].clone())
        } else {
            self.cursor = None;
            Some(self.draft.take().unwrap_or_default())
        }
    }

    /// The entries containing `filter`, newest first; every entry for an
    /// empty filter.
    pub(crate) fn search(&self, filter: &str) -> Vec<&str> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.contains(filter))
            .map(String::as_str)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScratchDirGuard;

    #[test]
    fn entries_round_trip_through_the_file_with_newlines_escaped() {
        let dir = ScratchDirGuard::new("history-round-trip");
        // The parent does not exist yet: the first append creates it.
        let path = dir.path.join("state").join("history");
        append(&path, "first").unwrap();
        append(&path, "two\nlines").unwrap();
        append(&path, "last").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, "first\ntwo<\\n>lines\nlast\n");
        let history = ComposerHistory::load(&path).unwrap();
        assert_eq!(history.entries, ["first", "two\nlines", "last"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = ScratchDirGuard::new("history-mode");
        let path = dir.path.join("history");
        append(&path, "secret prompt").unwrap();
        append(&path, "another").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
    }

    #[test]
    fn a_missing_file_is_an_empty_history() {
        let dir = ScratchDirGuard::new("history-missing");
        let history = ComposerHistory::load(&dir.path.join("history")).unwrap();
        assert_eq!(history, ComposerHistory::default());
    }

    #[test]
    fn the_file_keeps_the_newest_thousand_entries() {
        let dir = ScratchDirGuard::new("history-cap");
        let path = dir.path.join("history");
        let mut text = String::new();
        for i in 0..CAPACITY + 5 {
            text.push_str(&format!("entry {i}\n"));
        }
        fs::write(&path, text).unwrap();
        append(&path, "newest").unwrap();
        let history = ComposerHistory::load(&path).unwrap();
        assert_eq!(history.entries.len(), CAPACITY);
        assert_eq!(history.entries[0], "entry 6");
        assert_eq!(history.entries[CAPACITY - 1], "newest");
    }

    #[test]
    fn an_immediate_repeat_is_not_recorded_twice() {
        let mut history = ComposerHistory::default();
        history.push("same");
        history.push("same");
        history.push("other");
        history.push("same");
        assert_eq!(history.entries, ["same", "other", "same"]);
    }

    #[test]
    fn the_walk_goes_newest_first_and_gives_the_draft_back() {
        let mut history = ComposerHistory::default();
        assert_eq!(history.prev("draft"), None);
        history.push("one");
        history.push("two");
        history.push("three");
        assert_eq!(history.prev("draft"), Some("three"));
        assert_eq!(history.prev("ignored"), Some("two"));
        assert_eq!(history.prev("ignored"), Some("one"));
        assert_eq!(history.prev("ignored"), None);
        assert_eq!(history.next().as_deref(), Some("two"));
        assert_eq!(history.next().as_deref(), Some("three"));
        assert_eq!(history.next().as_deref(), Some("draft"));
        assert_eq!(history.next(), None);
        assert_eq!(history.prev("again"), Some("three"));
        history.push("four");
        assert_eq!(history.next(), None);
        assert_eq!(history.prev(""), Some("four"));
    }

    #[test]
    fn search_is_a_substring_match_newest_first() {
        let mut history = ComposerHistory::default();
        history.push("git status");
        history.push("cargo test");
        history.push("git log");
        assert_eq!(history.search("git"), ["git log", "git status"]);
        assert_eq!(history.search(""), ["git log", "cargo test", "git status"]);
        assert!(history.search("zzz").is_empty());
    }
}
