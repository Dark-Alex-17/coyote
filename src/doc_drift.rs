//! Doc drift checks: the wiki pages and the repo docs held to the crate symbols they
//! describe. This module is the in-crate home for checks of that kind, so a new one goes
//! here beside these and reads the symbol itself (`COMMANDS`, `HookEvent::ALL`, a config
//! default) rather than scraping Rust source.
//!
//! The wiki is a separate clone, so the wiki half of each test runs only when
//! `COYOTE_WIKI_DIR` points at a checkout; a relative value resolves against the crate
//! root. Without it a test runs its repo-only half, prints `skipping:` and passes. A value
//! naming a directory without `Mesh.md` fails rather than passing against nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::memory::{
    DEFAULT_MEMORY_DRILL_CAP, DEFAULT_MEMORY_FACTS_CAP, DEFAULT_MEMORY_RUN_STATE_TTL_DAYS,
};
use crate::config::mesh_config::{
    DEFAULT_ENVOY_MEMORY_MAX_BYTES, DEFAULT_ENVOY_MEMORY_MAX_PER_IDENTITY,
    DEFAULT_ENVOY_MEMORY_MAX_SESSIONS, DEFAULT_ENVOY_MEMORY_MAX_TURNS,
    DEFAULT_ENVOY_MEMORY_TTL_HOURS, EnvoyMemoryConfig,
};
use crate::function::mesh::{MESH_FUNCTION_PREFIX, mesh_function_declarations};
use crate::hooks::HookEvent;
use crate::repl::mesh::{COMMANDS, GROUPS, is_group};

const WIKI_DIR: &str = "COYOTE_WIKI_DIR";
const RECIPE: &str = "`COYOTE_WIKI_DIR=../coyote.wiki cargo test doc_drift`";

/// Section links that resolve to no heading today, as (wiki page or source file, link as
/// written). An entry no scan sees any more fails the test, so the list empties out as
/// the links are fixed.
const KNOWN_DANGLING_ANCHORS: &[(&str, &str)] = &[(
    "Mesh-Commands.md",
    "Mesh-File-Sharing#--attach-on-mesh-message-and-mesh-answer",
)];

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The wiki checkout, or `None` after printing the conventional `skipping:` line.
fn wiki_dir() -> Option<PathBuf> {
    let value = std::env::var_os(WIKI_DIR)
        .map(|value| value.to_string_lossy().trim().to_string())
        .filter(|value| !value.is_empty());
    let Some(value) = value else {
        eprintln!(
            "skipping: {WIKI_DIR} unset; point it at a coyote.wiki checkout and run {RECIPE}"
        );
        return None;
    };
    let dir = repo_root().join(value);
    assert!(
        dir.join("Mesh.md").is_file(),
        "{WIKI_DIR}={} has no Mesh.md; point it at a coyote.wiki checkout",
        dir.display()
    );
    Some(dir)
}

/// The file at `path`, CRLF line endings normalised so line scans hold on any checkout.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn wiki_page(wiki: &Path, name: &str) -> String {
    read(&wiki.join(name))
}

/// Every `*.md` page of the wiki checkout as (file name, text), by name.
fn wiki_pages(wiki: &Path) -> Vec<(String, String)> {
    let mut pages: Vec<(String, String)> = std::fs::read_dir(wiki)
        .unwrap_or_else(|e| panic!("read {}: {e}", wiki.display()))
        .map(|entry| entry.expect("wiki dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .map(|path| {
            let name = path
                .file_name()
                .expect("a read_dir entry has a name")
                .to_string_lossy()
                .to_string();
            (name, read(&path))
        })
        .collect();
    pages.sort();
    pages
}

/// `text` with every run of whitespace collapsed to one space, so a phrase the page
/// wrapped is found whole.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn report(what: &str, hits: &[String]) {
    assert!(
        hits.is_empty(),
        "{} {what}:\n{}",
        hits.len(),
        hits.join("\n")
    );
}

/// Pushes a hit unless the flattened `text` of `label` says `needle`.
fn require_phrase(hits: &mut Vec<String>, label: &str, text: &str, needle: &str) {
    if !flat(text).contains(needle) {
        hits.push(format!("{label}: does not say {needle:?}"));
    }
}

/// Each line of `text` as (1-based number, line, inside a fenced block); the fence lines
/// themselves are left out. A fence closes only on the marker that opened it.
fn markdown_lines(text: &str) -> Vec<(usize, &str, bool)> {
    let mut fence: Option<&str> = None;
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"]
            .into_iter()
            .find(|marker| trimmed.starts_with(marker));
        match (fence, marker) {
            (None, Some(marker)) => fence = Some(marker),
            (Some(open), Some(marker)) if open == marker => fence = None,
            _ => out.push((index + 1, line, fence.is_some())),
        }
    }
    out
}

