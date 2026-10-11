//! Hygiene pins over the comment lines of the files this branch owns. Two ignored pins
//! queue every comment line that cites planning vocabulary or carries a non-ASCII
//! character; the ungated tests hold the comment extractors, the needle matchers and the
//! allow-list itself. Every needle and fixture is assembled at runtime so the repository's
//! own label scanner does not trip on this file.
//!
//! Rust block comments are scanned as well as line comments, so the Rust class covers a
//! superset of what the hash and markdown classes yield.
//!
//! Three calibrations narrow the vocabulary pin: a spaced pass number only counts beside a
//! word naming the review itself, the transport's bare name is spared when one of its
//! nouns follows it, and the spec draft id needle wants an uppercase letter after its dash
//! so a lowercase repository name sharing the prefix is not a citation. The spec draft id,
//! the wire-spec id and the plan docs directory are matched on the raw comment text; every
//! other needle sees quotes and code spans removed.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::Path;
use std::process::Command;

use crate::mesh::test_support::TempDir;

/// Directories whose every tracked file is branch-owned. A trailing `*/` matches exactly
/// one directory name, so `scripts/mesh-relay.sh` is a file and is listed by name below.
const ALLOW_LISTED_DIRS: [&str; 3] = [
    "src/mesh/",
    "deployment/propagation-node/",
    "scripts/mesh-*/",
];

/// Branch-owned files outside those directories, plus the CI workflow: the one entry that
/// also exists on `main`, whose main-side lines carry no hit.
const ALLOW_LISTED_FILES: [&str; 35] = [
    CI_WORKFLOW,
    "assets/envoy/config.yaml",
    "assets/envoy/tools.py",
    "scripts/image-smoke.sh",
    "scripts/mesh-relay.ps1",
    "scripts/mesh-relay.sh",
    "scripts/reticulum.config.tmpl",
    "src/config/envoy.rs",
    "src/config/envoy_tools_tests.rs",
    "src/config/mesh_config.rs",
    "src/config/mesh_digest.rs",
    "src/config/mesh_envoy.rs",
    "src/config/mesh_hooks.rs",
    "src/config/mesh_snapshot.rs",
    "src/config/reserved_agents.rs",
    "src/function/mesh.rs",
    PINS_FILE,
    "src/repl/idle.rs",
    "src/repl/mesh.rs",
    "src/repl/mesh_share_vectors.rs",
    "src/repl/printer.rs",
    "src/utils/untrusted_content.rs",
    "src/utils/windows_acl.rs",
    "tests/docker_entrypoint_script.rs",
    "tests/envoy_repl.rs",
    "tests/fixtures/mesh-relay-task-harness.ps1",
    "tests/installer_script.rs",
    "tests/mesh_autostart_repl.rs",
    "tests/mesh_dependencies.rs",
    "tests/mesh_relay_script.rs",
    "tests/mesh_relay_task.rs",
    "tests/mesh_wiki_docs.rs",
    "tests/propagation_node_image.rs",
    "tests/pty_repl.rs",
    "tests/scripts_pins.rs",
];

const CI_WORKFLOW: &str = ".github/workflows/ci.yaml";
const PINS_FILE: &str = "src/hygiene_pins.rs";

/// `git <args>` in `root`: stdout on success, trimmed stderr on failure. A `cargo test`
/// launched from a git hook inherits `GIT_DIR` and friends, which would point every call
/// at the hook's repository regardless of `-C`; the fixtures below init and write to
/// temporary repositories, so those variables are dropped.
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git runs");
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn nul_separated(stdout: &[u8]) -> BTreeSet<String> {
    stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).expect("git paths are UTF-8"))
        .collect()
}

fn tracked_paths(root: &Path) -> BTreeSet<String> {
    nul_separated(&git(root, &["ls-files", "-z"]).expect("git ls-files"))
}

/// Whether the checks that compare the allow-list against `origin/main` can run: the ref
/// must resolve (a shallow CI checkout has none) and must not carry this file yet. Once
/// the branch has landed, every allow-listed path exists on `main` and the three-dot diff
/// is empty, so those checks describe a branch that no longer exists; the pins themselves
/// keep running.
fn origin_main_predates_the_pins(root: &Path) -> Result<(), String> {
    git(root, &["rev-parse", "--verify", "origin/main"])
        .map_err(|reason| format!("origin/main unavailable: {reason}"))?;
    let pins_on_main = ["origin/main:", PINS_FILE].concat();
    match git(root, &["cat-file", "-e", &pins_on_main]) {
        Ok(_) => Err(format!(
            "origin/main already carries {PINS_FILE}, so the branch has landed"
        )),
        Err(_) => Ok(()),
    }
}

fn matches_dir_glob(path: &str, glob: &str) -> bool {
    match glob.strip_suffix("*/") {
        Some(prefix) => path
            .strip_prefix(prefix)
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(segment, _)| !segment.is_empty()),
        None => path.starts_with(glob),
    }
}

fn is_allow_listed(path: &str) -> bool {
    ALLOW_LISTED_DIRS
        .iter()
        .any(|glob| matches_dir_glob(path, glob))
        || ALLOW_LISTED_FILES.contains(&path)
}

