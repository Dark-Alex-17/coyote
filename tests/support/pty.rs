//! The harness the REPL tests drive the built binary through: the fixture dirs a
//! probe boots from, a pty session sized like a real terminal, waits that answer
//! the REPL's cursor position queries, and the escape stripping the assertions
//! read the painted output through. The pty half is Unix only, as expectrl's
//! dev-dependency is.

use std::env;
use std::fs;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use expectrl::session::{OsProcess, OsStream};
#[cfg(unix)]
use expectrl::{Any, Expect, Regex, Session};

use super::bin::Dirs;

/// Upper bound on any single wait. The binary is a cold debug build that opens
/// a vault, resolves a model and, for the mesh tests, starts a Reticulum
/// transport before its first prompt.
#[cfg(unix)]
pub const EXPECT_TIMEOUT: Duration = Duration::from_secs(90);

#[cfg(unix)]
pub const TERMINAL_COLUMNS: u16 = 160;
#[cfg(unix)]
pub const TERMINAL_ROWS: u16 = 50;

/// The Device Status Report request crossterm sends to learn where the cursor
/// is, as a regex so it can share an `Any` with the pattern waited on.
#[cfg(unix)]
pub const CURSOR_POSITION_QUERY: &str = r"\x1b\[6n";
/// [`CURSOR_POSITION_QUERY`] as the bytes the REPL writes.
#[cfg(unix)]
pub const CURSOR_POSITION_QUERY_BYTES: &[u8] = b"\x1b[6n";

/// Reply to a query that no column move preceded; see
/// [`cursor_position_reply`] for the queries that follow one.
const CURSOR_POSITION_REPLY: &str = "\x1b[2;1R";

#[cfg(unix)]
pub type Target = Session<OsProcess, OsStream>;