/// The inline code spans of a prose line as (whole span, content). Delimiters are equal
/// backtick runs as CommonMark reads them, so a double-backtick span may quote a single
/// backtick; an unmatched run is plain text.
fn code_spans(line: &str) -> Vec<(Range<usize>, &str)> {
    let bytes = line.as_bytes();
    let run_at = |at: usize| bytes[at..].iter().take_while(|b| **b == b'`').count();
    let mut spans = Vec::new();
    let mut at = 0;
    while let Some(open) = line[at..].find('`').map(|offset| at + offset) {
        let len = run_at(open);
        let start = open + len;
        let mut search = start;
        let mut close = None;
        while let Some(next) = line[search..].find('`').map(|offset| search + offset) {
            let run = run_at(next);
            if run == len {
                close = Some(next);
                break;
            }
            search = next + run;
        }
        match close {
            Some(end) => {
                spans.push((open..end + len, &line[start..end]));
                at = end + len;
            }
            None => at = start,
        }
    }
    spans
}

/// The code a page shows, as (line, text): each fenced line whole and each inline code
/// span of a prose line. A span that wraps a backtick-quoted output line yields the code
/// spans inside the quote instead, so the prose of a quoted message is not read as code.
fn code_text(page: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    for (number, line, fenced) in markdown_lines(page) {
        if fenced {
            out.push((number, line));
            continue;
        }
        for (_, code) in code_spans(line) {
            match code
                .trim()
                .strip_prefix('`')
                .and_then(|c| c.strip_suffix('`'))
            {
                Some(quoted) => out.extend(
                    code_spans(quoted)
                        .into_iter()
                        .map(|(_, code)| (number, code)),
                ),
                None => out.push((number, code)),
            }
        }
    }
    out
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    ((1..=6).contains(&hashes) && line[hashes..].starts_with(' ')).then_some(hashes)
}

/// The heading lines of `page` outside fenced blocks, as (line, text).
fn headings(page: &str) -> Vec<(usize, &str)> {
    markdown_lines(page)
        .into_iter()
        .filter(|(_, line, fenced)| !fenced && heading_level(line).is_some())
        .map(|(number, line, _)| (number, line))
        .collect()
}

/// The anchor GitHub gives a heading: the `#`s stripped, lowercased, every character that
/// is not a letter, digit, space, hyphen or underscore dropped, spaces to hyphens.
fn github_slug(heading: &str) -> String {
    heading
        .trim_start_matches('#')
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// Every anchor `page`'s headings answer to. GitHub suffixes a repeated slug with `-1`,
/// `-2` and so on in page order.
fn heading_anchors(page: &str) -> BTreeSet<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut anchors = BTreeSet::new();
    for (_, heading) in headings(page) {
        let slug = github_slug(heading);
        let count = seen.entry(slug.clone()).or_default();
        anchors.insert(match *count {
            0 => slug,
            n => format!("{slug}-{n}"),
        });
        *count += 1;
    }
    anchors
}

/// A markdown table: the heading it sits under, its header cells and its body rows as
/// (line, cells).
struct Table<'a> {
    heading: Option<&'a str>,
    header: Vec<String>,
    rows: Vec<(usize, Vec<String>)>,
}

impl Table<'_> {
    /// The index of the first header cell starting with `name`.
    fn column(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|cell| cell.starts_with(name))
    }
}

/// The cells of a table row, split on unescaped pipes and trimmed; `\|` reads as `|`.
fn table_cells(row: &str) -> Vec<String> {
    let row = row.trim();
    let row = row.strip_prefix('|').unwrap_or(row);
    let row = match row.strip_suffix('|') {
        Some(inner) if !inner.ends_with('\\') => inner,
        _ => row,
    };
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cell.push('|');
            }
            '|' => cells.push(std::mem::take(&mut cell).trim().to_string()),
            c => cell.push(c),
        }
    }
    cells.push(cell.trim().to_string());
    cells
}

fn is_table_separator(line: &str) -> bool {
    let cells = table_cells(line);
    cells
        .iter()
        .all(|cell| cell.contains('-') && cell.chars().all(|c| matches!(c, '-' | ':' | ' ')))
}

/// Every table of `page` outside fenced blocks.
fn tables(page: &str) -> Vec<Table<'_>> {
    let lines = markdown_lines(page);
    let is_row = |at: usize| {
        lines
            .get(at)
            .is_some_and(|(_, line, fenced)| !fenced && line.trim_start().starts_with('|'))
    };
    let mut out = Vec::new();
    let mut heading = None;
    let mut at = 0;
    while at < lines.len() {
        let (_, line, fenced) = lines[at];
        if !fenced && heading_level(line).is_some() {
            heading = Some(line);
        }
        if is_row(at) && is_row(at + 1) && is_table_separator(lines[at + 1].1) {
            let mut rows = Vec::new();
            let mut next = at + 2;
            while is_row(next) {
                rows.push((lines[next].0, table_cells(lines[next].1)));
                next += 1;
            }
            out.push(Table {
                heading,
                header: table_cells(line),
                rows,
            });
            at = next;
        } else {
            at += 1;
        }
    }
    out
}

/// A cell holding one code span, without its backticks.
fn unbacktick(cell: &str) -> Option<&str> {
    cell.strip_prefix('`')?.strip_suffix('`')
}

/// The rows of `page`'s tables whose first cell is the code span `prefix<key>`, `key` a
/// plain identifier, as key to (line, Default cell).
fn keyed_defaults(page: &str, prefix: &str) -> BTreeMap<String, (usize, String)> {
    let mut out = BTreeMap::new();
    for table in tables(page) {
        let Some(column) = table.column("Default") else {
            continue;
        };
        for (line, cells) in &table.rows {
            let key = cells
                .first()
                .and_then(|cell| unbacktick(cell)?.strip_prefix(prefix))
                .filter(|key| {
                    !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                });
            if let (Some(key), Some(default)) = (key, cells.get(column)) {
                out.insert(key.to_string(), (*line, default.clone()));
            }
        }
    }
    out
}