/// The allow-list resolved against the index, never the working tree.
fn allow_listed_paths(root: &Path) -> BTreeSet<String> {
    tracked_paths(root)
        .into_iter()
        .filter(|path| is_allow_listed(path))
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Flavour {
    Plain,
    Python,
    PowerShell,
}

enum Class {
    Rust,
    Hash(Flavour),
    Markdown,
    Opaque,
}

fn class_of(path: &str) -> Class {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name == "Dockerfile" {
        return Class::Hash(Flavour::Plain);
    }
    match name.rsplit_once('.').map(|(_, extension)| extension) {
        Some("rs") => Class::Rust,
        Some("py") => Class::Hash(Flavour::Python),
        Some("ps1") => Class::Hash(Flavour::PowerShell),
        Some("sh" | "yaml" | "yml" | "toml" | "tmpl" | "config") => Class::Hash(Flavour::Plain),
        Some("md") => Class::Markdown,
        _ => Class::Opaque,
    }
}

/// The comment text of `source`, keyed by 1-based line, for the file class `path` names.
fn comment_lines(path: &str, source: &str) -> Vec<(usize, String)> {
    match class_of(path) {
        Class::Rust => rust_comment_lines(source),
        Class::Hash(flavour) => hash_comment_lines(source, flavour),
        Class::Markdown => markdown_prose_lines(source),
        Class::Opaque => Vec::new(),
    }
}

fn push_text(out: &mut Vec<(usize, String)>, line: usize, text: &str) {
    let text = text.trim();
    if !text.is_empty() {
        out.push((line, text.to_string()));
    }
}

/// Where `bytes[at]` opens a raw string (`r"`, `r#"`, `br"`, `cr##"`), the length of the
/// opener and its hash count. A preceding identifier byte means `r` is part of a name.
fn raw_string_opener(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let identifier_before = at
        .checked_sub(1)
        .and_then(|before| bytes.get(before))
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    if identifier_before {
        return None;
    }
    let mut end = at;
    if matches!(bytes[end], b'b' | b'c') {
        end += 1;
    }
    if bytes.get(end) != Some(&b'r') {
        return None;
    }
    end += 1;
    let hashes = bytes[end..].iter().take_while(|b| **b == b'#').count();
    end += hashes;
    (bytes.get(end) == Some(&b'"')).then_some((end + 1 - at, hashes))
}

/// The length of the char literal or lifetime opened by the `'` at `at`: an escape runs
/// to the next `'`, a single char closed by `'` is a literal, anything else is a lifetime.
fn char_or_lifetime_len(source: &str, at: usize) -> usize {
    let rest = &source[at + 1..];
    let mut chars = rest.chars();
    match chars.next() {
        Some('\\') => {
            chars.next();
            let consumed = rest.len() - chars.as_str().len();
            match chars.as_str().find('\'') {
                Some(close) => 1 + consumed + close + 1,
                None => 1,
            }
        }
        Some(c) if c != '\'' && chars.next() == Some('\'') => 1 + c.len_utf8() + 1,
        _ => 1,
    }
}

/// Text inside line and block comments, without the `//`, `///`, `//!`, `/*`, `*/`
/// markers. String, raw string and char literals are tracked across lines so a `//` or
/// `/*` inside one is not a comment; `#[doc = "..."]` is a string.
fn rust_comment_lines(source: &str) -> Vec<(usize, String)> {
    enum State {
        Code,
        Str,
        RawStr(usize),
        Block(usize),
    }
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut state = State::Code;
    let mut line = 1;
    let mut block_start = 0;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'\n' {
            if let State::Block(_) = state {
                push_text(&mut out, line, &source[block_start..at]);
                block_start = at + 1;
            }
            line += 1;
            at += 1;
            continue;
        }
        match state {
            State::Code => {
                let rest = &bytes[at..];
                if rest.starts_with(b"//") {
                    let end = at + rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
                    let text = source[at + 2..end].trim_start_matches('/');
                    push_text(&mut out, line, text.strip_prefix('!').unwrap_or(text));
                    at = end;
                } else if rest.starts_with(b"/*") {
                    state = State::Block(1);
                    at += 2;
                    block_start = at;
                } else if byte == b'"' {
                    state = State::Str;
                    at += 1;
                } else if let Some((opener, hashes)) = raw_string_opener(bytes, at) {
                    state = State::RawStr(hashes);
                    at += opener;
                } else if byte == b'\'' {
                    at += char_or_lifetime_len(source, at);
                } else {
                    at += 1;
                }
            }
            State::Str => match byte {
                b'\\' if bytes.get(at + 1) != Some(&b'\n') => at += 2,
                b'"' => {
                    state = State::Code;
                    at += 1;
                }
                _ => at += 1,
            },
            State::RawStr(hashes) => {
                if byte == b'"'
                    && bytes[at + 1..].iter().take_while(|b| **b == b'#').count() >= hashes
                {
                    state = State::Code;
                    at += 1 + hashes;
                } else {
                    at += 1;
                }
            }
            State::Block(depth) => {
                let rest = &bytes[at..];
                if rest.starts_with(b"/*") {
                    state = State::Block(depth + 1);
                    at += 2;
                } else if rest.starts_with(b"*/") {
                    if depth == 1 {
                        push_text(&mut out, line, &source[block_start..at]);
                        state = State::Code;
                    } else {
                        state = State::Block(depth - 1);
                    }
                    at += 2;
                } else {
                    at += 1;
                }
            }
        }
    }
    out
}

/// Bytes consumed by a quoted string after its opening `quote`, closing quote included;
/// an unclosed string runs to the end of the line.
fn quoted_len(rest: &[u8], quote: u8, escape: Option<u8>) -> usize {
    let mut at = 0;
    while at < rest.len() {
        if Some(rest[at]) == escape {
            at += 2;
        } else if rest[at] == quote {
            return at + 1;
        } else {
            at += 1;
        }
    }
    rest.len()
}

