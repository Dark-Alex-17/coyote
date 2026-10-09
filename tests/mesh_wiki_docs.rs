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
        28,
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

/// The `collision_protection:` comment on `Mesh-Configuration.md` is the template's line;
/// the setting's section there and the identity-tier Note on `Mesh-Trust-Model.md` each
/// name what sets the presence case apart (nothing marked, the refusal remembered from the
/// refusal itself, the key first heard never refused, trust of the new destination and
/// block clearing the memory); and the Note quotes the two presence lines with the wording
/// `presence_collision_text` emits.
#[test]
fn the_collision_protection_prose_in_the_wiki_matches_the_template_and_names_the_presence_case() {
    let Some(wiki) = wiki_dir() else { return };
    let comment_line = |label: &str, text: &str| -> String {
        text.lines()
            .find(|line| line.starts_with("  collision_protection: false    #"))
            .unwrap_or_else(|| panic!("{label} has no `  collision_protection: false    #` line"))
            .trim()
            .to_string()
    };
    let configuration = read(wiki.join("Mesh-Configuration.md"));
    assert_eq!(
        comment_line("Mesh-Configuration.md", &configuration),
        comment_line(
            "assets/config-template.yaml",
            &read(repo_root().join("assets/config-template.yaml"))
        ),
        "the collision_protection comment on Mesh-Configuration.md is not the template's"
    );

    let heading = "\n## `collision_protection`\n";
    let section = &configuration[configuration
        .find(heading)
        .expect("Mesh-Configuration.md has a `## collision_protection` section")..];
    let section = section[1..].split("\n## ").next().unwrap();
    let trust_model = read(wiki.join("Mesh-Trust-Model.md"));
    let note_lead = "\n> **Note:** An identity-tier grant";
    let note = &trust_model[trust_model
        .find(note_lead)
        .expect("Mesh-Trust-Model.md has the identity-tier Note")..];
    let note = note[1..].split("\n\n").next().unwrap();
    for (label, text) in [
        ("Mesh-Configuration.md's `## collision_protection`", section),
        ("Mesh-Trust-Model.md's identity-tier Note", note),
    ] {
        for needle in [
            "collision_protection",
            "nothing is marked",
            "remembered for as long as the node runs from the refusal itself",
            "names the key first heard holding the instance",
            "`.mesh trust <new destination>` admits the new key and clears the memory",
            "`.mesh block` of either key clears it too",
            "`.mesh untrust --identity <old key>` admits the new key",
        ] {
            assert!(
                text.contains(needle),
                "{label} does not say {needle:?}:\n{text}"
            );
        }
    }

    let source = read(repo_root().join("src/mesh/trust.rs"));
    let body = source
        .split("\nfn presence_collision_text(")
        .nth(1)
        .expect("src/mesh/trust.rs defines presence_collision_text")
        .split("\n}\n")
        .next()
        .unwrap();
    // The format strings continue over `\`-ended lines; join them as the compiler does.
    let mut emitted = String::new();
    let mut rest = body;
    while let Some(at) = rest.find("\\\n") {
        emitted.push_str(&rest[..at]);
        rest = rest[at + 2..].trim_start();
    }
    emitted.push_str(rest);
    let quoted = |lead: &str| -> String {
        note.lines()
            .find(|line| line.starts_with(lead))
            .unwrap_or_else(|| panic!("the identity-tier Note quotes no {lead:?} line:\n{note}"))
            .to_string()
    };
    let shared = [
        "was heard under identity",
        "is now presented under identity",
        "no record carries the instance, so nothing is marked",
        "otherwise .mesh block",
    ];
    let served = ["is served while it asks because collision_protection is off"];
    let refused = ["is refused when it asks", "run .mesh trust"];
    for fragment in shared.iter().chain(&served).chain(&refused) {
        assert!(
            emitted.contains(fragment),
            "presence_collision_text no longer says {fragment:?}; update the wiki and this test"
        );
    }
    for (lead, own, absent) in [
        ("> warning: instance ", &served[..], &refused[..]),
        ("> error: instance ", &refused[..], &served[..]),
    ] {
        let line = quoted(lead);
        for fragment in shared.iter().chain(own) {
            assert!(
                line.contains(fragment),
                "the quoted {lead:?} line does not say {fragment:?}: {line}"
            );
        }
        for fragment in absent {
            assert!(
                !line.contains(fragment),
                "the quoted {lead:?} line says {fragment:?}, which the emitted one does not: {line}"
            );
        }
    }
}