/// Pushes a hit for every key of `expected` that `documented` lacks or gives another
/// Default cell than the expected one, and for every key it has beyond.
fn default_hits(
    hits: &mut Vec<String>,
    label: &str,
    documented: &BTreeMap<String, (usize, String)>,
    expected: &[(String, String)],
) {
    for (key, expected_cell) in expected {
        match documented.get(key) {
            None => hits.push(format!("{label}: no `{key}` row")),
            Some((line, cell)) if cell != expected_cell => hits.push(format!(
                "{label}:{line}: `{key}` defaults to {cell}, the code to {expected_cell}"
            )),
            Some(_) => {}
        }
    }
    for (key, (line, _)) in documented {
        if !expected.iter().any(|(known, _)| known == key) {
            hits.push(format!(
                "{label}:{line}: `{key}` is not a key the code reads"
            ));
        }
    }
}

/// `n` in words as the pages count things: `nine`, `fifteen`, `forty-seven`.
fn number_word(n: usize) -> String {
    const ONES: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 10] = [
        "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    match n {
        0..=19 => ONES[n].to_string(),
        20..=99 if n.is_multiple_of(10) => TENS[n / 10].to_string(),
        20..=99 => format!("{}-{}", TENS[n / 10], ONES[n % 10]),
        _ => panic!("number_word covers 0 to 99, not {n}"),
    }
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// `n` with a comma between each group of three digits, as the pages print sizes.
fn with_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A test launched from a git hook inherits `GIT_DIR` and friends, which would point the
/// call at the hook's repository regardless of `-C`.
fn git_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    command
}

fn tracked_paths(root: &Path) -> BTreeSet<String> {
    let out = git_command(root, &["ls-files", "-z"])
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git ls-files: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    out.stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).expect("git paths are UTF-8"))
        .collect()
}

/// The length of the lowercase word `text` opens with: a letter, then letters and dashes.
fn word_len(text: &str) -> usize {
    if !text.starts_with(|c: char| c.is_ascii_lowercase()) {
        return 0;
    }
    text.find(|c: char| !(c.is_ascii_lowercase() || c == '-'))
        .unwrap_or(text.len())
}

/// The word a top-level `.mesh` verb takes before its arguments, read off its usage line:
/// `name` for a usage starting `.mesh session name`. `None` when the usage goes straight on
/// to arguments or flags, or `verb` has no top-level row.
fn mesh_sub_verb(verb: &str) -> Option<&'static str> {
    COMMANDS
        .iter()
        .find(|row| row.0.is_empty() && row.1 == verb)
        .and_then(|row| {
            row.2
                .strip_prefix(".mesh ")?
                .strip_prefix(verb)?
                .strip_prefix(' ')
        })
        .and_then(|args| args.split(' ').next())
        .filter(|word| word.starts_with(|c: char| c.is_ascii_lowercase()))
}

/// The command path `rest`, the text after `.mesh `, opens with and its length: one
/// word, plus the next when the first names a group or a top-level verb whose usage has a
/// word before its arguments, or when the first is `help` and the next names a group.
fn mesh_path(rest: &str) -> Option<(String, usize)> {
    let first = word_len(rest);
    if first == 0 {
        return None;
    }
    let word = &rest[..first];
    if let Some(tail) = rest[first..].strip_prefix(' ') {
        let second = word_len(tail);
        let takes_second = is_group(word)
            || mesh_sub_verb(word).is_some()
            || (word == "help" && is_group(&tail[..second]));
        if second > 0 && takes_second {
            let len = first + 1 + second;
            return Some((rest[..len].to_string(), len));
        }
    }
    Some((word.to_string(), first))
}

/// Whether `run_verb` takes `path` rather than refusing it as an unknown command: a
/// top-level verb, a top-level verb and the word its usage starts with, a group, a group
/// and one of its verbs or `help`, or `help` and a group.
fn mesh_dispatches(path: &str) -> bool {
    let has_row =
        |group: &str, verb: &str| COMMANDS.iter().any(|row| row.0 == group && row.1 == verb);
    match path.split_once(' ') {
        Some((group, verb)) if is_group(group) => verb == "help" || has_row(group, verb),
        Some(("help", group)) => is_group(group),
        Some((verb, sub)) => mesh_sub_verb(verb) == Some(sub),
        None => is_group(path) || has_row("", path),
    }
}

/// The usage of the `COMMANDS` row `path` runs; `None` for a bare group.
fn mesh_usage(path: &str) -> Option<&'static str> {
    let (group, verb) = match path.split_once(' ') {
        Some((group, verb)) if is_group(group) => (group, verb),
        Some((verb, _)) => ("", verb),
        None => ("", path),
    };
    COMMANDS
        .iter()
        .find(|row| row.0 == group && row.1 == verb)
        .map(|row| row.2)
}