/// Text after a `#` that stands at line start or after whitespace, outside quotes, so
/// `$#`, `${#x}` and `url#frag` are code. Python adds triple-quoted strings; PowerShell
/// adds `<# ... #>` block comments (every line inside is comment text) and here-strings.
fn hash_comment_lines(source: &str, flavour: Flavour) -> Vec<(usize, String)> {
    enum State {
        Code,
        Triple(&'static str),
        Block,
        HereString(u8),
    }
    let escape = |quote: u8| match (flavour, quote) {
        (Flavour::Python, _) | (Flavour::Plain, b'"') => Some(b'\\'),
        (Flavour::PowerShell, b'"') => Some(b'`'),
        _ => None,
    };
    let mut out = Vec::new();
    let mut state = State::Code;
    for (index, raw) in source.lines().enumerate() {
        let number = index + 1;
        let mut line = raw;
        loop {
            match state {
                State::Triple(delimiter) => match line.find(delimiter) {
                    Some(close) => {
                        line = &line[close + delimiter.len()..];
                        state = State::Code;
                    }
                    None => break,
                },
                State::HereString(quote) => {
                    if raw.as_bytes().first() == Some(&quote)
                        && raw.as_bytes().get(1) == Some(&b'@')
                    {
                        line = &raw[2..];
                        state = State::Code;
                    } else {
                        break;
                    }
                }
                State::Block => match line.find("#>") {
                    Some(close) => {
                        push_text(&mut out, number, &line[..close]);
                        line = &line[close + 2..];
                        state = State::Code;
                    }
                    None => {
                        push_text(&mut out, number, line);
                        break;
                    }
                },
                State::Code => {
                    let bytes = line.as_bytes();
                    let mut at = 0;
                    let mut next = None;
                    while at < bytes.len() && next.is_none() {
                        if !line.is_char_boundary(at) {
                            at += 1;
                            continue;
                        }
                        let byte = bytes[at];
                        let rest = &line[at..];
                        let triple = ["\"\"\"", "'''"].into_iter().find(|delimiter| {
                            flavour == Flavour::Python && rest.starts_with(delimiter)
                        });
                        if let Some(delimiter) = triple {
                            match rest[3..].find(delimiter) {
                                Some(close) => at += 3 + close + 3,
                                None => next = Some(State::Triple(delimiter)),
                            }
                        } else if flavour == Flavour::PowerShell && rest.starts_with("<#") {
                            match rest[2..].find("#>") {
                                Some(close) => {
                                    push_text(&mut out, number, &rest[2..2 + close]);
                                    at += 2 + close + 2;
                                }
                                None => {
                                    push_text(&mut out, number, &rest[2..]);
                                    next = Some(State::Block);
                                }
                            }
                        } else if flavour == Flavour::PowerShell
                            && (rest.starts_with("@\"") || rest.starts_with("@'"))
                            && rest[2..].trim().is_empty()
                        {
                            next = Some(State::HereString(bytes[at + 1]));
                        } else if byte == b'\'' || byte == b'"' {
                            at += 1 + quoted_len(&bytes[at + 1..], byte, escape(byte));
                        } else if byte == b'#' && (at == 0 || bytes[at - 1].is_ascii_whitespace()) {
                            push_text(&mut out, number, &rest[1..]);
                            at = bytes.len();
                        } else {
                            at += 1;
                        }
                    }
                    match next {
                        Some(entered) => {
                            state = entered;
                            line = "";
                        }
                        None => break,
                    }
                }
            }
        }
    }
    out
}

/// Prose lines outside ``` and ~~~ fences, with inline code spans replaced by a space.
fn markdown_prose_lines(source: &str) -> Vec<(usize, String)> {
    let mut fence: Option<&str> = None;
    let mut out = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        match fence {
            Some(marker) => {
                if trimmed.starts_with(marker) {
                    fence = None;
                }
            }
            None => match ["```", "~~~"]
                .into_iter()
                .find(|marker| trimmed.starts_with(marker))
            {
                Some(marker) => fence = Some(marker),
                None => push_text(&mut out, index + 1, &strip_spans(line)),
            },
        }
    }
    out
}

/// Splits `line` into (prose, code span) pairs; the last pair's span is empty.
fn split_spans(line: &str) -> Vec<(&str, &str)> {
    let mut parts = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let Some(close) = rest[open + 1..].find('`') else {
            break;
        };
        parts.push((&rest[..open], &rest[open + 1..open + 1 + close]));
        rest = &rest[open + close + 2..];
    }
    parts.push((rest, ""));
    parts
}

fn strip_spans(line: &str) -> String {
    let parts = split_spans(line);
    let mut out = String::with_capacity(line.len());
    for (index, (prose, _)) in parts.iter().enumerate() {
        out.push_str(prose);
        if index + 1 < parts.len() {
            out.push(' ');
        }
    }
    out
}

/// `line` without its code spans and double-quoted segments: a label or character quoted
/// inside a comment is being named, not used.
fn without_quoted(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = strip_spans(line);
    while let Some(open) = rest.find('"') {
        let Some(close) = rest[open + 1..].find('"') else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push(' ');
        rest = rest[open + close + 2..].to_string();
    }
    out.push_str(&rest);
    out
}

fn is_word_boundary(byte: Option<&u8>) -> bool {
    byte.is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
}

fn follows(line: &str, needle: &str, accept: impl Fn(&[u8]) -> bool) -> bool {
    line.match_indices(needle)
        .any(|(at, _)| accept(&line.as_bytes()[at + needle.len()..]))
}

