//! Pins `docs/mesh/PROTOCOL.md` to its own format contract and to the constants the code
//! actually uses. The checkers are line-oriented and std-only; the tests at the bottom run
//! them over fixtures (to prove each rule can go red) and over the real document.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

pub(crate) const SPEC: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/mesh/PROTOCOL.md"
));
const UPSTREAM_ISSUES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/mesh/upstream-issues.md"
));
#[cfg(test)]
const README: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"));

const EXPECTED_H1: &str = "# SCOPE — Session Coordination & Presence Exchange";
const EXPECTED_TAGLINE: &str = "\"SCOPE is a peer protocol by which running LLM sessions announce presence, share status, and exchange messages on their owners' behalf, over Reticulum, without a broker.\"";
const INTRODUCTION_HEADING: &str = "## 1. Introduction and scope";
const AREAS_SENTENCE_OPEN: &str = "Requirement ids have the form";
const SECTION_COUNT: usize = 21;
const CODE_POINT_HEADING: &str = "## 13. Code-point immutability";
const ON_DISK_SCHEMA_KIND: &str = "on-disk schema version";
const CONSTANTS_HEADING: &str = "## 19. Constants";
const INDEX_HEADING: &str = "## 21. Requirements index";
const BCP14: &str = "The key words \"MUST\", \"MUST NOT\", \"REQUIRED\", \"SHALL\", \"SHALL NOT\", \
    \"SHOULD\", \"SHOULD NOT\", \"RECOMMENDED\", \"NOT RECOMMENDED\", \"MAY\", and \"OPTIONAL\" in \
    this document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174] when, and \
    only when, they appear in all capitals, as shown here.";
const AREAS: [&str; 23] = [
    "DEST", "ANN", "ENV", "KNOCK", "STATUS", "MSG", "PROP", "VER", "TIME", "CANON", "EXT", "CODE",
    "SEC", "INV", "LOG", "LEN", "PART", "DISP", "LIST", "FETCH", "ACCESS", "SHARE", "SCHEMA",
];
/// Every compound keyword (`MUST NOT`, `NOT RECOMMENDED`, ...) contains one of these.
const KEYWORDS: [&str; 7] = [
    "MUST",
    "REQUIRED",
    "SHALL",
    "SHOULD",
    "RECOMMENDED",
    "MAY",
    "OPTIONAL",
];
/// Narrower than `KEYWORDS` on purpose: only these are checked lowercase; `Must`-style title
/// case is out of scope.
const LOWERCASE_KEYWORDS: [&str; 3] = ["must", "should", "may"];
const DEFINITION_OPEN: &str = "**[MESH-";
const POSITIONAL_TABLE_KEYS: [&str; 4] = ["Field", "Bytes", "Element", "Slot"];
const POSITIONAL_TABLE_COLUMNS: [&str; 3] =
    ["Type", "Sender puts", "Receiver action on any other value"];
pub(crate) const CATCH_ALL_ROW_PREFIXES: [&str; 2] = ["any other", "trailing"];
const CONSTANTS_TABLE_HEADER: &str = "| Constant | Value | Defined in | Pinned by |";
/// The "Pinned by" cell of a constant that only this module's table check pins.
const PINNED_BY_THIS_TABLE: &str = "spec_pins (this table)";
const CITED_IDENTIFIER_MIN_LEN: usize = 12;
const CITED_IDENTIFIER_MIN_UNDERSCORES: usize = 2;
const THREAT_MODEL_HEADING: &str = "### 15.1 Threat model";
const ATTACK_CLASSES: [&str; 7] = [
    "eavesdropping",
    "replay",
    "insertion",
    "deletion",
    "modification",
    "man in the middle",
    "denial of service",
];
const OUT_OF_SCOPE_HEADING: &str = "### 15.6 Out of scope";
const LENIENCY_OPEN: &str = "**[MESH-LEN-";
const LENIENCY_FIELDS: [&str; 3] = ["Why:", "Upstream:", "Removal:"];
const DRAFT_MENTION: &str = "draft ";
const PART_A: &str = "## Part A";
const PART_B: &str = "## Part B";
const DRAFT_HEADING: &str = "### ";
const DRAFT_STATUS: &str = "Status: Drafted, not yet filed";

#[derive(Debug)]
struct Definition {
    id: String,
    line: usize,
    slug: String,
}

#[derive(Debug)]
struct IndexEntry {
    id: String,
    anchor: String,
    line: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct ConstantRow {
    name: String,
    literal: String,
    pinned_by: String,
    line: usize,
}

struct Section {
    before: String,
    content: String,
    first_line: usize,
}

struct Table<'a> {
    line: usize,
    rows: Vec<&'a str>,
}

/// Normalises line endings and blanks fenced code blocks, keeping every line in place.
fn prepare(text: &str) -> String {
    let mut in_fence = false;
    text.replace("\r\n", "\n")
        .split('\n')
        .map(|line| {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                ""
            } else if in_fence {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Splits `line` into (prose, code span) pairs; the last pair's span is empty.
pub(crate) fn split_spans(line: &str) -> Vec<(&str, &str)> {
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

fn strip_code(text: &str) -> String {
    prepare(text)
        .lines()
        .map(strip_spans)
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_word_boundary(byte: Option<&u8>) -> bool {
    byte.is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
}

fn contains_word(line: &str, word: &str) -> bool {
    let bytes = line.as_bytes();
    line.match_indices(word).any(|(start, _)| {
        is_word_boundary(start.checked_sub(1).and_then(|at| bytes.get(at)))
            && is_word_boundary(bytes.get(start + word.len()))
    })
}

fn slug(heading: &str) -> String {
    heading
        .trim_start_matches('#')
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// Splits a table row on `|`, leaving a `|` inside a backtick code span in its cell. An
/// unclosed backtick is literal, as `split_spans` reads it.
fn cells(row: &str) -> Vec<&str> {
    let row = row.trim().trim_start_matches('|').trim_end_matches('|');
    let mut cells = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < row.len() {
        match row.as_bytes()[at] {
            b'`' => match row[at + 1..].find('`') {
                Some(close) => at += close + 2,
                None => break,
            },
            b'|' => {
                cells.push(row[start..at].trim());
                start = at + 1;
                at += 1;
            }
            _ => at += 1,
        }
    }
    cells.push(row[start..].trim());
    cells
}

fn tables(text: &str) -> Vec<Table<'_>> {
    let mut tables: Vec<Table<'_>> = Vec::new();
    let mut open = false;
    for (index, line) in text.lines().enumerate() {
        if line.trim_start().starts_with('|') {
            if open {
                tables.last_mut().expect("an open table").rows.push(line);
            } else {
                tables.push(Table {
                    line: index + 1,
                    rows: vec![line],
                });
                open = true;
            }
        } else {
            open = false;
        }
    }
    tables
}

fn backticked(cell: &str) -> Option<&str> {
    cell.strip_prefix('`')?
        .strip_suffix('`')
        .filter(|inner| !inner.is_empty() && !inner.contains('`'))
}

/// Parses `MESH-<AREA>-<NNN>` at the start of `text`, returning the id and what follows it.
fn parse_id(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("MESH-")?;
    let (area, rest) = rest.split_once('-')?;
    if !AREAS.contains(&area) {
        return None;
    }
    let digits = rest.get(..3)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let len = "MESH-".len() + area.len() + 1 + digits.len();
    Some((&text[..len], &text[len..]))
}

fn definitions_in_line(line: &str) -> Vec<Result<&str, String>> {
    line.match_indices(DEFINITION_OPEN)
        .map(|(start, _)| match parse_id(&line[start + "**[".len()..]) {
            Some((id, rest)) if rest.starts_with("]**") => Ok(id),
            _ => Err(line[start..].chars().take(24).collect()),
        })
        .collect()
}

/// The text before `heading` and the content between it and the next `## ` heading.
fn section(text: &str, heading: &str) -> Result<Section, String> {
    let prepared = prepare(text);
    let lines: Vec<&str> = prepared.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim_end() == heading)
        .ok_or_else(|| format!("no {heading:?} heading"))?;
    let end = lines[at + 1..]
        .iter()
        .position(|line| line.starts_with("## "))
        .map_or(lines.len(), |offset| at + 1 + offset);
    Ok(Section {
        before: lines[..at].join("\n"),
        content: lines[at + 1..end].join("\n"),
        first_line: at + 2,
    })
}

fn body(text: &str) -> Result<String, String> {
    section(text, INDEX_HEADING).map(|section| section.before)
}

fn check_headings(text: &str) -> Result<(), String> {
    let prepared = prepare(text);
    let h1s: Vec<&str> = prepared
        .lines()
        .filter(|line| line.starts_with("# "))
        .collect();
    if h1s != [EXPECTED_H1] {
        return Err(format!(
            "expected exactly one H1, {EXPECTED_H1:?}, found {h1s:?}"
        ));
    }
    let mut expected = 1;
    for (index, line) in prepared.lines().enumerate() {
        let Some(heading) = line.strip_prefix("## ") else {
            continue;
        };
        let numbered = heading
            .split_once(". ")
            .filter(|(number, title)| number.parse::<usize>() == Ok(expected) && !title.is_empty());
        if numbered.is_none() {
            return Err(format!(
                "line {}: expected section heading '## {expected}. Title', found {line:?}",
                index + 1
            ));
        }
        expected += 1;
    }
    if expected == SECTION_COUNT + 1 {
        Ok(())
    } else {
        Err(format!(
            "expected {SECTION_COUNT} sections, found {}",
            expected - 1
        ))
    }
}

fn check_boilerplate(text: &str) -> Result<(), String> {
    if prepare(text).lines().any(|line| line.contains(BCP14)) {
        Ok(())
    } else {
        Err("the BCP 14 boilerplate sentence is missing or altered".to_string())
    }
}

fn definitions(body: &str) -> Result<Vec<Definition>, Vec<String>> {
    let prepared = prepare(body);
    let stripped = strip_code(body);
    let mut found: Vec<Definition> = Vec::new();
    let mut violations = Vec::new();
    let mut seen = HashSet::new();
    let mut heading_slug = String::new();
    for (index, (raw, line)) in prepared.lines().zip(stripped.lines()).enumerate() {
        let number = index + 1;
        if raw.starts_with("## ") || raw.starts_with("### ") {
            heading_slug = slug(raw);
        }
        for parsed in definitions_in_line(line) {
            match parsed {
                Ok(id) if seen.insert(id.to_string()) => found.push(Definition {
                    id: id.to_string(),
                    line: number,
                    slug: heading_slug.clone(),
                }),
                Ok(id) => violations.push(format!("line {number}: duplicate requirement id {id}")),
                Err(snippet) => violations.push(format!(
                    "line {number}: malformed requirement id at {snippet:?}"
                )),
            }
        }
    }
    for area in AREAS {
        let prefix = format!("MESH-{area}-");
        let numbers: HashSet<usize> = found
            .iter()
            .filter(|d| d.id.starts_with(&prefix))
            .map(|d| d.id[prefix.len()..].parse().expect("three digits"))
            .collect();
        if let Some(missing) = (1..=numbers.len()).find(|n| !numbers.contains(n)) {
            violations.push(format!(
                "{area} defines {} ids but none is {prefix}{missing:03}",
                numbers.len()
            ));
        }
    }
    if violations.is_empty() {
        Ok(found)
    } else {
        Err(violations)
    }
}

/// Sentences and table cells, each of which needs its own requirement id. `": "` is not a
/// split point: the document uses it inside single requirements far too often.
fn fragments(line: &str) -> impl Iterator<Item = &str> {
    line.split('|')
        .flat_map(|cell| cell.split(". "))
        .flat_map(|sentence| sentence.split("; "))
}

fn check_normative_lines_carry_ids(body: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (index, line) in strip_code(body).lines().enumerate() {
        if line.contains(BCP14) {
            continue;
        }
        for fragment in fragments(line) {
            let Some(keyword) = KEYWORDS.iter().find(|word| contains_word(fragment, word)) else {
                continue;
            };
            if definitions_in_line(fragment).iter().any(Result::is_ok) {
                continue;
            }
            violations.push(format!(
                "line {}: {keyword} without a requirement id: {:?}",
                index + 1,
                fragment.trim()
            ));
        }
    }
    violations
}

fn check_no_lowercase_keywords(text: &str) -> Vec<String> {
    strip_code(text)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let word = LOWERCASE_KEYWORDS
                .iter()
                .find(|word| contains_word(line, word))?;
            Some(format!("line {}: lowercase {word:?}: {line:?}", index + 1))
        })
        .collect()
}

/// A table row whose first cell is a catch-all (`any other ...` or `trailing ...`); the
/// conformance coverage rule derives its required ids from these rows too. Callers pass
/// table rows; a prose line whose text starts with a prefix is accepted too, so filter to
/// rows first.
pub(crate) fn is_catch_all_row(row: &str) -> bool {
    cells(row).first().is_some_and(|cell| {
        CATCH_ALL_ROW_PREFIXES
            .iter()
            .any(|prefix| cell.starts_with(prefix))
    })
}

fn last_cell_defines_an_id(row: &str) -> bool {
    cells(row).last().is_some_and(|cell| {
        definitions_in_line(&strip_spans(cell))
            .iter()
            .any(Result::is_ok)
    })
}

fn check_catch_all_row(table: &Table<'_>) -> Option<String> {
    match table.rows.iter().skip(2).last() {
        Some(row) if !is_catch_all_row(row) => Some(format!(
            "line {}: positional table ends with {row:?} instead of a row starting with one of {CATCH_ALL_ROW_PREFIXES:?}",
            table.line
        )),
        Some(row) if !last_cell_defines_an_id(row) => Some(format!(
            "line {}: catch-all row {row:?} does not define a requirement id in its last cell",
            table.line
        )),
        Some(_) => None,
        None => Some(format!(
            "line {}: positional table has no data rows",
            table.line
        )),
    }
}

/// Checks every positional table (first header cell in `POSITIONAL_TABLE_KEYS`); `Ok` carries
/// how many there were. A table that carries the positional action column under any other
/// key is a violation too.
fn check_field_tables(text: &str) -> Result<usize, Vec<String>> {
    let prepared = prepare(text);
    let action_column = POSITIONAL_TABLE_COLUMNS[2];
    let mut count = 0;
    let mut violations = Vec::new();
    for table in tables(&prepared) {
        let header = table.rows[0];
        let header_cells = cells(header);
        let positional = header_cells
            .first()
            .is_some_and(|key| POSITIONAL_TABLE_KEYS.contains(key));
        if !positional {
            if header_cells.contains(&action_column) {
                violations.push(format!(
                    "line {}: table header {header:?} has the {action_column:?} column but does not start with one of {POSITIONAL_TABLE_KEYS:?}",
                    table.line
                ));
            }
            continue;
        }
        count += 1;
        if header_cells[1..] != POSITIONAL_TABLE_COLUMNS {
            violations.push(format!(
                "line {}: positional table header {header:?} does not continue with {POSITIONAL_TABLE_COLUMNS:?}",
                table.line
            ));
            continue;
        }
        violations.extend(check_catch_all_row(&table));
    }
    if violations.is_empty() {
        Ok(count)
    } else {
        Err(violations)
    }
}

fn looks_like_a_cited_identifier(span: &str) -> bool {
    span.len() >= CITED_IDENTIFIER_MIN_LEN
        && span.bytes().filter(|b| *b == b'_').count() >= CITED_IDENTIFIER_MIN_UNDERSCORES
        && span.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && span
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Every code span that looks like a test or item name, with its line number, outside fenced
/// code.
fn cited_identifiers(text: &str) -> Vec<(usize, String)> {
    prepare(text)
        .lines()
        .enumerate()
        .flat_map(|(index, line)| {
            split_spans(line)
                .into_iter()
                .map(|(_, span)| span)
                .filter(|span| looks_like_a_cited_identifier(span))
                .map(|span| (index + 1, span.to_string()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The `( ... )` groups of `line` outside code spans, outermost only.
fn parentheticals(line: &str) -> Vec<&str> {
    let mut groups = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    let mut in_span = false;
    for (at, byte) in line.bytes().enumerate() {
        match byte {
            b'`' => in_span = !in_span,
            b'(' if !in_span => {
                if depth == 0 {
                    start = at + 1;
                }
                depth += 1;
            }
            b')' if !in_span && depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    groups.push(&line[start..at]);
                }
            }
            _ => {}
        }
    }
    groups
}

/// The name of the function `line` declares, when it declares one.
fn declared_fn(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    for qualifier in [
        "pub(crate) ",
        "pub(super) ",
        "pub ",
        "const ",
        "async ",
        "unsafe ",
    ] {
        rest = rest.strip_prefix(qualifier).unwrap_or(rest);
    }
    let rest = rest.strip_prefix("fn ")?;
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
    (end > 0 && (rest[end..].starts_with('(') || rest[end..].starts_with('<')))
        .then_some(&rest[..end])
}

/// Every function declared under a `#[test]` or `#[tokio::test]` attribute, other
/// attributes between the two allowed.
pub(crate) fn test_functions(source: &str) -> BTreeSet<String> {
    let lines: Vec<&str> = source.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let name = declared_fn(line)?;
            lines[..index]
                .iter()
                .rev()
                .map(|above| above.trim())
                .filter(|above| !above.is_empty())
                .take_while(|above| above.starts_with("#["))
                .any(|attribute| attribute == "#[test]" || attribute.starts_with("#[tokio::test"))
                .then(|| name.to_string())
        })
        .collect()
}

/// The spec cites tests and items by name but also struct fields, wire keys and upstream
/// functions, so a citation resolves when the sources name it as a whole word anywhere.
fn names_identifier(source: &str, name: &str) -> bool {
    contains_word(source, name)
}

/// Every `.rs` file under `dir`, this file excepted: its fixtures name identifiers that exist
/// nowhere else, and a citation must not resolve against the guard that checks it.
fn rust_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let guard = Path::new(file!()).file_name();
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") && path.file_name() != guard {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// The concatenated contents of every `.rs` file under `dir`, as `rust_files` walks it.
pub(crate) fn rust_sources(dir: &Path) -> std::io::Result<String> {
    let mut out = String::new();
    for path in rust_files(dir)? {
        out.push_str(&std::fs::read_to_string(path)?);
        out.push('\n');
    }
    Ok(out)
}

/// Each `.rs` file under `root/src` as its `/`-joined path relative to `root` and its
/// contents, as `rust_files` walks it.
fn rust_source_files(root: &Path) -> std::io::Result<Vec<(String, String)>> {
    rust_files_under(root, "src")
}

/// Each `.rs` file under `root/<dir>` as its `/`-joined path relative to `root` and its
/// contents, as `rust_files` walks it.
fn rust_files_under(root: &Path, dir: &str) -> std::io::Result<Vec<(String, String)>> {
    rust_files(&root.join(dir))?
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            Ok((relative, std::fs::read_to_string(&path)?))
        })
        .collect()
}

/// A test cited inside a parenthetical that also names a `src/....rs` path is defined in one
/// of the files that parenthetical names; a citation without a path is left to
/// `check_cited_identifiers`.
fn check_cited_paths(
    text: &str,
    files: &[(String, String)],
    tests: &BTreeSet<String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (index, line) in prepare(text).lines().enumerate() {
        for group in parentheticals(line) {
            let paths = source_paths(group);
            if paths.is_empty() {
                continue;
            }
            let named: Vec<&str> = files
                .iter()
                .filter(|(path, _)| paths.contains(path))
                .map(|(_, source)| source.as_str())
                .collect();
            for (_, span) in split_spans(group) {
                if !tests.contains(span) {
                    continue;
                }
                let needle = format!("fn {span}(");
                if !named.iter().any(|source| source.contains(&needle)) {
                    problems.push(format!(
                        "line {}: `{span}` is not defined in {paths:?}",
                        index + 1
                    ));
                }
            }
        }
    }
    problems
}

fn check_cited_identifiers(text: &str, sources: &str) -> Vec<String> {
    cited_identifiers(text)
        .into_iter()
        .filter(|(_, name)| !names_identifier(sources, name))
        .map(|(line, name)| format!("line {line}: `{name}` is not named anywhere under src/"))
        .collect()
}

/// Every `src/....rs` path the document mentions, outside fenced code.
fn source_paths(text: &str) -> BTreeSet<String> {
    let prepared = prepare(text);
    prepared
        .match_indices("src/")
        .map(|(start, _)| {
            let end = prepared[start..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || "/_.-".contains(c)))
                .map_or(prepared.len(), |offset| start + offset);
            prepared[start..end].trim_end_matches('.').to_string()
        })
        .filter(|path| path.ends_with(".rs"))
        .collect()
}

/// The first table under `THREAT_MODEL_HEADING` has a row for each of `ATTACK_CLASSES` in
/// its first column.
fn check_threat_model(text: &str) -> Result<(), String> {
    let section = section(text, THREAT_MODEL_HEADING)?;
    let tables = tables(&section.content);
    let table = tables
        .first()
        .ok_or_else(|| format!("{THREAT_MODEL_HEADING:?} holds no table"))?;
    let attacks: Vec<String> = table.rows[1..]
        .iter()
        .map(|row| cells(row)[0].to_lowercase())
        .collect();
    let missing: Vec<&str> = ATTACK_CLASSES
        .iter()
        .copied()
        .filter(|class| !attacks.iter().any(|attack| attack.contains(class)))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "line {}: the threat model table has no row for {missing:?}",
            section.first_line + table.line - 1
        ))
    }
}