/// The `### Conversation memory` section on `Mesh.md` names the wipe verb, says the
/// memory is kept per identity and thread, and states every bound; the bounds are
/// spelt as their default values, not as the `mesh.envoy_memory.*` key names.
#[test]
fn the_conversation_memory_section_names_the_wipe_verb_the_isolation_and_every_bound() {
    let Some(wiki) = wiki_dir() else { return };
    let mesh = read(wiki.join("Mesh.md"));
    let heading = "\n### Conversation memory\n";
    let section_at = mesh
        .find(heading)
        .expect("Mesh.md has a `### Conversation memory` section");
    let section = mesh[section_at + 1..]
        .split("\n## ")
        .next()
        .unwrap()
        .split("\n### ")
        .next()
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    for phrase in [
        ".mesh memory forget",
        "identity",
        "thread",
        "`mesh.envoy_memory.*`",
        "256 threads across all peers",
        "16 per identity",
        "40 turns",
        "65536 bytes",
        "168 hours",
    ] {
        assert!(
            section.contains(phrase),
            "Mesh.md's `### Conversation memory` section does not say {phrase:?}:\n{section}"
        );
    }
}

/// Usage probe: `ttl_hours` counts from the thread's last *written* exchange — a load
/// that finds a thread does not renew it. Every surface an operator reads the key on says
/// so in the same words: the template and example comments, the README and Configuration
/// table rows, the Configuration page's yaml line and the field's own rustdoc; none of them
/// still says "after its last message".
#[test]
fn usage_probe_every_ttl_hours_twin_counts_from_the_last_written_exchange() {
    let Some(wiki) = wiki_dir() else { return };
    const WORDING: &str = "after its last written exchange";
    const STALE: &str = "after its last message";

    let yaml_line = |label: &str, text: &str| -> String {
        text.lines()
            .find(|line| line.trim_start().starts_with("ttl_hours:") && line.contains('#'))
            .unwrap_or_else(|| panic!("{label} has no commented `ttl_hours:` line"))
            .to_string()
    };
    let table_row = |label: &str, text: &str, head: &str| -> String {
        text.lines()
            .find(|line| line.starts_with(head))
            .unwrap_or_else(|| panic!("{label} has no `{head}` row"))
            .to_string()
    };
    let config_source = read(repo_root().join("src/config/mesh_config.rs"));
    let rustdoc = {
        let lines: Vec<&str> = config_source.lines().collect();
        let field = lines
            .iter()
            .position(|line| line.trim_start().starts_with("pub ttl_hours:"))
            .expect("EnvoyMemoryConfig has a `pub ttl_hours` field");
        lines[..field]
            .iter()
            .rev()
            .take_while(|line| line.trim_start().starts_with("///"))
            .map(|line| line.trim())
            .collect::<Vec<_>>()
            .join(" ")
    };

    let twins = [
        (
            "assets/config-template.yaml",
            yaml_line(
                "assets/config-template.yaml",
                &read(repo_root().join("assets/config-template.yaml")),
            ),
        ),
        (
            "config.example.yaml",
            yaml_line(
                "config.example.yaml",
                &read(repo_root().join("config.example.yaml")),
            ),
        ),
        (
            "Mesh-Configuration.md yaml",
            yaml_line(
                "Mesh-Configuration.md",
                &read(wiki.join("Mesh-Configuration.md")),
            ),
        ),
        (
            "Mesh-Configuration.md table",
            table_row(
                "Mesh-Configuration.md",
                &read(wiki.join("Mesh-Configuration.md")),
                "| `envoy_memory.ttl_hours`",
            ),
        ),
        (
            "README.md table",
            table_row(
                "README.md",
                &read(repo_root().join("README.md")),
                "| `mesh.envoy_memory.ttl_hours`",
            ),
        ),
        ("EnvoyMemoryConfig::ttl_hours rustdoc", rustdoc),
    ];
    for (label, text) in &twins {
        assert!(
            text.contains(WORDING),
            "{label} does not say `{WORDING}`: {text}"
        );
        assert!(
            !text.contains(STALE),
            "{label} still says `{STALE}`: {text}"
        );
    }
}