/// `follows`, with the needle starting a word.
fn follows_word(line: &str, needle: &str, accept: impl Fn(&[u8]) -> bool) -> bool {
    let bytes = line.as_bytes();
    line.match_indices(needle).any(|(at, _)| {
        is_word_boundary(at.checked_sub(1).and_then(|before| bytes.get(before)))
            && accept(&bytes[at + needle.len()..])
    })
}

fn contains_word(line: &str, word: &str) -> bool {
    follows_word(line, word, |rest| is_word_boundary(rest.first()))
}

/// Byte ranges of an uppercase `letter` standing as a word with one to `max_digits`
/// digits whose value lies in `values`, so `RequestClient` and a hex run ending in `T1` are
/// not citations.
fn letter_numbers<'a>(
    text: &'a str,
    letter: u8,
    max_digits: usize,
    values: RangeInclusive<u32>,
) -> impl Iterator<Item = (usize, usize)> + 'a {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().filter_map(move |(at, byte)| {
        if *byte != letter
            || !is_word_boundary(at.checked_sub(1).and_then(|before| bytes.get(before)))
        {
            return None;
        }
        let digits = bytes[at + 1..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        let end = at + 1 + digits;
        ((1..=max_digits).contains(&digits)
            && is_word_boundary(bytes.get(end))
            && text[at + 1..end]
                .parse::<u32>()
                .is_ok_and(|value| values.contains(&value)))
        .then_some((at, end))
    })
}

fn letter_number(text: &str, letter: u8, max_digits: usize, values: RangeInclusive<u32>) -> bool {
    letter_numbers(text, letter, max_digits, values)
        .next()
        .is_some()
}

/// Whether the R-id at `start..end` is the mesh transport's bare name: the third one,
/// followed by `/` or by one of the transport's nouns.
fn names_the_transport(text: &str, start: usize, end: usize) -> bool {
    let nouns = [
        "transport",
        "request",
        "requests",
        "dispatcher",
        "envelope",
        "client",
        "path",
        "frame",
        "frames",
    ];
    let rest = &text[end..];
    &text[start + 1..end] == "3"
        && (rest.starts_with('/') || {
            let word: String = rest
                .trim_start()
                .chars()
                .take_while(char::is_ascii_alphabetic)
                .collect();
            nouns.contains(&word.to_ascii_lowercase().as_str())
        })
}

/// A spec draft id, a wire-spec id or the plan docs directory: citations even when
/// quoted or inside a code span, so `text` is the raw comment line.
fn cites_spec_or_docs(text: &str) -> bool {
    let draft_prefix = ["SCO", "PE-"].concat();
    let wire_prefix = ["MES", "H-"].concat();
    let plan_docs = ["docs/", "mesh/"].concat();
    let draft_id_tail = |rest: &[u8]| {
        let letters = rest.iter().take_while(|b| b.is_ascii_uppercase()).count();
        letters > 0 && rest.get(letters) == Some(&b'-') && {
            let digits = rest[letters + 1..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            digits == 3 && is_word_boundary(rest.get(letters + 1 + digits))
        }
    };
    follows_word(text, &draft_prefix, |rest| {
        rest.first().is_some_and(u8::is_ascii_uppercase)
    }) || follows_word(text, &wire_prefix, draft_id_tail)
        || text.contains(&plan_docs)
}

/// A comment that cites planning vocabulary instead of behaviour: the plan prefix, a task
/// id, a review decision or its number, a numbered phase, a letter-dash-number or
/// letter-number plan label, the planning write-up, the comparison exercise, or a numbered
/// review pass. The spaced form of a numbered pass is also how the propagation protocol
/// counts its fetches, so it only counts beside a word naming the review itself; the
/// dashed form always counts. `text` has already had its quoted segments removed.
fn cites_plan_vocabulary(text: &str) -> bool {
    let starts_with_digit = |rest: &[u8]| rest.first().is_some_and(u8::is_ascii_digit);
    let anything = |_: &[u8]| true;
    let plan_prefix = ["PL", "AN-"].concat();
    let lettered_t = ["T", "-"].concat();
    let lower = text.to_ascii_lowercase();
    let task = ["ta", "sk-"].concat();
    let ruling = ["rul", "ing "].concat();
    let rulings = ["rul", "ings"].concat();
    let qualified_rulings = [
        ["user", " rul", "ing"].concat(),
        ["plan", " rul", "ing"].concat(),
    ];
    let phase = ["pha", "se "].concat();
    let dashed = [
        ["t", "-"].concat(),
        ["g", "-"].concat(),
        ["b", "-"].concat(),
    ];
    let design_doc = ["des", "ign doc"].concat();
    let bake_off = [["bake", "-off"].concat(), ["bake", " off"].concat()];
    let dashed_pass = ["ro", "und-"].concat();
    let spaced_pass = ["ro", "und "].concat();
    let review_words = [
        ["rev", "iew"].concat(),
        ["rev", "iewer"].concat(),
        ["pro", "be"].concat(),
        ["gaunt", "let"].concat(),
        ["adver", "sary"].concat(),
        ["find", "ing"].concat(),
        ["find", "ings"].concat(),
    ];
    follows_word(text, &plan_prefix, |rest| {
        rest.first().is_some_and(u8::is_ascii_alphanumeric)
    }) || follows_word(text, &lettered_t, |rest| {
        rest.first().is_some_and(|b| (b'a'..=b'k').contains(b)) && is_word_boundary(rest.get(1))
    }) || letter_number(text, b'T', 1, 1..=9)
        || letter_numbers(text, b'R', 2, 1..=99)
            .any(|(start, end)| !names_the_transport(text, start, end))
        || letter_number(text, b'G', 2, 1..=99)
        || letter_number(text, b'D', 2, 1..=20)
        || follows(&lower, &task, starts_with_digit)
        || follows(&lower, &ruling, starts_with_digit)
        || contains_word(&lower, &rulings)
        || qualified_rulings
            .iter()
            .any(|needle| contains_word(&lower, needle))
        || follows_word(&lower, &phase, starts_with_digit)
        || dashed
            .iter()
            .any(|needle| follows_word(&lower, needle, starts_with_digit))
        || follows_word(&lower, &design_doc, anything)
        || bake_off
            .iter()
            .any(|needle| follows_word(&lower, needle, anything))
        || follows_word(&lower, &dashed_pass, starts_with_digit)
        || (follows_word(&lower, &spaced_pass, starts_with_digit)
            && review_words.iter().any(|word| contains_word(&lower, word)))
}

fn vocabulary_hit(text: &str) -> bool {
    cites_spec_or_docs(text) || cites_plan_vocabulary(&without_quoted(text))
}

/// The first non-ASCII char outside a code span; a quoted character still counts.
fn ascii_hit(text: &str) -> Option<char> {
    strip_spans(text).chars().find(|c| !c.is_ascii())
}

fn non_ascii_suffix(c: char) -> String {
    format!("  [U+{:04X} '{c}']", c as u32)
}

fn read_comment_lines(root: &Path, path: &str) -> Vec<(usize, String)> {
    if let Class::Opaque = class_of(path) {
        return Vec::new();
    }
    let source =
        std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"));
    comment_lines(path, &source)
}

/// One `path:line: text` entry per comment line `flag` marks, with the suffix it returns.
fn queue_from(
    files: impl Iterator<Item = (String, Vec<(usize, String)>)>,
    flag: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let flag = &flag;
    files
        .flat_map(|(path, comments)| {
            comments.into_iter().filter_map(move |(number, text)| {
                flag(&text).map(|suffix| format!("{path}:{number}: {text}{suffix}"))
            })
        })
        .collect()
}

fn queue(root: &Path, flag: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let files = allow_listed_paths(root).into_iter().map(|path| {
        let comments = read_comment_lines(root, &path);
        (path, comments)
    });
    queue_from(files, flag)
}

fn report(what: &str, hits: &[String]) {
    assert!(
        hits.is_empty(),
        "{} comment line(s) in branch-owned files {what}:\n{}",
        hits.len(),
        hits.join("\n")
    );
}

#[test]
#[ignore = "red until the comment passes land"]
fn comments_in_branch_owned_files_cite_behaviour_not_plan_labels() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let hits = queue(root, |text| vocabulary_hit(text).then(String::new));
    report("cite planning vocabulary", &hits);
}

