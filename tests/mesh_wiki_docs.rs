//! Opt-in doc lint: pins the wiki's mesh pages and the repo README to the code
//! they describe.
//!
//! The wiki is a separate clone, so these tests run only when `COYOTE_WIKI_DIR`
//! points at it; a relative value is resolved against the crate root:
//!
//! ```sh
//! COYOTE_WIKI_DIR=../coyote.wiki cargo test --test mesh_wiki_docs
//! ```
//!
//! Without the variable every test prints `skipping:` and passes, so a plain
//! `cargo test` is unaffected. With it set to a directory that has no `Mesh.md`
//! the tests fail rather than pass against nothing.
//!
//! There is no lib target, so the code side of each pin is read from the source
//! text under `src/`: the REPL `VERBS` table, the hook event count and the
//! `mesh__*` tool names.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const WIKI_DIR: &str = "COYOTE_WIKI_DIR";
const RECIPE: &str = "`COYOTE_WIKI_DIR=../coyote.wiki cargo test --test mesh_wiki_docs`";
const PROTOCOL_PATH: &str = "docs/mesh/PROTOCOL.md";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The protocol's name as its spec's H1 spells it, which the Mesh pages lead with.
fn scope_lead() -> String {
    let spec = read(repo_root().join(PROTOCOL_PATH));
    let title = spec
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("# "))
        .unwrap_or_else(|| panic!("{PROTOCOL_PATH} line 1 is not an H1"))
        .trim();
    assert!(!title.is_empty(), "{PROTOCOL_PATH} has an empty H1");
    assert!(
        title.starts_with("SCOPE"),
        "{PROTOCOL_PATH}'s H1 does not name SCOPE: {title:?}"
    );
    title.to_string()
}

/// The wiki checkout, or `None` after printing the conventional `skipping:` line.
/// A set variable that does not point at the wiki is a failure, not a skip.
fn wiki_dir() -> Option<PathBuf> {
    let value = env::var_os(WIKI_DIR)
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

/// Every `"..."` literal in `source`, in order, with `\"` and `\\` unescaped.
fn string_literals(source: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut chars = source.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut literal = String::new();
        loop {
            match chars.next() {
                Some('"') => break,
                Some('\\') => match chars.next() {
                    Some(escaped @ ('"' | '\\')) => literal.push(escaped),
                    other => panic!(
                        "unexpected escape {other:?} in {literal:?}; extend string_literals in tests/mesh_wiki_docs.rs"
                    ),
                },
                Some(c) => literal.push(c),
                None => panic!("unterminated string literal: {literal:?}"),
            }
        }
        literals.push(literal);
    }
    literals
}

/// The `(verb, description, usage)` rows of `VERBS` in `src/repl/mesh.rs`.
fn repl_verbs() -> Vec<(String, String, String)> {
    let source = read(repo_root().join("src").join("repl").join("mesh.rs"));
    let start = source
        .find("const VERBS")
        .expect("src/repl/mesh.rs declares VERBS");
    let block = &source[start..];
    let end = block
        .find("\n];")
        .expect("the VERBS block ends with a `];` line");
    let literals = string_literals(&block[..end]);
    assert_eq!(
        literals.len() % 3,
        0,
        "VERBS holds {} string literals, not a multiple of three",
        literals.len()
    );
    literals
        .chunks(3)
        .map(|row| (row[0].clone(), row[1].clone(), row[2].clone()))
        .collect()
}

/// The cells of a markdown table row, split on unescaped pipes and trimmed.
fn table_cells(row: &str) -> Vec<String> {
    let mut cells = vec![String::new()];
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cells.last_mut().unwrap().push('|');
            }
            '|' => cells.push(String::new()),
            c => cells.last_mut().unwrap().push(c),
        }
    }
    let outer = cells.len() - 1;
    cells
        .drain(1..outer)
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn unbacktick<'a>(cell: &'a str, what: &str) -> &'a str {
    cell.strip_prefix('`')
        .and_then(|cell| cell.strip_suffix('`'))
        .unwrap_or_else(|| panic!("{what} cell is not a code span: {cell:?}"))
}

/// The `(verb, usage, description)` rows of the verb table on `Mesh-Commands.md`.
fn wiki_verb_rows(page: &str) -> Vec<(String, String, String)> {
    let section = page
        .split("\n## The verbs\n")
        .nth(1)
        .expect("Mesh-Commands.md has a `## The verbs` section");
    let section = section.split("\n## ").next().unwrap();
    section
        .lines()
        .filter(|line| line.starts_with('|'))
        .skip(2)
        .map(|row| {
            let cells = table_cells(row);
            assert_eq!(
                cells.len(),
                4,
                "verb table row has {} cells: {row:?}",
                cells.len()
            );
            (
                unbacktick(&cells[0], "verb").to_string(),
                unbacktick(&cells[1], "usage").to_string(),
                cells[2].clone(),
            )
        })
        .collect()
}