/// A line under `OUT_OF_SCOPE_HEADING` places cryptographic agility with Reticulum.
fn check_crypto_agility_out_of_scope(text: &str) -> Result<(), String> {
    let section = section(text, OUT_OF_SCOPE_HEADING)?;
    if section
        .content
        .lines()
        .any(|line| line.contains("cryptographic agility") && line.contains("Reticulum"))
    {
        Ok(())
    } else {
        Err(format!(
            "{OUT_OF_SCOPE_HEADING:?} has no line placing cryptographic agility with Reticulum"
        ))
    }
}

fn leading_digits(text: &str) -> Option<&str> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    (end > 0).then_some(&text[..end])
}

/// The number of every `draft <part><n>` mention in `line`.
fn draft_mentions(line: &str, part: char) -> Vec<&str> {
    let needle = format!("{DRAFT_MENTION}{part}");
    line.match_indices(needle.as_str())
        .filter_map(|(start, needle)| leading_digits(&line[start + needle.len()..]))
        .collect()
}

/// The number of every `### A<n>` heading between `PART_A` and `PART_B` of `issues`.
fn part_a_drafts(issues: &str) -> Vec<&str> {
    let heading = format!("{DRAFT_HEADING}A");
    issues
        .lines()
        .skip_while(|line| !line.starts_with(PART_A))
        .take_while(|line| !line.starts_with(PART_B))
        .filter_map(|line| leading_digits(line.strip_prefix(heading.as_str())?))
        .collect()
}

/// The lines of `issues` under its `### A<number>` heading, up to the next `### ` heading.
fn draft_block(issues: &str, number: &str) -> Option<String> {
    let heading = format!("{DRAFT_HEADING}A{number}");
    let lines: Vec<&str> = issues.lines().collect();
    let at = lines.iter().position(|line| {
        line.strip_prefix(heading.as_str())
            .is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_digit()))
    })?;
    Some(
        lines[at + 1..]
            .iter()
            .take_while(|line| !line.starts_with(DRAFT_HEADING))
            .copied()
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Every leniency row states `LENIENCY_FIELDS`, its removal condition is not "none", and
/// each draft it points at is a `### A<n>` block of `issues` still reading `DRAFT_STATUS`;
/// in return every Part A draft is cited by some row and no row cites a Part B draft.
fn check_leniency_rows(text: &str, issues: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut cited = BTreeSet::new();
    let text = prepare(text);
    for (index, line) in text.lines().enumerate() {
        if !line.contains(LENIENCY_OPEN) {
            continue;
        }
        let number = index + 1;
        for field in LENIENCY_FIELDS {
            if !line.contains(field) {
                problems.push(format!("line {number}: no {field:?}"));
            }
        }
        if line
            .split_once(LENIENCY_FIELDS[2])
            .is_some_and(|(_, removal)| removal.trim_start().to_lowercase().starts_with("none"))
        {
            problems.push(format!("line {number}: the removal condition is \"none\""));
        }
        for draft in draft_mentions(line, 'B') {
            problems.push(format!(
                "line {number}: cites draft B{draft}, which no leniency rests on"
            ));
        }
        for draft in draft_mentions(line, 'A') {
            cited.insert(draft);
            match draft_block(issues, draft) {
                None => problems.push(format!(
                    "line {number}: draft A{draft} has no heading in docs/mesh/upstream-issues.md"
                )),
                Some(block) if !block.contains(DRAFT_STATUS) => problems.push(format!(
                    "line {number}: draft A{draft} does not read {DRAFT_STATUS:?}"
                )),
                Some(_) => {}
            }
        }
    }
    for draft in part_a_drafts(issues) {
        if !cited.contains(draft) {
            problems.push(format!("draft A{draft} is cited by no leniency row"));
        }
    }
    problems
}

fn parse_constants_table(text: &str) -> Result<Vec<ConstantRow>, String> {
    let section = section(text, CONSTANTS_HEADING)?;
    let tables = tables(&section.content);
    let [table] = tables.as_slice() else {
        return Err(format!(
            "{CONSTANTS_HEADING:?} holds {} tables, expected exactly one",
            tables.len()
        ));
    };
    if cells(table.rows[0]) != cells(CONSTANTS_TABLE_HEADER) {
        return Err(format!(
            "line {}: constants table header {:?} is not {CONSTANTS_TABLE_HEADER:?}",
            section.first_line + table.line - 1,
            table.rows[0]
        ));
    }
    table
        .rows
        .iter()
        .enumerate()
        .skip(2)
        .map(|(offset, row)| {
            let line = section.first_line + table.line + offset - 1;
            let cells = cells(row);
            let [name, literal, _, pinned_by] = cells.as_slice() else {
                return Err(format!(
                    "line {line}: constants row {row:?} does not have four cells"
                ));
            };
            match (backticked(name), backticked(literal)) {
                (Some(name), Some(literal)) => Ok(ConstantRow {
                    name: name.to_string(),
                    literal: literal.to_string(),
                    pinned_by: pinned_by.to_string(),
                    line,
                }),
                _ => Err(format!(
                    "line {line}: constants row {row:?} needs a backticked name and literal"
                )),
            }
        })
        .collect()
}

fn check_constants(actual: &[ConstantRow], expected: &[(&str, String)]) -> Result<(), String> {
    for (row, (got, want)) in actual.iter().zip(expected).enumerate() {
        if (got.name.as_str(), &got.literal) != (want.0, &want.1) {
            return Err(format!(
                "constants row {}: expected `{}` = `{}`, found `{}` = `{}`",
                row + 1,
                want.0,
                want.1,
                got.name,
                got.literal
            ));
        }
    }
    match actual.len().cmp(&expected.len()) {
        Ordering::Less => Err(format!(
            "constants table is missing `{}`",
            expected[actual.len()].0
        )),
        Ordering::Greater => Err(format!(
            "constants table has an extra row `{}`",
            actual[expected.len()].name
        )),
        Ordering::Equal => Ok(()),
    }
}

/// Every "Pinned by" cell names a test or item the sources define as a whole word, or is
/// the sentinel for a constant only the table itself pins.
fn check_pinned_by(rows: &[ConstantRow], sources: &str) -> Vec<String> {
    rows.iter()
        .filter(|row| {
            row.pinned_by != PINNED_BY_THIS_TABLE
                && (row.pinned_by.is_empty()
                    || row.pinned_by.contains(char::is_whitespace)
                    || !names_identifier(sources, &row.pinned_by))
        })
        .map(|row| {
            format!(
                "line {}: `{}` is pinned by {:?}, which is neither {PINNED_BY_THIS_TABLE:?} nor an identifier named under src/",
                row.line, row.name, row.pinned_by
            )
        })
        .collect()
}

fn parse_index_line(line: &str) -> Option<(&str, &str)> {
    let (id, rest) = parse_id(line.strip_prefix("- [")?)?;
    let (anchor, text) = rest.strip_prefix("](#")?.split_once(") -- ")?;
    (!anchor.is_empty() && !text.trim().is_empty()).then_some((id, anchor))
}

fn index_entries(text: &str) -> Result<Vec<IndexEntry>, Vec<String>> {
    let section = section(text, INDEX_HEADING).map_err(|error| vec![error])?;
    let mut entries = Vec::new();
    let mut violations = Vec::new();
    for (offset, line) in section.content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let number = section.first_line + offset;
        match parse_index_line(line) {
            Some((id, anchor)) => entries.push(IndexEntry {
                id: id.to_string(),
                anchor: anchor.to_string(),
                line: number,
            }),
            None => violations.push(format!(
                "line {number}: index entry {line:?} is not '- [MESH-AREA-NNN](#anchor) -- text'"
            )),
        }
    }
    if violations.is_empty() {
        Ok(entries)
    } else {
        Err(violations)
    }
}

/// Every requirement id the section 21 index lists, in index order.
pub(crate) fn requirement_ids(spec: &str) -> Result<Vec<String>, Vec<String>> {
    index_entries(spec).map(|entries| entries.into_iter().map(|entry| entry.id).collect())
}

fn check_index_ids(definitions: &[Definition], entries: &[IndexEntry]) -> Result<(), String> {
    for (definition, entry) in definitions.iter().zip(entries) {
        if definition.id != entry.id {
            return Err(format!(
                "line {}: index lists {} where the definition order has {}",
                entry.line, entry.id, definition.id
            ));
        }
    }
    match entries.len().cmp(&definitions.len()) {
        Ordering::Less => Err(format!(
            "index is missing {}",
            definitions[entries.len()].id
        )),
        Ordering::Greater => Err(format!(
            "line {}: index lists {} which has no definition",
            entries[definitions.len()].line,
            entries[definitions.len()].id
        )),
        Ordering::Equal => Ok(()),
    }
}

fn check_index_anchors(definitions: &[Definition], entries: &[IndexEntry]) -> Result<(), String> {
    for entry in entries {
        let Some(definition) = definitions.iter().find(|d| d.id == entry.id) else {
            return Err(format!(
                "line {}: {} has no definition",
                entry.line, entry.id
            ));
        };
        if definition.slug != entry.anchor {
            return Err(format!(
                "line {}: {} is indexed under #{} but defined under #{}",
                entry.line, entry.id, entry.anchor, definition.slug
            ));
        }
    }
    Ok(())
}