#[test]
#[ignore = "red until the comment passes land"]
fn comments_in_branch_owned_files_are_ascii() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let hits = queue(root, |text| ascii_hit(text).map(non_ascii_suffix));
    report("carry a non-ASCII character", &hits);
}

fn lines(fixture: &[&str]) -> String {
    fixture.join("\n")
}

fn expected(entries: &[(usize, &str)]) -> Vec<(usize, String)> {
    entries
        .iter()
        .map(|(line, text)| (*line, text.to_string()))
        .collect()
}

#[test]
fn rust_extractor_keeps_comments_and_skips_literals() {
    let source = lines(&[
        "// plain",
        "/// doc",
        "//! inner",
        "let s = \"not // a comment\";",
        "let t = \"spans",
        "  // two lines\";",
        "let r = r#\"raw // text\"#; // after raw",
        "let b = br\"// bytes\";",
        "let q = '\"'; // after a char literal",
        "let lt: &'a str = x; // after a lifetime",
        "/* block */ let y = 1; // trailing",
        "/* multi",
        "   line */",
        "let e = '\\''; // after an escaped quote",
        "#[doc = \"// attribute\"]",
        "let u = '\\u{2014}'; // after a unicode escape",
        "/* outer /* nested */ still */ let z = 2;",
    ]);
    assert_eq!(
        comment_lines("src/fixture.rs", &source),
        expected(&[
            (1, "plain"),
            (2, "doc"),
            (3, "inner"),
            (7, "after raw"),
            (9, "after a char literal"),
            (10, "after a lifetime"),
            (11, "block"),
            (11, "trailing"),
            (12, "multi"),
            (13, "line"),
            (14, "after an escaped quote"),
            (16, "after a unicode escape"),
            (17, "outer /* nested */ still"),
        ])
    );
}

#[test]
fn hash_extractor_wants_whitespace_before_the_hash_and_skips_quotes() {
    let shell = lines(&[
        "#!/bin/sh",
        "echo hi # trailing",
        "echo \"#\" not",
        "echo ${#x} $# also not",
        "curl http://x#y",
        "  # indented",
        "echo 'it # is' # after single",
        "echo \"esc \\\" # not\" # after escape",
    ]);
    let shell_comments = expected(&[
        (1, "!/bin/sh"),
        (2, "trailing"),
        (6, "indented"),
        (7, "after single"),
        (8, "after escape"),
    ]);
    for name in [
        "scripts/fixture.sh",
        "fixture.yaml",
        "fixture.toml",
        "scripts/fixture.config.tmpl",
        "deployment/fixture.config",
        "deployment/fixture/Dockerfile",
    ] {
        assert_eq!(comment_lines(name, &shell), shell_comments, "{name}");
    }
    let yaml = lines(&[
        "key: \"a # b\" # real",
        "run: |",
        "  echo # inside the block",
    ]);
    assert_eq!(
        comment_lines(".github/workflows/fixture.yaml", &yaml),
        expected(&[(1, "real"), (3, "inside the block")])
    );
    let dash = '\u{2014}';
    let wide = lines(&[
        &format!("echo {dash} # after a wide char"),
        &format!("echo \"\\{dash}\" # after an escaped wide char"),
    ]);
    assert_eq!(
        comment_lines("scripts/fixture.sh", &wide),
        expected(&[(1, "after a wide char"), (2, "after an escaped wide char")])
    );
}