/// Usage probe: the identity-tier Note on `Mesh-Trust-Model.md` quotes the two presence
/// lines with the instance as `<dest8>` — `presence_collision_text` shortens the old
/// destination and has no label to print — while the record-collision lines quoted above
/// it keep `<label or dest8>`, as `key_change_text` prints the record's label when it has
/// one. The placeholders are not interchangeable: a reader copying the wiki's line must
/// know which one names a labelled record and which one never does.
#[test]
fn the_identity_tier_note_quotes_the_presence_lines_with_the_instance_as_dest8() {
    let Some(wiki) = wiki_dir() else { return };
    let trust_model = read(wiki.join("Mesh-Trust-Model.md"));
    let note_lead = "\n> **Note:** An identity-tier grant";
    let note_at = trust_model
        .find(note_lead)
        .expect("Mesh-Trust-Model.md has the identity-tier Note");
    let note = trust_model[note_at + 1..].split("\n\n").next().unwrap();
    for lead in ["> warning: instance ", "> error: instance "] {
        let line = note
            .lines()
            .find(|line| line.starts_with(lead))
            .unwrap_or_else(|| panic!("the identity-tier Note quotes no {lead:?} line:\n{note}"));
        assert!(
            line.starts_with(&format!("{lead}<dest8> was heard under identity <old>")),
            "the quoted presence line does not name the instance as <dest8>: {line}"
        );
        assert!(
            !line.contains("<label or dest8>"),
            "a presence line has no label to print: {line}"
        );
    }

    let record_lines: Vec<&str> = trust_model[..note_at]
        .lines()
        .filter(|line| {
            line.starts_with("error: instance ") || line.starts_with("warning: instance ")
        })
        .collect();
    assert!(
        !record_lines.is_empty(),
        "Mesh-Trust-Model.md quotes the record-collision lines above the Note"
    );
    for line in record_lines {
        assert!(
            line.starts_with("error: instance <label or dest8> is bound to identity <old>")
                || line
                    .starts_with("warning: instance <label or dest8> is bound to identity <old>"),
            "a record-collision line keeps its label: {line}"
        );
    }

    let source = read(repo_root().join("src/mesh/trust.rs"));
    let body = source
        .split("\nfn presence_collision_text(")
        .nth(1)
        .expect("src/mesh/trust.rs defines presence_collision_text")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        body.contains("let instance = short(old_destination);"),
        "presence_collision_text no longer shortens the destination; re-read the wiki's <dest8>"
    );
}