// Redundant under a test-only module, but `r3/tests.rs` splits production code from tests
// at exactly this marker when it counts the sites that name a refusal code.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mesh_config::{
        DEFAULT_ENVOY_MEMORY_MAX_BYTES, DEFAULT_ENVOY_MEMORY_MAX_PER_IDENTITY,
        DEFAULT_ENVOY_MEMORY_MAX_SESSIONS, DEFAULT_ENVOY_MEMORY_MAX_TURNS,
        DEFAULT_ENVOY_MEMORY_TTL_HOURS, DEFAULT_FETCH_MAX_BYTES, DEFAULT_INLINE_MAX_BYTES,
        DEFAULT_PEER_MAX_CONCURRENT, DEFAULT_PEER_MAX_MESSAGES_PER_HOUR,
        DEFAULT_PEER_MAX_TOKENS_PER_HOUR, MAX_FETCH_FILE_BYTES, MAX_INLINE_FILE_TOTAL,
    };
    use crate::config::mesh_envoy::ENVOY_RUN_TIMEOUT_SECS;
    use crate::function::mesh::FETCH_INLINE_TEXT_MAX_BYTES;
    use crate::mesh::message::{MAX_PARTS, MAX_PARTS_BYTES};
    use crate::mesh::r3::{
        MAX_FETCH_RESPONSE_BYTES, RESPONSE_FRAME_PREFIX, RefusalCode, RequestId, ResponseFrame,
    };
    use crate::mesh::{
        access, announce, card, envoy_sessions, events, fetch, grants, identity, knock, knocks,
        limits, message, node, peers, pending, propagation, propagation_fetch, propagation_nodes,
        protocol, r3, schema, shares, trust, wire_path,
    };
    use lxmf_core::constants::{FIELD_CUSTOM_DATA, FIELD_CUSTOM_TYPE};
    use rmpv::Value;
    use rns_transport::hash::ADDRESS_HASH_SIZE;
    use rns_transport::resource::MAX_EFFICIENT_SIZE;
    use std::time::Duration;

    const EXPECTED_LITERALS: &str = r#"1,1,10,16,262144,128,"/knock","/status","/message",30,10,10,2,20,16,0xf0,0xf1,0xf3,0xf4,0xf5,0xf6,0xfd,0xfe,"SCOPE",64,300,900,3,2700,1800,1024,"scope.knock/1",200,15,10,256,3,600,256,16,1,0,1,2,64,280,64,64,120,280,"scope.peer/1",1,120,4000,64,4096,8,15,10,604800,256,3600,120,256,1,60,100000,120,26,60,2,60,1024,64,240,131072,112,4096,15552000,4096,86400,3,900,256,0,32,0xfb,0xfc,8,64,256,64,8,2,2,2,2,1,2,1,8,106496,98304,65536,"/list","/fetch",1024,64,1000,100000,64,2048,120,128,1048447,4194304,4194304,4198400,92 c4 10,200,16,32,"/access","scope.access/1",16,500,5,900,1,1,1,16,32768,1048575,4096,1,1,256,16,40,65536,168"#;

    fn expected_constants() -> Vec<(&'static str, String)> {
        let secs = |d: Duration| d.as_secs().to_string();
        let quoted = |s: &str| format!("{s:?}");
        let byte = |v: u8| format!("0x{v:02x}");
        let code = |c: RefusalCode| byte(c as u8);
        let hex_bytes = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        vec![
            (
                "MESH_PROTOCOL_VERSION",
                protocol::MESH_PROTOCOL_VERSION.to_string(),
            ),
            (
                "MESH_PROTOCOL_MIN_SUPPORTED",
                protocol::MESH_PROTOCOL_MIN_SUPPORTED.to_string(),
            ),
            ("NAME_HASH_LEN", r3::NAME_HASH_LEN.to_string()),
            ("ADDRESS_HASH_SIZE", ADDRESS_HASH_SIZE.to_string()),
            ("MAX_R3_PAYLOAD_BYTES", r3::MAX_R3_PAYLOAD_BYTES.to_string()),
            ("MAX_R3_NESTING_DEPTH", r3::MAX_R3_NESTING_DEPTH.to_string()),
            ("KNOCK_PATH", quoted(r3::KNOCK_PATH)),
            ("STATUS_PATH", quoted(r3::STATUS_PATH)),
            ("MESSAGE_PATH", quoted(r3::MESSAGE_PATH)),
            ("DEFAULT_REQUEST_TIMEOUT", secs(r3::DEFAULT_REQUEST_TIMEOUT)),
            ("DEFAULT_LINK_TIMEOUT", secs(r3::DEFAULT_LINK_TIMEOUT)),
            (
                "DEFAULT_RESPONSE_SEND_TIMEOUT",
                secs(r3::DEFAULT_RESPONSE_SEND_TIMEOUT),
            ),
            ("PEER_RESOLVE_TIMEOUT", secs(r3::PEER_RESOLVE_TIMEOUT)),
            ("HANDLER_TIMEOUT", secs(r3::HANDLER_TIMEOUT)),
            (
                "MAX_CONCURRENT_INBOUND_REQUESTS",
                r3::MAX_CONCURRENT_INBOUND_REQUESTS.to_string(),
            ),
            ("RefusalCode::NoIdentity", code(RefusalCode::NoIdentity)),
            ("RefusalCode::NoAccess", code(RefusalCode::NoAccess)),
            ("RefusalCode::InvalidKey", code(RefusalCode::InvalidKey)),
            ("RefusalCode::InvalidData", code(RefusalCode::InvalidData)),
            ("RefusalCode::InvalidStamp", code(RefusalCode::InvalidStamp)),
            ("RefusalCode::Throttled", code(RefusalCode::Throttled)),
            ("RefusalCode::NotFound", code(RefusalCode::NotFound)),
            ("RefusalCode::Timeout", code(RefusalCode::Timeout)),
            (
                "ANNOUNCE_MAGIC",
                quoted(std::str::from_utf8(&announce::ANNOUNCE_MAGIC).unwrap()),
            ),
            (
                "MAX_DISPLAY_NAME_BYTES",
                announce::MAX_DISPLAY_NAME_BYTES.to_string(),
            ),
            (
                "REANNOUNCE_FLOOR_SECS",
                announce::REANNOUNCE_FLOOR_SECS.to_string(),
            ),
            ("HEARTBEAT_SECS", announce::HEARTBEAT_SECS.to_string()),
            (
                "PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT",
                announce::PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT.to_string(),
            ),
            ("PEER_TTL", secs(peers::PEER_TTL)),
            ("PEER_STALE_AFTER", secs(peers::PEER_STALE_AFTER)),
            (
                "PEER_TABLE_MAX_ENTRIES",
                peers::PEER_TABLE_MAX_ENTRIES.to_string(),
            ),
            ("KNOCK_TYPE", quoted(knock::KNOCK_TYPE)),
            (
                "KNOCK_INTRO_MAX_CHARS",
                knocks::KNOCK_INTRO_MAX_CHARS.to_string(),
            ),
            ("KNOCK_REQUEST_TIMEOUT", secs(knock::KNOCK_REQUEST_TIMEOUT)),
            ("KNOCK_LINK_TIMEOUT", secs(knock::KNOCK_LINK_TIMEOUT)),
            (
                "KNOCK_GATE_MAX_IDENTITIES",
                knock::KNOCK_GATE_MAX_IDENTITIES.to_string(),
            ),
            ("KNOCK_BUCKET_BURST", knock::KNOCK_BUCKET_BURST.to_string()),
            (
                "KNOCK_BUCKET_REFILL_INTERVAL",
                secs(knock::KNOCK_BUCKET_REFILL_INTERVAL),
            ),
            (
                "KNOCK_CACHE_MAX_ENTRIES",
                knocks::KNOCK_CACHE_MAX_ENTRIES.to_string(),
            ),
            (
                "KNOCK_CACHE_MAX_PER_IDENTITY",
                knocks::KNOCK_CACHE_MAX_PER_IDENTITY.to_string(),
            ),
            ("STATUS_CARD_VERSION", card::STATUS_CARD_VERSION.to_string()),
            ("STATE_UNKNOWN", card::STATE_UNKNOWN.to_string()),
            ("STATE_IDLE", card::STATE_IDLE.to_string()),
            ("STATE_WORKING", card::STATE_WORKING.to_string()),
            (
                "DISPLAY_NAME_MAX_CHARS",
                card::DISPLAY_NAME_MAX_CHARS.to_string(),
            ),
            ("OBJECTIVE_MAX_CHARS", card::OBJECTIVE_MAX_CHARS.to_string()),
            ("REPO_NAME_MAX_CHARS", card::REPO_NAME_MAX_CHARS.to_string()),
            ("BRANCH_MAX_CHARS", card::BRANCH_MAX_CHARS.to_string()),
            (
                "PLAN_TITLE_MAX_CHARS",
                card::PLAN_TITLE_MAX_CHARS.to_string(),
            ),
            ("TODO_GOAL_MAX_CHARS", card::TODO_GOAL_MAX_CHARS.to_string()),
            ("PEER_MESSAGE_TYPE", quoted(message::PEER_MESSAGE_TYPE)),
            ("PEER_WIRE_VERSION", message::PEER_WIRE_VERSION.to_string()),
            (
                "PEER_TITLE_MAX_CHARS",
                message::PEER_TITLE_MAX_CHARS.to_string(),
            ),
            (
                "PEER_CONTENT_MAX_CHARS",
                message::PEER_CONTENT_MAX_CHARS.to_string(),
            ),
            ("PEER_ID_MAX_CHARS", message::PEER_ID_MAX_CHARS.to_string()),
            (
                "PEER_FIELDS_MAX_BYTES",
                message::PEER_FIELDS_MAX_BYTES.to_string(),
            ),
            (
                "PEER_FIELDS_MAX_DEPTH",
                message::PEER_FIELDS_MAX_DEPTH.to_string(),
            ),
            ("PEER_REQUEST_TIMEOUT", secs(message::PEER_REQUEST_TIMEOUT)),
            ("PEER_LINK_TIMEOUT", secs(message::PEER_LINK_TIMEOUT)),
            ("PENDING_TTL", secs(pending::PENDING_TTL)),
            (
                "PENDING_MAX_ENTRIES",
                pending::PENDING_MAX_ENTRIES.to_string(),
            ),
            ("PEER_WINDOW", secs(limits::PEER_WINDOW)),
            (
                "PEER_RETRY_AFTER_CAPACITY",
                secs(limits::PEER_RETRY_AFTER_CAPACITY),
            ),
            (
                "PEER_LIMITS_MAX_IDENTITIES",
                limits::PEER_LIMITS_MAX_IDENTITIES.to_string(),
            ),
            (
                "DEFAULT_PEER_MAX_CONCURRENT",
                DEFAULT_PEER_MAX_CONCURRENT.to_string(),
            ),
            (
                "DEFAULT_PEER_MAX_MESSAGES_PER_HOUR",
                DEFAULT_PEER_MAX_MESSAGES_PER_HOUR.to_string(),
            ),
            (
                "DEFAULT_PEER_MAX_TOKENS_PER_HOUR",
                DEFAULT_PEER_MAX_TOKENS_PER_HOUR.to_string(),
            ),
            ("ENVOY_RUN_TIMEOUT_SECS", ENVOY_RUN_TIMEOUT_SECS.to_string()),
            (
                "MAX_ACCEPTED_STAMP_COST",
                propagation::MAX_ACCEPTED_STAMP_COST.to_string(),
            ),
            (
                "PROPAGATION_TRANSFER_TIMEOUT",
                secs(propagation::PROPAGATION_TRANSFER_TIMEOUT),
            ),
            (
                "PROPAGATION_REJECT_WINDOW",
                secs(propagation::PROPAGATION_REJECT_WINDOW),
            ),
            (
                "FETCH_REQUEST_TIMEOUT",
                secs(propagation_fetch::FETCH_REQUEST_TIMEOUT),
            ),
            (
                "MAX_LISTED_IDS",
                propagation_fetch::MAX_LISTED_IDS.to_string(),
            ),
            (
                "MAX_WANTS_PER_FETCH",
                propagation_fetch::MAX_WANTS_PER_FETCH.to_string(),
            ),
            (
                "FETCH_TRANSFER_LIMIT_KB",
                propagation_fetch::FETCH_TRANSFER_LIMIT_KB.to_string(),
            ),
            (
                "MAX_FETCHED_MESSAGE_BYTES",
                propagation_fetch::MAX_FETCHED_MESSAGE_BYTES.to_string(),
            ),
            (
                "MIN_FETCHED_MESSAGE_BYTES",
                propagation_fetch::MIN_FETCHED_MESSAGE_BYTES.to_string(),
            ),
            (
                "DEDUP_CAPACITY",
                propagation_fetch::DEDUP_CAPACITY.to_string(),
            ),
            ("DEDUP_HORIZON", secs(propagation_fetch::DEDUP_HORIZON)),
            ("BODY_DEDUP_CAPACITY", node::BODY_DEDUP_CAPACITY.to_string()),
            ("BODY_DEDUP_HORIZON", secs(node::BODY_DEDUP_HORIZON)),
            (
                "MAX_UNKNOWN_SOURCE_DEFERRALS",
                propagation_fetch::MAX_UNKNOWN_SOURCE_DEFERRALS.to_string(),
            ),
            (
                "UNKNOWN_SOURCE_DEFERRAL_HORIZON",
                secs(propagation_fetch::UNKNOWN_SOURCE_DEFERRAL_HORIZON),
            ),
            (
                "MAX_DEFERRED_IDS",
                propagation_fetch::MAX_DEFERRED_IDS.to_string(),
            ),
            (
                "REQUIRED_DELIVERY_STAMP_COST",
                propagation_fetch::REQUIRED_DELIVERY_STAMP_COST.to_string(),
            ),
            (
                "PROPAGATION_NODE_TABLE_MAX_ENTRIES",
                propagation_nodes::PROPAGATION_NODE_TABLE_MAX_ENTRIES.to_string(),
            ),
            ("FIELD_CUSTOM_TYPE", byte(FIELD_CUSTOM_TYPE)),
            ("FIELD_CUSTOM_DATA", byte(FIELD_CUSTOM_DATA)),
            ("LOGGED_HASH_CHARS", r3::LOGGED_HASH_CHARS.to_string()),
            (
                "PEER_INBOX_CAPACITY",
                message::PEER_INBOX_CAPACITY.to_string(),
            ),
            (
                "INBOUND_MAX_ENTRIES",
                pending::INBOUND_MAX_ENTRIES.to_string(),
            ),
            (
                "KNOCK_QUEUE_CAPACITY",
                knock::KNOCK_QUEUE_CAPACITY.to_string(),
            ),
            (
                "ENVOY_QUEUE_MAX",
                crate::config::mesh_envoy::ENVOY_QUEUE_MAX.to_string(),
            ),
            ("TRUST_FILE_VERSION", trust::TRUST_FILE_VERSION.to_string()),
            (
                "KNOCK_RECORD_VERSION",
                knocks::KNOCK_RECORD_VERSION.to_string(),
            ),
            (
                "PENDING_RECORD_VERSION",
                pending::PENDING_RECORD_VERSION.to_string(),
            ),
            (
                "INBOUND_RECORD_VERSION",
                pending::INBOUND_RECORD_VERSION.to_string(),
            ),
            (
                "PREDECESSOR_RECORD_VERSION",
                identity::PREDECESSOR_RECORD_VERSION.to_string(),
            ),
            ("PEER_TABLE_VERSION", peers::PEER_TABLE_VERSION.to_string()),
            (
                "PROPAGATION_STORE_VERSION",
                propagation_fetch::PROPAGATION_STORE_VERSION.to_string(),
            ),
            ("MAX_PARTS", MAX_PARTS.to_string()),
            ("MAX_PARTS_BYTES", MAX_PARTS_BYTES.to_string()),
            ("MAX_INLINE_FILE_TOTAL", MAX_INLINE_FILE_TOTAL.to_string()),
            (
                "DEFAULT_INLINE_MAX_BYTES",
                DEFAULT_INLINE_MAX_BYTES.to_string(),
            ),
            ("LIST_PATH", quoted(r3::LIST_PATH)),
            ("FETCH_PATH", quoted(r3::FETCH_PATH)),
            (
                "WIRE_PATH_MAX_BYTES",
                wire_path::WIRE_PATH_MAX_BYTES.to_string(),
            ),
            (
                "WIRE_PATH_MAX_SEGMENTS",
                wire_path::WIRE_PATH_MAX_SEGMENTS.to_string(),
            ),
            ("LIST_PAGE_SIZE", shares::LIST_PAGE_SIZE.to_string()),
            (
                "DEFAULT_LIST_WALK_BOUND",
                shares::DEFAULT_LIST_WALK_BOUND.to_string(),
            ),
            ("CURSOR_MAX_BYTES", fetch::CURSOR_MAX_BYTES.to_string()),
            ("LIST_PAGE_HEADROOM", fetch::LIST_PAGE_HEADROOM.to_string()),
            (
                "FILE_FETCH_REQUEST_TIMEOUT",
                secs(fetch::FILE_FETCH_REQUEST_TIMEOUT),
            ),
            (
                "OK_REPLY_FRAMING_BYTES",
                fetch::OK_REPLY_FRAMING_BYTES.to_string(),
            ),
            (
                "SINGLE_SEGMENT_FETCH_CEILING",
                fetch::SINGLE_SEGMENT_FETCH_CEILING.to_string(),
            ),
            (
                "DEFAULT_FETCH_MAX_BYTES",
                DEFAULT_FETCH_MAX_BYTES.to_string(),
            ),
            ("MAX_FETCH_FILE_BYTES", MAX_FETCH_FILE_BYTES.to_string()),
            (
                "MAX_FETCH_RESPONSE_BYTES",
                MAX_FETCH_RESPONSE_BYTES.to_string(),
            ),
            ("RESPONSE_FRAME_PREFIX", hex_bytes(&RESPONSE_FRAME_PREFIX)),
            ("ABOUT_MAX_CHARS", card::ABOUT_MAX_CHARS.to_string()),
            ("CAPS_MAX_ENTRIES", card::CAPS_MAX_ENTRIES.to_string()),
            ("CAP_MAX_CHARS", card::CAP_MAX_CHARS.to_string()),
            ("ACCESS_PATH", quoted(r3::ACCESS_PATH)),
            ("ACCESS_TYPE", quoted(access::ACCESS_TYPE)),
            ("ACCESS_MAX_PATHS", access::ACCESS_MAX_PATHS.to_string()),
            (
                "ACCESS_REASON_MAX_CHARS",
                access::ACCESS_REASON_MAX_CHARS.to_string(),
            ),
            (
                "ACCESS_MAX_PENDING_PER_IDENTITY",
                access::ACCESS_MAX_PENDING_PER_IDENTITY.to_string(),
            ),
            ("DEFAULT_GRANT_TTL", secs(grants::DEFAULT_GRANT_TTL)),
            (
                "SHARES_FILE_VERSION",
                shares::SHARES_FILE_VERSION.to_string(),
            ),
            (
                "GRANT_RECORD_VERSION",
                grants::GRANT_RECORD_VERSION.to_string(),
            ),
            ("DEFAULT_GRANT_USES", grants::DEFAULT_GRANT_USES.to_string()),
            ("GRANT_MAX_PATHS", grants::GRANT_MAX_PATHS.to_string()),
            (
                "FETCH_INLINE_TEXT_MAX_BYTES",
                FETCH_INLINE_TEXT_MAX_BYTES.to_string(),
            ),
            ("MAX_EFFICIENT_SIZE", MAX_EFFICIENT_SIZE.to_string()),
            (
                "PRESENCE_SURFACED_CAP",
                trust::PRESENCE_SURFACED_CAP.to_string(),
            ),
            (
                "ENVOY_SESSION_VERSION",
                envoy_sessions::ENVOY_SESSION_VERSION.to_string(),
            ),
            (
                "ENVOY_SESSION_INDEX_VERSION",
                envoy_sessions::ENVOY_SESSION_INDEX_VERSION.to_string(),
            ),
            (
                "DEFAULT_ENVOY_MEMORY_MAX_SESSIONS",
                DEFAULT_ENVOY_MEMORY_MAX_SESSIONS.to_string(),
            ),
            (
                "DEFAULT_ENVOY_MEMORY_MAX_PER_IDENTITY",
                DEFAULT_ENVOY_MEMORY_MAX_PER_IDENTITY.to_string(),
            ),
            (
                "DEFAULT_ENVOY_MEMORY_MAX_TURNS",
                DEFAULT_ENVOY_MEMORY_MAX_TURNS.to_string(),
            ),
            (
                "DEFAULT_ENVOY_MEMORY_MAX_BYTES",
                DEFAULT_ENVOY_MEMORY_MAX_BYTES.to_string(),
            ),
            (
                "DEFAULT_ENVOY_MEMORY_TTL_HOURS",
                DEFAULT_ENVOY_MEMORY_TTL_HOURS.to_string(),
            ),
        ]
    }

    const VALID_SPEC: &str = "\
# SCOPE — Session Coordination & Presence Exchange

## 1. Introduction and scope

Plain prose. The mayor was dismayed by the mustard.

## 2. Conventions and requirements language

The key words \"MUST\", \"MUST NOT\", \"REQUIRED\", \"SHALL\", \"SHALL NOT\", \"SHOULD\", \"SHOULD NOT\", \"RECOMMENDED\", \"NOT RECOMMENDED\", \"MAY\", and \"OPTIONAL\" in this document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174] when, and only when, they appear in all capitals, as shown here.

## 3. Third

## 4. Destination naming

**[MESH-DEST-001]** A node MUST hash the name.
**[MESH-DEST-002]** A node SHOULD NOT reuse it; the code says `must` and `**[MESH-` here (`hash`, src/mesh/mod.rs).
**[MESH-DEST-004]** Out of order is fine. **[MESH-DEST-003]** Each sentence MUST carry its own id.

```text
Fenced lines may say must and MUST without an id.
# and may even look like a heading
```

