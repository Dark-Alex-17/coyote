//! Pins `docs/mesh/PROTOCOL.md` to its own format contract and to the constants the code
//! actually uses. The checkers are line-oriented and std-only; the tests at the bottom run
//! them over fixtures (to prove each rule can go red) and over the real document.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

pub(crate) const SPEC: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/mesh/PROTOCOL.md"
));

const EXPECTED_H1: &str = "# Coyote Mesh Protocol, version 1: wire format";
const SECTION_COUNT: usize = 16;
const CONSTANTS_HEADING: &str = "## 15. Constants";
const INDEX_HEADING: &str = "## 16. Requirements index";
const BCP14: &str = "The key words \"MUST\", \"MUST NOT\", \"REQUIRED\", \"SHALL\", \"SHALL NOT\", \
    \"SHOULD\", \"SHOULD NOT\", \"RECOMMENDED\", \"NOT RECOMMENDED\", \"MAY\", and \"OPTIONAL\" in \
    this document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174] when, and \
    only when, they appear in all capitals, as shown here.";
const AREAS: [&str; 12] = [
    "DEST", "ANN", "ENV", "KNOCK", "STATUS", "MSG", "PROP", "VER", "TIME", "CANON", "EXT", "CODE",
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
const CATCH_ALL_ROW_PREFIXES: [&str; 2] = ["any other", "trailing"];
const CONSTANTS_TABLE_HEADER: &str = "| Constant | Value | Defined in | Pinned by |";
/// The "Pinned by" cell of a constant that only this module's table check pins.
const PINNED_BY_THIS_TABLE: &str = "spec_pins (this table)";
const CITED_IDENTIFIER_MIN_LEN: usize = 12;
const CITED_IDENTIFIER_MIN_UNDERSCORES: usize = 2;

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

fn is_catch_all_row(row: &str) -> bool {
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

/// The spec cites tests and items by name but also struct fields, wire keys and upstream
/// functions, so a citation resolves when the sources name it as a whole word anywhere.
fn names_identifier(source: &str, name: &str) -> bool {
    contains_word(source, name)
}

/// The concatenated contents of every `.rs` file under `dir`, this file excepted: its
/// fixtures name identifiers that exist nowhere else, and a citation must not resolve
/// against the guard that checks it.
fn rust_sources(dir: &Path) -> std::io::Result<String> {
    let guard = Path::new(file!()).file_name();
    let mut out = String::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") && path.file_name() != guard {
                out.push_str(&std::fs::read_to_string(path)?);
                out.push('\n');
            }
        }
    }
    Ok(out)
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

/// Every requirement id the section 16 index lists, in index order.
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
        DEFAULT_PEER_MAX_CONCURRENT, DEFAULT_PEER_MAX_MESSAGES_PER_HOUR,
        DEFAULT_PEER_MAX_TOKENS_PER_HOUR,
    };
    use crate::config::mesh_envoy::ENVOY_RUN_TIMEOUT_SECS;
    use crate::mesh::r3::RefusalCode;
    use crate::mesh::{
        announce, card, knock, knocks, limits, message, peers, pending, propagation,
        propagation_fetch, propagation_nodes, protocol, r3,
    };
    use lxmf_core::constants::{FIELD_CUSTOM_DATA, FIELD_CUSTOM_TYPE};
    use rns_transport::hash::ADDRESS_HASH_SIZE;
    use std::time::Duration;

    const EXPECTED_LITERALS: &str = r#"1,1,10,16,262144,"/knock","/status","/message",30,10,10,2,20,16,0xf0,0xf1,0xf3,0xf4,0xf5,0xf6,0xfd,0xfe,"COYM",64,300,900,3,2700,1800,1024,"coyote.knock/1",200,15,10,256,3,600,256,16,1,0,1,2,64,280,64,64,120,280,"coyote.peer/1",1,120,4000,64,4096,8,15,10,604800,256,3600,120,256,1,60,100000,120,26,60,2,60,1024,64,240,131072,112,4096,15552000,3,256,0,32,0xfb,0xfc"#;

    fn expected_constants() -> Vec<(&'static str, String)> {
        let secs = |d: Duration| d.as_secs().to_string();
        let quoted = |s: &str| format!("{s:?}");
        let byte = |v: u8| format!("0x{v:02x}");
        let code = |c: RefusalCode| byte(c as u8);
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
            (
                "MAX_UNKNOWN_SOURCE_DEFERRALS",
                propagation_fetch::MAX_UNKNOWN_SOURCE_DEFERRALS.to_string(),
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
        ]
    }

    const VALID_SPEC: &str = "\
# Coyote Mesh Protocol, version 1: wire format

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

## 15. Constants

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | `1` | a.rs | spec_pins (this table) |
| `BETA` | `\"b\"` | b.rs | decode_whole_frame_test |

## 16. Requirements index

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
        let wrong_h1 = sections(&(1..=16).collect::<Vec<_>>()).replace("version 1", "version 2");
        assert!(check_headings(&wrong_h1).is_err());
        assert!(check_headings(&sections(&(1..=16).collect::<Vec<_>>())).is_ok());
        assert!(
            check_headings(&sections(&(1..=15).collect::<Vec<_>>()))
                .unwrap_err()
                .contains("found 15")
        );
        let gap: Vec<usize> = (1..=17).filter(|n| *n != 3).collect();
        assert!(
            check_headings(&sections(&gap))
                .unwrap_err()
                .contains("line 7")
        );
        let unnumbered = format!("{}\n## Appendix\n", sections(&(1..=16).collect::<Vec<_>>()));
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
## 15. Constants

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | `1` | a.rs | x |

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `BETA` | `2` | b.rs | y |

## 16. Requirements index
";
    const CONSTANTS_BAD_HEADER: &str = "\
## 15. Constants

| Name | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | `1` | a.rs | x |
";
    const CONSTANTS_UNBACKTICKED: &str = "\
## 15. Constants

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | 1 | a.rs | x |
";
    const CONSTANTS_THREE_CELLS: &str = "\
## 15. Constants

| Constant | Value | Defined in | Pinned by |
|---|---|---|---|
| `ALPHA` | `1` | a.rs |
";
    const CONSTANTS_PINNED_BY: &str = "\
## 15. Constants

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
## 16. Requirements index

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
        let sources = rust_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src")).unwrap();
        let cited = cited_identifiers(SPEC);
        assert!(!cited.is_empty(), "the spec cites no tests");
        assert_eq!(
            check_cited_identifiers(SPEC, &sources),
            Vec::<String>::new()
        );
    }

    #[test]
    fn spec_constants_table_matches_the_code() {
        let actual = parse_constants_table(SPEC).unwrap();
        check_constants(&actual, &expected_constants()).unwrap();
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
}