/// The `.mesh peers` section on `Mesh-Commands.md` lists the trust labels `trust_label`
/// prints and reads `denied` as the grant's standing, not as how the node answered the
/// row: nothing there ties a label to `collision_protection` or to a served verdict.
#[test]
fn the_mesh_peers_section_labels_rows_by_the_grant_and_says_nothing_of_a_served_verdict() {
    let Some(wiki) = wiki_dir() else { return };
    let commands = read(wiki.join("Mesh-Commands.md"));
    let heading = "\n### `.mesh peers`\n";
    let section_at = commands
        .find(heading)
        .expect("Mesh-Commands.md has a `### .mesh peers` section");
    let section = commands[section_at + 1..]
        .split("\n### ")
        .next()
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    let lead = "Trust is one of ";
    let sentence_at = section.find(lead).unwrap_or_else(|| {
        panic!("the `.mesh peers` section does not list the trust labels:\n{section}")
    });
    let sentence = section[sentence_at..].split(". ").next().unwrap();
    assert_eq!(
        sentence,
        "Trust is one of `trusted`, `untrusted`, `denied`, `blocked`; `denied` is an instance `.mesh untrust` refused while its identity stays trusted"
    );
    for phrase in [
        "collision_protection",
        "by the verdict",
        "serves it under",
        "reads as refused",
        "how the node answers",
    ] {
        assert!(
            !section.contains(phrase),
            "the `.mesh peers` section ties a trust label to the verdict ({phrase:?}):\n{section}"
        );
    }

    let source = read(repo_root().join("src/function/mesh.rs"));
    let body = source
        .split("\npub(crate) fn trust_label(")
        .nth(1)
        .expect("src/function/mesh.rs defines trust_label")
        .split("\n}\n")
        .next()
        .unwrap();
    let mut labels = string_literals(body);
    labels.sort();
    let (listed, _) = sentence[lead.len()..].split_once(';').unwrap();
    let mut named: Vec<String> = listed
        .split(", ")
        .map(|label| unbacktick(label, "trust label").to_string())
        .collect();
    named.sort();
    assert_eq!(
        named, labels,
        "the wiki's trust labels are not trust_label's"
    );
}

/// The `from_secs(N)` of the one `const NAME: Duration` line in `path`.
fn const_secs(path: &str, name: &str) -> u64 {
    let source = read(repo_root().join(path));
    let line = source
        .lines()
        .find(|line| line.contains(&format!("const {name}: Duration = Duration::from_secs(")))
        .unwrap_or_else(|| panic!("{path} does not define const {name}"));
    let (_, rest) = line.split_once("from_secs(").unwrap();
    rest.split(')')
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("{path} {name}: {e}"))
}

/// The value of the one `const NAME: u64 = ...;` line in `path`, following an alias to
/// another constant in the same file.
fn const_u64(path: &str, name: &str) -> u64 {
    let source = read(repo_root().join(path));
    let mut name = name.to_string();
    loop {
        let line = source
            .lines()
            .find(|line| line.contains(&format!("const {name}: u64 = ")))
            .unwrap_or_else(|| panic!("{path} does not define const {name}"));
        let (_, value) = line.split_once(" = ").unwrap();
        let value = value.trim().trim_end_matches(';');
        if value.starts_with(|c: char| c.is_ascii_digit()) {
            return value
                .replace('_', "")
                .parse()
                .unwrap_or_else(|e| panic!("{path} {name}: {e}"));
        }
        name = value.to_string();
    }
}