/// Every `.mesh` command a page shows in its code, as (line, path, rest). The sigil
/// stands at the start of the code or after whitespace, a quote, a bracket or a prompt.
/// `rest` runs to the next sample and stops at a backtick, `;`, `,`, `(` or `)`, quoted
/// strings dropped, so the prose around a sample is not read as its flags.
fn mesh_samples(page: &str) -> Vec<(usize, String, String)> {
    const SIGIL: &str = ".mesh ";
    let mut out = Vec::new();
    for (line, code) in code_text(page) {
        let starts: Vec<usize> = code
            .match_indices(SIGIL)
            .map(|(at, _)| at)
            .filter(|at| {
                code[..*at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| c.is_whitespace() || "\"'`([>".contains(c))
            })
            .collect();
        for (index, start) in starts.iter().enumerate() {
            let end = starts.get(index + 1).copied().unwrap_or(code.len());
            let tail = &code[start + SIGIL.len()..end];
            let Some((path, len)) = mesh_path(tail) else {
                continue;
            };
            let mut rest = String::new();
            let mut quoted = false;
            for c in tail[len..].chars() {
                match c {
                    '"' => quoted = !quoted,
                    _ if quoted => {}
                    '`' | ';' | ',' | '(' | ')' => break,
                    c => rest.push(c),
                }
            }
            out.push((line, path, rest));
        }
    }
    out
}

/// Every `--flag` token in `text`.
fn flags_in(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|word| {
            word.strip_prefix("--")
                .is_some_and(|name| name.starts_with(|c: char| c.is_ascii_lowercase()))
        })
        .collect()
}

/// `label:line: ...` for every flag a `.mesh` sample of `page` spells that its row's usage
/// does not name, and how many samples carried a flag. `--help` is accepted after every
/// verb, so it is never a hit.
fn unknown_flag_hits(label: &str, page: &str) -> (Vec<String>, usize) {
    let mut hits = Vec::new();
    let mut flagged = 0;
    for (line, path, rest) in mesh_samples(page) {
        let flags = flags_in(&rest);
        if flags.is_empty() {
            continue;
        }
        flagged += 1;
        let Some(usage) = mesh_usage(&path) else {
            continue;
        };
        let known = flags_in(usage);
        for flag in flags {
            if flag != "--help" && !known.contains(&flag) {
                hits.push(format!(
                    "{label}:{line}: `{path}` takes no {flag} (usage: {usage})"
                ));
            }
        }
    }
    (hits, flagged)
}

/// Every `](Page#anchor)` or `](#anchor)` link on a prose line of `text`, code spans
/// left out, as (line, target page, anchor, link as written). A same-page link targets
/// `page` itself; links that leave the wiki are skipped.
fn section_links<'a>(page: &'a str, text: &'a str) -> Vec<(usize, &'a str, &'a str, &'a str)> {
    let mut out = Vec::new();
    for (number, line, fenced) in markdown_lines(text) {
        if fenced {
            continue;
        }
        let spans: Vec<Range<usize>> = code_spans(line).into_iter().map(|(span, _)| span).collect();
        for (at, _) in line.match_indices("](") {
            if spans.iter().any(|span| span.contains(&at)) {
                continue;
            }
            let target = &line[at + 2..];
            let Some(close) = target.find(')') else {
                continue;
            };
            let link = target[..close].split_whitespace().next().unwrap_or("");
            let Some((target_page, anchor)) = link.split_once('#') else {
                continue;
            };
            if target_page.contains(['/', '.', ':']) {
                continue;
            }
            let target_page = if target_page.is_empty() {
                page
            } else {
                target_page
            };
            out.push((number, target_page, anchor, link));
        }
    }
    out
}

/// Every `wiki/<Page>#<anchor>` URL in the tracked Rust files under `src/`, as (file,
/// line, page, anchor, link as written).
fn source_wiki_links(root: &Path) -> Vec<(String, usize, String, String, String)> {
    const URL_PATH: &str = "/wiki/";
    let mut out = Vec::new();
    for file in tracked_paths(root) {
        if !(file.starts_with("src/") && file.ends_with(".rs")) {
            continue;
        }
        let source = read(&root.join(&file));
        for (index, line) in source.lines().enumerate() {
            for (at, _) in line.match_indices(URL_PATH) {
                let link: String = line[at + URL_PATH.len()..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '#'))
                    .collect();
                if let Some((page, anchor)) = link.split_once('#') {
                    out.push((
                        file.clone(),
                        index + 1,
                        page.to_string(),
                        anchor.to_string(),
                        link.clone(),
                    ));
                }
            }
        }
    }
    out
}

/// (heading, path, usage, summary) for each `COMMANDS` row, as the commands page lays it
/// out: a top-level row under `### Top level`, any other under its group's heading.
fn expected_command_rows() -> Vec<(String, String, &'static str, &'static str)> {
    COMMANDS
        .iter()
        .map(|(group, verb, usage, summary)| {
            if group.is_empty() {
                (
                    "### Top level".to_string(),
                    verb.to_string(),
                    *usage,
                    *summary,
                )
            } else {
                (
                    format!("### `{group}`"),
                    format!("{group} {verb}"),
                    *usage,
                    *summary,
                )
            }
        })
        .collect()
}

