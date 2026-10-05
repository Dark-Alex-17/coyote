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
//! There is no lib target, so the code side of each pin is read from the repo's
//! text: the REPL `VERBS` table, the hook counts and the `mesh__*` tool names under
//! `src/`, the spec's H1 in `docs/mesh/PROTOCOL.md`, and the README.

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
    // Line endings are normalised so the `\n`-anchored block and heading searches below
    // hold on a CRLF checkout.
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
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
    // Assembled at runtime so a repo-wide guard against the old identifiers never trips on
    // this test's own text.
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

    let mut pages = mesh_pages(&wiki);
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

/// Every `Mesh*.md` page in the wiki checkout.
fn mesh_pages(wiki: &Path) -> Vec<PathBuf> {
    let pages: Vec<PathBuf> = fs::read_dir(wiki)
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
    pages
}

/// `label:line` for every staged-path example in `text` whose peer segment, the one right
/// after `inbox/<instance id>/`, `inbox/<instance_id>/` or the `<inbox_dir>/…` spellings
/// of the same root, is not the full lowercase 32-hex destination hash: the
/// `<peer-dest32>` placeholder and other non-hex names pass; a literal that starts with a
/// hex digit but is not exactly 32 lowercase hex digits (shortened, truncated with an
/// ellipsis, or upper-cased) does not, and neither does a retired 8-hex placeholder
/// (`<peer-dest8>`, `<dest8>`, `<peer8>`). A segment ends at a path separator, a
/// backtick, a space or sentence punctuation, so prose around an example is not judged.
fn short_peer_directory_hits(label: &str, text: &str) -> Vec<String> {
    let markers = [
        "inbox/<instance id>/",
        "inbox/<instance_id>/",
        "<inbox_dir>/<instance id>/",
        "<inbox_dir>/<instance_id>/",
    ];
    let retired = ["<peer-dest8>", "<dest8>", "<peer8>"];
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for marker in markers {
            for (at, _) in line.match_indices(marker) {
                let rest = &line[at + marker.len()..];
                let segment: &str = rest
                    .split(['/', '`', ' ', ')', ',', ';', '.', ':'])
                    .next()
                    .unwrap_or_default();
                let looks_literal = segment
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_hexdigit());
                let full_lowercase_hash = segment.len() == 32
                    && segment
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
                if (looks_literal && !full_lowercase_hash) || retired.contains(&segment) {
                    hits.push(format!(
                        "{label}:{}: peer directory `{segment}` is not a full lowercase 32-hex destination",
                        index + 1
                    ));
                }
            }
        }
    }
    hits
}

#[test]
fn every_staged_path_example_names_the_peer_directory_by_the_full_destination_hash() {
    let Some(wiki) = wiki_dir() else { return };
    let short = format!(
        "inbox/<instance id>/5e6f7a8b/ci.log\n\
         inbox/<instance id>/5e6f7a8b…/ci.log\n\
         <inbox_dir>/<instance_id>/5e6f7a8b)\n\
         inbox/<instance_id>/{}/x\n\
         inbox/<instance_id>/<peer-dest8>/x\n",
        "5E6F7A8B".repeat(4)
    );
    let control = short_peer_directory_hits("fixture", &short);
    assert_eq!(
        control.len(),
        5,
        "the scan does not go red on each short, truncated, upper-cased or retired peer directory: {control:?}"
    );
    let full = format!(
        "inbox/<instance id>/{}/ci.log\ninbox/<instance_id>/<peer-dest32>/<path>\n",
        "5e6f7a8b".repeat(4)
    );
    let control = short_peer_directory_hits("fixture", &full);
    assert!(
        control.is_empty(),
        "a full hash or a placeholder is not a short peer directory: {control:?}"
    );

    let mut pages = mesh_pages(&wiki);
    pages.push(repo_root().join("README.md"));
    let hits: Vec<String> = pages
        .iter()
        .flat_map(|path| short_peer_directory_hits(&path.display().to_string(), &read(path)))
        .collect();
    assert!(hits.is_empty(), "{}", hits.join("\n"));
    let documented = pages
        .iter()
        .any(|path| read(path).contains("inbox/<instance_id>/<peer-dest32>/"));
    assert!(
        documented,
        "no Mesh*.md page documents the `inbox/<instance_id>/<peer-dest32>/` layout"
    );
}