/// Usage probe: Mesh-Configuration.md promises the budget and timer semantics the code
/// enforces. The three `peer_max_*` budgets say `0 = unlimited` on every surface that
/// documents them (wiki, template, example, README) and none still says "must be 1 or
/// more", which `knock_retention_hours` keeps on all four; the two refusal rows for the
/// timers quote the sentence `validate` emits, floor and cap and all; the floors, the
/// cap, `/status`'s 30 s, `/fetch`'s 120 s and the status sweep's 5 s the Timers section
/// cites are the constants in the code; and the timers are shown as `null` in every
/// documented default.
#[test]
fn usage_probe_the_configuration_page_promises_the_budget_and_timer_semantics_the_code_enforces() {
    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Configuration.md"));
    let template = read(repo_root().join("assets/config-template.yaml"));
    let example = read(repo_root().join("config.example.yaml"));
    let readme = read(repo_root().join("README.md"));

    let yaml_line = |label: &str, text: &str, key: &str| -> String {
        text.lines()
            .find(|line| line.trim_start().starts_with(&format!("{key}:")) && line.contains('#'))
            .unwrap_or_else(|| panic!("{label} has no commented `{key}:` line"))
            .to_string()
    };
    let readme_row = |key: &str| -> String {
        readme
            .lines()
            .find(|line| line.starts_with(&format!("| `mesh.{key}`")))
            .unwrap_or_else(|| panic!("README.md has no `mesh.{key}` row"))
            .to_string()
    };
    let wiki_row = |key: &str| -> String {
        page.lines()
            .find(|line| line.starts_with(&format!("| `{key}`")))
            .unwrap_or_else(|| panic!("Mesh-Configuration.md has no `{key}` table row"))
            .to_string()
    };

    for key in [
        "peer_max_concurrent",
        "peer_max_messages_per_hour",
        "peer_max_tokens_per_hour",
    ] {
        for (label, line) in [
            (
                "Mesh-Configuration.md",
                yaml_line("Mesh-Configuration.md", &page, key),
            ),
            (
                "assets/config-template.yaml",
                yaml_line("template", &template, key),
            ),
            ("config.example.yaml", yaml_line("example", &example, key)),
        ] {
            assert!(
                line.ends_with("0 = unlimited)"),
                "{label}'s `{key}` comment does not end with `0 = unlimited)`: {line}"
            );
            assert!(
                !line.contains("must be 1 or more"),
                "{label}'s `{key}` comment still says must be 1 or more: {line}"
            );
        }
        for (label, row) in [
            ("README.md", readme_row(key)),
            ("Mesh-Configuration.md", wiki_row(key)),
        ] {
            assert!(
                row.contains("`0` = unlimited"),
                "{label}'s `{key}` row does not say `0` = unlimited: {row}"
            );
            assert!(
                !row.contains("must be `1` or more"),
                "{label}'s `{key}` row still says must be `1` or more: {row}"
            );
        }
    }
    for (label, line) in [
        (
            "assets/config-template.yaml",
            yaml_line("template", &template, "knock_retention_hours"),
        ),
        (
            "config.example.yaml",
            yaml_line("example", &example, "knock_retention_hours"),
        ),
        ("README.md", readme_row("knock_retention_hours")),
    ] {
        assert!(
            line.contains("must be 1 or more") || line.contains("must be `1` or more"),
            "{label}'s knock_retention_hours no longer says must be 1 or more: {line}"
        );
    }
    assert!(
        wiki_row("knock_retention_hours").contains(">= 1"),
        "{}",
        wiki_row("knock_retention_hours")
    );

    let request_floor = const_secs("src/mesh/message.rs", "PEER_REQUEST_TIMEOUT");
    let link_floor = const_secs("src/mesh/r3/client.rs", "DEFAULT_LINK_TIMEOUT");
    let status = const_secs("src/mesh/r3/client.rs", "DEFAULT_REQUEST_TIMEOUT");
    let fetch = const_secs("src/mesh/fetch.rs", "FILE_FETCH_REQUEST_TIMEOUT");
    let sweep = const_secs("src/function/mesh.rs", "STATUS_REQUEST_TIMEOUT");
    let cap = const_u64("src/config/mesh_config.rs", "MAX_TIMEOUT_SECS");

    let config_source = read(repo_root().join("src/config/mesh_config.rs"));
    let validate_body = config_source
        .split("\n    pub fn validate(")
        .nth(1)
        .expect("src/config/mesh_config.rs defines MeshConfig::validate")
        .split("\n    }\n")
        .next()
        .unwrap();
    let sentence = string_literals(validate_body)
        .into_iter()
        .find(|literal| {
            literal.starts_with("mesh.{name} is {value}, which is out of range; use {floor} (")
        })
        .expect("src/config/mesh_config.rs emits the timer-floor sentence");
    let refusal_row = |key: &str| -> String {
        page.lines()
            .find(|line| line.starts_with(&format!("| `{key}` under ")))
            .unwrap_or_else(|| panic!("Mesh-Configuration.md has no refusal row for `{key}`"))
            .to_string()
    };
    for (key, floor) in [
        ("request_timeout_secs", request_floor),
        ("link_timeout_secs", link_floor),
    ] {
        let row = refusal_row(key);
        let cells = table_cells(&row);
        assert_eq!(cells.len(), 2, "{row}");
        assert_eq!(
            cells[0],
            format!("`{key}` under {floor} or over {cap}"),
            "{row}"
        );
        let expected = sentence
            .replace("{name}", key)
            .replace("{value}", "<value>")
            .replace("{floor}", &floor.to_string())
            .replace("{MAX_TIMEOUT_SECS}", &cap.to_string());
        assert_eq!(
            unbacktick(&cells[1], "refusal"),
            expected,
            "Mesh-Configuration.md's `{key}` refusal row is not validate's sentence"
        );
        assert!(
            wiki_row(key).contains(&format!("| {floor} to {cap} when enabled |")),
            "{}",
            wiki_row(key)
        );
        for (label, line) in [
            (
                "Mesh-Configuration.md",
                yaml_line("Mesh-Configuration.md", &page, key),
            ),
            (
                "assets/config-template.yaml",
                yaml_line("template", &template, key),
            ),
            ("config.example.yaml", yaml_line("example", &example, key)),
        ] {
            assert!(
                line.trim_start().starts_with(&format!("{key}: null ")),
                "{label} does not document `{key}: null`: {line}"
            );
            assert!(
                line.contains(&format!("under {floor} is refused")),
                "{label}'s `{key}` comment does not name the floor {floor}: {line}"
            );
            assert!(
                line.ends_with(&format!("; {floor} to {cap})")),
                "{label}'s `{key}` comment does not end with the range: {line}"
            );
            assert!(line.contains("never lowers"), "{label}: {line}");
        }
        assert!(readme_row(key).contains("| `null`"), "{}", readme_row(key));
    }

    let heading = "\n## Timers\n";
    let timers = &page[page
        .find(heading)
        .expect("Mesh-Configuration.md has a Timers section")..];
    let timers = timers[1..].split("\n## ").next().unwrap();
    for needle in [
        format!(
            "{request_floor} s / {link_floor} s on `/message`, `/knock`, `/list` and `/access`"
        ),
        format!("{status} s / {link_floor} s on `/status`"),
        format!("{fetch} s / {link_floor} s on `/fetch`"),
        format!("({request_floor} for the request, {link_floor} for the link)"),
        format!("`with_status: true` ({sweep} s / {sweep} s)"),
        format!("over one year ({cap})"),
        "neither ever lowers one".to_string(),
        "`.mesh info`".to_string(),
        "`null` while unset".to_string(),
    ] {
        assert!(
            timers.contains(&needle),
            "the Timers section does not say {needle:?}:\n{timers}"
        );
    }
}