/// `label:line: ...` for every way the first table headed `Tool` in `page` differs from
/// `names`: its row count, a name with no row, a row naming no declared tool.
fn tool_table_hits(label: &str, page: &str, names: &[String]) -> Vec<String> {
    let tables = tables(page);
    let Some(table) = tables
        .iter()
        .find(|table| table.header.first().is_some_and(|cell| cell == "Tool"))
    else {
        return vec![format!("{label}: no table headed `Tool`")];
    };
    let mut hits = Vec::new();
    if table.rows.len() != names.len() {
        hits.push(format!(
            "{label}: the tool table has {} rows, mesh_function_declarations has {}",
            table.rows.len(),
            names.len()
        ));
    }
    let documented: Vec<(usize, &str)> = table
        .rows
        .iter()
        .map(|(line, cells)| {
            let cell = cells.first().map_or("", String::as_str);
            (*line, unbacktick(cell).unwrap_or(cell))
        })
        .collect();
    for name in names {
        if !documented.iter().any(|(_, documented)| documented == name) {
            hits.push(format!("{label}: the tool table has no `{name}` row"));
        }
    }
    for (line, documented) in &documented {
        if !names.iter().any(|name| name == documented) {
            hits.push(format!(
                "{label}:{line}: `{documented}` is not a declared tool"
            ));
        }
    }
    hits
}

/// `expected_command_rows` laid out as the commands page lays them out: a `## The commands`
/// section with one `Command | Usage | What it does | Node` table per run of a group.
fn render_command_tables() -> String {
    let escape = |cell: &str| cell.replace('|', "\\|");
    let mut out = String::from("## The commands\n");
    let mut current = None;
    for (heading, path, usage, summary) in expected_command_rows() {
        if current.as_deref() != Some(heading.as_str()) {
            out.push_str(&format!(
                "\n{heading}\n\n| Command | Usage | What it does | Node |\n|---|---|---|---|\n"
            ));
            current = Some(heading);
        }
        out.push_str(&format!(
            "| `{path}` | `{}` | {} | on |\n",
            escape(usage),
            escape(summary)
        ));
    }
    out
}

/// `label:line: ...` for every way the `## The commands` tables of `page` differ from
/// `COMMANDS`: group headings, row count and order, and each row's heading, path, usage
/// and summary.
fn command_table_hits(label: &str, page: &str) -> Vec<String> {
    let heads = headings(page);
    let Some(start) = heads
        .iter()
        .position(|(_, heading)| *heading == "## The commands")
    else {
        return vec![format!("{label}: no `## The commands` section")];
    };
    let first = heads[start].0;
    let end = heads[start + 1..]
        .iter()
        .find(|(_, heading)| heading_level(heading).is_some_and(|level| level <= 2))
        .map_or(usize::MAX, |(line, _)| *line);
    let in_section = |line: usize| line > first && line < end;
    let mut hits = Vec::new();

    let documented_groups: BTreeSet<&str> = heads
        .iter()
        .filter(|(line, heading)| in_section(*line) && heading_level(heading) == Some(3))
        .filter_map(|(_, heading)| heading.strip_prefix("### `")?.strip_suffix('`'))
        .collect();
    let groups: BTreeSet<&str> = GROUPS.iter().map(|(group, _)| *group).collect();
    if documented_groups != groups {
        hits.push(format!(
            "{label}: the group headings are {documented_groups:?}, GROUPS has {groups:?}"
        ));
    }

    let mut documented = Vec::new();
    let page_tables = tables(page);
    for table in &page_tables {
        if !table
            .rows
            .first()
            .is_some_and(|(line, _)| in_section(*line))
        {
            continue;
        }
        for (line, cells) in &table.rows {
            let path = cells.first().and_then(|cell| unbacktick(cell));
            let usage = cells.get(1).and_then(|cell| unbacktick(cell));
            match (path, usage, cells.get(2), cells.len()) {
                (Some(path), Some(usage), Some(summary), 4) => documented.push((
                    *line,
                    table.heading.unwrap_or(""),
                    path,
                    usage,
                    summary.as_str(),
                )),
                _ => hits.push(format!(
                    "{label}:{line}: not a `command` | `usage` | summary | node row: {cells:?}"
                )),
            }
        }
    }
    let expected = expected_command_rows();
    if documented.len() != expected.len() {
        hits.push(format!(
            "{label}: the tables hold {} rows, COMMANDS has {}",
            documented.len(),
            expected.len()
        ));
    }
    for (index, (heading, path, usage, summary)) in expected.iter().enumerate() {
        let Some((line, found_heading, found_path, found_usage, found_summary)) =
            documented.get(index)
        else {
            hits.push(format!("{label}: no row {} for `{path}`", index + 1));
            continue;
        };
        if found_path != path {
            hits.push(format!(
                "{label}:{line}: row {} is `{found_path}`, COMMANDS has `{path}`",
                index + 1
            ));
            continue;
        }
        if found_heading != heading {
            hits.push(format!(
                "{label}:{line}: `{path}` sits under {found_heading:?}, not {heading:?}"
            ));
        }
        if found_usage != usage {
            hits.push(format!(
                "{label}:{line}: `{path}` usage {found_usage:?}, COMMANDS has {usage:?}"
            ));
        }
        if found_summary != summary {
            hits.push(format!(
                "{label}:{line}: `{path}` summary {found_summary:?}, COMMANDS has {summary:?}"
            ));
        }
    }
    for (line, _, path, _, _) in documented.iter().skip(expected.len()) {
        hits.push(format!("{label}:{line}: `{path}` has no COMMANDS row"));
    }
    hits
}