/// The staged-path scan judges the segment right after either `inbox/<instance id>/`
/// marker on its own: every literal hex run that is not exactly 32 digits is a hit (8, 16,
/// 31 and 33), a placeholder or any non-hex name is not, several examples on one line are
/// each judged with that line's number, and a CRLF checkout reads back as the LF text with
/// the same hits. The wiki itself keeps at least one literal 32-hex staged path (the
/// `.mesh inbox` example) so the pin never passes vacuously.
#[test]
fn usage_probe_the_peer_directory_scan_judges_each_literal_and_reads_crlf_alike() {
    let Some(wiki) = wiki_dir() else { return };
    let full = "5e6f7a8b".repeat(4);
    let thirty_one = &full[..31];
    let text = format!(
        "intro\n\
         `inbox/<instance id>/{full}/a.log` and `inbox/<instance_id>/5e6f7a8b9c0d1e2f/b.log`\n\
         inbox/<instance id>/<peer-dest32>/<path> inbox/<instance_id>/<peer>/x inbox/<instance id>/peer-dir/y\n\
         staged at <cache>/mesh/inbox/<instance id>/{full}a/c.log\n\
         inbox/<instance_id>/{thirty_one}/d.log inbox/<instance id>/5e6f7a8b/e.log\n\
         inbox/<instance id>/{full}\n"
    );
    // Within one line the order of hits is not a contract; across lines it is the line number.
    let mut hits = short_peer_directory_hits("page", &text);
    hits.sort();
    let mut expected = vec![
        "page:2: peer directory `5e6f7a8b9c0d1e2f` is not a full lowercase 32-hex destination"
            .to_string(),
        format!("page:4: peer directory `{full}a` is not a full lowercase 32-hex destination"),
        format!("page:5: peer directory `{thirty_one}` is not a full lowercase 32-hex destination"),
        "page:5: peer directory `5e6f7a8b` is not a full lowercase 32-hex destination".to_string(),
    ];
    expected.sort();
    assert_eq!(hits, expected);

    // A CRLF checkout of the same page reads back as the LF text and yields the same hits
    // at the same line numbers.
    let dir = env::temp_dir().join(format!("probe-wiki-crlf-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let page = dir.join("Mesh-Probe.md");
    fs::write(&page, text.replace('\n', "\r\n")).unwrap();
    let read_back = read(&page);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        read_back, text,
        "CRLF read is not normalised to the LF text"
    );
    let mut crlf_hits = short_peer_directory_hits("page", &read_back);
    crlf_hits.sort();
    assert_eq!(crlf_hits, hits);

    // The real wiki carries a literal, full-hash staged path example, so the lint judges
    // at least one concrete example rather than only placeholders.
    let markers = ["inbox/<instance id>/", "inbox/<instance_id>/"];
    let literal_examples: Vec<String> = mesh_pages(&wiki)
        .iter()
        .flat_map(|path| {
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            read(path)
                .lines()
                .enumerate()
                .filter(|(_, line)| {
                    markers.iter().any(|marker| {
                        line.match_indices(marker).any(|(at, _)| {
                            let segment = line[at + marker.len()..]
                                .split(['/', '`', ' '])
                                .next()
                                .unwrap_or_default();
                            segment.len() == 32
                                && segment
                                    .bytes()
                                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                        })
                    })
                })
                .map(|(index, _)| format!("{label}:{}", index + 1))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        !literal_examples.is_empty(),
        "no Mesh*.md page shows a staged path with a literal 32-lowercase-hex peer directory"
    );
    assert!(
        literal_examples
            .iter()
            .any(|at| at.starts_with("Mesh-Commands.md:")),
        "the `.mesh inbox` example on Mesh-Commands.md shows no literal full-hash staged path: {literal_examples:?}"
    );
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

/// Every spelling of the staging root the wiki uses (`inbox/<instance id>/`,
/// `inbox/<instance_id>/` and the two `<inbox_dir>/…` forms) is judged alike: the peer
/// segment ends at `/`, a backtick, a space, `)`, `,`, `;` or the line end, so a shortened
/// hash followed by sentence punctuation is quoted WITHOUT that punctuation in the hit; the
/// full lowercase hash in the same positions is never a hit; and each retired placeholder
/// is named in its own hit. The wiki itself exercises the `<inbox_dir>` spelling (the
/// `Mesh-Configuration.md` `inbox_dir` paragraph), so that marker is not a dead branch.
#[test]
fn usage_probe_every_root_spelling_and_segment_terminator_judges_the_peer_segment_alike() {
    let Some(wiki) = wiki_dir() else { return };
    let full = "5e6f7a8b".repeat(4);
    let roots = [
        "inbox/<instance id>/",
        "inbox/<instance_id>/",
        "<inbox_dir>/<instance id>/",
        "<inbox_dir>/<instance_id>/",
    ];
    let terminators = ["/ci.log", ")", ",", ";", " then", "`", ""];

    for root in roots {
        for terminator in terminators {
            // A shortened hash is a hit and the hit quotes only the hash, never the
            // punctuation after it.
            let short = format!("staged at <cache_dir>/mesh/{root}5e6f7a8b{terminator}\n");
            assert_eq!(
                short_peer_directory_hits("page", &short),
                vec![
                    "page:1: peer directory `5e6f7a8b` is not a full lowercase 32-hex destination"
                        .to_string()
                ],
                "root {root:?} + terminator {terminator:?}"
            );
            // The full lowercase hash in the same slot is never a hit.
            let ok = format!("staged at <cache_dir>/mesh/{root}{full}{terminator}\n");
            assert!(
                short_peer_directory_hits("page", &ok).is_empty(),
                "root {root:?} + terminator {terminator:?} flags the full hash: {:?}",
                short_peer_directory_hits("page", &ok)
            );
        }
        // Upper- and mixed-case full-length hashes are hits under every root spelling.
        for cased in [
            full.to_uppercase(),
            format!("{}{}", full[..8].to_uppercase(), &full[8..]),
        ] {
            let hits = short_peer_directory_hits("page", &format!("{root}{cased}/x\n"));
            assert_eq!(
                hits,
                vec![format!(
                    "page:1: peer directory `{cased}` is not a full lowercase 32-hex destination"
                )],
                "root {root:?}"
            );
        }
        // Each retired placeholder is a hit naming itself; the current one and prose
        // placeholders are not.
        for retired in ["<peer-dest8>", "<dest8>", "<peer8>"] {
            let hits = short_peer_directory_hits("page", &format!("{root}{retired}/x\n"));
            assert_eq!(
                hits,
                vec![format!(
                    "page:1: peer directory `{retired}` is not a full lowercase 32-hex destination"
                )],
                "root {root:?}"
            );
        }
        for placeholder in [
            "<peer-dest32>",
            "<peer's full destination hash, lowercase>",
            "<peer>",
        ] {
            let text = format!("{root}{placeholder}/<path>\n");
            assert!(
                short_peer_directory_hits("page", &text).is_empty(),
                "root {root:?} flags the placeholder {placeholder:?}"
            );
        }
    }

    // Two short examples on one line under different root spellings are two hits with the
    // same line number; a full hash between them is not.
    let mixed = format!(
        "a\n`inbox/<instance id>/5e6f7a8b/x` or `<inbox_dir>/<instance_id>/{full}/y` or <inbox_dir>/<instance id>/<dest8>/z\n"
    );
    let mut hits = short_peer_directory_hits("page", &mixed);
    hits.sort();
    assert_eq!(
        hits,
        vec![
            "page:2: peer directory `5e6f7a8b` is not a full lowercase 32-hex destination"
                .to_string(),
            "page:2: peer directory `<dest8>` is not a full lowercase 32-hex destination"
                .to_string(),
        ]
    );

    // The wiki really uses the `<inbox_dir>/<instance id>/` spelling in a scanned page, so
    // that marker is exercised by the lint rather than only by this fixture.
    let inbox_dir_examples: Vec<String> = mesh_pages(&wiki)
        .iter()
        .flat_map(|path| {
            let label = path.file_name().unwrap().to_string_lossy().to_string();
            read(path)
                .lines()
                .enumerate()
                .filter(|(_, line)| {
                    line.contains("<inbox_dir>/<instance id>/")
                        || line.contains("<inbox_dir>/<instance_id>/")
                })
                .map(|(index, _)| format!("{label}:{}", index + 1))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        inbox_dir_examples
            .iter()
            .any(|at| at.starts_with("Mesh-Configuration.md:")),
        "no scanned Mesh*.md page spells the staging root as `<inbox_dir>/<instance id>/`: {inbox_dir_examples:?}"
    );
}

/// The interop README's `COYOTE_WIKI_DIR` row and CONTRIBUTING.md carry the opt-in recipe,
/// and the row names every page family this file reads (`Mesh*`, `Hooks`, `Home`, the
/// README) and the staging-inbox layout pin, so an operator can tell from the docs what
/// turning the variable on will judge.
#[test]
fn usage_probe_the_docs_name_the_recipe_and_every_page_family_the_lint_reads() {
    let Some(_wiki) = wiki_dir() else { return };
    let readme = read(repo_root().join("scripts/mesh-interop/README.md"));
    let row = readme
        .lines()
        .find(|line| line.starts_with("| `COYOTE_WIKI_DIR`"))
        .expect("scripts/mesh-interop/README.md has a `COYOTE_WIKI_DIR` table row");
    for needle in [
        "`Mesh*`",
        "`Hooks`",
        "`Home`",
        "README",
        "staging-inbox layout",
        "skipping:",
    ] {
        assert!(
            row.contains(needle),
            "the COYOTE_WIKI_DIR row does not say {needle:?}: {row}"
        );
    }
    assert!(
        readme.contains(RECIPE),
        "scripts/mesh-interop/README.md lacks the recipe {RECIPE}"
    );
    let contributing = read(repo_root().join("CONTRIBUTING.md"));
    assert!(
        contributing.contains(RECIPE),
        "CONTRIBUTING.md lacks the recipe {RECIPE}"
    );
}