| Bytes | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| 0..4 | ASCII | magic | refuse |
| trailing bytes | any | nothing | **[MESH-DEST-005]** The receiver MUST ignore them (`decode_whole_frame_test`). |

## 5. Fifth

## 6. Sixth

### 6.2 The Envelope

**[MESH-ENV-001]** Unknown keys MAY appear.

| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `v` | uint | 1 | refuse |
| any other key | any | nothing | **[MESH-ENV-002]** The receiver MUST ignore it. |

| Result | Outcome |
|---|---|
| anything | fine |

## 7. Seventh

## 8. Eighth

## 9. Ninth

## 10. Tenth

## 11. Eleventh

## 12. Twelfth

## 13. Thirteenth

## 14. Fourteenth

## 15. Fifteenth

## 16. Sixteenth

## 17. Seventeenth

## 18. Eighteenth

## 19. Constants

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | `1` | a.rs | spec_pins (this table) |
| `BETA` | `\"b\"` | b.rs | decode_whole_frame_test |

## 20. Twentieth

## 21. Requirements index

- [MESH-DEST-001](#4-destination-naming) -- hash the name
- [MESH-DEST-002](#4-destination-naming) -- no reuse
- [MESH-DEST-004](#4-destination-naming) -- out of order
- [MESH-DEST-003](#4-destination-naming) -- own id
- [MESH-DEST-005](#4-destination-naming) -- trailing bytes
- [MESH-ENV-001](#62-the-envelope) -- unknown keys
- [MESH-ENV-002](#62-the-envelope) -- unknown keys
";

    fn valid_expected() -> Vec<(&'static str, String)> {
        vec![("ALPHA", "1".to_string()), ("BETA", "\"b\"".to_string())]
    }

    fn sections(numbers: &[usize]) -> String {
        let mut text = format!("{EXPECTED_H1}\n");
        for number in numbers {
            text.push_str(&format!("\n## {number}. Section {number}\n"));
        }
        text
    }

    #[test]
    fn code_stripping_blanks_fences_and_spans_but_keeps_line_numbers() {
        let text = "a `x` b\n```\nMUST\n```\nc\r\nd `unclosed";
        assert_eq!(strip_code(text), "a   b\n\n\n\nc\nd `unclosed");
    }

    #[test]
    fn whole_word_matching_respects_ascii_word_boundaries() {
        assert!(contains_word("a MUST b", "MUST"));
        assert!(contains_word("MUST", "MUST"));
        assert!(contains_word("(MUST)", "MUST"));
        assert!(!contains_word("MUSTARD", "MUST"));
        assert!(!contains_word("dismay", "may"));
        assert!(!contains_word("MUST_", "MUST"));
        assert!(contains_word("é may é", "may"));
    }

    #[test]
    fn slugs_follow_the_github_rules() {
        assert_eq!(slug("## 4. Destination naming"), "4-destination-naming");
        assert_eq!(slug("### 6.2 The Envelope"), "62-the-envelope");
        assert_eq!(
            slug("### 6.2 The `Envelope`, again"),
            "62-the-envelope-again"
        );
    }

    #[test]
    fn the_valid_fixture_passes_every_checker() {
        check_headings(VALID_SPEC).unwrap();
        check_boilerplate(VALID_SPEC).unwrap();
        let body = body(VALID_SPEC).unwrap();
        let definitions = definitions(&body).unwrap();
        assert_eq!(
            definitions
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            [
                "MESH-DEST-001",
                "MESH-DEST-002",
                "MESH-DEST-004",
                "MESH-DEST-003",
                "MESH-DEST-005",
                "MESH-ENV-001",
                "MESH-ENV-002"
            ]
        );
        assert_eq!(definitions[0].line, 15);
        assert_eq!(check_normative_lines_carry_ids(&body), Vec::<String>::new());
        assert_eq!(
            check_no_lowercase_keywords(VALID_SPEC),
            Vec::<String>::new()
        );
        assert_eq!(check_field_tables(VALID_SPEC), Ok(2));
        assert_eq!(
            cited_identifiers(VALID_SPEC),
            [(27, "decode_whole_frame_test".to_string())]
        );
        assert!(check_cited_identifiers(VALID_SPEC, "fn decode_whole_frame_test()").is_empty());
        assert_eq!(
            source_paths(VALID_SPEC).into_iter().collect::<Vec<_>>(),
            ["src/mesh/mod.rs"]
        );
        let constants = parse_constants_table(VALID_SPEC).unwrap();
        check_constants(&constants, &valid_expected()).unwrap();
        assert!(check_pinned_by(&constants, "fn decode_whole_frame_test()").is_empty());
        let entries = index_entries(VALID_SPEC).unwrap();
        check_index_ids(&definitions, &entries).unwrap();
        check_index_anchors(&definitions, &entries).unwrap();
        assert_eq!(
            requirement_ids(VALID_SPEC).unwrap(),
            [
                "MESH-DEST-001",
                "MESH-DEST-002",
                "MESH-DEST-004",
                "MESH-DEST-003",
                "MESH-DEST-005",
                "MESH-ENV-001",
                "MESH-ENV-002"
            ]
        );
    }

    #[test]
    fn headings_checker_rejects_extra_h1s_gaps_and_short_documents() {
        let two_h1s = format!("{EXPECTED_H1}\n# Another\n");
        assert!(
            check_headings(&two_h1s)
                .unwrap_err()
                .contains("exactly one H1")
        );
        let wrong_h1 = sections(&(1..=21).collect::<Vec<_>>()).replace("Presence", "Absence");
        assert!(check_headings(&wrong_h1).is_err());
        assert!(check_headings(&sections(&(1..=21).collect::<Vec<_>>())).is_ok());
        assert!(
            check_headings(&sections(&(1..=20).collect::<Vec<_>>()))
                .unwrap_err()
                .contains("found 20")
        );
        let gap: Vec<usize> = (1..=22).filter(|n| *n != 3).collect();
        assert!(
            check_headings(&sections(&gap))
                .unwrap_err()
                .contains("line 7")
        );
        let unnumbered = format!("{}\n## Appendix\n", sections(&(1..=21).collect::<Vec<_>>()));
        assert!(check_headings(&unnumbered).is_err());
    }

    #[test]
    fn boilerplate_checker_wants_the_sentence_verbatim() {
        assert!(check_boilerplate(VALID_SPEC).is_ok());
        let altered = VALID_SPEC.replace("as shown here", "as shown below");
        assert!(check_boilerplate(&altered).is_err());
    }

    const MALFORMED_ID: &str = "## 4. A\n\n**[MESH-DEST-01]** A node MUST hash.\n";
    const UNKNOWN_AREA: &str = "## 4. A\n\n**[MESH-FOO-001]** A node MUST hash.\n";
    const UNCLOSED_ID: &str = "## 4. A\n\n**[MESH-DEST-001] A node MUST hash.\n";
    const DUPLICATE_ID: &str =
        "## 4. A\n\n**[MESH-DEST-001]** A node MUST hash.\n**[MESH-DEST-001]** Again MUST.\n";
    const AREA_GAP: &str =
        "## 4. A\n\n**[MESH-DEST-001]** A node MUST hash.\n**[MESH-DEST-003]** Skipped MUST.\n";
    const AREA_STARTS_LATE: &str = "## 4. A\n\n**[MESH-DEST-002]** A node MUST hash.\n";
    const AREA_OUT_OF_ORDER: &str =
        "## 4. A\n\n**[MESH-ENV-002]** A node MUST hash.\n**[MESH-ENV-001]** It MUST too.\n";

    #[test]
    fn definitions_checker_rejects_malformed_duplicate_and_gapped_ids() {
        for (fixture, needle) in [
            (MALFORMED_ID, "malformed"),
            (UNKNOWN_AREA, "malformed"),
            (UNCLOSED_ID, "malformed"),
            (DUPLICATE_ID, "duplicate"),
            (AREA_GAP, "none is MESH-DEST-002"),
            (AREA_STARTS_LATE, "none is MESH-DEST-001"),
        ] {
            let violations = definitions(fixture).unwrap_err();
            assert!(
                violations.iter().any(|v| v.contains(needle)),
                "{fixture:?} -> {violations:?}"
            );
        }
    }

    #[test]
    fn definitions_checker_accepts_an_area_defined_out_of_order() {
        let found = definitions(AREA_OUT_OF_ORDER).unwrap();
        assert_eq!(
            found.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["MESH-ENV-002", "MESH-ENV-001"]
        );
    }

    const KEYWORD_WITHOUT_ID: &str = "## 4. A\n\nA node MUST hash.\n\nA node MAY NOT.\n";
    const KEYWORD_INSIDE_WORD: &str = "## 4. A\n\nThe MUSTARD is OPTIONALLY `MUST` fine.\n";
    const SECOND_SENTENCE_WITHOUT_ID: &str =
        "**[MESH-ENV-001]** The receiver MUST ignore it. The sender MUST NOT send it.\n";
    const BOTH_SENTENCES_WITH_IDS: &str = "**[MESH-ENV-001]** The receiver MUST ignore it. \
        **[MESH-ENV-002]** The sender MUST NOT send it; the `MUST` span is code.\n";
    const CELL_WITHOUT_ID: &str = "| `v` | **[MESH-ENV-001]** MUST be 1 | MUST refuse |\n";

    #[test]
    fn normative_lines_checker_reports_lines_lacking_an_id() {
        let violations = check_normative_lines_carry_ids(KEYWORD_WITHOUT_ID);
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(violations[0].starts_with("line 3: MUST"));
        assert!(violations[1].starts_with("line 5: MAY"));
        assert!(check_normative_lines_carry_ids(KEYWORD_INSIDE_WORD).is_empty());
        assert_eq!(check_normative_lines_carry_ids(UNCLOSED_ID).len(), 1);
    }

    #[test]
    fn normative_lines_checker_wants_an_id_per_sentence_and_per_cell() {
        assert_eq!(
            check_normative_lines_carry_ids(SECOND_SENTENCE_WITHOUT_ID),
            ["line 1: MUST without a requirement id: \"The sender MUST NOT send it.\""]
        );
        assert_eq!(
            check_normative_lines_carry_ids(CELL_WITHOUT_ID),
            ["line 1: MUST without a requirement id: \"MUST refuse\""]
        );
        assert!(check_normative_lines_carry_ids(BOTH_SENTENCES_WITH_IDS).is_empty());
    }

    const LOWERCASE_KEYWORD: &str = "## 4. A\n\nA node must hash.\nIt should.\nIt may (or Must).\n";
    const LOWERCASE_ONLY_IN_CODE: &str = "## 4. A\n\nThe `must` field and `should_retry`.\n";

    #[test]
    fn lowercase_keyword_checker_ignores_code_and_longer_words() {
        let violations = check_no_lowercase_keywords(LOWERCASE_KEYWORD);
        assert_eq!(violations.len(), 3, "{violations:?}");
        assert!(violations[0].starts_with("line 3: lowercase \"must\""));
        assert!(violations[2].starts_with("line 5: lowercase \"may\""));
        assert!(check_no_lowercase_keywords(LOWERCASE_ONLY_IN_CODE).is_empty());
        assert!(check_no_lowercase_keywords("mayor dismay mustard shoulder").is_empty());
    }

    const FIELD_TABLE_BAD_HEADER: &str = "\
| Field | Type | Sender | Receiver action on any other value |
|---|---|---|---|
| any other key | any | nothing | ignore |
";
    const FIELD_TABLE_NO_CATCH_ALL_ROW: &str = "\
| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `v` | uint | 1 | refuse |
";
    const BYTES_TABLE_NO_CATCH_ALL_ROW: &str = "\
| Bytes | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| 0..4 | ASCII | magic | refuse |
";
    const SLOT_TABLE_UPPERCASE_CATCH_ALL: &str = "\
| Slot | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| 0 | uint | 1 | refuse |
| Any other slot | any | nothing | **[MESH-ANN-001]** The receiver MUST ignore it. |
";
    const FIELD_TABLE_CATCH_ALL_WITHOUT_ID: &str = "\
| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `v` | uint | 1 | refuse |
| any other key | any | nothing | ignore |
";
    const FIELD_TABLE_CATCH_ALL_ID_IN_CODE: &str = "\
| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| `v` | uint | 1 | refuse |
| any other key | any | nothing | `**[MESH-ENV-001]**` ignore |
";
    const ACTION_COLUMN_UNDER_OTHER_KEY: &str = "\
| Key | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
| any other key | any | nothing | **[MESH-ENV-001]** The receiver MUST ignore it. |
";
    const FIELD_TABLE_EMPTY: &str = "\
| Field | Type | Sender puts | Receiver action on any other value |
|---|---|---|---|
";

    #[test]
    fn positional_table_checker_wants_the_header_and_a_catch_all_last_row() {
        assert!(check_field_tables(FIELD_TABLE_BAD_HEADER).unwrap_err()[0].contains("header"));
        for fixture in [
            FIELD_TABLE_NO_CATCH_ALL_ROW,
            BYTES_TABLE_NO_CATCH_ALL_ROW,
            SLOT_TABLE_UPPERCASE_CATCH_ALL,
        ] {
            let violations = check_field_tables(fixture).unwrap_err();
            assert!(
                violations[0].contains("instead of a row starting with"),
                "{fixture:?} -> {violations:?}"
            );
        }
        assert!(check_field_tables(FIELD_TABLE_EMPTY).unwrap_err()[0].contains("no data rows"));
        for fixture in [
            FIELD_TABLE_CATCH_ALL_WITHOUT_ID,
            FIELD_TABLE_CATCH_ALL_ID_IN_CODE,
        ] {
            let violations = check_field_tables(fixture).unwrap_err();
            assert!(
                violations[0].contains("does not define a requirement id"),
                "{fixture:?} -> {violations:?}"
            );
        }
        assert_eq!(
            check_field_tables("| Other | table |\n|---|---|\n| a | b |\n"),
            Ok(0)
        );
    }

    #[test]
    fn positional_table_checker_rejects_the_action_column_under_a_foreign_key() {
        let violations = check_field_tables(ACTION_COLUMN_UNDER_OTHER_KEY).unwrap_err();
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].starts_with("line 1: table header") && violations[0].contains("Key"),
            "{violations:?}"
        );
    }

    #[test]
    fn cited_identifier_scanner_keeps_only_long_snake_case_code_spans() {
        let text = "`short_name` and `displayname_x` but `two_under_scores` (`fn_with_1_digit`).\n\
                    ```\n`fenced_out_of_scope`\n```\n`CamelCase_name_here`, `_leading_under_score`, prose_not_in_code_span\n";
        assert_eq!(
            cited_identifiers(text),
            [
                (1, "two_under_scores".to_string()),
                (1, "fn_with_1_digit".to_string())
            ]
        );
        let sources = "pub fn two_under_scores() {}\nconst fn_with_1_digit_x: u8 = 0;\n";
        assert_eq!(
            check_cited_identifiers(text, sources),
            ["line 1: `fn_with_1_digit` is not named anywhere under src/"]
        );
        assert!(names_identifier(
            "pub two_under_scores: u64,",
            "two_under_scores"
        ));
        assert!(names_identifier(
            "\"two_under_scores\": secs,",
            "two_under_scores"
        ));
        assert!(!names_identifier(
            "fn two_under_scores_more() {}",
            "two_under_scores"
        ));
    }

    #[test]
    fn rust_source_walker_reads_nested_files() {
        let sources =
            rust_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mesh")).unwrap();
        assert!(sources.contains("fn destination_address"));
        assert!(sources.contains("fn refusal_codes_round_trip_the_wire_and_reject_other_values"));
        assert!(
            !sources.contains("fn rust_source_walker_reads_nested_files"),
            "the walk must skip this file so a citation cannot resolve against a fixture"
        );
    }

    #[test]
    fn rust_source_file_lister_keys_each_file_by_its_crate_relative_path() {
        let files = rust_source_files(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mesh = files
            .iter()
            .find(|(path, _)| path == "src/mesh/mod.rs")
            .expect("src/mesh/mod.rs is listed");
        assert!(mesh.1.contains("fn destination_address"));
        assert!(files.iter().all(|(path, _)| path.starts_with("src/")));
        assert!(!files.iter().any(|(path, _)| path.ends_with("spec_pins.rs")));
    }

    #[test]
    fn test_function_scanner_reads_through_attributes_and_skips_plain_fns() {
        let source = "\
#[test]
fn plain_test() {}

#[tokio::test(flavor = \"multi_thread\")]
#[ignore = \"slow\"]
async fn async_test() {}

/// Doc comments sit above the attribute.
#[test]

fn after_a_blank() {}

#[derive(Debug)]
fn attributed_but_not_a_test() {}

pub(crate) fn helper() {}
// fn commented_out() {}
";
        let expected: BTreeSet<String> = ["plain_test", "async_test", "after_a_blank"]
            .map(String::from)
            .into();
        assert_eq!(test_functions(source), expected);
        assert_eq!(declared_fn("    pub async fn go<T>(x: T) {}"), Some("go"));
        assert_eq!(declared_fn("    let fn_count = 1;"), None);
        assert_eq!(declared_fn("fn (tuple)"), None);
    }

    #[test]
    fn parenthetical_scanner_keeps_outer_groups_and_ignores_parens_in_spans() {
        assert_eq!(
            parentheticals("a (b (c) d) e (`(` f) g (h"),
            vec!["b (c) d", "`(` f"]
        );
        assert_eq!(parentheticals("no groups"), Vec::<&str>::new());
    }

    #[test]
    fn cited_path_checker_wants_a_test_in_a_file_its_parenthetical_names() {
        let files = vec![
            (
                "src/a.rs".to_string(),
                "#[test]\nfn alpha_test_one() {}\n".to_string(),
            ),
            (
                "src/b.rs".to_string(),
                "#[test]\nfn beta_test_two() {}\nfn some_helper_fn() {}\n".to_string(),
            ),
        ];
        let tests: BTreeSet<String> = ["alpha_test_one", "beta_test_two"].map(String::from).into();
        let ok = "\
A (`alpha_test_one`, src/a.rs; `beta_test_two`, src/b.rs).
B (`alpha_test_one`) and (`some_helper_fn`, src/a.rs) and (`beta_test_two`, src/a.rs, src/b.rs).
```text
C (`alpha_test_one`, src/b.rs) is fenced.
```
";
        assert_eq!(check_cited_paths(ok, &files, &tests), Vec::<String>::new());
        let drifted = "\
A (`alpha_test_one`, src/b.rs).
B (see `(` in a span) (`beta_test_two`, src/a.rs).
";
        let problems = check_cited_paths(drifted, &files, &tests);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[0].starts_with("line 1: `alpha_test_one` is not defined in"),
            "{}",
            problems[0]
        );
        assert!(
            problems[1].starts_with("line 2: `beta_test_two` is not defined in"),
            "{}",
            problems[1]
        );
    }

    #[test]
    fn cells_keep_a_pipe_inside_a_code_span_in_its_cell() {
        assert_eq!(
            cells("| `x || y` | concatenation | `a` |"),
            ["`x || y`", "concatenation", "`a`"]
        );
        assert_eq!(cells("| a | `unclosed | b |"), ["a", "`unclosed | b"]);
        assert_eq!(cells("|---|---|"), ["---", "---"]);
    }

    #[test]
    fn source_path_scanner_reads_paths_outside_fenced_code() {
        let text = "see src/mesh/mod.rs. Also `src/mesh/knock.rs`, src/x.rs) and src/no.\n```\nsrc/fenced.rs\n```\n";
        assert_eq!(
            source_paths(text).into_iter().collect::<Vec<_>>(),
            ["src/mesh/knock.rs", "src/mesh/mod.rs", "src/x.rs"]
        );
    }

    const CONSTANTS_TWO_TABLES: &str = "\
## 19. Constants

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | `1` | a.rs | x |

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `BETA` | `2` | b.rs | y |

## 21. Requirements index
";
    const CONSTANTS_BAD_HEADER: &str = "\
## 19. Constants

| Name | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | `1` | a.rs | x |
";
    const CONSTANTS_UNBACKTICKED: &str = "\
## 19. Constants

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | 1 | a.rs | x |
";
    const CONSTANTS_THREE_CELLS: &str = "\
## 19. Constants

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | `1` | a.rs |
";
    const CONSTANTS_PINNED_BY: &str = "\
## 19. Constants

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | `1` | a.rs | spec_pins (this table) |
| `BETA` | `2` | b.rs | beta_is_pinned_here |
| `GAMMA` | `3` | c.rs | gamma_test_that_nothing_defines |
| `DELTA` | `4` | d.rs | some prose instead |
| `EPS` | `5` | e.rs | |
";

    #[test]
    fn constants_table_parser_wants_one_table_of_backticked_rows() {
        assert!(
            parse_constants_table(CONSTANTS_TWO_TABLES)
                .unwrap_err()
                .contains("2 tables")
        );
        assert!(
            parse_constants_table(CONSTANTS_BAD_HEADER)
                .unwrap_err()
                .contains("header")
        );
        let unbackticked = parse_constants_table(CONSTANTS_UNBACKTICKED).unwrap_err();
        assert!(unbackticked.starts_with("line 5:"), "{unbackticked}");
        let three_cells = parse_constants_table(CONSTANTS_THREE_CELLS).unwrap_err();
        assert!(three_cells.contains("four cells"), "{three_cells}");
        assert!(
            parse_constants_table("# nothing\n")
                .unwrap_err()
                .contains("no ")
        );
    }

    #[test]
    fn pinned_by_checker_wants_the_sentinel_or_a_defined_identifier() {
        let rows = parse_constants_table(CONSTANTS_PINNED_BY).unwrap();
        assert_eq!(rows[1].pinned_by, "beta_is_pinned_here");
        let violations = check_pinned_by(&rows, "fn beta_is_pinned_here() {}\nfn some() {}");
        assert_eq!(violations.len(), 3, "{violations:?}");
        assert!(
            violations[0].starts_with("line 7: `GAMMA`"),
            "{}",
            violations[0]
        );
        assert!(
            violations[1].starts_with("line 8: `DELTA`"),
            "{}",
            violations[1]
        );
        assert!(
            violations[2].starts_with("line 9: `EPS`"),
            "{}",
            violations[2]
        );
    }

    #[test]
    fn constants_comparison_names_the_first_divergent_row() {
        let actual = parse_constants_table(VALID_SPEC).unwrap();
        let mut wrong_value = valid_expected();
        wrong_value[1].1 = "\"c\"".to_string();
        assert_eq!(
            check_constants(&actual, &wrong_value).unwrap_err(),
            "constants row 2: expected `BETA` = `\"c\"`, found `BETA` = `\"b\"`"
        );
        let mut renamed = valid_expected();
        renamed[0].0 = "ALPHA_2";
        assert!(
            check_constants(&actual, &renamed)
                .unwrap_err()
                .contains("row 1")
        );
        let mut missing = valid_expected();
        missing.push(("GAMMA", "3".to_string()));
        assert_eq!(
            check_constants(&actual, &missing).unwrap_err(),
            "constants table is missing `GAMMA`"
        );
        assert_eq!(
            check_constants(&actual, &valid_expected()[..1]).unwrap_err(),
            "constants table has an extra row `BETA`"
        );
    }

    const INDEX_BAD_LINES: &str = "\
## 21. Requirements index

- [MESH-DEST-001](#4-destination-naming) -- fine
- [MESH-DEST-002](#4-destination-naming) missing the dashes
* [MESH-DEST-003](#4-destination-naming) -- wrong bullet
- [MESH-DEST-004]() -- empty anchor
- [MESH-DEST-005](#a) --
";

    #[test]
    fn index_parser_rejects_every_deviation_from_the_entry_form() {
        let violations = index_entries(INDEX_BAD_LINES).unwrap_err();
        assert_eq!(violations.len(), 4, "{violations:?}");
        assert!(violations[0].starts_with("line 4:"));
        assert!(index_entries("# no index\n").unwrap_err()[0].contains("no "));
        assert_eq!(requirement_ids(INDEX_BAD_LINES).unwrap_err(), violations);
    }

    #[test]
    fn index_checkers_want_definition_order_and_heading_anchors() {
        let body = body(VALID_SPEC).unwrap();
        let definitions = definitions(&body).unwrap();
        let entries = index_entries(VALID_SPEC).unwrap();

        let mut swapped = index_entries(VALID_SPEC).unwrap();
        swapped.swap(0, 1);
        assert!(
            check_index_ids(&definitions, &swapped)
                .unwrap_err()
                .contains("MESH-DEST-002")
        );
        assert_eq!(
            check_index_ids(&definitions, &entries[..5]).unwrap_err(),
            "index is missing MESH-ENV-001"
        );
        assert!(
            check_index_ids(&definitions[..5], &entries)
                .unwrap_err()
                .contains("MESH-ENV-001 which has no definition")
        );

        let mut moved = index_entries(VALID_SPEC).unwrap();
        moved[5].anchor = "6-sixth".to_string();
        assert!(
            check_index_anchors(&definitions, &moved)
                .unwrap_err()
                .contains("indexed under #6-sixth but defined under #62-the-envelope")
        );
        assert!(check_index_anchors(&definitions[..5], &entries).is_err());
    }

    #[test]
    fn expected_constants_render_to_the_pinned_literal_sequence() {
        let rendered = expected_constants()
            .iter()
            .map(|(_, literal)| literal.as_str())
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(rendered, EXPECTED_LITERALS);
    }

    #[test]
    fn spec_has_one_h1_and_numbered_sections() {
        check_headings(SPEC).unwrap();
    }

    #[test]
    fn spec_opens_with_the_tagline_and_defines_a_session() {
        let mut lines = SPEC.lines();
        assert_eq!(lines.next(), Some(EXPECTED_H1));
        let opening = lines
            .find(|line| !line.trim().is_empty())
            .expect("a line after the H1");
        assert!(
            opening.starts_with(EXPECTED_TAGLINE),
            "the spec no longer opens with the tagline: {opening:?}"
        );
        assert!(
            opening.contains("A session is"),
            "the opening line no longer defines a session: {opening:?}"
        );
    }

    /// The reference implementation is named once, where section 1 says what it is;
    /// everywhere else the text speaks of sessions, nodes, requesters and responders.
    #[test]
    fn coyote_is_named_once_in_the_introduction() {
        // Assembled at runtime so the mesh source guard does not match this test's text.
        let name = ["Coy", "ote"].concat();
        let prose = strip_code(SPEC);
        let everywhere = prose.matches(name.as_str()).count();
        assert_eq!(
            everywhere, 1,
            "{name} is named {everywhere} times outside code"
        );
        let introduction = section(&prose, INTRODUCTION_HEADING).unwrap();
        assert_eq!(
            introduction.content.matches(name.as_str()).count(),
            1,
            "{name} is not named under {INTRODUCTION_HEADING:?}"
        );
    }

    /// The README's mesh section opens with the protocol's name and tagline as the spec
    /// states them, then says what the reference implementation is and what "mesh" names.
    /// Both are derived from the spec's pinned strings so the two documents cannot drift.
    #[test]
    fn readme_mesh_section_leads_with_the_scope_name_tagline_and_reference_sentence() {
        let lines: Vec<&str> = README.lines().collect();
        let headings: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| **line == "### Mesh")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(headings.len(), 1, "### Mesh headings at lines {headings:?}");
        let paragraph = lines[headings[0] + 1..]
            .iter()
            .skip_while(|line| line.trim().is_empty())
            .take_while(|line| !line.trim().is_empty())
            .copied()
            .collect::<Vec<&str>>()
            .join(" ");
        let protocol = EXPECTED_H1
            .strip_prefix("# ")
            .expect("the H1 carries a heading marker");
        assert!(
            paragraph.starts_with(protocol),
            "the mesh section does not open with {protocol:?}: {paragraph:?}"
        );
        let tagline = EXPECTED_TAGLINE
            .strip_prefix('"')
            .and_then(|tagline| tagline.strip_suffix('"'))
            .expect("the tagline is quoted");
        assert!(
            paragraph.contains(tagline),
            "the mesh section lacks the tagline: {paragraph:?}"
        );
        // Assembled at runtime so the mesh source guard does not match this test's text.
        let name = ["Coy", "ote"].concat();
        let reference = format!(
            "{name} is the reference implementation of SCOPE, and \"mesh\" is {name}'s name for its SCOPE feature."
        );
        assert!(
            paragraph.contains(&reference),
            "the mesh section lacks {reference:?}: {paragraph:?}"
        );
    }

    /// The README speaks the SCOPE wire vocabulary only: the announce magic and the
    /// backtick-wrapped destination prefixes from before the rename must not appear.
    /// The needles carry their backtick so file names like the log and config do not trip.
    #[test]
    fn readme_never_spells_the_pre_scope_wire_vocabulary() {
        // Assembled at runtime so the mesh source guard does not match this test's text.
        let needles = [
            ["COY", "M"].concat(),
            ["`coy", "ote.mesh"].concat(),
            ["`coy", "ote.peer/"].concat(),
            ["`coy", "ote.knock/"].concat(),
        ];
        let scan = |text: &str| -> Vec<String> {
            text.lines()
                .enumerate()
                .flat_map(|(index, line)| {
                    needles
                        .iter()
                        .filter(move |needle| line.contains(needle.as_str()))
                        .map(move |needle| format!("README.md:{}: spells {needle}", index + 1))
                })
                .collect()
        };
        let fixture = format!("fine line\nthe {} destination\n", needles[1]);
        let control = scan(&fixture);
        assert_eq!(
            control.len(),
            1,
            "the scan does not go red on a fixture: {control:?}"
        );
        let hits = scan(README);
        assert!(hits.is_empty(), "{}", hits.join("\n"));
    }

    /// The `mesh.fetch.max_bytes` row states the single-segment ceiling as the number a
    /// user can compare against their setting; a Rust constant name does not belong in a
    /// user-facing config table. The number is the code's, so the row cannot drift.
    #[test]
    fn readme_fetch_row_spells_the_single_segment_ceiling_as_a_number() {
        let row = README
            .lines()
            .find(|line| line.starts_with("| `mesh.fetch.max_bytes`"))
            .expect("the README has a mesh.fetch.max_bytes row");
        let clause = format!(
            "`min(max_bytes, {})` (about 1 MiB)",
            fetch::SINGLE_SEGMENT_FETCH_CEILING
        );
        assert!(
            row.contains(&clause),
            "the fetch row lacks {clause:?}: {row:?}"
        );
        assert!(
            !row.contains("SINGLE_SEGMENT_FETCH_CEILING"),
            "the fetch row names the constant instead of its value: {row:?}"
        );
    }

    #[test]
    fn the_areas_sentence_names_every_area_token() {
        let sentence = SPEC
            .lines()
            .find(|line| line.starts_with(AREAS_SENTENCE_OPEN))
            .unwrap_or_else(|| panic!("no line starts with {AREAS_SENTENCE_OPEN:?}"));
        let missing: Vec<&str> = AREAS
            .into_iter()
            .filter(|area| !sentence.contains(&format!("`{area}`")))
            .collect();
        assert!(missing.is_empty(), "the areas sentence omits {missing:?}");
        let unknown: Vec<&str> = split_spans(sentence)
            .into_iter()
            .map(|(_, span)| span)
            .filter(|span| {
                !span.is_empty()
                    && span.bytes().all(|b| b.is_ascii_uppercase())
                    && !AREAS.contains(span)
            })
            .collect();
        assert!(
            unknown.is_empty(),
            "the areas sentence names {unknown:?}, which are not areas"
        );
    }

    #[test]
    fn spec_has_the_bcp14_boilerplate() {
        check_boilerplate(SPEC).unwrap();
    }

    #[test]
    fn spec_requirement_ids_are_unique_and_indexed() {
        let definitions = definitions(&body(SPEC).unwrap()).unwrap();
        assert!(!definitions.is_empty(), "the spec defines no requirements");
        let entries = index_entries(SPEC).unwrap();
        check_index_ids(&definitions, &entries).unwrap();
    }

    #[test]
    fn spec_never_lowercases_a_normative_keyword_outside_code() {
        assert_eq!(check_no_lowercase_keywords(SPEC), Vec::<String>::new());
    }

    #[test]
    fn spec_every_normative_line_carries_a_requirement_id() {
        assert_eq!(
            check_normative_lines_carry_ids(&body(SPEC).unwrap()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn spec_index_anchors_name_existing_headings() {
        let definitions = definitions(&body(SPEC).unwrap()).unwrap();
        let entries = index_entries(SPEC).unwrap();
        check_index_anchors(&definitions, &entries).unwrap();
    }

    #[test]
    fn spec_positional_tables_share_the_header_and_end_with_a_catch_all_row() {
        let count = check_field_tables(SPEC).unwrap();
        assert!(count > 0, "the spec has no positional tables");
    }

    #[test]
    fn spec_cited_tests_exist() {
        let files = rust_source_files(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let sources = files
            .iter()
            .map(|(_, source)| source.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let cited = cited_identifiers(SPEC);
        assert!(!cited.is_empty(), "the spec cites no tests");
        assert_eq!(
            check_cited_identifiers(SPEC, &sources),
            Vec::<String>::new()
        );
        let tests = test_functions(&sources);
        assert!(!tests.is_empty(), "no test functions under src/");
        assert_eq!(
            check_cited_paths(SPEC, &files, &tests),
            Vec::<String>::new()
        );
    }

    /// Usage probe: the protocol spec's definition and table rows spell
    /// the SCOPE wire identifiers everywhere. Nothing is released, so there is no
    /// "formerly" form to allow: the pre-SCOPE announce magic (as text or as its hex
    /// bytes), the old `<app>.mesh` destination name in any spelling of application and
    /// aspect, the old `<app>.<kind>/1` LXMF type tags and the bare `<app>.` prefix may
    /// not appear on any line.
    #[test]
    fn usage_probe_spec_spells_no_pre_scope_wire_identifier() {
        // Assembled at runtime so the mesh source guard does not match this test's text.
        let old_app = ["coy", "ote"].concat();
        let needles = [
            ["COY", "M"].concat(),
            "43 4f 59 4d".to_string(),
            format!("{old_app}."),
            format!("{old_app}.mesh"),
            format!("{old_app}.peer/"),
            format!("{old_app}.knock/"),
            format!("application `{old_app}`"),
            "aspect `mesh.".to_string(),
            format!("DestinationName::new(\"{old_app}\""),
            format!("`{old_app}`, `.`, `mesh."),
        ];
        let hits: Vec<String> = SPEC
            .lines()
            .enumerate()
            .flat_map(|(index, line)| {
                needles
                    .iter()
                    .filter(|needle| line.contains(needle.as_str()))
                    .map(move |needle| {
                        format!("docs/mesh/PROTOCOL.md:{}: spells {needle:?}", index + 1)
                    })
            })
            .collect();
        assert!(hits.is_empty(), "{}", hits.join("\n"));

        // Positive control: the SCOPE forms are what the same rows spell now.
        for present in [
            "`\"SCOPE\"`",
            "53 43 4f 50 45",
            "scope.session.<instance_id>",
            "DestinationName::new(\"scope\", \"session.<instance_id>\")",
            "| `scope.session.<instance_id>` | destination name: application `scope`, aspect `session.<instance_id>` | `DestinationName::new(\"scope\", \"session.<instance_id>\")` | 4 |",
            "\"scope.knock/1\"",
            "\"scope.peer/1\"",
        ] {
            assert!(
                SPEC.contains(present),
                "the spec no longer spells {present:?}"
            );
        }
    }

    /// The sentences the withdrawn `.mesh fetch` spelling of the SYNC verb stood in.
    /// `.mesh fetch` is now the file verb, so the pin below matches these sentences and
    /// never the bare words; keep every needle a `.mesh fetch` sentence so the pin stays a
    /// strict narrowing of the old bare-token tripwire.
    const WITHDRAWN_SYNC_PHRASINGS: [&str; 5] = [
        "`.mesh fetch` runs a fetch",
        "on demand with `.mesh fetch`",
        "from `.mesh fetch`;",
        "only `.mesh fetch` runs",
        "`.mesh fetch` is refused",
    ];

    fn names_fetch_as_the_sync_verb(line: &str) -> bool {
        WITHDRAWN_SYNC_PHRASINGS
            .iter()
            .any(|phrase| line.contains(phrase))
    }

    /// The verb that pulls held messages is `.mesh sync`; the spec names it and never
    /// its withdrawn name. `.mesh fetch` is the file verb, so the needle is the sync
    /// sentences the old spelling stood in, not the bare words.
    #[test]
    fn usage_probe_spec_names_the_sync_verb_not_the_fetch_verb() {
        let hits: Vec<String> = SPEC
            .lines()
            .enumerate()
            .filter(|(_, line)| names_fetch_as_the_sync_verb(line))
            .map(|(index, _)| {
                format!(
                    "docs/mesh/PROTOCOL.md:{}: names .mesh fetch as the sync verb",
                    index + 1
                )
            })
            .collect();
        assert!(hits.is_empty(), "{}", hits.join("\n"));
        assert!(
            SPEC.contains("`.mesh sync`"),
            "the spec no longer names `.mesh sync`"
        );
    }

    /// Usage probe: the narrowed needle is still red-capable for every withdrawn SYNC
    /// sentence, is a strict narrowing of the old bare `.mesh fetch` pin, and spares the
    /// sentences the FILE verb will carry in the spec.
    #[test]
    fn usage_probe_sync_needle_trips_the_withdrawn_sentences_and_spares_the_file_verb() {
        for phrase in WITHDRAWN_SYNC_PHRASINGS {
            assert!(
                phrase.contains(".mesh fetch"),
                "{phrase:?} would flag a line the old bare-token pin did not"
            );
            let line = format!("Some prose, {phrase} in the middle of a sentence.");
            assert!(
                names_fetch_as_the_sync_verb(&line),
                "the withdrawn sentence {phrase:?} no longer trips the pin"
            );
        }
        // The live spec line the sync sentences came from: with the sync verb swapped
        // back to the old spelling it must trip, as written it must not.
        let live = SPEC
            .lines()
            .find(|line| line.contains("only `.mesh sync` runs a fetch"))
            .expect("the spec still carries the `.mesh sync` sentence");
        assert!(!names_fetch_as_the_sync_verb(live));
        assert!(names_fetch_as_the_sync_verb(
            &live.replace("`.mesh sync`", "`.mesh fetch`")
        ));
        for file_verb in [
            "`.mesh fetch <destination> <path> [--if-sha256 <hex>]` pulls one shared file into the staging inbox.",
            "The operator pulls it with `.mesh fetch`; the staged path, size and sha256 are printed, never the bytes.",
            "A `too_large` reply from `.mesh fetch` names the limit.",
        ] {
            assert!(
                !names_fetch_as_the_sync_verb(file_verb),
                "file-verb sentence {file_verb:?} trips the sync pin"
            );
        }
        // Known collision the spec author must write around: the file verb also obeys
        // the mesh gate, yet "`.mesh fetch` is refused" is one of the withdrawn sync
        // sentences, so that exact spelling trips the pin whichever verb it describes.
        assert!(names_fetch_as_the_sync_verb(
            "`.mesh fetch` is refused while the node is off."
        ));
    }

    /// Usage probe: section 5.1's layout line, every offset the field
    /// table spells, and both worked hex examples are derived from the live five-byte
    /// magic, not left at the old four-byte arithmetic. The examples must round-trip
    /// through the live codec to exactly the version and name the prose gives them.
    #[test]
    fn usage_probe_spec_announce_layout_widths_and_examples_follow_the_live_magic() {
        let magic_len = announce::ANNOUNCE_MAGIC.len();
        let header_len = magic_len + 2;
        let max_total = header_len + announce::MAX_DISPLAY_NAME_BYTES;
        let magic_text = std::str::from_utf8(&announce::ANNOUNCE_MAGIC).unwrap();

        let section = section(SPEC, "### 5.1 Application data").unwrap();
        // `section` runs to the next `## ` heading; stop at 5.2.
        let content = section.content.as_str();
        let end = content.find("\n### ").unwrap_or(content.len());
        let text = &content[..end];

        let layout = text
            .lines()
            .find(|line| line.starts_with("Layout: "))
            .expect("section 5.1 opens with a Layout line");
        assert_eq!(
            layout,
            format!(
                "Layout: `magic({magic_len}) || version(2) || display_name(0..={})`; total length {header_len} to {max_total} bytes. There is no length prefix.",
                announce::MAX_DISPLAY_NAME_BYTES
            )
        );

        let tables = tables(text);
        let [table] = tables.as_slice() else {
            panic!("section 5.1 holds one table");
        };
        let rows: Vec<Vec<&str>> = table.rows.iter().map(|row| cells(row)).collect();
        let field = |name: &str| -> Vec<&str> {
            rows.iter()
                .find(|cells| cells[0].starts_with(name))
                .unwrap_or_else(|| panic!("section 5.1 has no {name} row"))
                .clone()
        };
        let magic_row = field("magic");
        assert_eq!(magic_row[0], format!("magic, bytes 0..{magic_len}"));
        assert_eq!(magic_row[1], format!("{magic_len} bytes"));
        assert_eq!(magic_row[2], format!("`ANNOUNCE_MAGIC` = `{magic_text:?}`"));
        assert!(
            magic_row[3].contains(&format!("shorter than {header_len} bytes"))
                && magic_row[3]
                    .contains(&format!("first {magic_len} bytes are not `{magic_text:?}`")),
            "MESH-ANN-001 row: {}",
            magic_row[3]
        );
        assert_eq!(
            field("version")[0],
            format!("version, bytes {magic_len}..{header_len}")
        );
        assert_eq!(
            field("display_name")[0],
            format!("display_name, bytes {header_len}..end")
        );
        assert!(
            field("any other byte")[3].contains(&format!("from offset {header_len} to the end")),
            "MESH-ANN-005 row: {}",
            field("any other byte")[3]
        );

        // The worked examples decode with the live codec to what the prose says, and the
        // named one re-encodes to the very bytes printed.
        let examples = text
            .lines()
            .find(|line| line.starts_with("Examples ("))
            .expect("section 5.1 gives worked examples");
        let hex_spans: Vec<Vec<u8>> = examples
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|span| {
                span.split(' ')
                    .all(|byte| byte.len() == 2 && u8::from_str_radix(byte, 16).is_ok())
            })
            .map(|span| {
                span.split(' ')
                    .map(|byte| u8::from_str_radix(byte, 16).unwrap())
                    .collect()
            })
            .collect();
        let [named, bare] = hex_spans.as_slice() else {
            panic!("section 5.1 gives two hex examples, found {hex_spans:?}");
        };
        assert!(
            examples.contains("is version 1, display name `Alex`"),
            "{examples}"
        );
        assert!(
            examples.contains("is version `0x0102`, no display name"),
            "{examples}"
        );
        assert_eq!(named.len(), header_len + "Alex".len());
        assert_eq!(
            announce::AnnounceAppData::decode(named),
            Some(announce::AnnounceAppData {
                version: 1,
                display_name: Some("Alex".to_string()),
            }),
            "{named:02x?}"
        );
        assert_eq!(
            announce::AnnounceAppData {
                version: 1,
                display_name: Some("Alex".to_string()),
            }
            .encode()
            .unwrap(),
            *named,
            "the printed example is not what the live encoder emits"
        );
        assert_eq!(bare.len(), header_len);
        assert_eq!(
            announce::AnnounceAppData::decode(bare),
            Some(announce::AnnounceAppData {
                version: 0x0102,
                display_name: None,
            }),
            "{bare:02x?}"
        );
    }

    const THREAT_ROWS: &str = "\
| Attack | Scope | Where |
|---|---|---|
| Eavesdropping on a Link | Reticulum's | MESH-SEC-001 |
| Replay | in scope | MESH-SEC-003 |
| Insertion | in scope | MESH-SEC-001 |
| Deletion | out of scope | MESH-SEC-004 |
| Modification | in scope | MESH-SEC-006 |
| Man in the middle | in scope | MESH-SEC-008 |
| Denial of service | in scope | MESH-SEC-010 |
";

    fn security_section(heading: &str, content: &str) -> String {
        format!("## 15. Security\n\n{heading}\n\n{content}\n## 16. Invariants\n")
    }

    #[test]
    fn threat_model_checker_wants_a_row_per_attack_class() {
        check_threat_model(&security_section(THREAT_MODEL_HEADING, THREAT_ROWS)).unwrap();
        let without_replay: String = THREAT_ROWS
            .lines()
            .filter(|row| !row.starts_with("| Replay"))
            .map(|row| format!("{row}\n"))
            .collect();
        let error = check_threat_model(&security_section(THREAT_MODEL_HEADING, &without_replay))
            .unwrap_err();
        assert!(
            error.starts_with("line 5:") && error.contains("[\"replay\"]"),
            "{error}"
        );
        assert!(
            check_threat_model(&security_section(THREAT_MODEL_HEADING, ""))
                .unwrap_err()
                .contains("no table")
        );
        assert!(check_threat_model("## 15. Security\n").is_err());
    }

    #[test]
    fn out_of_scope_checker_wants_crypto_agility_placed_with_reticulum() {
        let bullet = |text: &str| security_section(OUT_OF_SCOPE_HEADING, &format!("- {text}\n"));
        check_crypto_agility_out_of_scope(&bullet(
            "Cryptography and cryptographic agility are Reticulum's.",
        ))
        .unwrap();
        assert!(
            check_crypto_agility_out_of_scope(&bullet("Cryptography is Reticulum's.")).is_err()
        );
        assert!(
            check_crypto_agility_out_of_scope(&bullet("Cryptographic agility is inherited."))
                .is_err()
        );
        assert!(check_crypto_agility_out_of_scope("## 15. Security\n").is_err());
    }

    const ISSUES: &str = "\
## Part A

### A1. First

- Status: Drafted, not yet filed.

### A10. Tenth

- Status: Drafted, not yet filed.

## Part B

### B1. Inherited

- Status: Drafted, not yet filed.
";

    #[test]
    fn leniency_checker_wants_why_upstream_a_removal_and_a_resolvable_draft() {
        let row = |id: &str, tail: &str| {
            format!("**[MESH-LEN-{id}]** A node MUST bend. Why: because. {tail}")
        };
        let ok = row(
            "001",
            "Upstream: docs/mesh/upstream-issues.md, draft A1 and draft A10. \
             Removal: when upstream bends back.",
        );
        assert_eq!(check_leniency_rows(&ok, ISSUES), Vec::<String>::new());
        assert_eq!(
            check_leniency_rows(
                &format!(
                    "{ok}\n{}",
                    row("002", "Upstream: none. Removal: None planned.")
                ),
                ISSUES
            ),
            vec!["line 2: the removal condition is \"none\"".to_string()]
        );
        let filed = ISSUES.replace(
            "### A10",
            "### A2. Second\n\n- Status: Filed as #7.\n\n### A10",
        );
        let problems = check_leniency_rows(
            &row(
                "001",
                "Removal: when draft A2 lands, draft A3 is filed and draft A10 too.",
            ),
            &filed,
        );
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[0].contains("no \"Upstream:\""), "{}", problems[0]);
        assert!(
            problems[1].contains("draft A2 does not read"),
            "{}",
            problems[1]
        );
        assert!(
            problems[2].contains("draft A3 has no heading"),
            "{}",
            problems[2]
        );
        assert_eq!(problems[3], "draft A1 is cited by no leniency row");
        let uncited = ISSUES.replace(
            "## Part B",
            "### A4. Fourth\n\n- Status: Drafted, not yet filed.\n\n## Part B",
        );
        assert_eq!(
            check_leniency_rows(&ok, &uncited),
            vec!["draft A4 is cited by no leniency row".to_string()]
        );
        assert_eq!(
            check_leniency_rows(
                &format!(
                    "{ok}\n{}",
                    row("002", "Upstream: draft B1. Removal: later.")
                ),
                ISSUES
            ),
            vec!["line 2: cites draft B1, which no leniency rests on".to_string()]
        );
        assert_eq!(
            check_leniency_rows(
                &format!("{ok}\n**[MESH-SEC-001]** Not a leniency; see draft B1."),
                ISSUES
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn spec_threat_model_names_every_attack_class() {
        check_threat_model(SPEC).unwrap();
    }

    #[test]
    fn spec_states_crypto_agility_is_inherited_and_out_of_scope() {
        check_crypto_agility_out_of_scope(SPEC).unwrap();
    }

    #[test]
    fn spec_leniency_rows_carry_why_upstream_and_removal() {
        assert!(
            prepare(SPEC)
                .lines()
                .any(|line| line.contains(LENIENCY_OPEN)),
            "the spec has no leniency rows"
        );
        assert_eq!(
            check_leniency_rows(SPEC, UPSTREAM_ISSUES),
            Vec::<String>::new()
        );
    }

    #[test]
    fn spec_constants_table_matches_the_code() {
        let actual = parse_constants_table(SPEC).unwrap();
        check_constants(&actual, &expected_constants()).unwrap();
    }

    /// The cells of every body row of the one table under `CODE_POINT_HEADING`.
    fn registry_rows() -> Vec<Vec<String>> {
        let section = section(SPEC, CODE_POINT_HEADING).unwrap();
        let tables = tables(&section.content);
        let [table] = tables.as_slice() else {
            panic!(
                "{CODE_POINT_HEADING:?} holds {} tables, expected exactly one",
                tables.len()
            );
        };
        table
            .rows
            .iter()
            .skip(2)
            .map(|row| cells(row).into_iter().map(str::to_string).collect())
            .collect()
    }

    #[test]
    fn registry_on_disk_schema_version_rows_match_the_live_constants() {
        let live = [
            ("TRUST_FILE_VERSION", trust::TRUST_FILE_VERSION),
            ("KNOCK_RECORD_VERSION", knocks::KNOCK_RECORD_VERSION),
            ("PENDING_RECORD_VERSION", pending::PENDING_RECORD_VERSION),
            ("INBOUND_RECORD_VERSION", pending::INBOUND_RECORD_VERSION),
            (
                "PREDECESSOR_RECORD_VERSION",
                identity::PREDECESSOR_RECORD_VERSION,
            ),
            ("PEER_TABLE_VERSION", peers::PEER_TABLE_VERSION),
            (
                "PROPAGATION_STORE_VERSION",
                propagation_fetch::PROPAGATION_STORE_VERSION,
            ),
            ("SHARES_FILE_VERSION", shares::SHARES_FILE_VERSION),
            ("GRANT_RECORD_VERSION", grants::GRANT_RECORD_VERSION),
            (
                "ENVOY_SESSION_VERSION",
                envoy_sessions::ENVOY_SESSION_VERSION,
            ),
            (
                "ENVOY_SESSION_INDEX_VERSION",
                envoy_sessions::ENVOY_SESSION_INDEX_VERSION,
            ),
        ];
        let registry = registry_rows();
        let rows: Vec<(u64, Vec<&str>)> = registry
            .iter()
            .filter(|cells| cells.get(1).map(String::as_str) == Some(ON_DISK_SCHEMA_KIND))
            .map(|cells| {
                let [value, _, names, _] = cells.as_slice() else {
                    panic!("registry row {cells:?} does not have four cells");
                };
                let value = backticked(value)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_else(|| panic!("registry row {cells:?} has no backticked version"));
                let names = names
                    .split(", ")
                    .map(|name| {
                        backticked(name)
                            .unwrap_or_else(|| panic!("registry row {cells:?} names {name:?}"))
                    })
                    .collect();
                (value, names)
            })
            .collect();
        let listed: Vec<&str> = rows
            .iter()
            .flat_map(|(_, names)| names.iter().copied())
            .collect();
        let expected: Vec<&str> = live.iter().map(|(name, _)| *name).collect();
        assert_eq!(listed, expected);
        for (value, names) in &rows {
            let [name] = names.as_slice() else {
                panic!("registry row {names:?} must list one constant");
            };
            let (_, want) = live.iter().find(|(live, _)| live == name).unwrap();
            assert_eq!(value, want, "`{name}`");
        }
    }

    /// The envoy memory file name for one (identity, thread) is fixed by this vector: a
    /// change to the digest input would silently orphan every record on disk.
    #[test]
    fn envoy_session_key_is_the_truncated_sha256_of_identity_nul_thread() {
        let identity = "0123456789abcdef0123456789abcdef";
        let key = envoy_sessions::session_key(identity, "thread-one").unwrap();

        assert_eq!(key, "8ebad0226f3717c3cebe929e4b7c49ca");
        assert_eq!(key.len(), 32);
        assert!(
            key.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{key}"
        );
        assert_ne!(
            key,
            envoy_sessions::session_key(identity, "thread-two").unwrap()
        );
        assert_ne!(
            key,
            envoy_sessions::session_key("fedcba9876543210fedcba9876543210", "thread-one").unwrap()
        );
    }

    /// The registry rows for the code points the code spells as constants or wire names
    /// list exactly those values, so a value renamed or added in code shows up here.
    #[test]
    fn registry_rows_name_the_live_code_points() {
        let paths = [
            ("KNOCK_PATH", r3::KNOCK_PATH),
            ("STATUS_PATH", r3::STATUS_PATH),
            ("MESSAGE_PATH", r3::MESSAGE_PATH),
            ("LIST_PATH", r3::LIST_PATH),
            ("FETCH_PATH", r3::FETCH_PATH),
            ("ACCESS_PATH", r3::ACCESS_PATH),
        ];
        assert_eq!(paths.map(|(_, path)| path), r3::KNOWN_PATHS);
        let spans = |words: &[&str]| {
            words
                .iter()
                .map(|word| format!("`{word}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let dispositions = [
            message::Disposition::Answered,
            message::Disposition::Escalated,
            message::Disposition::Refused,
            message::Disposition::BudgetExhausted,
        ]
        .map(message::Disposition::wire_name);
        // String literals at the `status_reply` call sites of src/mesh/fetch.rs.
        let fetch_statuses = [
            "ok",
            "not_modified",
            "not_shared",
            "invalid_path",
            "too_large",
        ];
        let access_statuses = [
            access::AccessOutcome::Pending.status(),
            access::AccessOutcome::Granted { expires: 0.0 }.status(),
            access::AccessOutcome::Refused(access::AccessRefusal::Duplicate).status(),
        ];
        let access_reasons = [
            access::AccessRefusal::Duplicate,
            access::AccessRefusal::TooManyPending,
        ]
        .map(access::AccessRefusal::wire_name);
        let decisions = [
            events::AccessDecision::Granted,
            events::AccessDecision::Denied,
        ]
        .map(events::AccessDecision::wire_name);
        let rules: Vec<&str> = wire_path::RULES.iter().map(|(name, _)| *name).collect();
        let mut expected: Vec<(String, &str, Option<String>)> = paths
            .iter()
            .map(|(name, path)| {
                (
                    format!("`{path:?}`"),
                    "request path",
                    Some(format!("`{name}`")),
                )
            })
            .collect();
        expected.extend([
            (spans(&["text", "data", "file"]), "`type` value", None),
            (spans(&dispositions), "`disposition` value", None),
            (spans(&fetch_statuses), "fetch `status` value", None),
            (spans(&access_statuses), "access `status` value", None),
            (spans(&access_reasons), "access `reason` value", None),
            (spans(&decisions), "decision `status` value", None),
            (spans(&rules), "`rule` value", None),
            ("`\"fetch\"`".to_string(), "capability", None),
            (
                format!("`{:?}`", knock::KNOCK_TYPE),
                "LXMF type tag",
                Some("`KNOCK_TYPE`".to_string()),
            ),
            (
                format!("`{:?}`", message::PEER_MESSAGE_TYPE),
                "LXMF type tag",
                Some("`PEER_MESSAGE_TYPE`".to_string()),
            ),
            (
                format!("`{:?}`", access::ACCESS_TYPE),
                "LXMF type tag",
                Some("`ACCESS_TYPE`".to_string()),
            ),
        ]);
        let rows = registry_rows();
        let missing: Vec<String> = expected
            .iter()
            .filter(|(first, kind, value)| {
                !rows.iter().any(|cells| {
                    let [first_cell, kind_cell, value_cell, _] = cells.as_slice() else {
                        panic!("registry row {cells:?} does not have four cells");
                    };
                    first_cell == first
                        && kind_cell.contains(kind)
                        && value.as_ref().is_none_or(|value| value_cell == value)
                })
            })
            .map(|(first, kind, value)| {
                format!("| {first} | {kind} | {} |", value.as_deref().unwrap_or(""))
            })
            .collect();
        assert!(
            missing.is_empty(),
            "the registry lacks rows for:\n{}",
            missing.join("\n")
        );
    }

    /// The `/fetch` worked example spells the protocol's file bound in its `too_large`
    /// line, and the sentence under it names the limit the reference answers while the
    /// single-segment ceiling stands, so dropping the ceiling or moving it moves the note.
    #[test]
    fn fetch_example_notes_the_live_ceiling_under_the_protocol_bound() {
        let lines: Vec<&str> = SPEC.lines().collect();
        let example = lines
            .iter()
            .position(|line| line.contains(r#""status": "too_large", "limit": "#))
            .expect("the /fetch example has a too_large line");
        assert!(
            lines[example].ends_with(&format!(r#""limit": {MAX_FETCH_FILE_BYTES} }}"#)),
            "{:?}",
            lines[example]
        );
        assert_eq!(
            lines[example + 1],
            "```",
            "the too_large line closes the example"
        );
        let note = lines[example + 3];
        assert_eq!(lines[example + 2], "", "{note:?}");
        for needle in [
            "`SINGLE_SEGMENT_FETCH_CEILING`",
            "MESH-LEN-007",
            &format!("`limit: {}`", fetch::SINGLE_SEGMENT_FETCH_CEILING),
        ] {
            assert!(note.contains(needle), "{note:?} lacks {needle:?}");
        }
        assert!(
            !note.contains(&MAX_FETCH_FILE_BYTES.to_string()),
            "the note names the ceiling, not the bound the example already spells: {note:?}"
        );
    }

    /// MESH-SEC-019 states why `/list` entries travel unfenced where fetched text does
    /// not: the wire-path grammar of section 10.13 admits no character that could forge
    /// a fence marker. Dropping the clause would leave the asymmetry unexplained, and
    /// widening the grammar would make the clause false, so the paragraph is pinned to
    /// both the clause and the section it leans on.
    #[test]
    fn sec_019_states_why_list_entries_are_returned_unfenced() {
        let definition = SPEC
            .lines()
            .find(|line| line.starts_with("**[MESH-SEC-019]**"))
            .expect("MESH-SEC-019 is defined");
        for clause in [
            "`/list` entries are returned unfenced",
            "the wire-path grammar admitting no line terminator, control or invisible character",
            "(section 10.13;",
        ] {
            assert!(
                definition.contains(clause),
                "{definition:?} lacks {clause:?}"
            );
        }
        assert!(
            SPEC.contains("### 10.13 Wire paths"),
            "the clause names a section this document no longer has"
        );
    }

    /// MESH-LEN-007 binds the reference on the pinned transport, not every responder:
    /// the single-segment ceiling is a leniency the reference applies while that
    /// transport's multi-segment defect stands, and the protocol's own serving limit
    /// stays MESH-FETCH-026. Widening the subject back to "a responder MUST" would turn
    /// the workaround into a protocol limit, so both the subject and the disclaimer are
    /// pinned.
    #[test]
    fn len_007_binds_the_reference_on_the_pinned_transport_not_the_protocol() {
        let definition = SPEC
            .lines()
            .find(|line| line.starts_with("**[MESH-LEN-007]**"))
            .expect("MESH-LEN-007 is defined");
        for clause in [
            "a responder on the pinned rns-transport MUST cap its serving limit at `SINGLE_SEGMENT_FETCH_CEILING`",
            "not a limit of this protocol, whose serving limit is MESH-FETCH-026 (MESH-FETCH-027",
        ] {
            assert!(
                definition.contains(clause),
                "{definition:?} lacks {clause:?}"
            );
        }
    }

    /// Every field a store file reads under a `#[serde(default)]` is named on the
    /// MESH-CODE-005 line, by its on-disk key where the attribute renames it. The files
    /// are those of `ON_DISK_STRUCTS` in `schema.rs`: the stores plus `message.rs`, whose
    /// `PeerMessage` and `Part` ride inside a pending record; none of them holds a
    /// wire-only struct with a default. The line also states the one exception to the
    /// bump rule, an additive default that reads an older record as what it was, and
    /// names MESH-SCHEMA-003 as its instance, so the two paragraphs cannot drift apart.
    #[test]
    fn code_005_names_every_on_disk_serde_default_field() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut fields = BTreeSet::new();
        for (file, _) in schema::ON_DISK_STRUCTS {
            let path = format!("src/mesh/{file}");
            let source = std::fs::read_to_string(root.join(&path)).unwrap();
            let lines: Vec<&str> = source.lines().map(str::trim).collect();
            let mut index = 0;
            while index < lines.len() {
                let start = index;
                index += 1;
                let Some(opened) = lines[start].strip_prefix("#[serde(") else {
                    continue;
                };
                let mut attribute = opened.to_string();
                while !attribute.ends_with(")]") {
                    let Some(next) = lines.get(index) else {
                        panic!("unterminated serde attribute at {path}:{}", start + 1);
                    };
                    attribute.push(' ');
                    attribute.push_str(next);
                    index += 1;
                }
                let attribute = attribute.strip_suffix(")]").unwrap();
                let arguments: Vec<&str> = attribute.split(',').map(str::trim).collect();
                if !arguments
                    .iter()
                    .any(|argument| *argument == "default" || argument.starts_with("default ="))
                {
                    continue;
                }
                let field = lines[index..]
                    .iter()
                    .find(|next| !next.starts_with("#[") && !next.starts_with("///"))
                    .unwrap_or_else(|| panic!("{path}:{}: no field below", start + 1));
                let declaration = field
                    .trim_start_matches("pub(crate) ")
                    .trim_start_matches("pub ");
                assert!(
                    !declaration.starts_with("struct ") && !declaration.starts_with("enum "),
                    "{path}:{}: struct-level #[serde(default)]; its fields cannot be enumerated here",
                    start + 1
                );
                let declared = declaration.split(':').next().unwrap().trim();
                let renamed = arguments
                    .iter()
                    .find_map(|argument| argument.strip_prefix("rename = \""))
                    .and_then(|rest| rest.split('"').next());
                fields.insert(renamed.unwrap_or(declared).to_string());
            }
        }
        assert!(fields.len() > 10, "{fields:?}");
        let definition = SPEC
            .lines()
            .find(|line| line.starts_with("**[MESH-CODE-005]**"))
            .expect("MESH-CODE-005 is defined");
        let missing: Vec<&String> = fields
            .iter()
            .filter(|field| !definition.contains(&format!("`{field}`")))
            .collect();
        assert_eq!(missing, Vec::<&String>::new());

        for clause in [
            "a field removed, renamed or retyped, or a field added without a default, MUST bump the store's constant",
            "a field added with a `#[serde(default)]` whose default is the only value an older record could have held MAY land inside the version (MESH-SCHEMA-003 is the instance)",
        ] {
            assert!(
                definition.contains(clause),
                "{definition:?} lacks {clause:?}"
            );
        }
        assert!(
            !definition.contains("a field added included"),
            "the unconditional bump rule is back: {definition:?}"
        );
        let instance = SPEC
            .lines()
            .find(|line| line.starts_with("**[MESH-SCHEMA-003]**"))
            .expect("MESH-SCHEMA-003 is defined");
        let additive = ["kind", "paths", "reason"];
        for field in additive {
            assert!(
                fields.contains(field),
                "pending.rs no longer defaults `{field}`"
            );
            assert!(instance.contains(&format!("`{field}`")), "{instance:?}");
        }
        assert!(
            instance.contains("`INBOUND_RECORD_VERSION` = `2`")
                && instance.contains("without `kind`, `paths` or `reason`"),
            "{instance:?}"
        );
        assert!(
            definition
                .contains("on an inbound record `kind`, `paths` and `reason` (MESH-SCHEMA-003)"),
            "{definition:?}"
        );
    }

    #[test]
    fn env_042_lists_exactly_the_known_paths_in_order() {
        let line = SPEC
            .lines()
            .find(|line| line.contains("**[MESH-ENV-042]**"))
            .expect("MESH-ENV-042 is defined");
        let listed: Vec<&str> = split_spans(line)
            .into_iter()
            .map(|(_, span)| span)
            .filter(|span| span.starts_with('/'))
            .collect();
        assert_eq!(listed, r3::KNOWN_PATHS);
    }

    #[test]
    fn sec_024_pins_the_envoy_memory_contract() {
        let line = SPEC
            .lines()
            .find(|line| line.starts_with("**[MESH-SEC-024]**"))
            .expect("MESH-SEC-024 is defined");
        let bounds: Vec<String> = expected_constants()
            .into_iter()
            .filter(|(name, _)| name.starts_with("DEFAULT_ENVOY_MEMORY_"))
            .map(|(name, value)| format!("`{name}` = `{value}`"))
            .collect();
        assert_eq!(bounds.len(), 5, "{bounds:?}");
        let clauses = [
            "keyed by the proved sending identity",
            "`session_key`",
            "MUST never be loaded",
            "MUST be bounded in count, size and age",
            "MAY rely on retained state only within a thread",
            "MUST NOT assume",
            "SHOULD remain answerable",
            "retains none",
            "never the brief, the per-run peer section the node composes (the instance, kind and route of the message), a system prompt or the owner's own transcript",
            "the peer-chosen name and message id travelling inside the fenced turn as data (MESH-SEC-009)",
            "hours since it was last written",
            "nor is such a record ever one of the owner's own sessions",
            "goes with its trust",
        ];
        for clause in clauses
            .iter()
            .copied()
            .chain(bounds.iter().map(String::as_str))
        {
            assert!(line.contains(clause), "{line:?} lacks {clause:?}");
        }
    }

    /// Every clause of the sender-facing contract the requirement was written to carry:
    /// not keyed by destination or thread alone, never loaded for another identity,
    /// bounded in count, size and age, the owner's transcript excluded, a receiver
    /// retaining none conformant, a root or evicted thread starting clean, and the
    /// revocation verbs that take the state with the trust.
    #[test]
    fn usage_probe_sec_024_carries_every_clause_of_the_senders_contract() {
        let line = SPEC
            .lines()
            .find(|line| line.starts_with("**[MESH-SEC-024]**"))
            .expect("MESH-SEC-024 is defined");
        for clause in [
            "MAY retain envoy conversation state",
            "per (sending identity, thread)",
            "never by the destination",
            "nor by the thread alone",
            "on behalf of another identity",
            "names another identity being refused",
            "bounded in count, size and age",
            "MUST NOT assume retained state",
            "retains none being conformant",
            "a root message, or one naming a thread the receiver no longer holds, starting clean",
            "never the brief, the per-run peer section the node composes (the instance, kind and route of the message), a system prompt or the owner's own transcript",
            "the peer-chosen name and message id travelling inside the fenced turn as data (MESH-SEC-009)",
            "`.mesh untrust --identity` and `.mesh block` forgetting every thread",
            "untrust of one destination leaves them in place",
            "charged to the sending identity's token budget",
        ] {
            assert!(line.contains(clause), "MESH-SEC-024 lacks {clause:?}");
        }
        let index_row = SPEC
            .lines()
            .find(|line| line.starts_with("- [MESH-SEC-024](#154-trust-boundary)"))
            .expect("MESH-SEC-024 has a section 21 index row");
        assert!(index_row.contains("envoy memory"), "{index_row}");
        assert!(
            SPEC.contains("| MESH-SEC-024 | `a_second_message_in_the_thread_is_driven_with_the_first_exchange`"),
            "MESH-SEC-024 has a section 20 ENFORCED_BY row"
        );
    }

    #[test]
    fn wire_path_rule_table_names_the_rules_in_order() {
        const HEADER: &str = "| Rule | Condition | Receiver action |";
        let section = section(SPEC, "### 10.13 Wire paths").unwrap();
        let tables = tables(&section.content);
        let table = tables
            .iter()
            .find(|table| table.rows[0].trim() == HEADER)
            .unwrap_or_else(|| panic!("no {HEADER:?} table under 10.13"));
        let listed: Vec<&str> = table.rows[2..]
            .iter()
            .map(|row| {
                let first = cells(row)[0];
                backticked(first)
                    .unwrap_or_else(|| panic!("rule cell {first:?} is not a code span"))
            })
            .collect();
        let live: Vec<&str> = wire_path::RULES.iter().map(|(name, _)| *name).collect();
        assert_eq!(listed, live);
    }

    fn follows(line: &str, needle: &str, accept: impl Fn(&[u8]) -> bool) -> bool {
        line.match_indices(needle)
            .any(|(at, _)| accept(&line.as_bytes()[at + needle.len()..]))
    }

    /// `follows`, with the needle starting a word, so a `render_spec(s)` call is no citation.
    fn follows_word(line: &str, needle: &str, accept: impl Fn(&[u8]) -> bool) -> bool {
        let bytes = line.as_bytes();
        line.match_indices(needle).any(|(at, _)| {
            is_word_boundary(at.checked_sub(1).and_then(|before| bytes.get(before)))
                && accept(&bytes[at + needle.len()..])
        })
    }

    /// A line that cites a planning artefact (a task id, a lettered review criterion, a
    /// review round, a ruling, a commit, a letter-dash-number plan label in a comment)
    /// instead of the behaviour it tests, in any letter case. The needles are assembled at
    /// runtime so this file does not spell them. A task id quoted inside a code span or a
    /// string literal is fixture input, not a citation, and a line that names the
    /// `ILLUSTRATIVE_IDS` list is the fixture that keeps one such id on purpose.
    fn cites_a_plan_label(line: &str) -> bool {
        if line.contains("ILLUSTRATIVE_IDS") {
            return false;
        }
        let line = line.to_ascii_lowercase();
        let task = ["task", "-"].concat();
        let lettered = [
            "criterion".to_string(),
            "acceptance".to_string(),
            "spec".to_string(),
            ["usage", " probe"].concat(),
            "amendment".to_string(),
        ];
        let lettered_numbered = [
            ["b", "-"].concat(),
            ["g", "-"].concat(),
            ["t", "-"].concat(),
        ];
        let rounds = [
            ["probe,", " round "].concat(),
            ["review", " round "].concat(),
            ["criteria", " "].concat(),
        ];
        let ruling = ["ruling", " "].concat();
        let shorthand = "(r";
        let round_dash = ["round", "-"].concat();
        let plain = [
            ["plan", " criterion"].concat(),
            ["plan", " ruling"].concat(),
            ["user", " ruling"].concat(),
        ];
        let commit = [" fix", " commit"];
        let letter_in_parens = |rest: &[u8]| {
            rest.first() == Some(&b'(')
                && rest.get(1).is_some_and(u8::is_ascii_lowercase)
                && rest.get(2) == Some(&b')')
        };
        let after_optional_round = |rest: &[u8]| {
            let rest = match rest {
                [b' ', b'r', digits @ ..] if digits.first().is_some_and(u8::is_ascii_digit) => {
                    &digits[digits.iter().take_while(|b| b.is_ascii_digit()).count()..]
                }
                rest => rest,
            };
            letter_in_parens(rest.strip_prefix(b" ").unwrap_or(rest))
        };
        let after_a_short_hash = |before: &[u8]| {
            let hex = before
                .iter()
                .rev()
                .take_while(|b| b.is_ascii_hexdigit())
                .count();
            let token = &before[before.len() - hex..];
            (7..=40).contains(&hex)
                && token.iter().any(u8::is_ascii_digit)
                && before[..before.len() - hex]
                    .last()
                    .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_')
        };
        let round_shorthand = |rest: &[u8]| {
            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            let rest = &rest[digits..];
            digits > 0
                && (rest.first() == Some(&b')')
                    || rest.strip_prefix(b" ").is_some_and(letter_in_parens))
        };
        let ruling_shorthand_at_start = |text: &str| {
            let bytes = text.as_bytes();
            bytes.first() == Some(&b'r') && bytes.get(1).is_some_and(u8::is_ascii_digit) && {
                let digits = bytes[1..].iter().take_while(|b| b.is_ascii_digit()).count();
                bytes.get(1 + digits) == Some(&b':')
            }
        };
        let comment_text = line
            .trim_start()
            .strip_prefix("//")
            .map(|text| text.trim_start_matches('/').trim_start());
        let starts_with_digit = |rest: &[u8]| rest.first().is_some_and(u8::is_ascii_digit);
        follows(&without_quoted(&line), &task, starts_with_digit)
            || lettered
                .iter()
                .any(|needle| follows_word(&line, needle, after_optional_round))
            || comment_text.is_some_and(|text| {
                lettered_numbered
                    .iter()
                    .any(|needle| follows_word(&without_quoted(text), needle, starts_with_digit))
            })
            || comment_text.is_some_and(|text| letter_in_parens(text.as_bytes()))
            || rounds
                .iter()
                .chain(std::iter::once(&ruling))
                .any(|needle| follows(&line, needle, starts_with_digit))
            || plain.iter().any(|needle| line.contains(needle.as_str()))
            || comment_text.is_some_and(|text| follows(text, shorthand, round_shorthand))
            || comment_text.is_some_and(ruling_shorthand_at_start)
            || follows(&line, &round_dash, starts_with_digit)
            || commit.iter().any(|needle| {
                line.match_indices(needle)
                    .any(|(at, _)| after_a_short_hash(&line.as_bytes()[..at]))
            })
    }

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

    #[test]
    fn plan_label_scanner_trips_each_shape_and_spares_prose() {
        for hit in [
            ["// see TASK", "-118 for why"].concat(),
            ["// see task", "-118 for why"].concat(),
            ["/// Usage probe, criterion", " (b): the root"].concat(),
            ["/// Criterion", " (b): the root"].concat(),
            ["/// acceptance", " (c): the peer"].concat(),
            ["// the spec", "(a) says"].concat(),
            ["/// usage", " probe r2 (d): bare"].concat(),
            ["/// amendment", " (b) covers"].concat(),
            "/// (x) the second reply".to_string(),
            "// (a) the second reply".to_string(),
            ["// ruling", " 3 settled it"].concat(),
            ["// per the user", " ruling"].concat(),
            ["// ---- review", " round 2 ----"].concat(),
            ["// ---- usage probe,", " round 2 ----"].concat(),
            ["/// Plan", " criterion (f): at every cap"].concat(),
            ["// the plan", " ruling was"].concat(),
            ["/// usage", " probe r10 (b): the tenth"].concat(),
            ["// settled (R", "3): the reply"].concat(),
            ["// ---- usage probe (r", "4) ----"].concat(),
            ["// settled (r", "2 (b)): the reply"].concat(),
            ["// criteria", " 3+4 hold"].concat(),
            ["// the 3a3d1d1", " fix narrowed it"].concat(),
            [
                "// after 0123456789abcdef0123456789abcdef01234567",
                " commit",
            ]
            .concat(),
            ["// the preview prints first (B", "-40)"].concat(),
            ["/// g", "-12 covers the relay"].concat(),
            ["// per t", "-3 the cap holds"].concat(),
            ["// see B", "-7"].concat(),
            ["// R", "8: judged before anything is printed"].concat(),
            ["// r", "3: the peer refuses the reference"].concat(),
            ["// The round", "-3 README claimed"].concat(),
        ] {
            assert!(cites_a_plan_label(&hit), "{hit:?}");
        }
        for miss in [
            "// the TASK list is drained".to_string(),
            "// a criterion (the first) applies".to_string(),
            "// the rule (a) above".to_string(),
            "// a spec (RFC) is cited".to_string(),
            "// reviewed in round 2 of the audit".to_string(),
            "// the ruling stands".to_string(),
            "// a planned criterion is unmet".to_string(),
            "// B6 and G8 are hex here".to_string(),
            "// the R3 transport frames it".to_string(),
            "// the R3 transport (r3 for short) frames it".to_string(),
            "// r3 frames it".to_string(),
            "// round trip".to_string(),
            "// the transport (r3, lowercase on the wire) frames it".to_string(),
            "// Err(R3Error::Shutdown) ends it".to_string(),
            "assert_eq!(r1.len(), 2);".to_string(),
            "// 9f1c3b0e6d8a4f2b9c7e1a5d3b8f6c04 is a wire id".to_string(),
            "// a fix for the relay".to_string(),
            "// the cache fix narrowed it".to_string(),
            "// a defaced fix is reverted".to_string(),
            "// the criteria differ".to_string(),
            "// render_spec(s) builds the table".to_string(),
            ["// the rule quotes `// TASK", "-002` as its example"].concat(),
            ["let id = \"TASK", "-002\";"].concat(),
            ["const ILLUSTRATIVE_IDS: [&str; 1] = [\"TASK", "-002\"];"].concat(),
            "// the a1b2c3 hash names it".to_string(),
            "// sub-1 is the first child".to_string(),
            "// the b-tree is balanced".to_string(),
            "// t-shirt sizing".to_string(),
            ["let label = \"B", "-40\";"].concat(),
            ["  models: [{name: g", "-1}]"].concat(),
            ["/// Node A asks in thread `t", "-1`; node B answers"].concat(),
        ] {
            assert!(!cites_a_plan_label(&miss), "{miss:?}");
        }
    }

    #[test]
    fn source_comments_cite_behaviour_not_plan_labels() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<(String, String)> = rust_source_files(root).unwrap();
        files.extend(rust_files_under(root, "tests").unwrap());
        files.push(("docs/mesh/PROTOCOL.md".to_string(), SPEC.to_string()));
        let hits: Vec<String> = files
            .iter()
            .flat_map(|(path, source)| {
                source
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| cites_a_plan_label(line))
                    .map(move |(index, line)| format!("{path}:{}: {}", index + 1, line.trim()))
            })
            .collect();
        assert!(
            hits.is_empty(),
            "code comments describe behaviour; cite a MESH- id or the behaviour instead of a \
             plan label:\n{}",
            hits.join("\n")
        );
    }

    /// `source` line for line, so line numbers hold, with every `#[cfg(test)]`-attributed
    /// inline module and every plain `//` comment line blanked. Doc comments (`///`,
    /// `//!`) stay: they render as rustdoc and as `clap` help. A top-level module closes
    /// at the first `}` on a line of its own, as rustfmt lays it out.
    fn lines_outside_test_modules(source: &str) -> Vec<&str> {
        let mut lines: Vec<&str> = source.lines().collect();
        let mut at = 0;
        while at < lines.len() {
            let opens_test_module = lines[at] == "#[cfg(test)]"
                && lines.get(at + 1).is_some_and(|opener| {
                    opener.ends_with('{')
                        && (opener.starts_with("mod ") || opener.starts_with("pub(crate) mod "))
                });
            if opens_test_module {
                while at < lines.len() && lines[at] != "}" {
                    lines[at] = "";
                    at += 1;
                }
            } else if is_a_plain_comment(lines[at]) {
                lines[at] = "";
            }
            at += 1;
        }
        lines
    }

    fn is_a_plain_comment(line: &str) -> bool {
        let rest = line.trim_start().strip_prefix("//");
        rest.is_some_and(|rest| !rest.starts_with('/') && !rest.starts_with('!'))
    }

    /// The workspace config directory's name reaches the code through
    /// `paths::workspace_config_dir_name` and `paths::workspace_config_dirs`, so an
    /// override renames it everywhere at once, help and display strings included. Only
    /// `paths.rs` and the constant's own definition may spell it; test code may, since a
    /// fixture lays out a directory by its default name. A file whose module is declared
    /// under `#[cfg(test)]` is test code wholesale. A plain `//` comment may say what it
    /// likes; a doc comment is held to the helper because it renders, as rustdoc or as
    /// `clap` help. The name counts when it stands as a word: opened by a quote, a
    /// backtick, a space, a slash, a brace or a parenthesis and not run on into a longer
    /// identifier, so `.coyote_password` and `.coyote-case-probe` are other names.
    #[test]
    fn only_the_paths_helper_spells_the_workspace_config_dir_name() {
        // Assembled at runtime so the scan does not match this test's own text.
        let constant = ["WORKSPACE_", "COYOTE_DIR_NAME"].concat();
        let name = [".coy", "ote"].concat();
        let spells_the_name = |line: &str| {
            line.match_indices(&name).any(|(at, _)| {
                // The name is a whole word: whatever precedes it is not part of an
                // identifier, and neither is whatever follows, so `.coyote_password` and
                // `.coyote-case-probe` are other names while `\.coyote\` and `=.coyote` are
                // hits. A string escape such as `\n` or `\t` ends in a letter but is not a
                // word either.
                let part_of_a_word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
                let before = &line[..at];
                let after_an_escape = before.ends_with("\\n") || before.ends_with("\\t");
                (after_an_escape || !before.chars().next_back().is_some_and(part_of_a_word))
                    && !line[at + name.len()..]
                        .chars()
                        .next()
                        .is_some_and(part_of_a_word)
            })
        };
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = rust_source_files(root).unwrap();
        let test_modules: HashSet<String> = files
            .iter()
            .flat_map(|(_, source)| {
                source
                    .lines()
                    .zip(source.lines().skip(1))
                    .filter_map(|(attribute, item)| {
                        (attribute.trim() == "#[cfg(test)]")
                            .then(|| item.trim().trim_start_matches("pub(crate) "))
                            .and_then(|item| item.strip_prefix("mod "))
                            .and_then(|item| item.strip_suffix(';'))
                            .map(str::to_string)
                    })
            })
            .collect();
        let is_test_file = |path: &str| {
            path.split('/')
                .any(|part| test_modules.contains(part.strip_suffix(".rs").unwrap_or(part)))
        };
        let scan = |path: &str, source: &str| -> Vec<String> {
            if path == "src/config/paths.rs" || is_test_file(path) {
                return Vec::new();
            }
            lines_outside_test_modules(source)
                .iter()
                .enumerate()
                .filter(|(_, line)| {
                    let defines_the_constant = path == "src/config/mod.rs"
                        && line
                            .trim_start()
                            .starts_with(&format!("pub(crate) const {constant}"));
                    !defines_the_constant && (line.contains(&constant) || spells_the_name(line))
                })
                .map(|(index, line)| format!("{path}:{}: {}", index + 1, line.trim()))
                .collect()
        };
        for red in [
            format!(
                "fn help() -> &'static str {{\n    \"Save under {name}/ in the workspace\"\n}}\n"
            ),
            format!("/// Disable loading workspace macros from {name}/macros\n"),
            format!("let dir = \"{name}\";\n"),
            format!("let file = root.join(\"/{name}/mcp.json\");\n"),
            format!("/// The `{name}` directory holds it.\n"),
            format!("/// Saved under {name}, beside the sources.\n"),
            format!("let name = {constant};\n"),
            format!("let key = \"C:\\\\Users\\\\me\\\\{name}\\\\mesh\\\\identity.key\";\n"),
            format!("let entry = \"target/\\n{name}/memory/\\n\";\n"),
            format!("/// e.g. COYOTE_WORKSPACE_CONFIG_DIR={name}\n"),
        ] {
            let control = scan("src/fixture.rs", &red);
            assert_eq!(
                control.len(),
                1,
                "the scan does not go red on {red:?}: {control:?}"
            );
        }
        for green in [
            format!("let probe = \"{name}-case-probe\";\n"),
            format!("let file = home.join(\"{name}_password\");\n"),
            format!("// a {name} comment is not help\n"),
        ] {
            let control = scan("src/fixture.rs", &green);
            assert!(
                control.is_empty(),
                "the scan goes red on {green:?}: {control:?}"
            );
        }
        let hits: Vec<String> = files
            .iter()
            .flat_map(|(path, source)| scan(path, source))
            .collect();
        assert!(
            hits.is_empty(),
            "spell the workspace config directory through paths::workspace_config_dir_name \
             or paths::workspace_config_dirs:\n{}",
            hits.join("\n")
        );
    }

    #[test]
    fn spec_constants_are_pinned_by_tests_that_exist() {
        let sources = rust_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src")).unwrap();
        let rows = parse_constants_table(SPEC).unwrap();
        assert!(
            rows.iter().any(|row| row.pinned_by != PINNED_BY_THIS_TABLE),
            "the table pins nothing by test"
        );
        assert_eq!(check_pinned_by(&rows, &sources), Vec::<String>::new());
    }

    #[test]
    fn spec_requirement_ids_are_listed_once_each_in_area_order() {
        let ids = requirement_ids(SPEC).unwrap();
        assert!(ids.len() > 200, "{} ids", ids.len());
        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "an id is indexed twice");
        assert!(
            ids.iter()
                .all(|id| parse_id(id).is_some_and(|(_, rest)| rest.is_empty()))
        );
    }

    #[test]
    fn frozen_areas_define_no_id_past_their_published_maximum() {
        let definitions = definitions(&body(SPEC).unwrap()).unwrap();
        let frozen = [
            ("DEST", 10),
            ("ANN", 33),
            ("ENV", 50),
            ("VER", 14),
            ("EXT", 8),
            ("CODE", 5),
            ("KNOCK", 29),
            ("STATUS", 30),
            ("MSG", 59),
            ("PROP", 42),
            ("TIME", 11),
            ("CANON", 13),
        ];
        let highest: Vec<(&str, usize)> = frozen
            .iter()
            .map(|(area, _)| {
                let prefix = format!("MESH-{area}-");
                let max = definitions
                    .iter()
                    .filter_map(|d| d.id.strip_prefix(&prefix))
                    .map(|digits| digits.parse::<usize>().expect("three digits"))
                    .max()
                    .unwrap_or_else(|| panic!("{area} defines no ids"));
                (*area, max)
            })
            .collect();
        assert_eq!(
            highest,
            frozen.to_vec(),
            "a new requirement in a frozen area moves this pin deliberately"
        );
    }

    #[test]
    fn spec_source_paths_exist() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let paths = source_paths(SPEC);
        assert!(!paths.is_empty(), "the spec names no source files");
        let missing: Vec<&String> = paths
            .iter()
            .filter(|path| !root.join(path).is_file())
            .collect();
        assert_eq!(missing, Vec::<&String>::new());
    }

    /// The fetch response bound is the largest file plus framing, and a reply carrying
    /// exactly the largest file fits under it.
    #[test]
    fn fetch_response_bound_is_the_file_bound_plus_framing() {
        assert_eq!(
            MAX_FETCH_RESPONSE_BYTES,
            MAX_FETCH_FILE_BYTES as usize + 4096
        );
        let largest = ResponseFrame {
            request_id: RequestId::from([0; 16]),
            data: Value::Map(vec![
                (Value::from("v"), Value::from(1)),
                (Value::from("status"), Value::from("ok")),
                (Value::from("size"), Value::from(MAX_FETCH_FILE_BYTES)),
                (Value::from("sha256"), Value::Binary(vec![0; 32])),
                (
                    Value::from("bytes"),
                    Value::Binary(vec![0; MAX_FETCH_FILE_BYTES as usize]),
                ),
            ]),
        }
        .encode();
        assert!(
            largest.len() <= MAX_FETCH_RESPONSE_BYTES,
            "{} bytes, max {MAX_FETCH_RESPONSE_BYTES}",
            largest.len()
        );
    }
}