#[test]
fn the_commands_page_tables_list_the_repl_command_table_row_for_row() {
    let rendered = render_command_tables();
    assert_eq!(
        command_table_hits("fixture", &rendered),
        Vec::<String>::new()
    );
    let (_, path, _, summary) = &expected_command_rows()[0];
    let changed = format!("{summary}, changed");
    let mutated = rendered.replacen(&format!("| {summary} |"), &format!("| {changed} |"), 1);
    let line = mutated
        .lines()
        .position(|line| line.contains(&changed))
        .expect("the mutation lands on a row")
        + 1;
    assert_eq!(
        command_table_hits("fixture", &mutated),
        [format!(
            "fixture:{line}: `{path}` summary {changed:?}, COMMANDS has {summary:?}"
        )]
    );

    let Some(wiki) = wiki_dir() else { return };
    let label = "Mesh-Commands.md";
    report(
        "command table rows that differ from COMMANDS",
        &command_table_hits(label, &wiki_page(&wiki, label)),
    );
}

#[test]
fn every_mesh_path_a_wiki_page_spells_is_one_the_repl_dispatches() {
    for path in [
        "on",
        "people",
        "people list",
        "people help",
        "help",
        "help access",
        "session",
        "session name",
    ] {
        assert!(mesh_dispatches(path), "{path}");
    }
    for path in [
        "peers",
        "people lst",
        "people on",
        "session rename",
        "help on",
        "status people",
    ] {
        assert!(!mesh_dispatches(path), "{path}");
    }
    let dot = ".mesh";
    let fixture = format!(
        "prose `{dot} people list` and ``{dot} peers``\n```text\n> {dot} on --yes; {dot} people lst\n{dot} help shows them\n```\nprose {dot} off outside code\n`` `Unknown {dot} verb 'x'. Type `{dot} help access` for the list.` ``\n"
    );
    let paths: Vec<(usize, String)> = mesh_samples(&fixture)
        .into_iter()
        .map(|(line, path, _)| (line, path))
        .collect();
    assert_eq!(
        paths,
        [
            (1, "people list".to_string()),
            (1, "peers".to_string()),
            (3, "on".to_string()),
            (3, "people lst".to_string()),
            (4, "help".to_string()),
            (7, "help access".to_string()),
        ]
    );

    let Some(wiki) = wiki_dir() else { return };
    let mut hits = Vec::new();
    let mut samples = 0;
    for (label, page) in wiki_pages(&wiki) {
        if !page.contains(".mesh ") {
            continue;
        }
        for (line, path, _) in mesh_samples(&page) {
            samples += 1;
            if !mesh_dispatches(&path) {
                hits.push(format!("{label}:{line}: `{path}`"));
            }
        }
    }
    assert!(
        samples >= 100,
        "only {samples} `.mesh` samples found across the wiki; the scan reads nothing"
    );
    report(
        "`.mesh` paths a wiki page spells that the REPL refuses as unknown commands",
        &hits,
    );
}

#[test]
fn every_flag_a_wiki_mesh_sample_spells_is_in_that_commands_usage() {
    let dot = ".mesh";
    let fixture = format!(
        "prose `{dot} files request bob a.md --reasons \"x --yes\"` and `{dot} on --yes --help`\n```text\n> {dot} off --force (or --macro)\n```\n"
    );
    let (hits, flagged) = unknown_flag_hits("fixture", &fixture);
    assert_eq!(
        flagged, 3,
        "the fixture's flagged samples were not all read"
    );
    assert_eq!(
        hits.len(),
        2,
        "the scan does not go red on exactly --reasons and --force: {hits:?}"
    );
    assert!(
        hits[0].contains("--reasons") && hits[1].contains("--force"),
        "{hits:?}"
    );

    let Some(wiki) = wiki_dir() else { return };
    let mut hits = Vec::new();
    let mut flagged = 0;
    for (label, page) in wiki_pages(&wiki) {
        if !page.contains(".mesh ") {
            continue;
        }
        let (page_hits, page_flagged) = unknown_flag_hits(&label, &page);
        hits.extend(page_hits);
        flagged += page_flagged;
    }
    assert!(
        flagged >= 20,
        "only {flagged} `.mesh` samples with a flag found across the wiki; the scan reads nothing"
    );
    report(
        "flags a wiki `.mesh` sample spells that the command's usage does not name",
        &hits,
    );
}