/// The `envoy_memory:` block is documented wherever the other mesh keys are: the template,
/// the example, the README table and the Configuration page's yaml block and Keys table
/// each carry all six keys; every documented default is the `DEFAULT_ENVOY_MEMORY_*`
/// constant (and `enabled` the `false` of the `Default` impl); the template, example and
/// wiki yaml lines are byte-equal; and `enabled`'s README and wiki rows say the memory is
/// off by default.
#[test]
fn every_envoy_memory_key_is_documented_on_every_surface_with_the_default_the_code_uses() {
    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Configuration.md"));
    let template = read(repo_root().join("assets/config-template.yaml"));
    let example = read(repo_root().join("config.example.yaml"));
    let readme = read(repo_root().join("README.md"));

    // The lines indented deeper than `envoy_memory:`, so a leaf such as `enabled:` is
    // looked for in the block and not found on `mesh.enabled`.
    let block = |label: &str, text: &str| -> String {
        let lines: Vec<&str> = text.lines().collect();
        let start = lines
            .iter()
            .position(|line| line.trim_start().starts_with("envoy_memory:"))
            .unwrap_or_else(|| panic!("{label} has no `envoy_memory:` block"));
        let indent = |line: &str| line.len() - line.trim_start().len();
        let head = indent(lines[start]);
        lines[start + 1..]
            .iter()
            .take_while(|line| indent(line) > head)
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
    };
    let template_block = block("assets/config-template.yaml", &template);
    let example_block = block("config.example.yaml", &example);
    let page_block = block("Mesh-Configuration.md", &page);
    let yaml_line = |label: &str, block: &str, key: &str| -> String {
        block
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("{key}:")) && line.contains('#'))
            .unwrap_or_else(|| {
                panic!("{label}'s envoy_memory block has no commented `{key}:` line")
            })
            .to_string()
    };
    let readme_row = |key: &str| -> String {
        readme
            .lines()
            .find(|line| line.starts_with(&format!("| `mesh.envoy_memory.{key}`")))
            .unwrap_or_else(|| panic!("README.md has no `mesh.envoy_memory.{key}` row"))
            .to_string()
    };
    let wiki_row = |key: &str| -> String {
        page.lines()
            .find(|line| line.starts_with(&format!("| `envoy_memory.{key}`")))
            .unwrap_or_else(|| {
                panic!("Mesh-Configuration.md has no `envoy_memory.{key}` table row")
            })
            .to_string()
    };

    let config_source = read(repo_root().join("src/config/mesh_config.rs"));
    let default_impl = config_source
        .split("\nimpl Default for EnvoyMemoryConfig {")
        .nth(1)
        .expect("src/config/mesh_config.rs implements Default for EnvoyMemoryConfig")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        default_impl.contains("enabled: false,"),
        "EnvoyMemoryConfig::default no longer turns the memory off:\n{default_impl}"
    );
    let bound = |name: &str| const_u64("src/config/mesh_config.rs", name).to_string();
    let keys = [
        ("enabled", "false".to_string()),
        ("max_sessions", bound("DEFAULT_ENVOY_MEMORY_MAX_SESSIONS")),
        (
            "max_per_identity",
            bound("DEFAULT_ENVOY_MEMORY_MAX_PER_IDENTITY"),
        ),
        ("max_turns", bound("DEFAULT_ENVOY_MEMORY_MAX_TURNS")),
        ("max_bytes", bound("DEFAULT_ENVOY_MEMORY_MAX_BYTES")),
        ("ttl_hours", bound("DEFAULT_ENVOY_MEMORY_TTL_HOURS")),
    ];

    for (key, default) in &keys {
        let template_line = yaml_line("assets/config-template.yaml", &template_block, key);
        assert_eq!(
            yaml_line("config.example.yaml", &example_block, key),
            template_line,
            "config.example.yaml's `envoy_memory.{key}` line differs from the template"
        );
        assert_eq!(
            yaml_line("Mesh-Configuration.md", &page_block, key),
            template_line,
            "Mesh-Configuration.md's `envoy_memory.{key}` line differs from the template"
        );
        assert!(
            template_line
                .trim_start()
                .starts_with(&format!("{key}: {default} ")),
            "the template's `envoy_memory.{key}` value is not {default}: {template_line}"
        );
        assert!(
            template_line.contains(&format!("(default: {default}")),
            "the template's `envoy_memory.{key}` comment does not name the default {default}: {template_line}"
        );
        for (label, row) in [
            ("README.md", readme_row(key)),
            ("Mesh-Configuration.md", wiki_row(key)),
        ] {
            assert!(
                table_cells(&row)
                    .iter()
                    .any(|cell| cell == &format!("`{default}`")),
                "{label}'s `envoy_memory.{key}` row does not carry the default `{default}`: {row}"
            );
        }
        if *key == "enabled" {
            for (label, row) in [
                ("README.md", readme_row(key)),
                ("Mesh-Configuration.md", wiki_row(key)),
            ] {
                assert!(
                    row.contains("off by default"),
                    "{label}'s `envoy_memory.enabled` row does not say the memory is off by default: {row}"
                );
            }
        } else {
            assert!(
                template_line.ends_with("must be 1 or more)"),
                "the template's `envoy_memory.{key}` comment does not end with `must be 1 or more)`: {template_line}"
            );
            assert!(
                readme_row(key).contains("must be `1` or more"),
                "{}",
                readme_row(key)
            );
            assert_eq!(
                table_cells(&wiki_row(key)).last().map(String::as_str),
                Some(">= 1 when enabled"),
                "{}",
                wiki_row(key)
            );
        }
    }
}