#[test]
fn python_extractor_skips_docstrings() {
    let source = lines(&[
        "def f():",
        "    \"\"\"docstring # not a comment",
        "    still # not\"\"\"",
        "    x = '''one-line # not''' # yes",
        "    return 1  # also yes",
    ]);
    assert_eq!(
        comment_lines("assets/fixture.py", &source),
        expected(&[(4, "yes"), (5, "also yes")])
    );
}

#[test]
fn powershell_extractor_scans_block_comments_and_skips_here_strings() {
    let source = lines(&[
        "<# block",
        "  inside #>",
        "$x = @\"",
        "  # here-string",
        "\"@",
        "Write-Host 'a' # trailing",
        "$y = \"`\"#not\" # yes",
        "$z = @'",
        "# also a here-string",
        "'@ # after",
        "<# one line #> # tail",
    ]);
    assert_eq!(
        comment_lines("scripts/fixture.ps1", &source),
        expected(&[
            (1, "block"),
            (2, "inside"),
            (6, "trailing"),
            (7, "yes"),
            (10, "after"),
            (11, "one line"),
            (11, "tail"),
        ])
    );
}

#[test]
fn markdown_extractor_keeps_prose_outside_fences_without_code_spans() {
    let source = lines(&[
        "# Title",
        "Prose with `code # span` kept",
        "```sh",
        "# fenced",
        "```",
        "  ~~~",
        "not prose",
        "  ~~~",
        "after",
    ]);
    assert_eq!(
        comment_lines("deployment/fixture/README.md", &source),
        expected(&[(1, "# Title"), (2, "Prose with   kept"), (9, "after")])
    );
}

#[test]
fn opaque_files_have_no_comment_lines() {
    assert!(comment_lines("src/mesh/fuzz/corpus/x.bin", "# not\n// not").is_empty());
    assert!(comment_lines("fixture.json", "// not").is_empty());
}

#[test]
fn every_comment_class_present_in_the_allow_list_yields_comment_lines() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut per_class: BTreeMap<String, usize> = BTreeMap::new();
    for path in allow_listed_paths(root) {
        if let Class::Opaque = class_of(&path) {
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or(&path);
        let label = name
            .rsplit_once('.')
            .map_or(name, |(_, extension)| extension);
        *per_class.entry(label.to_string()).or_default() += read_comment_lines(root, &path).len();
    }
    for (label, count) in &per_class {
        println!("{label}: {count} comment line(s)");
    }
    let dead: Vec<&String> = per_class
        .iter()
        .filter_map(|(label, count)| (*count == 0).then_some(label))
        .collect();
    assert!(dead.is_empty(), "no comment lines extracted from: {dead:?}");
}

#[test]
fn vocabulary_matcher_trips_each_shape_and_spares_prose() {
    for hit in [
        ["PL", "AN-mesh-scope lands it"].concat(),
        ["see ta", "sk-12 for why"].concat(),
        ["see TA", "SK-118"].concat(),
        ["rul", "ing 3 settled it"].concat(),
        ["the rul", "ings are listed"].concat(),
        ["per the user", " rul", "ing"].concat(),
        ["the plan", " rul", "ing was"].concat(),
        ["pha", "se 2 adds it"].concat(),
        ["T", "-c covers it"].concat(),
        ["per t", "-3 the cap holds"].concat(),
        ["g", "-12 covers the relay"].concat(),
        ["see B", "-7"].concat(),
        ["T", "1 is the first"].concat(),
        ["R", "3 settled it"].concat(),
        ["G", "12 holds"].concat(),
        ["G", "1-foo"].concat(),
        ["D", "20 says"].concat(),
        ["D", "1 says"].concat(),
        ["the des", "ign doc says"].concat(),
        ["the Des", "ign Document says"].concat(),
        ["the bake", "-off winner"].concat(),
        ["the bake", " off winner"].concat(),
        ["ro", "und 2 of rev", "iew"].concat(),
        ["fixed in rev", "iew ro", "und 2"].concat(),
        ["usage pro", "be, ro", "und 3"].concat(),
        ["the Ro", "und-3 README claimed"].concat(),
        ["adver", "sary ro", "und 2 find", "ing"].concat(),
        ["SCO", "PE-MSG-041"].concat(),
        ["MES", "H-ENV-039 covers it"].concat(),
        ["see docs/", "mesh/PROTOCOL.md"].concat(),
        ["see `docs/", "mesh/PROTOCOL.md`"].concat(),
        ["pins `MES", "H-ENV-039`"].concat(),
        ["R", "3 judged before anything is printed"].concat(),
        ["settled in R", "4"].concat(),
    ] {
        assert!(vocabulary_hit(&hit), "{hit:?}");
    }
    for miss in [
        ["9f1c3b0eT", "1a2 is a wire id"].concat(),
        ["0xT", "1 is not a label"].concat(),
        ["ro", "und trip"].concat(),
        ["the ro", "und 1 reply was not a list of ids"].concat(),
        ["the fetch waits in ro", "und 1"].concat(),
        ["aro", "und 3 peers"].concat(),
        ["Err(R", "3Error::Shutdown) ends it"].concat(),
        ["the R", "3Client frames it"].concat(),
        ["the R", "3 transport frames it"].concat(),
        ["one R", "3 request on /message"].concat(),
        ["the R", "3 dispatcher derives"].concat(),
        ["R", "3/message carries it"].concat(),
        ["g", "1-mini is the model"].concat(),
        ["D", "21 misses"].concat(),
        ["D", "0 misses"].concat(),
        "hand-off and handoff are product words".to_string(),
        ["the pha", "se ends"].concat(),
        ["the rul", "ing stands"].concat(),
        "tasks are drained".to_string(),
        ["ta", "sk-list"].concat(),
        ["see `TA", "SK-12` quoted"].concat(),
        ["the \"TA", "SK-12\" string"].concat(),
        ["the \"R", "3\" label"].concat(),
        ["MES", "H-ENV-39 misses"].concat(),
        ["MES", "H-env-039 misses"].concat(),
        ["SCO", "PE-protocol misses"].concat(),
        ["the t", "-shirt and the b", "-tree"].concat(),
        ["PL", "ANNING is a word"].concat(),
    ] {
        assert!(!vocabulary_hit(&miss), "{miss:?}");
    }
}