#[test]
fn the_docs_count_and_name_the_hook_events_the_code_registers() {
    let count = HookEvent::ALL.len();
    let names: Vec<&str> = HookEvent::ALL.iter().map(|event| event.as_str()).collect();
    let mesh_count = names
        .iter()
        .filter(|name| name.starts_with("mesh."))
        .count();
    let parenthetical = format!("({count} events)");
    let mut hits = Vec::new();
    require_phrase(
        &mut hits,
        "README.md",
        &read(&repo_root().join("README.md")),
        &parenthetical,
    );
    report("repo docs that miscount the hook events", &hits);

    let Some(wiki) = wiki_dir() else { return };
    let hooks = wiki_page(&wiki, "Hooks.md");
    let mesh_word = number_word(mesh_count);
    require_phrase(
        &mut hits,
        "Hooks.md",
        &hooks,
        &format!("The {count} events"),
    );
    require_phrase(
        &mut hits,
        "Hooks.md",
        &hooks,
        &format!("The {mesh_word} `mesh.*` events"),
    );
    require_phrase(
        &mut hits,
        "Home.md",
        &wiki_page(&wiki, "Home.md"),
        &parenthetical,
    );
    require_phrase(
        &mut hits,
        "Mesh.md",
        &wiki_page(&wiki, "Mesh.md"),
        &format!("{mesh_word} `mesh.*` hook events"),
    );
    let tables = tables(&hooks);
    let table = tables
        .iter()
        .find(|table| table.header.first().is_some_and(|cell| cell == "Event"))
        .expect("Hooks.md has a table headed `Event`");
    if table.rows.len() != count {
        hits.push(format!(
            "Hooks.md: the events table has {} rows, HookEvent::ALL has {count}",
            table.rows.len()
        ));
    }
    let documented: Vec<(usize, &str)> = table
        .rows
        .iter()
        .map(|(line, cells)| {
            let cell = cells.first().map_or("", String::as_str);
            (*line, unbacktick(cell).unwrap_or(cell))
        })
        .collect();
    for name in &names {
        if !documented.iter().any(|(_, documented)| documented == name) {
            hits.push(format!("Hooks.md: the events table has no `{name}` row"));
        }
    }
    for (line, documented) in &documented {
        if !names.contains(documented) {
            hits.push(format!(
                "Hooks.md:{line}: `{documented}` is not a registered event"
            ));
        }
    }
    report("wiki statements that differ from HookEvent::ALL", &hits);
}

#[test]
fn the_mesh_pages_count_and_list_the_mesh_tools_the_code_declares() {
    let names: Vec<String> = mesh_function_declarations()
        .into_iter()
        .map(|declaration| declaration.name)
        .collect();
    let fixture = format!(
        "| Tool | What it does |\n|---|---|\n{}",
        names[1..]
            .iter()
            .map(|name| format!("| `{name}` | x |\n"))
            .collect::<String>()
    );
    assert_eq!(
        tool_table_hits("fixture", &fixture, &names),
        [
            format!(
                "fixture: the tool table has {} rows, mesh_function_declarations has {}",
                names.len() - 1,
                names.len()
            ),
            format!("fixture: the tool table has no `{}` row", names[0]),
        ]
    );

    let Some(wiki) = wiki_dir() else { return };
    let word = number_word(names.len());
    let mesh = wiki_page(&wiki, "Mesh.md");
    let mut hits = tool_table_hits("Mesh.md", &mesh, &names);
    require_phrase(
        &mut hits,
        "Mesh.md",
        &mesh,
        &format!("{} `{MESH_FUNCTION_PREFIX}*` tools", capitalized(&word)),
    );
    require_phrase(
        &mut hits,
        "Mesh-File-Sharing.md",
        &wiki_page(&wiki, "Mesh-File-Sharing.md"),
        &format!("of the {word} `{MESH_FUNCTION_PREFIX}*` tools"),
    );
    report(
        "wiki statements that differ from mesh_function_declarations",
        &hits,
    );
}

#[test]
fn every_envoy_memory_key_carries_the_code_default_on_every_surface() {
    let config = EnvoyMemoryConfig::default();
    assert_eq!(
        (
            config.max_sessions,
            config.max_per_identity,
            config.max_turns,
            config.max_bytes,
            config.ttl_hours,
        ),
        (
            DEFAULT_ENVOY_MEMORY_MAX_SESSIONS,
            DEFAULT_ENVOY_MEMORY_MAX_PER_IDENTITY,
            DEFAULT_ENVOY_MEMORY_MAX_TURNS,
            DEFAULT_ENVOY_MEMORY_MAX_BYTES,
            DEFAULT_ENVOY_MEMORY_TTL_HOURS,
        ),
        "EnvoyMemoryConfig::default no longer takes its bounds from the DEFAULT_ENVOY_MEMORY_* consts"
    );
    let serde_json::Value::Object(fields) =
        serde_json::to_value(&config).expect("EnvoyMemoryConfig serializes")
    else {
        panic!("EnvoyMemoryConfig does not serialize to a map");
    };
    let expected: Vec<(String, String)> = fields
        .into_iter()
        .map(|(key, value)| (key, format!("`{value}`")))
        .collect();
    let mut hits = Vec::new();

    let label = "config.example.yaml";
    let example = read(&repo_root().join(label));
    let lines: Vec<(usize, &str)> = example
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l))
        .collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    let start = lines
        .iter()
        .position(|(_, line)| line.trim_start().starts_with("envoy_memory:"))
        .unwrap_or_else(|| panic!("{label} has no `envoy_memory:` block"));
    let head = indent(lines[start].1);
    let documented: BTreeMap<String, (usize, String)> = lines[start + 1..]
        .iter()
        .take_while(|(_, line)| indent(line) > head)
        .filter(|(_, line)| !line.trim_start().starts_with('#'))
        .filter_map(|(number, line)| {
            let (key, value) = line.trim().split_once(':')?;
            let value = value.split('#').next().unwrap_or("").trim();
            Some((key.to_string(), (*number, format!("`{value}`"))))
        })
        .collect();
    default_hits(&mut hits, label, &documented, &expected);
    let readme = keyed_defaults(&read(&repo_root().join("README.md")), "mesh.envoy_memory.");
    default_hits(&mut hits, "README.md", &readme, &expected);
    report("repo envoy_memory entries that differ from the code", &hits);

    let Some(wiki) = wiki_dir() else { return };
    let label = "Mesh-Configuration.md";
    let documented = keyed_defaults(&wiki_page(&wiki, label), "envoy_memory.");
    default_hits(&mut hits, label, &documented, &expected);
    report("wiki envoy_memory rows that differ from the code", &hits);
}