/// A fresh dir `<prefix>-<label>-<nanos>` under the process temp dir.
pub fn fresh_dir(prefix: &str, label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!("{prefix}-{label}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The config, home and temp dirs one spawned binary runs against, removed on
/// drop so a failed run does not leave them behind. Each test file writes its
/// own config into `config_dir` and builds its own command from [`Probe::dirs`].
pub struct Probe {
    pub config_dir: PathBuf,
    pub home_dir: PathBuf,
    pub temp_root: PathBuf,
}

impl Probe {
    /// Three fresh dirs named after `prefix` and `label`, with the vault
    /// password at [`Probe::vault_pass`] already written.
    pub fn fresh(prefix: &str, label: &str) -> Self {
        let probe = Self {
            config_dir: fresh_dir(prefix, &format!("{label}-cfg")),
            home_dir: fresh_dir(prefix, &format!("{label}-home")),
            temp_root: fresh_dir(prefix, &format!("{label}-tmp")),
        };
        fs::write(probe.vault_pass(), "test-password\n").unwrap();
        probe
    }

    pub fn vault_pass(&self) -> PathBuf {
        self.config_dir.join("vault-pass")
    }

    /// The probe's dirs for [`super::bin::coyote_command`], the cache under
    /// `temp_root`.
    pub fn dirs(&self) -> Dirs {
        Dirs {
            config: self.config_dir.clone(),
            cache: self.temp_root.join("coyote"),
            home: self.home_dir.clone(),
            temp: self.temp_root.clone(),
        }
    }

    pub fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.config_dir);
        let _ = fs::remove_dir_all(&self.home_dir);
        let _ = fs::remove_dir_all(&self.temp_root);
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// `command` in a pty [`TERMINAL_COLUMNS`] by [`TERMINAL_ROWS`], every wait
/// bounded by [`EXPECT_TIMEOUT`].
#[cfg(unix)]
pub fn spawn_repl(command: Command) -> Target {
    spawn_repl_with_columns(command, TERMINAL_COLUMNS)
}

/// [`spawn_repl`] with the pty `columns` wide instead of the default.
#[cfg(unix)]
pub fn spawn_repl_with_columns(command: Command, columns: u16) -> Target {
    let mut session = Session::spawn(command).expect("spawn coyote in a pty");
    session
        .get_process_mut()
        .set_window_size(columns, TERMINAL_ROWS)
        .expect("set pty window size");
    session.set_expect_timeout(Some(EXPECT_TIMEOUT));
    session
}

/// Sends one reply per cursor position query `read` holds, wherever it sits,
/// each judged by the bytes that led up to it, and says whether there was one.
/// A query left unanswered would cost the REPL its query timeout and leave
/// every later reply one behind.
#[cfg(unix)]
pub fn answer_cursor_queries(session: &mut Target, read: &[u8]) -> bool {
    let query = CURSOR_POSITION_QUERY_BYTES;
    let mut answered = false;
    let mut from = 0;
    while let Some(at) = read[from..]
        .windows(query.len())
        .position(|window| window == query)
    {
        let at = from + at;
        session
            .send(cursor_position_reply(&read[..at]))
            .expect("reply to the cursor position query");
        from = at + query.len();
        answered = true;
    }
    answered
}

/// Waits for the regex `pattern`, answering every cursor position query that
/// arrives first, and returns the bytes that came before the match.
#[cfg(unix)]
pub fn wait_for(session: &mut Target, pattern: &str) -> Vec<u8> {
    wait_for_match(session, pattern).0
}

/// Like [`wait_for`], but returns the escape-stripped UTF-8 text of what came
/// before the match and the match itself.
#[cfg(unix)]
pub fn wait_for_text(session: &mut Target, pattern: &str) -> String {
    let (before, matched) = wait_for_match(session, pattern);
    utf8_text(&[before, matched].concat())
}

/// Like [`wait_for`], but returns the escape-stripped text of the row the match
/// landed on, through the end of the match.
#[cfg(unix)]
pub fn wait_for_row(session: &mut Target, pattern: &str) -> String {
    let (before, matched) = wait_for_match(session, pattern);
    row_of(&before, &matched)
}

/// Waits for the regex `pattern`, answering every cursor position query that
/// arrives first, and returns the bytes before the first match and the bytes
/// from that match through the last one the read held. expectrl consumes the
/// stream through the right-most match of the needle that fired, so when one
/// read holds the needle twice (a table whose rows repeat a note, a prompt
/// repainted twice) the bytes between the matches are gone from the stream
/// after this call; keeping them in the matched half is what lets a caller
/// rebuild the whole transcript. Which rows land in one read depends on the
/// platform's pty, so a transcript built from the first match alone is whole
/// on one machine and missing rows on another.
///
/// A timeout panics with what the REPL printed instead and whether it is still
/// running.
#[cfg(unix)]
pub fn wait_for_match(session: &mut Target, pattern: &str) -> (Vec<u8>, Vec<u8>) {
    let mut before = Vec::new();
    loop {
        let captures = session
            .expect(Any([Regex(pattern), Regex(CURSOR_POSITION_QUERY)]))
            .unwrap_or_else(|err| {
                // At most 3 s of further reads, the last 2000 characters of them.
                session.set_expect_timeout(Some(Duration::from_secs(1)));
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut pending = String::new();
                while Instant::now() < deadline {
                    match session.expect(Regex(r"(?s).+")) {
                        Ok(rest) => pending.push_str(&strip_escapes(rest.as_bytes())),
                        Err(_) => break,
                    }
                }
                let skip = pending.chars().count().saturating_sub(2000);
                let pending: String = pending.chars().skip(skip).collect();
                let status = session.get_process().status().map_or_else(
                    |err| format!("unknown ({err})"),
                    |status| format!("{status:?}"),
                );
                panic!(
                    "waiting for {pattern:?}: {err}\nREPL process: {status}\nread before it: {:?}\npending: {pending:?}",
                    strip_escapes(&before)
                )
            });
        let involved = captures.as_bytes();
        let split = captures.before().len();
        answer_cursor_queries(session, involved);
        if captures.get(0).unwrap_or_default() == CURSOR_POSITION_QUERY_BYTES {
            // The queries themselves are escape sequences the text helpers strip, so
            // the whole read goes into `before`.
            before.extend_from_slice(involved);
            continue;
        }
        before.extend_from_slice(&involved[..split]);
        return (before, involved[split..].to_vec());
    }
}

/// The escape-stripped text of the row `matched` landed on: everything after
/// the last line break before it, with a trailing `\r` trimmed.
pub fn row_of(before: &[u8], matched: &[u8]) -> String {
    let text = strip_escapes(before) + &strip_escapes(matched);
    text.rsplit('\n')
        .next()
        .unwrap_or_default()
        .trim_end_matches('\r')
        .to_string()
}

/// The reply to a cursor position query that arrived after `before`. Before
/// every prompt the REPL moves the cursor to a sentinel column (`ESC[<n>G`),
/// queries, and trusts the answer only when the column matches, so a query
/// preceded by a column move is answered with that column verbatim (the
/// terminal reports the same 1-based number the move named). A query with no
/// move before it gets the fixed reply.
pub fn cursor_position_reply(before: &[u8]) -> String {
    match last_column_move(before) {
        Some(column) => format!("\x1b[2;{column}R"),
        None => CURSOR_POSITION_REPLY.to_string(),
    }
}

/// The parameter of the last `ESC[<n>G` in `bytes`, verbatim.
fn last_column_move(bytes: &[u8]) -> Option<&str> {
    let mut found = None;
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let start = i + 2;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start && bytes.get(end) == Some(&b'G') {
                found = std::str::from_utf8(&bytes[start..end]).ok();
            }
        }
        i += 1;
    }
    found
}

/// Drops CSI and OSC sequences and two-byte ESC pairs; every other byte is kept as the
/// Latin-1 character of its value, which is what the prompt and ASCII-only assertions
/// read. A table's box-drawing characters need [`utf8_text`] instead.
pub fn strip_escapes(bytes: &[u8]) -> String {
    strip_escape_bytes(bytes)
        .into_iter()
        .map(char::from)
        .collect()
}

/// The bytes of `bytes` with CSI and OSC sequences (BEL- or ST-terminated) and
/// two-byte ESC pairs dropped.
fn strip_escape_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            match bytes.get(i + 1) {
                Some(b'[') => {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                Some(b']') => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != 0x07 {
                        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                Some(_) => {
                    i += 2;
                    continue;
                }
                None => break,
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// [`strip_escapes`] decoding the remaining bytes as UTF-8, so the box-drawing
/// characters a table is drawn with come through as the characters the user sees.
pub fn utf8_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&strip_escape_bytes(bytes)).into_owned()
}