#[test]
fn ascii_matcher_names_the_first_offending_char_outside_code_spans() {
    let dash = '\u{2014}';
    assert_eq!(ascii_hit(&format!("a {dash} dash")), Some(dash));
    assert_eq!(ascii_hit(&format!("a `{dash}` span")), None);
    assert_eq!(ascii_hit(&format!("a \"{dash}\" quote")), Some(dash));
    assert_eq!(ascii_hit("plain ascii"), None);
    for named in [
        '\u{2013}', '\u{2026}', '\u{2192}', '\u{21D2}', '\u{A7}', '\u{D7}', '\u{2264}', '\u{2265}',
        '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2713}', '\u{2717}', '\u{2022}',
    ] {
        assert_eq!(ascii_hit(&format!("x {named} y")), Some(named));
    }
    assert_eq!(non_ascii_suffix(dash), format!("  [U+2014 '{dash}']"));
}

#[test]
fn ascii_pin_reads_only_comment_text() {
    let dash = '\u{2014}';
    let first_hit = |name: &str, source: &str| {
        comment_lines(name, source)
            .into_iter()
            .find_map(|(line, text)| ascii_hit(&text).map(|c| (line, c)))
    };
    assert_eq!(
        first_hit("src/f.rs", &format!("let x = 1; // a {dash} dash")),
        Some((1, dash))
    );
    assert_eq!(
        first_hit("src/f.rs", &format!("let s = \"{dash}\"; // ok")),
        None
    );
    assert_eq!(first_hit("f.yaml", &format!("key: \"{dash}\" # ok")), None);
    assert_eq!(
        first_hit("scripts/f.sh", &format!("echo x # a {dash} dash")),
        Some((1, dash))
    );
    assert_eq!(
        first_hit(
            "f.ps1",
            &lines(&["$x = @\"", &format!("{dash}"), "\"@ # ok"])
        ),
        None
    );
    assert_eq!(first_hit("f.md", &format!("a `{dash}` span")), None);
    assert_eq!(
        first_hit("f.md", &lines(&["```", &format!("{dash}"), "```", "ok"])),
        None
    );
    assert_eq!(
        first_hit("f.md", &format!("prose {dash} here")),
        Some((1, dash))
    );
}

#[test]
fn vocabulary_pin_reads_only_comment_text() {
    let id = ["TA", "SK-12"].concat();
    let first_hit = |name: &str, source: &str| {
        comment_lines(name, source)
            .into_iter()
            .find_map(|(line, text)| vocabulary_hit(&text).then_some(line))
    };
    assert_eq!(
        first_hit("src/f.rs", &format!("let s = \"{id}\"; // ok")),
        None
    );
    assert_eq!(first_hit("f.yaml", &format!("key: \"{id}\" # ok")), None);
    assert_eq!(
        first_hit("f.ps1", &lines(&["$x = @\"", &id, "\"@ # ok"])),
        None
    );
    assert_eq!(first_hit("f.md", &lines(&["```", &id, "```", "ok"])), None);
    assert_eq!(
        first_hit("src/f.rs", &format!("let x = 1; // see {id}")),
        Some(1)
    );
    assert_eq!(
        first_hit("scripts/f.sh", &format!("echo x # see {id}")),
        Some(1)
    );
    assert_eq!(
        first_hit("f.ps1", &lines(&["<# block", &format!("  see {id} #>")])),
        Some(2)
    );
    assert_eq!(first_hit("f.md", &format!("prose cites {id}")), Some(1));
}

#[test]
fn queue_renders_path_line_and_text_in_file_order() {
    let files = |first: &str, second: &str, third: &str| {
        vec![
            (
                "src/a.rs".to_string(),
                expected(&[(3, first), (7, "clean"), (12, second)]),
            ),
            (
                "scripts/b.sh".to_string(),
                expected(&[(1, "!/bin/sh"), (4, third)]),
            ),
        ]
    };
    let task = ["see ta", "sk-12"].concat();
    let pass = ["ro", "und-2 fix"].concat();
    let wire = ["pins `MES", "H-ENV-039`"].concat();
    assert_eq!(
        queue_from(files(&task, &pass, &wire).into_iter(), |text| {
            vocabulary_hit(text).then(String::new)
        }),
        [
            format!("src/a.rs:3: {task}"),
            format!("src/a.rs:12: {pass}"),
            format!("scripts/b.sh:4: {wire}"),
        ]
    );
    let dash = '\u{2014}';
    let arrow = '\u{2192}';
    let dashed = format!("a {dash} dash");
    let pointed = format!("b {arrow} arrow");
    let quoted = format!("c \"{dash}\" quoted");
    assert_eq!(
        queue_from(files(&dashed, &pointed, &quoted).into_iter(), |text| {
            ascii_hit(text).map(non_ascii_suffix)
        }),
        [
            format!("src/a.rs:3: {dashed}  [U+2014 '{dash}']"),
            format!("src/a.rs:12: {pointed}  [U+2192 '{arrow}']"),
            format!("scripts/b.sh:4: {quoted}  [U+2014 '{dash}']"),
        ]
    );
}