/// N from `const ALL: [HookEvent; N]` in `src/hooks.rs`.
fn hook_event_count() -> usize {
    let source = read(repo_root().join("src").join("hooks.rs"));
    let marker = "const ALL: [HookEvent; ";
    let start = source.find(marker).expect("src/hooks.rs declares ALL") + marker.len();
    let digits: String = source[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .unwrap_or_else(|e| panic!("ALL length {digits:?}: {e}"))
}

/// How many `HookEvent::as_str` arms name a `mesh.*` event.
fn mesh_hook_event_count() -> usize {
    let source = read(repo_root().join("src").join("hooks.rs"));
    let start = source
        .find("pub fn as_str(self)")
        .expect("src/hooks.rs defines HookEvent::as_str");
    let body = &source[start..];
    let end = body.find("\n    }\n").expect("the as_str body closes");
    body[..end]
        .lines()
        .filter(|line| line.contains("=> \"mesh."))
        .count()
}

/// The tool names `mesh_function_declarations()` registers, without their `mesh__` prefix.
fn mesh_tool_names() -> Vec<String> {
    let source = read(repo_root().join("src").join("function").join("mesh.rs"));
    let start = source
        .find("fn mesh_function_declarations()")
        .expect("src/function/mesh.rs defines mesh_function_declarations");
    let body = &source[start..];
    let end = body.find("\n}\n").expect("the fn body closes");
    let marker = "name: format!(\"{MESH_FUNCTION_PREFIX}";
    body[..end]
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix(marker))
        .map(|rest| rest[..rest.find('"').expect("the name literal closes")].to_string())
        .collect()
}

/// `label:line: spells <needle>` for every line of `text` that spells one of `needles`,
/// each of `file_names` removed from the line first.
fn pre_scope_vocabulary_hits(
    label: &str,
    text: &str,
    needles: &[String],
    file_names: &[String],
) -> Vec<String> {
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = file_names.iter().fold(line.to_string(), |line, file_name| {
            line.replace(file_name, "")
        });
        for needle in needles {
            if line.contains(needle.as_str()) {
                hits.push(format!("{label}:{}: spells {needle}", index + 1));
            }
        }
    }
    hits
}

#[test]
fn every_verb_row_in_the_wiki_table_matches_the_repl_verbs_table() {
    let Some(wiki) = wiki_dir() else { return };
    let verbs = repl_verbs();
    assert_eq!(
        verbs.len(),
        27,
        "parsed {} VERBS rows from src/repl/mesh.rs; if the table grew, update this count",
        verbs.len()
    );
    let rows = wiki_verb_rows(&read(wiki.join("Mesh-Commands.md")));
    assert_eq!(
        rows.len(),
        verbs.len(),
        "Mesh-Commands.md's verb table has {} rows, src/repl/mesh.rs VERBS has {}",
        rows.len(),
        verbs.len()
    );
    for (verb, description, usage) in &verbs {
        let (_, documented_usage, documented_description) = rows
            .iter()
            .find(|(row_verb, _, _)| row_verb == verb)
            .unwrap_or_else(|| panic!("Mesh-Commands.md's verb table has no `{verb}` row"));
        assert_eq!(
            documented_usage, usage,
            "Mesh-Commands.md's usage for `{verb}` differs from src/repl/mesh.rs"
        );
        assert_eq!(
            documented_description, description,
            "Mesh-Commands.md's \"What it does\" for `{verb}` differs from src/repl/mesh.rs"
        );
    }
}

#[test]
fn the_protocol_link_appears_once_per_page_and_before_the_first_section_heading() {
    let Some(wiki) = wiki_dir() else { return };
    for page in ["Mesh-Commands.md", "Mesh-Configuration.md"] {
        let text = read(wiki.join(page));
        // A markdown link spells the path twice (text and href), so count lines.
        let links: Vec<usize> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(PROTOCOL_PATH))
            .map(|(index, _)| index + 1)
            .collect();
        assert_eq!(
            links.len(),
            1,
            "{page} mentions {PROTOCOL_PATH} on lines {links:?}; expected exactly one"
        );
        let first_heading = text
            .lines()
            .position(|line| line.starts_with("## "))
            .map(|index| index + 1)
            .unwrap_or_else(|| panic!("{page} has no `## ` heading"));
        assert!(
            links[0] < first_heading,
            "{page} links {PROTOCOL_PATH} on line {} but its first `## ` heading is line {first_heading}; the link belongs in the intro",
            links[0]
        );
    }
}