#[test]
fn the_memory_page_states_the_caps_and_ttl_the_code_defaults_to() {
    let Some(wiki) = wiki_dir() else { return };
    let label = "Memory.md";
    let page = wiki_page(&wiki, label);
    let mut hits = Vec::new();
    let expected: Vec<(String, String)> = [
        ("facts_cap", DEFAULT_MEMORY_FACTS_CAP),
        ("drill_cap", DEFAULT_MEMORY_DRILL_CAP),
    ]
    .into_iter()
    .map(|(key, cap)| (key.to_string(), format!("{} chars", with_thousands(cap))))
    .collect();
    default_hits(
        &mut hits,
        label,
        &keyed_defaults(&page, "memory_"),
        &expected,
    );
    require_phrase(
        &mut hits,
        label,
        &page,
        &format!("`memory_run_state_ttl_days` (default {DEFAULT_MEMORY_RUN_STATE_TTL_DAYS})"),
    );
    report("memory defaults that differ from the code", &hits);
}

#[test]
fn every_section_link_on_or_into_a_mesh_page_and_every_source_wiki_url_resolves_to_a_heading() {
    assert_eq!(github_slug("### `.mesh config show`"), "mesh-config-show");
    assert_eq!(
        github_slug("## Talking to peers (tools)"),
        "talking-to-peers-tools"
    );
    assert_eq!(
        heading_anchors("# Top\n## Top\n```text\n# Top\n```\n### Top\n#hashtag\n"),
        BTreeSet::from(["top".to_string(), "top-1".to_string(), "top-2".to_string()])
    );
    assert_eq!(
        section_links(
            "Here",
            "[a](There#one) [b](#two) `[c](There#code)` [d](https://x.example/p#q)\n```\n[e](There#fenced)\n```\n"
        ),
        [(1, "There", "one", "There#one"), (1, "Here", "two", "#two"),]
    );
    let root = repo_root();
    let source_links = source_wiki_links(root);
    assert!(
        !source_links.is_empty(),
        "no wiki URL with an anchor found in the tracked src/**/*.rs files; the scan reads nothing"
    );

    let Some(wiki) = wiki_dir() else { return };
    let pages = wiki_pages(&wiki);
    let anchors: BTreeMap<&str, BTreeSet<String>> = pages
        .iter()
        .map(|(name, text)| (name.trim_end_matches(".md"), heading_anchors(text)))
        .collect();
    let resolves = |page: &str, anchor: &str| {
        anchors
            .get(page)
            .is_some_and(|slugs| slugs.contains(anchor))
    };
    let mut dangling: Vec<(String, usize, String)> = Vec::new();
    let mut checked = 0;
    for (name, text) in &pages {
        let page = name.trim_end_matches(".md");
        for (line, target, anchor, link) in section_links(page, text) {
            if !(page.starts_with("Mesh") || target.starts_with("Mesh")) {
                continue;
            }
            checked += 1;
            if !resolves(target, anchor) {
                dangling.push((name.clone(), line, link.to_string()));
            }
        }
    }
    for (file, line, page, anchor, link) in &source_links {
        checked += 1;
        if !resolves(page, anchor) {
            dangling.push((file.clone(), *line, link.clone()));
        }
    }
    assert!(
        checked >= 20,
        "only {checked} section links found; the scan reads nothing"
    );
    let mut seen = BTreeSet::new();
    let mut hits = Vec::new();
    for (label, line, link) in &dangling {
        match KNOWN_DANGLING_ANCHORS
            .iter()
            .find(|(known_label, known_link)| known_label == label && known_link == link)
        {
            Some(entry) => {
                seen.insert(*entry);
            }
            None => hits.push(format!("{label}:{line} -> {link}")),
        }
    }
    for entry in KNOWN_DANGLING_ANCHORS {
        if !seen.contains(entry) {
            hits.push(format!(
                "KNOWN_DANGLING_ANCHORS lists {entry:?}, which no longer dangles; drop it"
            ));
        }
    }
    report("section links that resolve to no heading", &hits);
}

#[test]
fn number_words_spell_the_counts_the_pages_use() {
    assert_eq!(number_word(9), "nine");
    assert_eq!(number_word(15), "fifteen");
    assert_eq!(number_word(40), "forty");
    assert_eq!(number_word(47), "forty-seven");
    assert_eq!(capitalized("nine"), "Nine");
    assert_eq!(with_thousands(4_000), "4,000");
    assert_eq!(with_thousands(1_234_567), "1,234,567");
    assert_eq!(with_thousands(999), "999");
}