#[test]
fn dir_globs_match_one_directory_segment() {
    assert!(matches_dir_glob(
        "scripts/mesh-interop/setup.sh",
        "scripts/mesh-*/"
    ));
    assert!(matches_dir_glob(
        "scripts/mesh-netns/a/b.sh",
        "scripts/mesh-*/"
    ));
    assert!(!matches_dir_glob(
        "scripts/mesh-relay.sh",
        "scripts/mesh-*/"
    ));
    assert!(!matches_dir_glob("scripts/mesh-/x", "scripts/mesh-*/"));
    assert!(matches_dir_glob("src/mesh/mod.rs", "src/mesh/"));
    assert!(!matches_dir_glob("src/mesh_other/mod.rs", "src/mesh/"));
}

#[test]
fn allow_listed_paths_ignores_untracked_files() {
    let tmp = TempDir::new("hygiene-allow-list");
    let root = tmp.path.as_path();
    git(root, &["init", "-q"]).expect("git init");
    let mesh = root.join("src").join("mesh");
    std::fs::create_dir_all(&mesh).expect("mkdir");
    std::fs::write(mesh.join("tracked.rs"), "// tracked\n").expect("write");
    git(root, &["add", "src/mesh/tracked.rs"]).expect("git add");
    std::fs::write(mesh.join("scratch.rs"), "// scratch\n").expect("write");
    assert_eq!(
        allow_listed_paths(root),
        BTreeSet::from(["src/mesh/tracked.rs".to_string()])
    );
}

/// The two `origin/main` comparisons run on a branch that has not landed, skip while the
/// ref is missing, and retire once `main` carries this file.
#[test]
fn main_comparisons_retire_once_the_pins_have_landed() {
    let tmp = TempDir::new("hygiene-origin-main");
    let root = tmp.path.as_path();
    let commit = |message: &str| {
        git(
            root,
            &[
                "-c",
                "user.name=hygiene pins",
                "-c",
                "user.email=pins@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                message,
            ],
        )
        .expect("git commit");
    };
    git(root, &["init", "-q"]).expect("git init");
    std::fs::write(root.join("README.md"), "before\n").expect("write");
    git(root, &["add", "README.md"]).expect("git add");
    commit("before the pins");
    let missing = origin_main_predates_the_pins(root).unwrap_err();
    assert!(
        missing.starts_with("origin/main unavailable: "),
        "{missing}"
    );

    git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]).expect("update-ref");
    assert_eq!(origin_main_predates_the_pins(root), Ok(()));

    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join(PINS_FILE), "// pins\n").expect("write");
    git(root, &["add", PINS_FILE]).expect("git add");
    commit("the pins land");
    git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]).expect("update-ref");
    let landed = origin_main_predates_the_pins(root).unwrap_err();
    assert!(landed.contains("already carries"), "{landed}");
}

#[test]
fn every_allow_listed_path_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tracked = tracked_paths(root);
    for glob in ALLOW_LISTED_DIRS {
        assert!(
            tracked.iter().any(|path| matches_dir_glob(path, glob)),
            "{glob} matches no tracked file"
        );
    }
    for file in ALLOW_LISTED_FILES {
        assert!(tracked.contains(file), "{file} is not tracked");
    }
    if let Err(reason) = origin_main_predates_the_pins(root) {
        println!("{reason}; skipping the not-on-main check");
        return;
    }
    let on_main = nul_separated(
        &git(root, &["ls-tree", "-r", "--name-only", "-z", "origin/main"]).expect("git ls-tree"),
    );
    let shared: Vec<String> = allow_listed_paths(root)
        .into_iter()
        .filter(|path| path != CI_WORKFLOW && on_main.contains(path))
        .collect();
    assert!(
        shared.is_empty(),
        "allow-listed paths that also exist on origin/main:\n{}",
        shared.join("\n")
    );
}

/// `--no-renames` keeps a branch-added file classed as an addition whatever the user's
/// `diff.renames` setting would otherwise pair it with.
#[test]
fn the_allow_list_is_the_three_dot_diff_minus_docs_and_examples() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Err(reason) = origin_main_predates_the_pins(root) {
        println!("{reason}; skipping the diff check");
        return;
    }
    let added = git(
        root,
        &[
            "diff",
            "--no-renames",
            "--diff-filter=A",
            "--name-only",
            "-z",
            "origin/main...HEAD",
        ],
    )
    .expect("git diff");
    let mut from_diff: BTreeSet<String> = nul_separated(&added)
        .into_iter()
        .filter(|path| !path.starts_with("docs/") && !path.starts_with("examples/"))
        .collect();
    from_diff.insert(CI_WORKFLOW.to_string());
    let allow_listed = allow_listed_paths(root);
    let missing: Vec<&String> = from_diff.difference(&allow_listed).collect();
    let extra: Vec<&String> = allow_listed.difference(&from_diff).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "added on this branch but not allow-listed: {missing:?}\nallow-listed but not added on this branch: {extra:?}"
    );
}