#[test]
fn the_hooks_page_counts_the_events_the_code_registers() {
    let Some(wiki) = wiki_dir() else { return };
    let count = hook_event_count();
    let needle = format!("The {count} events");
    assert!(
        read(wiki.join("Hooks.md")).contains(&needle),
        "Hooks.md does not say {needle:?}; src/hooks.rs ALL has {count} events"
    );
    let parenthetical = format!("({count} events)");
    for (label, path) in [
        ("README.md", repo_root().join("README.md")),
        ("Home.md", wiki.join("Home.md")),
    ] {
        assert!(
            read(path).contains(&parenthetical),
            "{label} does not say {parenthetical:?}; src/hooks.rs ALL has {count} events"
        );
    }
}

#[test]
fn the_hooks_page_counts_the_mesh_events_in_words() {
    let Some(wiki) = wiki_dir() else { return };
    let count = mesh_hook_event_count();
    let word = match count {
        15 => "fifteen",
        count => panic!(
            "src/hooks.rs names {count} mesh.* events; update Hooks.md and the number word here"
        ),
    };
    let needle = format!("The {word} `mesh.*` events");
    assert!(
        read(wiki.join("Hooks.md")).contains(&needle),
        "Hooks.md does not say {needle:?}; src/hooks.rs as_str names {count} mesh.* events"
    );
}

#[test]
fn the_mesh_page_lists_every_registered_tool_and_counts_them_in_words() {
    let Some(wiki) = wiki_dir() else { return };
    let names = mesh_tool_names();
    let word = match names.len() {
        9 => "Nine",
        count => panic!(
            "src/function/mesh.rs registers {count} mesh__ tools; update Mesh.md and the number word here"
        ),
    };
    let page = read(wiki.join("Mesh.md"));
    let lead = format!("{word} `mesh__*` tools");
    assert!(page.contains(&lead), "Mesh.md does not say {lead:?}");
    for name in &names {
        let span = format!("`mesh__{name}`");
        assert!(page.contains(&span), "Mesh.md does not mention {span}");
    }
    let rows = page
        .lines()
        .filter(|line| line.starts_with("| `mesh__"))
        .count();
    assert_eq!(
        rows,
        names.len(),
        "Mesh.md's tool table has {rows} rows for {} registered tools",
        names.len()
    );
}

#[test]
fn no_mesh_page_or_the_readme_spells_the_pre_scope_wire_vocabulary() {
    let Some(wiki) = wiki_dir() else { return };
    // Assembled at runtime so the mesh source guard does not match this test's text.
    let magic = ["COY", "M"].concat();
    let backticked_old_name = ["`coy", "ote."].concat();
    // A backticked `<name>.<ext>` with one of these extensions is a file name the
    // install steps spell, not wire vocabulary.
    let file_names: Vec<String> = ["exe", "log", "yaml", "json"]
        .iter()
        .map(|ext| format!("{backticked_old_name}{ext}`"))
        .collect();
    let needles = [magic, backticked_old_name];
    let fixture = ["fine line\nthe `coy", "ote.mesh destination\n"].concat();
    let control = pre_scope_vocabulary_hits("fixture", &fixture, &needles, &file_names);
    assert_eq!(
        control.len(),
        1,
        "the scan does not go red on a fixture: {control:?}"
    );
    let file_name_only = ["run `coy", "ote.exe` first\n"].concat();
    let control = pre_scope_vocabulary_hits("fixture", &file_name_only, &needles, &file_names);
    assert!(
        control.is_empty(),
        "a file name is not wire vocabulary: {control:?}"
    );

    let mut pages: Vec<PathBuf> = fs::read_dir(&wiki)
        .unwrap_or_else(|e| panic!("read {}: {e}", wiki.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("Mesh") && name.ends_with(".md"))
        })
        .collect();
    assert!(
        !pages.is_empty(),
        "no Mesh*.md pages under {}",
        wiki.display()
    );
    pages.push(repo_root().join("README.md"));

    let mut hits = Vec::new();
    for path in &pages {
        hits.extend(pre_scope_vocabulary_hits(
            &path.display().to_string(),
            &read(path),
            &needles,
            &file_names,
        ));
    }
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

#[test]
fn the_mesh_page_and_the_readme_mesh_section_lead_with_the_scope_expansion() {
    let Some(wiki) = wiki_dir() else { return };
    let scope_lead = scope_lead();
    let page = read(wiki.join("Mesh.md"));
    let first = page.lines().next().unwrap_or_default();
    assert!(
        first.starts_with(&scope_lead),
        "Mesh.md line 1 does not open with {scope_lead:?}: {first:?}"
    );

    let readme = read(repo_root().join("README.md"));
    let mut after_heading = readme.lines().skip_while(|line| *line != "### Mesh");
    after_heading
        .next()
        .expect("README.md has a `### Mesh` heading");
    let lead = after_heading
        .find(|line| !line.trim().is_empty())
        .expect("README.md has text under `### Mesh`");
    assert!(
        lead.starts_with(&scope_lead),
        "README.md's `### Mesh` section does not open with {scope_lead:?}: {lead:?}"
    );
}
