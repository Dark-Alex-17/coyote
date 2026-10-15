//! Opt-in doc lint: checks the wiki's mesh pages and the repo README against the
//! code they describe.
//!
//! The wiki is a separate clone, so these tests run only when `COYOTE_WIKI_DIR`
//! points at it; a relative value is resolved against the crate root:
//!
//! ```sh
//! COYOTE_WIKI_DIR=../coyote.wiki cargo test --test mesh_wiki_docs
//! ```
//!
//! Without the variable the wiki half of every test is skipped (printed as
//! `skipping:`) and the repo-only assertions still run, so a plain `cargo test`
//! is unaffected. With it set to a directory that has no `Mesh.md` the tests
//! fail rather than pass against nothing.
//!
//! There is no lib target, so the code side of each check is read from the repo's text:
//! the REPL `VERBS` table, `interface_warnings` and `interface_row` in `src/repl/mesh.rs`,
//! the failure texts and `InterfaceState` rows in `src/mesh/node.rs`, the hook counts and
//! the `mesh__*` tool names under `src/`, the limits and the empty-`interfaces` bail in
//! `src/config/mesh_config.rs`, the yaml twins (`assets/config-template.yaml`,
//! `config.example.yaml`), `scripts/mesh-relay.{sh,ps1}`, the `Dockerfile`, the README
//! and the propagation node's README. The assertions that read only the repo run on
//! every `cargo test`, ahead of each test's wiki gate.
//! Two rules read the mesh documentation's prose on its own, fenced code and code spans
//! set aside: every prose line is ASCII, and none spells planning vocabulary.

use std::env;
use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

const WIKI_DIR: &str = "COYOTE_WIKI_DIR";
const RECIPE: &str = "`COYOTE_WIKI_DIR=../coyote.wiki cargo test --test mesh_wiki_docs`";
const SPEC_REPO: &str = "https://github.com/Dark-Alex-17/scope-protocol";
/// The protocol's name as the wiki's Mesh page and the README's mesh section lead with it.
/// Twin of SCOPE_NAME in src/mesh/pins.rs; both are checked against the README lead.
const SCOPE_LEAD: &str = "SCOPE: Session Coordination & Presence Exchange";
/// The shipped `mesh.interfaces` default as the README's and the Configuration page's
/// Default cells spell it.
const INTERFACES_DEFAULT: &str = "`[{type: private, host: 127.0.0.1, port: 4242}]`";
/// The live-sharing sentence the two-sessions prose carries on the Deployment and
/// Configuration pages alike, whitespace flattened: the trust list is reloaded before
/// every decision and written under its lock, so nothing needs splitting for a change
/// made in one REPL to hold in the other.
const TRUST_FILE_TWIN: &str = "`mesh/trust.yaml` is shared live: a trust, untrust or block made in one REPL is seen by the other before its next decision, and two REPLs never overwrite each other's records; the write is serialised through `mesh/trust.yaml.lock`.";
/// The release from which the image runs rnsd, as the README, the propagation node's
/// README and the Containers page qualify it; one edit site when the cut is numbered
/// differently.
const IMAGE_RNSD_SINCE: &str = "from v0.10.4";

/// The bare release out of [`IMAGE_RNSD_SINCE`].
fn image_rnsd_release() -> &'static str {
    IMAGE_RNSD_SINCE
        .rsplit(' ')
        .next()
        .expect("IMAGE_RNSD_SINCE ends with the release")
}

/// Every `vX.Y.Z` token in `text`, a sentence-final `.` stripped, so a page that names a
/// release names it in every spelling the sweep compares against the one in
/// [`IMAGE_RNSD_SINCE`].
fn release_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
        .map(|word| word.trim_end_matches('.'))
        .filter(|word| {
            word.strip_prefix('v').is_some_and(|rest| {
                rest.split('.').count() == 3
                    && rest
                        .split('.')
                        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .map(str::to_owned)
        .collect()
}

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
fn the_spec_repository_link_appears_before_the_first_section_heading_on_the_pages_that_describe_the_wire()
 {
    let Some(wiki) = wiki_dir() else { return };
    for page in [
        "Mesh.md",
        "Mesh-Commands.md",
        "Mesh-Configuration.md",
        "Mesh-File-Sharing.md",
    ] {
        let text = read(wiki.join(page));
        let links: Vec<usize> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(SPEC_REPO))
            .map(|(index, _)| index + 1)
            .collect();
        assert!(
            !links.is_empty(),
            "{page} never links {SPEC_REPO}; the link belongs in the intro"
        );
        // The verb and key pages keep one canonical pointer in their intro; the overview and file-sharing pages may cite the spec inline.
        if page == "Mesh-Commands.md" || page == "Mesh-Configuration.md" {
            assert_eq!(
                links.len(),
                1,
                "{page} links {SPEC_REPO} on lines {links:?}; expected exactly one"
            );
        }
        let first_heading = text
            .lines()
            .position(|line| line.starts_with("## "))
            .map(|index| index + 1)
            .unwrap_or_else(|| panic!("{page} has no `## ` heading"));
        assert!(
            links[0] < first_heading,
            "{page} first links {SPEC_REPO} on line {} but its first `## ` heading is line {first_heading}; the link belongs in the intro",
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
/// after `inbox/<instance id>/`, `inbox/<instance_id>/` or the `<inbox_dir>/...` spellings
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
/// `.mesh inbox` example) so the check never passes vacuously.
#[test]
fn the_peer_directory_scan_judges_each_literal_and_reads_crlf_alike() {
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
    let readme = read(repo_root().join("README.md"));
    let mut after_heading = readme.lines().skip_while(|line| *line != "### Mesh");
    after_heading
        .next()
        .expect("README.md has a `### Mesh` heading");
    let lead = after_heading
        .find(|line| !line.trim().is_empty())
        .expect("README.md has text under `### Mesh`");
    assert!(
        lead.starts_with(SCOPE_LEAD),
        "README.md's `### Mesh` section does not open with {SCOPE_LEAD:?}: {lead:?}"
    );
    let section = readme
        .lines()
        .skip_while(|line| *line != "### Mesh")
        .skip(1)
        .take_while(|line| !line.starts_with("### ") && !line.starts_with("## "))
        .collect::<Vec<&str>>()
        .join("\n");
    assert!(
        section.contains(SPEC_REPO),
        "README.md's `### Mesh` section never links {SPEC_REPO}"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh.md"));
    let first = page.lines().next().unwrap_or_default();
    assert!(
        first.starts_with(SCOPE_LEAD),
        "Mesh.md line 1 does not open with {SCOPE_LEAD:?}: {first:?}"
    );
}

/// Every spelling of the staging root the wiki uses (`inbox/<instance id>/`,
/// `inbox/<instance_id>/` and the two `<inbox_dir>/...` forms) is judged alike: the peer
/// segment ends at `/`, a backtick, a space, `)`, `,`, `;` or the line end, so a shortened
/// hash followed by sentence punctuation is quoted WITHOUT that punctuation in the hit; the
/// full lowercase hash in the same positions is never a hit; and each retired placeholder
/// is named in its own hit. The wiki itself exercises the `<inbox_dir>` spelling (the
/// `Mesh-Configuration.md` `inbox_dir` paragraph), so that marker is not a dead branch.
#[test]
fn every_root_spelling_and_segment_terminator_judges_the_peer_segment_alike() {
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
/// README) and the staging-inbox layout check, so an operator can tell from the docs what
/// turning the variable on will judge.
#[test]
fn the_docs_name_the_recipe_and_every_page_family_the_lint_reads() {
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
        commented_entry(
            label,
            text,
            "`  collision_protection: false    #` line",
            |line| line.starts_with("  collision_protection: false    #"),
        )
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
        "whose index cannot be read is counted and named so you can empty it with `.mesh memory forget all`",
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

/// `ttl_hours` counts from the thread's last *written* exchange: a load that finds a
/// thread does not renew it. Every surface an operator reads the key on says so in the
/// same words: the template and example comments, the README and Configuration table
/// rows, the Configuration page's yaml line and the field's own rustdoc; none of them
/// says "after its last message".
#[test]
fn every_ttl_hours_twin_counts_from_the_last_written_exchange() {
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

/// The identity-tier Note on `Mesh-Trust-Model.md` quotes the two presence lines with the
/// instance as `<dest8>` (`presence_collision_text` shortens the old destination and has
/// no label to print), while the record-collision lines quoted above it keep
/// `<label or dest8>`, as `key_change_text` prints the record's label when it has one.
/// The placeholders are not interchangeable: a reader copying the wiki's line must know
/// which one names a labelled record and which one never does.
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

/// The `.mesh peers` section quotes the footer legend as `peers_legend` renders it: the
/// sample ends with it and the prose repeats it. The legend's numbers come from the
/// constants, so the expected line is built from `peers_legend`'s own format literal and
/// the heartbeat, period, stale and age-out constants read from source; `hourly` is the
/// word the code picks for a heartbeat of exactly one hour, and the age-out is rendered
/// by the rule `period_text` applies, `about N h` when it runs past the hour.
#[test]
fn the_mesh_peers_section_quotes_the_legend_as_the_code_renders_it() {
    let heartbeat = const_u64("src/mesh/announce.rs", "HEARTBEAT_SECS");
    assert_eq!(
        heartbeat, 3600,
        "the legend says `hourly`; reword heartbeat_cadence and this pin with the heartbeat"
    );
    let period = const_u64("src/mesh/announce.rs", "HEARTBEAT_PERIOD_SECS");
    let peers = read(repo_root().join("src/mesh/peers.rs"));
    let stale_line = peers
        .lines()
        .find(|line| line.contains("const PEER_STALE_AFTER: Duration = Duration::from_secs("))
        .expect("src/mesh/peers.rs derives PEER_STALE_AFTER from the heartbeat");
    let stale_heartbeats: u64 = stale_line
        .split_once("from_secs(")
        .unwrap()
        .1
        .split(" * HEARTBEAT_PERIOD_SECS")
        .next()
        .unwrap()
        .parse()
        .expect("PEER_STALE_AFTER is a whole number of heartbeats");
    let announce = read(repo_root().join("src/mesh/announce.rs"));
    let missed_line = announce
        .lines()
        .find(|line| line.contains("const PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT: u32 = "))
        .expect("src/mesh/announce.rs defines PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT");
    let missed: u64 = missed_line
        .split_once(" = ")
        .unwrap()
        .1
        .trim_end_matches(';')
        .parse()
        .expect("PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT is a literal");
    let ttl_secs = period * missed;
    let age_out = hours_text(ttl_secs);

    let template = string_literals(&fn_body("src/repl/mesh.rs", "peers_legend"))
        .into_iter()
        .next()
        .expect("peers_legend formats one literal");
    let pieces: Vec<&str> = template.split("{}").collect();
    assert_eq!(
        pieces.len(),
        4,
        "peers_legend fills three placeholders: {template:?}"
    );
    let legend = format!(
        "{}{stale_heartbeats}{}hourly{}{age_out}{}",
        pieces[0], pieces[1], pieces[2], pieces[3]
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Commands.md"));
    let peers_section = section("Mesh-Commands.md", &page, "### `.mesh peers`");
    let sample = peers_section
        .split("```text\n")
        .nth(1)
        .and_then(|block| block.split("\n```").next())
        .expect("the `.mesh peers` section opens with a text sample");
    assert_eq!(
        sample.lines().last().unwrap_or_default(),
        legend,
        "the `.mesh peers` sample does not end with the legend the code prints"
    );
    let prose = flat(&prose_outside_fences(&peers_section));
    assert!(
        prose.contains(&legend),
        "the `.mesh peers` prose does not quote the legend {legend:?}:\n{prose}"
    );
}

/// `text` with every fenced code block removed, so a pin on a page's prose is not met by
/// a sample alone.
fn prose_outside_fences(text: &str) -> String {
    text.split("```").step_by(2).collect::<Vec<_>>().join(" ")
}

/// The heartbeat period is stated on the pages in words that follow `HEARTBEAT_SECS`:
/// Hooks.md quotes the `mesh.peer.discovered` throttle as `every {HEARTBEAT_SECS} s`, the
/// pages that count a wait in heartbeats say `an hour`, and none of them keeps a spelling
/// of the 15-minute heartbeat the constant was before. The stale spellings looked for are
/// the ones that named the heartbeat, so the lend and grant `15 minutes` and lxmd's `30
/// minutes`, other periods, can be reworded freely. The pages that explain the ticker's
/// `a minute of slack` follow `HEARTBEAT_SLACK_SECS`.
#[test]
fn the_pages_state_the_heartbeat_period_as_the_constant_sets_it() {
    let heartbeat = const_u64("src/mesh/announce.rs", "HEARTBEAT_SECS");
    assert_eq!(
        heartbeat, 3600,
        "the pages say `an hour`; reword them and this pin with the heartbeat"
    );
    assert_eq!(
        const_u64("src/mesh/announce.rs", "HEARTBEAT_SLACK_SECS"),
        60,
        "the pages say `a minute of slack`; reword them and this pin with the slack"
    );
    let Some(wiki) = wiki_dir() else { return };

    let hooks = flat(&read(wiki.join("Hooks.md")));
    let throttle = format!("or every {heartbeat} s");
    assert!(
        hooks.contains(&throttle),
        "Hooks.md does not say {throttle:?} of the mesh.peer.discovered throttle"
    );

    for (page, needle) in [
        ("Mesh-Commands.md", "an hour (one peer heartbeat)"),
        ("Mesh.md", "one peer heartbeat (an hour)"),
        ("Mesh-Deployment.md", "No peers after an hour"),
        ("Mesh-Commands.md", "a minute of slack"),
        ("Mesh-Platform-Support.md", "a minute of slack"),
        ("Mesh-Deployment.md", "a minute of slack"),
    ] {
        assert!(
            flat(&read(wiki.join(page))).contains(needle),
            "{page} does not say {needle:?}"
        );
    }

    let mut pages = mesh_pages(&wiki);
    pages.push(wiki.join("Hooks.md"));
    for path in pages {
        let text = flat(&read(&path));
        for stale in [
            "every 900 s",
            "15 minutes (one peer heartbeat)",
            "No peers after 15 minutes",
            "every 15 minutes",
        ] {
            assert!(
                !text.contains(stale),
                "{} still says {stale:?} of the heartbeat; it is {heartbeat} s",
                path.display()
            );
        }
    }
}

/// The Mesh and Deployment pages tell a new joiner what to expect of the peer table in the
/// same words: a peer never heard before shows up within one heartbeat, and a peer already
/// in the table answers without waiting for one. `an hour` is the heartbeat.
#[test]
fn the_mesh_and_deployment_pages_promise_peers_within_a_heartbeat_in_the_same_words() {
    let heartbeat = const_u64("src/mesh/announce.rs", "HEARTBEAT_SECS");
    assert_eq!(
        heartbeat, 3600,
        "the pages say `within an hour`; reword them and this pin with the heartbeat"
    );
    let Some(wiki) = wiki_dir() else { return };
    for page in ["Mesh.md", "Mesh-Deployment.md"] {
        let text = flat(&read(wiki.join(page)));
        for needle in [
            "Brand-new peers appear within an hour",
            "known peers are reachable at once",
        ] {
            assert!(text.contains(needle), "{page} does not say {needle:?}");
        }
    }
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

/// The value of the one `const NAME: u64 = ...;` line in `path`: a literal, an alias to
/// another constant in the same file, or a `+` sum of those.
fn const_u64(path: &str, name: &str) -> u64 {
    let source = read(repo_root().join(path));
    let line = source
        .lines()
        .find(|line| line.contains(&format!("const {name}: u64 = ")))
        .unwrap_or_else(|| panic!("{path} does not define const {name}"));
    let (_, value) = line.split_once(" = ").unwrap();
    value
        .trim()
        .trim_end_matches(';')
        .split(" + ")
        .map(|term| {
            if term.starts_with(|c: char| c.is_ascii_digit()) {
                term.replace('_', "")
                    .parse()
                    .unwrap_or_else(|e| panic!("{path} {name}: {e}"))
            } else {
                const_u64(path, term)
            }
        })
        .sum()
}

/// A period of an hour or more as `period_text` in `src/repl/mesh.rs` renders it: `N h`
/// on the hour, else `about N h` to the nearest hour.
fn hours_text(secs: u64) -> String {
    if secs.is_multiple_of(3600) {
        format!("{} h", secs / 3600)
    } else {
        format!("about {} h", (secs + 1800) / 3600)
    }
}

/// Mesh-Configuration.md promises the budget and timer semantics the code enforces. The
/// three `peer_max_*` budgets say `0 = unlimited` on every surface that documents them
/// (wiki, template, example, README) and none says "must be 1 or more", which
/// `knock_retention_hours` keeps on all four; the two refusal rows for the timers quote
/// the sentence `validate` emits, floor and cap and all; the floors, the cap, `/status`'s
/// 30 s, `/fetch`'s 120 s and the status sweep's 5 s the Timers section cites are the
/// constants in the code; and the timers are shown as `null` in every documented
/// default.
#[test]
fn the_configuration_page_promises_the_budget_and_timer_semantics_the_code_enforces() {
    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Configuration.md"));
    let template = read(repo_root().join("assets/config-template.yaml"));
    let example = read(repo_root().join("config.example.yaml"));
    let readme = read(repo_root().join("README.md"));

    let yaml_line = |label: &str, text: &str, key: &str| -> String {
        commented_entry(label, text, &format!("commented `{key}:` line"), |line| {
            line.trim_start().starts_with(&format!("{key}:")) && line.contains('#')
        })
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
                !comment_text(&line).contains("must be 1 or more"),
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
    let link_floor = const_secs("src/mesh/request/client.rs", "DEFAULT_LINK_TIMEOUT");
    let status = const_secs("src/mesh/request/client.rs", "DEFAULT_REQUEST_TIMEOUT");
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
            let comment = comment_text(&line);
            assert!(
                line.trim_start().starts_with(&format!("{key}: null ")),
                "{label} does not document `{key}: null`: {line}"
            );
            assert!(
                comment.contains(&format!("under {floor} is refused")),
                "{label}'s `{key}` comment does not name the floor {floor}: {line}"
            );
            assert!(
                line.ends_with(&format!("; {floor} to {cap})")),
                "{label}'s `{key}` comment does not end with the range: {line}"
            );
            assert!(comment.contains("never lowers"), "{label}: {line}");
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
        commented_entry(
            label,
            block,
            &format!("commented `{key}:` line in its envoy_memory block"),
            |line| line.trim_start().starts_with(&format!("{key}:")) && line.contains('#'),
        )
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

/// The Mesh page's lead, the text before its first section, names the local daemon and
/// the loopback endpoint the shipped default dials.
#[test]
fn the_mesh_page_names_the_local_daemon_and_its_loopback_endpoint_before_the_first_section() {
    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh.md"));
    let lead = lead(&page);
    for needle in ["rnsd", "127.0.0.1:4242"] {
        assert!(
            lead.contains(needle),
            "Mesh.md does not name {needle} before its first `## ` heading:\n{lead}"
        );
    }
}

/// The first line of `text` that `is_key` accepts, followed by the comment-only lines
/// right under it whose `#` sits in the key line's `#` column, joined with `\n`: one
/// commented yaml entry as the template wraps a long comment onto continuation lines.
/// A commented-out entry such as `# - type: lan` opens its own `#` column and is not
/// gathered. `what` names the entry in the panic when `text` has none.
fn commented_entry(label: &str, text: &str, what: &str, is_key: impl Fn(&str) -> bool) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|line| is_key(line))
        .unwrap_or_else(|| panic!("{label} has no {what}"));
    let column = lines[at].find('#');
    let continuation = lines[at + 1..]
        .iter()
        .take_while(|line| {
            line.find('#')
                .is_some_and(|hash| Some(hash) == column && line[..hash].trim().is_empty())
        })
        .copied();
    std::iter::once(lines[at])
        .chain(continuation)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The comment of a [`commented_entry`] as one line: the text after each line's `#`,
/// trimmed and joined with a space, so a phrase the wrap split is found whole.
fn comment_text(entry: &str) -> String {
    entry
        .lines()
        .filter_map(|line| line.split_once('#').map(|(_, comment)| comment.trim()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `interfaces:` line of `text` and every line indented deeper than it.
fn interfaces_block(label: &str, text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.trim_start().starts_with("interfaces:"))
        .unwrap_or_else(|| panic!("{label} has no `interfaces:` block"));
    let indent = |line: &str| line.len() - line.trim_start().len();
    let head = indent(lines[start]);
    let body = lines[start + 1..]
        .iter()
        .take_while(|line| indent(line) > head)
        .copied();
    std::iter::once(lines[start])
        .chain(body)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `mesh.enabled`'s commented yaml line is one text on every surface and names both
/// moments the REPL joins (a session at start, a session entered later), the `.mesh off`
/// hold and the one-shot exclusion; the README row, the Keys row, the `enabled` section,
/// the Mesh lead and the Deployment walkthrough say the same in prose. The
/// template/example/README comparison runs on every run.
#[test]
fn the_enabled_line_names_the_session_entered_later_on_every_surface() {
    let yaml_line = |label: &str, text: &str| -> String {
        let mesh = text
            .find("\nmesh:\n")
            .unwrap_or_else(|| panic!("{label} has no top-level `mesh:` line"));
        commented_entry(
            label,
            &text[mesh + 1..],
            "commented `mesh.enabled` line",
            |line| line.starts_with("  enabled:") && line.contains('#'),
        )
    };
    let template = yaml_line(
        "assets/config-template.yaml",
        &read(repo_root().join("assets/config-template.yaml")),
    );
    let example = yaml_line(
        "config.example.yaml",
        &read(repo_root().join("config.example.yaml")),
    );
    assert_eq!(
        example, template,
        "config.example.yaml's `mesh.enabled` line differs from the template"
    );
    let comment = comment_text(&template);
    for needle in [
        "whenever this REPL has a session",
        "at start with --session",
        "when you enter one with .session",
        ".mesh off keeps it off for the rest of the run",
        "one-shot runs (--macro, a prompt on the command line) never join",
        "Nothing leaves the machine while this is false",
    ] {
        assert!(
            comment.contains(needle),
            "the template's `mesh.enabled` comment does not say {needle:?}: {comment}"
        );
    }
    let readme_row = read(repo_root().join("README.md"))
        .lines()
        .find(|line| line.starts_with("| `mesh.enabled`"))
        .expect("README.md has a `mesh.enabled` row")
        .to_string();
    for needle in [
        "when you enter one with `.session`",
        "`.mesh off` keeps it off for the rest of the run",
        "one-shot runs never join",
    ] {
        assert!(
            readme_row.contains(needle),
            "README.md's `mesh.enabled` row does not say {needle:?}: {readme_row}"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let configuration = read(wiki.join("Mesh-Configuration.md"));
    assert_eq!(
        yaml_line("Mesh-Configuration.md", &configuration),
        template,
        "Mesh-Configuration.md's `mesh.enabled` yaml line differs from the template"
    );
    let keys_row = configuration
        .lines()
        .find(|line| line.starts_with("| `enabled`"))
        .expect("Mesh-Configuration.md's Keys table has an `enabled` row");
    assert!(
        keys_row.contains("starts with a session or when you enter one"),
        "{keys_row}"
    );
    let prose = |page: &str, needles: &[&str]| {
        let flat = read(wiki.join(page))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for needle in needles {
            assert!(flat.contains(needle), "{page} does not say {needle:?}");
        }
    };
    prose(
        "Mesh-Configuration.md",
        &[
            "when you enter one later with `.session <name>` or `.agent <name> <session>`",
            "A macro run from the prompt whose steps enter a session joins once the macro is done.",
            "A typed `.mesh off` holds for the rest of the run",
            "`.exit session` leaves the mesh, and so does `.exit agent` while the agent holds a session",
        ],
    );
    prose(
        "Mesh.md",
        &[
            "or when you enter one with `.session <name>` or `.agent <name> <session>`; leaving the session by any path leaves the mesh.",
        ],
    );
    prose(
        "Mesh-Deployment.md",
        &[
            "`.session a` then joins by itself",
            "`.mesh off` keeps the mesh off for the rest of the run",
            "`.exit session` leaves the mesh",
        ],
    );
    prose(
        "Mesh-Commands.md",
        &[
            "it stays off for the rest of this run; `.mesh on` joins again",
            "Leaving the mesh: the session that owns this node's instance id is being exited.",
        ],
    );
}

/// The `interfaces:` block is one text on every surface: the example and the
/// Configuration page's yaml carry the template's lines byte for byte, and the template's
/// default is the private loopback entry, which the page's Keys table spells as the README does.
/// The template/example comparison runs on every run.
#[test]
fn the_interfaces_block_is_byte_identical_across_the_template_the_example_and_the_configuration_page()
 {
    let template = interfaces_block(
        "assets/config-template.yaml",
        &read(repo_root().join("assets/config-template.yaml")),
    );
    let example = interfaces_block(
        "config.example.yaml",
        &read(repo_root().join("config.example.yaml")),
    );
    assert_eq!(
        example, template,
        "config.example.yaml's `interfaces:` block differs from the template"
    );
    for needle in ["type: private", "127.0.0.1"] {
        assert!(
            template.contains(needle),
            "the template's `interfaces:` block does not say {needle}:\n{template}"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let configuration = read(wiki.join("Mesh-Configuration.md"));
    let page = interfaces_block("Mesh-Configuration.md", &configuration);
    assert_eq!(
        page, template,
        "Mesh-Configuration.md's `interfaces:` block differs from the template"
    );
    let row = configuration
        .lines()
        .find(|line| line.starts_with("| `interfaces`"))
        .expect("Mesh-Configuration.md's Keys table has an `interfaces` row");
    assert_eq!(
        table_cells(row)[2],
        INTERFACES_DEFAULT,
        "Mesh-Configuration.md's `interfaces` Default cell is not the shipped loopback entry: {row}"
    );
}

/// `label: spells <needle>` for every needle `text` spells, whitespace runs collapsed to
/// one space and case ignored, so a phrase wrapped across lines is caught.
fn local_routing_hits(label: &str, text: &str, needles: &[&str]) -> Vec<String> {
    let flat = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    needles
        .iter()
        .filter(|needle| flat.contains(&needle.to_lowercase()))
        .map(|needle| format!("{label}: spells {needle}"))
        .collect()
}

/// Sessions on one host reach each other through the host's rnsd; neither a Mesh page nor
/// the in-repo READMEs route them through a relay or point at the retired two-instance
/// recipe. The READMEs are scanned on every run; the wiki pages only when `COYOTE_WIKI_DIR`
/// is set.
#[test]
fn no_mesh_page_or_the_propagation_node_readme_routes_local_sessions_through_the_relay() {
    let needles = [
        "second instance on the same machine reaches the mesh through this relay",
        "doubles as the `private` relay",
        "two instances on one machine therefore go through a relay",
        "two-instance recipe",
    ];
    let wrapped = "so a second instance on the same machine reaches the mesh\n  through this relay with `type: private`.\n";
    let control = local_routing_hits("fixture", wrapped, &needles);
    assert_eq!(
        control.len(),
        1,
        "the scan does not go red on a phrase wrapped across lines: {control:?}"
    );
    let control = local_routing_hits("fixture", "a team relay in rnsd's config\n", &needles);
    assert!(
        control.is_empty(),
        "the team relay in rnsd's config is not local routing: {control:?}"
    );

    let mut pages = vec![
        repo_root().join("README.md"),
        repo_root().join("deployment/propagation-node/README.md"),
    ];
    if let Some(wiki) = wiki_dir() {
        pages.extend(mesh_pages(&wiki));
    }
    let hits: Vec<String> = pages
        .iter()
        .flat_map(|path| local_routing_hits(&path.display().to_string(), &read(path), &needles))
        .collect();
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

/// The text of a `format!` literal outside its `{...}` placeholders, each piece trimmed;
/// pieces shorter than 12 characters are dropped as too common to check, except the first,
/// which anchors the line. A literal with an escaped brace must be unescaped first, as the
/// empty-interfaces check does.
fn format_fragments(literal: &str) -> Vec<String> {
    assert!(
        !literal.contains("{{"),
        "{literal:?} escapes a brace; unescape its doubled braces first, as the empty-interfaces pin does"
    );
    let mut pieces = Vec::new();
    let mut rest = literal;
    while let Some(open) = rest.find('{') {
        pieces.push(&rest[..open]);
        let close = rest[open..]
            .find('}')
            .map(|offset| open + offset)
            .unwrap_or_else(|| panic!("unclosed placeholder in {literal:?}"));
        rest = &rest[close + 1..];
    }
    pieces.push(rest);
    pieces
        .into_iter()
        .enumerate()
        .map(|(index, piece)| (index, piece.trim()))
        .filter(|(index, piece)| !piece.is_empty() && (*index == 0 || piece.chars().count() >= 12))
        .map(|(_, piece)| piece.to_string())
        .collect()
}

/// The source of `name` in `path`: from its `fn` line to the first `\n}\n` after it.
fn fn_body(path: &str, name: &str) -> String {
    let source = read(repo_root().join(path));
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("{path} declares fn {name}"));
    let end = source[start..]
        .find("\n}\n")
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("{path}'s fn {name} does not end with `}}` on its own line"));
    source[start..end].to_string()
}

/// The Commands and Deployment pages quote what `.mesh on` prints for an interface that
/// did not come up as the code formats it: the three `WARNING:` lines, the `Mesh relay`
/// failure text, the hint appended when every configured interface is a loopback relay,
/// the `lan` bind failure and the three `InterfaceState` rows of `.mesh info`. Every piece
/// of text around a placeholder is on both pages byte for byte. The code side is checked
/// on every run.
#[test]
fn the_commands_and_deployment_pages_quote_the_interface_warnings_and_states_as_the_code_prints_them()
 {
    assert_eq!(
        format_fragments("WARNING: {label} is unreachable ({reason}); tail here."),
        ["WARNING:", "is unreachable (", "); tail here."],
        "format_fragments does not split a literal on its placeholders"
    );

    let warnings: Vec<String> = string_literals(&fn_body("src/repl/mesh.rs", "interface_warnings"))
        .into_iter()
        .filter(|literal| literal.starts_with("WARNING: "))
        .collect();
    assert_eq!(
        warnings.len(),
        3,
        "interface_warnings in src/repl/mesh.rs does not format exactly three WARNING lines: {warnings:?}"
    );
    let failures: Vec<String> = string_literals(&fn_body("src/mesh/node.rs", "join_tcp"))
        .into_iter()
        .filter(|literal| literal.starts_with("Mesh relay "))
        .collect();
    assert_eq!(
        failures.len(),
        1,
        "join_tcp in src/mesh/node.rs does not format exactly one `Mesh relay` failure: {failures:?}"
    );
    let hints = string_literals(&fn_body("src/mesh/node.rs", "loopback_hint"));
    assert_eq!(
        hints.len(),
        1,
        "loopback_hint in src/mesh/node.rs does not format exactly one literal: {hints:?}"
    );
    let lan_failures = string_literals(&fn_body("src/mesh/node.rs", "lan_bind_failure_message"));
    assert_eq!(
        lan_failures.len(),
        1,
        "lan_bind_failure_message in src/mesh/node.rs does not format exactly one literal: {lan_failures:?}"
    );
    let node_source = read(repo_root().join("src/mesh/node.rs"));
    let display = node_source
        .split("\nimpl fmt::Display for InterfaceState {")
        .nth(1)
        .expect("src/mesh/node.rs implements Display for InterfaceState")
        .split("\n}\n")
        .next()
        .unwrap();
    let states = string_literals(display);
    assert_eq!(
        states,
        [
            "connected",
            "unreachable, retrying: {reason}",
            "unreachable: {reason}"
        ],
        "InterfaceState's Display pieces moved; update the wiki and this test"
    );

    let Some(wiki) = wiki_dir() else { return };
    for page_name in ["Mesh-Commands.md", "Mesh-Deployment.md"] {
        let page = read(wiki.join(page_name));
        for literal in warnings
            .iter()
            .chain(&failures)
            .chain(&hints)
            .chain(&lan_failures)
            .chain(&states)
        {
            for fragment in format_fragments(literal) {
                assert!(
                    page.contains(&fragment),
                    "{page_name} does not quote {fragment:?} from {literal:?}"
                );
            }
        }
        for needle in ["unreachable, retrying: <reason>", "connected"] {
            assert!(
                page.contains(needle),
                "{page_name} does not show the `.mesh info` interface state {needle:?}"
            );
        }
    }
}

/// The Configuration page's validation table quotes the empty-`interfaces` error as the
/// code bails, braces unescaped. The bail's shape is checked on every run.
#[test]
fn the_configuration_page_quotes_the_empty_interfaces_sentence_as_the_code_bails() {
    let source = read(repo_root().join("src/config/mesh_config.rs"));
    let start = source
        .find("\"mesh.interfaces is empty;")
        .expect("src/config/mesh_config.rs formats the empty-interfaces error");
    let bail = source[..start]
        .rfind("bail!(")
        .expect("the empty-interfaces error is raised with bail!");
    assert!(
        source[bail + "bail!(".len()..start].trim().is_empty(),
        "the empty-interfaces literal is not the bail!'s first argument"
    );
    let end = source[start..]
        .find("\"\n")
        .map(|offset| start + offset)
        .expect("the empty-interfaces literal ends its line");
    let literals = string_literals(&source[bail..=end]);
    assert_eq!(
        literals.len(),
        1,
        "the empty-interfaces bail! does not carry exactly one literal: {literals:?}"
    );
    let sentence = literals[0].replace("{{", "{").replace("}}", "}");

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Configuration.md"));
    assert!(
        page.contains(&sentence),
        "Mesh-Configuration.md does not quote the empty-interfaces error as the code bails:\n{sentence}"
    );
}

/// The text of `page` before its first `## ` heading.
fn lead(page: &str) -> String {
    page.lines()
        .take_while(|line| !line.starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text under `heading`, an `## ` or `### ` line of `page`, up to the next heading of
/// the same or a higher level.
fn section(label: &str, page: &str, heading: &str) -> String {
    let marker = format!("\n{heading}\n");
    let at = page
        .find(&marker)
        .unwrap_or_else(|| panic!("{label} has no `{heading}` heading"));
    let body = &page[at + marker.len()..];
    let level = heading.bytes().take_while(|b| *b == b'#').count();
    let end = (2..=level)
        .filter_map(|depth| body.find(&format!("\n{} ", "#".repeat(depth))))
        .min()
        .unwrap_or(body.len());
    body[..end].to_string()
}

/// `text` with every whitespace run collapsed to one space, so a sentence the page wraps is
/// matched as prose.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The source of the bash function `name` in `path`: from its `name() {` line to the next
/// `}` on its own line.
fn bash_fn_body(path: &str, name: &str) -> String {
    let source = read(repo_root().join(path));
    let head = format!("{name}() {{");
    let start = source
        .find(&head)
        .unwrap_or_else(|| panic!("{path} defines {name}()"));
    let end = source[start..]
        .find("\n}\n")
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("{path}'s {name}() does not end with `}}` on its own line"));
    source[start..end].to_string()
}

/// The lines between each `cat <<'EOF'` or `cat <<EOF` line of `body` and its closing `EOF`.
fn heredoc_bodies(body: &str) -> Vec<String> {
    let mut bodies = Vec::new();
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        if !(line.contains("cat <<'EOF'") || line.contains("cat <<EOF")) {
            continue;
        }
        let mut heredoc = Vec::new();
        loop {
            match lines.next() {
                Some("EOF") => break,
                Some(line) => heredoc.push(line),
                None => panic!("unterminated heredoc in:\n{body}"),
            }
        }
        bodies.push(heredoc.join("\n"));
    }
    bodies
}

/// The anchor GitHub gives a markdown heading: the `#`s stripped, lowercased, every
/// character that is not a letter, digit, space, hyphen or underscore dropped (which takes
/// backticks and the `.` of `.mesh on` with it), spaces to hyphens.
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

/// Every heading line of `page` outside its fenced code blocks.
fn headings(page: &str) -> Vec<&str> {
    let mut fenced = false;
    page.lines()
        .filter(|line| {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                return false;
            }
            !fenced && line.starts_with('#')
        })
        .collect()
}

/// The slug of every heading of `page` outside its fenced code blocks.
fn heading_slugs(page: &str) -> Vec<String> {
    headings(page).into_iter().map(github_slug).collect()
}

/// One hit, `label:line` followed by the `Page#anchor` target, for every `](Page#anchor)`
/// link in `text` whose `Page` is in `anchors` and whose `anchor` is none of that page's
/// heading slugs.
fn dangling_section_links(label: &str, text: &str, anchors: &[(&str, Vec<String>)]) -> Vec<String> {
    let mut dangling = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for (at, _) in line.match_indices("](") {
            let target = &line[at + 2..];
            let Some(close) = target.find(')') else {
                continue;
            };
            let Some((page, anchor)) = target[..close].split_once('#') else {
                continue;
            };
            let Some((_, slugs)) = anchors.iter().find(|(name, _)| *name == page) else {
                continue;
            };
            if !slugs.iter().any(|slug| slug == anchor) {
                dangling.push(format!("{label}:{} → {page}#{anchor}", index + 1));
            }
        }
    }
    dangling
}

/// The block from the line starting `**Which setup?**` through the first following line
/// ending `the loopback hop.`, inclusive.
fn decision_tree(label: &str, page: &str) -> String {
    let lines: Vec<&str> = page.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.starts_with("**Which setup?**"))
        .unwrap_or_else(|| panic!("{label} has no `**Which setup?**` line"));
    let end = lines[start..]
        .iter()
        .position(|line| line.ends_with("the loopback hop."))
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("{label}'s decision tree does not end with `the loopback hop.`"));
    lines[start..=end].join("\n")
}

/// The Mesh page's lead carries the three setups of the decision tree, the trust sentence
/// and the one-command daemon setup; its Pages row for Mesh Deployment names the daemon;
/// the interfaces list spells `private` as the loopback default ahead of `lan`; the Quick
/// start points at the daemon setup; and the inbox section quotes the idle line a late
/// relay prints.
#[test]
fn the_mesh_page_lead_carries_the_three_setups_and_the_pages_row_names_the_daemon() {
    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh.md"));
    let intro = flat(&lead(&page));
    for needle in [
        "Running without rnsd",
        "lxmd",
        "mutually trusted",
        "interface-level switch",
        "--with-mesh",
        "-WithMesh",
        "scripts/mesh-relay.sh",
    ] {
        assert!(
            intro.contains(needle),
            "Mesh.md does not say {needle:?} before its first `## ` heading:\n{intro}"
        );
    }
    let pages = section("Mesh.md", &page, "## Pages");
    let deployment_row = pages
        .lines()
        .find(|line| line.contains("[Mesh Deployment](Mesh-Deployment)"))
        .unwrap_or_else(|| {
            panic!("Mesh.md's Pages section has no Mesh Deployment bullet:\n{pages}")
        });
    assert!(
        deployment_row.contains("rnsd"),
        "the Mesh Deployment bullet does not name rnsd: {deployment_row}"
    );
    let how = section("Mesh.md", &page, "## How it works");
    let bullets: Vec<&str> = how.lines().filter(|line| line.starts_with("- `")).collect();
    let bullet = |kind: &str| {
        bullets
            .iter()
            .position(|line| line.starts_with(&format!("- `{kind}`")))
            .unwrap_or_else(|| panic!("Mesh.md's interfaces list has no `{kind}` bullet:\n{how}"))
    };
    let private_at = bullet("private");
    let lan_at = bullet("lan");
    assert_eq!(
        bullets[private_at],
        "- `private`: TCP client to a relay; the default dials the local rnsd on `127.0.0.1:4242`.",
        "Mesh.md's `private` bullet changed"
    );
    assert_eq!(
        bullets[lan_at],
        "- `lan`: Reticulum AutoInterface, link-local only; the no-daemon fallback.",
        "Mesh.md's `lan` bullet changed"
    );
    assert!(
        private_at < lan_at,
        "Mesh.md lists `lan` before the `private` default:\n{how}"
    );
    let quick_start = flat(&section("Mesh.md", &page, "## Quick start"));
    assert!(
        quick_start.contains("the per-host daemon setup is on"),
        "Mesh.md's Quick start does not point at the daemon setup:\n{quick_start}"
    );
    let inbox = section("Mesh.md", &page, "## Notifications and the inbox");
    assert!(
        inbox.contains("[mesh] <label> connected"),
        "Mesh.md's inbox section does not quote the `[mesh] <label> connected` idle line:\n{inbox}"
    );
}

/// The decision tree, from `**Which setup?**` through `the loopback hop.`, is one text on
/// the Mesh and Deployment pages.
#[test]
fn the_decision_tree_is_byte_identical_on_the_mesh_and_deployment_pages() {
    let Some(wiki) = wiki_dir() else { return };
    let fixture = "intro\n**Which setup?** lead\n1. one\n\nreached through\nthe loopback hop.\nAll of it is elsewhere.\n";
    assert_eq!(
        decision_tree("fixture", fixture),
        "**Which setup?** lead\n1. one\n\nreached through\nthe loopback hop.",
        "decision_tree does not stop at the first `the loopback hop.` line"
    );
    let mesh = decision_tree("Mesh.md", &read(wiki.join("Mesh.md")));
    let deployment = decision_tree("Mesh-Deployment.md", &read(wiki.join("Mesh-Deployment.md")));
    assert_eq!(
        mesh, deployment,
        "the decision tree differs between Mesh.md and Mesh-Deployment.md"
    );
    assert!(
        !mesh.contains("All of it is on"),
        "the Mesh.md pointer sentence leaked into the twin block:\n{mesh}"
    );
}

/// The Deployment page keeps its section order, leads with the one-liner and the loopback
/// default, climbs the ladder loopback, then team relay, then `type: public`, and quotes
/// the relay script: every line of the rnsd config it writes, its firewall sentence (on
/// the Platform page too), its usage lines, its exit codes, both root refusals and the
/// enable command it prints when no user session bus answers. The script-side shape
/// checks run on every run.
#[test]
fn the_deployment_page_keeps_its_section_order_and_quotes_the_relay_script() {
    const SH: &str = "scripts/mesh-relay.sh";
    const PS1: &str = "scripts/mesh-relay.ps1";
    let config_lines: Vec<String> = ["settings_text", "interfaces_text", "config_text"]
        .iter()
        .flat_map(|name| heredoc_bodies(&bash_fn_body(SH, name)))
        .flat_map(|body| {
            body.lines()
                .map(|line| {
                    line.trim()
                        .replace("$RELAY_HOST", "HOST")
                        .replace("$RELAY_PORT", "PORT")
                })
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        config_lines.iter().any(|line| line == "target_host = HOST")
            && config_lines
                .iter()
                .any(|line| line == "enable_transport = True"),
        "the heredocs of {SH} no longer carry the relay stanza and the transport flag: {config_lines:?}"
    );

    let firewall = bash_fn_body(SH, "firewall_warning");
    let sentence_at = firewall
        .find("local sentence=\"")
        .expect("firewall_warning declares `local sentence`")
        + "local sentence=\"".len();
    let sentence = &firewall[sentence_at..sentence_at + firewall[sentence_at..].find('"').unwrap()];
    assert!(
        sentence.starts_with("Firewall: "),
        "firewall_warning's sentence moved: {sentence:?}"
    );

    let usage: Vec<String> = string_literals(&bash_fn_body(SH, "usage"))
        .into_iter()
        .filter(|literal| literal.starts_with("  -"))
        .collect();
    assert_eq!(usage.len(), 8, "usage() lists eight options: {usage:?}");

    let header = read(repo_root().join(SH));
    let exit_codes = flat(
        &header
            .lines()
            .skip_while(|line| !line.starts_with("# Exit codes:"))
            .map_while(|line| line.strip_prefix("# "))
            .collect::<Vec<_>>()
            .join(" "),
    );
    assert!(
        exit_codes.starts_with("Exit codes: 0 ") && exit_codes.contains("; 3 "),
        "{SH}'s `# Exit codes:` header lines moved: {exit_codes:?}"
    );

    let root_refusal = string_literals(&bash_fn_body(SH, "refuse_root"))
        .into_iter()
        .find(|literal| literal.starts_with("running as root"))
        .expect("refuse_root errs with `running as root`");
    let ps1 = read(repo_root().join(PS1));
    let admin_lead = "Write-Failure 'running as Administrator";
    let admin_at = ps1
        .find(admin_lead)
        .expect("mesh-relay.ps1 refuses Administrator")
        + "Write-Failure '".len();
    let admin_refusal = &ps1[admin_at..admin_at + ps1[admin_at..].find('\'').unwrap()];

    let no_bus_enable = string_literals(&bash_fn_body(SH, "service_linux"))
        .into_iter()
        .find(|literal| {
            literal
                .trim_start()
                .starts_with("systemctl --user daemon-reload && ")
        })
        .expect("service_linux prints the enable command when no user bus answers")
        .trim()
        .replace("$SERVICE_NAME", "coyote-rnsd");
    assert!(
        no_bus_enable.ends_with("enable --now coyote-rnsd"),
        "service_linux's no-bus enable command moved: {no_bus_enable:?}"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Deployment.md"));
    let platform = read(wiki.join("Mesh-Platform-Support.md"));

    let sections: Vec<&str> = headings(&page)
        .into_iter()
        .filter_map(|line| line.strip_prefix("## "))
        .collect();
    assert_eq!(
        sections,
        [
            "The ladder",
            "Setting up rnsd",
            "Two laptops on one network",
            "Joining with some interfaces down",
            "Running without rnsd",
            "Two sessions on one host",
            "Relay gotchas",
            "Propagation nodes",
            "Identity portability",
            "Success looks like",
        ],
        "Mesh-Deployment.md's `## ` headings moved"
    );
    for heading in headings(&page) {
        assert!(
            !heading.contains("Two instances on one machine"),
            "Mesh-Deployment.md keeps the retired heading: {heading}"
        );
    }

    let intro = flat(&lead(&page));
    for needle in [
        "private 127.0.0.1:4242",
        "rnsd",
        "scripts/mesh-relay.sh | bash",
    ] {
        assert!(
            intro.contains(needle),
            "Mesh-Deployment.md does not say {needle:?} before its first `## ` heading:\n{intro}"
        );
    }

    let ladder = section("Mesh-Deployment.md", &page, "## The ladder");
    let rungs: Vec<&str> = ladder
        .lines()
        .filter(|line| line.starts_with("### "))
        .collect();
    assert_eq!(rungs.len(), 3, "the ladder has three rungs: {rungs:?}");
    for (rung, needle) in rungs.iter().zip(["rnsd", "relay", "type: public"]) {
        assert!(
            rung.contains(needle),
            "ladder rung {rung:?} does not name {needle:?}; the order is loopback, team relay, public"
        );
    }
    assert!(
        page.contains("\"Private\" means network reachability, not access control."),
        "Mesh-Deployment.md no longer says what \"private\" means"
    );

    let setup = section("Mesh-Deployment.md", &page, "## Setting up rnsd");
    let setup_lines: Vec<&str> = setup.lines().map(str::trim).collect();
    for line in &config_lines {
        assert!(
            setup_lines.contains(&line.as_str()),
            "Mesh-Deployment.md's `## Setting up rnsd` does not quote the config line {line:?}"
        );
    }
    assert!(
        flat(&setup).contains(&no_bus_enable),
        "Mesh-Deployment.md's `## Setting up rnsd` does not quote the no-bus enable command {no_bus_enable:?}"
    );

    for (label, text) in [
        ("Mesh-Deployment.md", &page),
        ("Mesh-Platform-Support.md", &platform),
    ] {
        assert!(
            text.contains(sentence),
            "{label} does not quote the firewall sentence verbatim:\n{sentence}"
        );
    }

    for line in &usage {
        let (flag, effect) = line
            .trim()
            .split_once("  ")
            .unwrap_or_else(|| panic!("usage line {line:?} has no two-space gap"));
        assert!(
            page.contains(&format!("`{flag}`")),
            "Mesh-Deployment.md's flag table has no `{flag}`"
        );
        if effect.contains("${") {
            continue;
        }
        assert!(
            page.contains(effect.trim()),
            "Mesh-Deployment.md's flag table does not say {:?} for `{flag}`",
            effect.trim()
        );
    }

    let prose = flat(&page).replace('`', "");
    assert!(
        prose.contains(&exit_codes),
        "Mesh-Deployment.md does not spell the exit codes as {SH}'s header does:\n{exit_codes}"
    );

    for refusal in [root_refusal.as_str(), admin_refusal] {
        assert!(
            page.contains(refusal),
            "Mesh-Deployment.md does not quote the refusal {refusal:?}"
        );
    }
}

/// The Platform page's rnsd section names the three services the scripts register and the
/// linger hint, quotes the macOS and Windows firewall prompts as the scripts word them,
/// carries the macOS firewall pointer `firewall_pointer_darwin` prints (the log line, the
/// System Settings path and the loopback-only exemption, read from the script), and links
/// the Deployment sections that replaced the two-instance recipe. The script-side shape
/// checks run on every run.
#[test]
fn the_platform_page_names_the_services_and_quotes_the_firewall_prompts() {
    let firewall = bash_fn_body("scripts/mesh-relay.sh", "firewall_warning");
    let macos_at = firewall
        .find("\"$sentence ")
        .expect("firewall_warning appends the macOS prompt to the sentence")
        + "\"$sentence ".len();
    let macos = &firewall[macos_at..macos_at + firewall[macos_at..].find('"').unwrap()];
    let ps1 = read(repo_root().join("scripts/mesh-relay.ps1"));
    let windows_lead = "when one is configured). ";
    let windows_at = ps1
        .find(windows_lead)
        .expect("mesh-relay.ps1 appends the Windows prompt to the sentence")
        + windows_lead.len();
    let windows = &ps1[windows_at..windows_at + ps1[windows_at..].find('\'').unwrap()];
    let prompts = [("macOS", macos), ("Windows", windows)];
    for (platform, prompt) in prompts {
        assert!(
            prompt.starts_with(&format!("{platform} will ask")),
            "the {platform} prompt moved: {prompt:?}"
        );
    }

    let pointer_fn = bash_fn_body("scripts/mesh-relay.sh", "firewall_pointer_darwin");
    let pointer_at = pointer_fn
        .find("log \"")
        .expect("firewall_pointer_darwin logs one sentence")
        + "log \"".len();
    let pointer = &pointer_fn[pointer_at..pointer_at + pointer_fn[pointer_at..].find('"').unwrap()];
    let log_line = "No multicast echoes received";
    let settings_path = "System Settings > Network > Firewall > Options";
    let unaffected = pointer
        .rsplit(". ")
        .next()
        .expect("the pointer ends with the loopback-only sentence");
    assert!(
        pointer.contains(log_line)
            && pointer.contains(settings_path)
            && unaffected.starts_with("Nodes ")
            && unaffected.contains("loopback relay")
            && unaffected.ends_with("are unaffected."),
        "firewall_pointer_darwin's wording moved:\n{pointer}"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Platform-Support.md"));
    let rnsd = section(
        "Mesh-Platform-Support.md",
        &page,
        "## Running rnsd per platform",
    );
    for needle in [
        "coyote-rnsd",
        "com.coyote.rnsd",
        "Coyote rnsd",
        "loginctl enable-linger",
    ] {
        assert!(
            rnsd.contains(needle),
            "Mesh-Platform-Support.md's rnsd section does not name {needle:?}:\n{rnsd}"
        );
    }

    for (platform, prompt) in prompts {
        assert!(
            rnsd.contains(prompt),
            "Mesh-Platform-Support.md does not quote the {platform} prompt verbatim:\n{prompt}"
        );
    }

    // The macOS firewall pointer: the page names the same log line and settings path the
    // script prints, and its loopback-only exemption agrees with the script's sentence.
    let rnsd_prose = flat(&rnsd).replace('`', "");
    for needle in [log_line, settings_path] {
        assert!(
            rnsd_prose.contains(needle),
            "Mesh-Platform-Support.md's rnsd section does not carry the firewall pointer's {needle:?}:\n{rnsd}"
        );
    }
    let page_unaffected = rnsd_prose
        .split(". ")
        .find(|sentence| sentence.contains("unaffected"))
        .unwrap_or_else(|| {
            panic!("Mesh-Platform-Support.md's rnsd section has no 'unaffected' sentence:\n{rnsd}")
        });
    assert!(
        page_unaffected.starts_with("Nodes ")
            && page_unaffected.contains("loopback relay")
            && page_unaffected.ends_with("are unaffected"),
        "Mesh-Platform-Support.md's loopback-only sentence disagrees with the script's {unaffected:?}:\n{page_unaffected}"
    );

    for anchor in [
        "Mesh-Deployment#setting-up-rnsd",
        "Mesh-Deployment#running-without-rnsd",
    ] {
        assert!(
            page.contains(&format!("]({anchor})")),
            "Mesh-Platform-Support.md does not link {anchor}"
        );
    }
    assert!(
        !page.contains("two-instances-on-one-machine"),
        "Mesh-Platform-Support.md still links the retired two-instance recipe"
    );
}

/// Every `](Page#anchor)` link from a Mesh page into one of the restructured pages names a
/// heading that page has, by its GitHub slug.
#[test]
fn every_section_link_into_the_restructured_pages_resolves_to_a_heading() {
    let Some(wiki) = wiki_dir() else { return };
    assert_eq!(github_slug("### `.mesh info`"), "mesh-info");
    assert_eq!(
        github_slug("## Talking to peers (tools)"),
        "talking-to-peers-tools"
    );
    assert_eq!(
        github_slug("## `collision_protection`"),
        "collision_protection"
    );
    let fixture_anchors = [("Mesh", vec!["the-brief".to_string()])];
    let fixture =
        "see [x](Mesh#the-brief) and [y](Mesh#no-such-heading)\nand [z](Other#whatever)\n";
    assert_eq!(
        dangling_section_links("fixture", fixture, &fixture_anchors),
        ["fixture:1 → Mesh#no-such-heading"],
        "the scan does not flag exactly the dangling link into a watched page"
    );
    assert_eq!(
        heading_slugs("# Top\n```text\n# not a heading\n```\n## Real\n"),
        ["top", "real"],
        "heading_slugs does not skip fenced code"
    );

    let anchors: Vec<(&str, Vec<String>)> = [
        "Mesh",
        "Mesh-Deployment",
        "Mesh-Platform-Support",
        "Mesh-Commands",
        "Mesh-Configuration",
    ]
    .into_iter()
    .map(|page| (page, heading_slugs(&read(wiki.join(format!("{page}.md"))))))
    .collect();
    let dangling: Vec<String> = mesh_pages(&wiki)
        .iter()
        .flat_map(|path| {
            let label = path.file_name().unwrap().to_string_lossy().to_string();
            dangling_section_links(&label, &read(path), &anchors)
        })
        .collect();
    assert!(
        dangling.is_empty(),
        "section links that resolve to no heading:\n{}",
        dangling.join("\n")
    );
}

/// The `.mesh info` sample on the Commands page pads its labels to `MESH_INFO_LABEL_WIDTH`
/// and shows an `interfaces[0]` row as `interface_row` formats it: label, two spaces, state.
/// The width and the row's shape are read on every run.
#[test]
fn the_commands_page_info_sample_pads_labels_to_the_code_width_and_shows_the_interface_row() {
    let source = read(repo_root().join("src/config/mesh_config.rs"));
    let marker = "pub const MESH_INFO_LABEL_WIDTH: usize = ";
    let at = source
        .find(marker)
        .expect("src/config/mesh_config.rs declares MESH_INFO_LABEL_WIDTH")
        + marker.len();
    let width: usize = source[at..]
        .split(';')
        .next()
        .unwrap()
        .parse()
        .expect("MESH_INFO_LABEL_WIDTH is a literal");
    assert!(
        string_literals(&fn_body("src/repl/mesh.rs", "interface_row"))
            .contains(&"{}  {}".to_string()),
        "interface_row no longer joins the label and state with two spaces; update the sample and this test"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Commands.md"));
    let info = section("Mesh-Commands.md", &page, "### `.mesh info`");
    let sample = info
        .split("```text\n")
        .skip(1)
        .map(|block| block.split("\n```").next().unwrap())
        .find(|block| block.starts_with("  node"))
        .expect("the `.mesh info` section has a text block starting with `  node`");
    let mut interface_rows = 0;
    for row in sample.lines() {
        let body = row
            .strip_prefix("  ")
            .unwrap_or_else(|| panic!("sample row is not indented by two: {row:?}"));
        if body.starts_with("selection:") {
            continue;
        }
        let gap = body
            .find("  ")
            .unwrap_or_else(|| panic!("sample row has no value: {row:?}"));
        let name = &body[..gap];
        let value_at = 2 + gap + body[gap..].len() - body[gap..].trim_start().len();
        assert_eq!(
            value_at,
            2 + width,
            "the value of `{name}` starts at byte {value_at}, not {} as MESH_INFO_LABEL_WIDTH pads it: {row:?}",
            2 + width
        );
        if name == "interfaces[0]" {
            interface_rows += 1;
            assert_eq!(
                &row[value_at..],
                "private 127.0.0.1:4242  connected",
                "the interfaces[0] row is not what interface_row formats for the default"
            );
        }
    }
    assert_eq!(
        interface_rows, 1,
        "the `.mesh info` sample shows one interfaces[0] row:\n{sample}"
    );
}

/// The `.mesh on` section's samples follow the shipped default: the preview's `To whom:`
/// line names the private audience as `audience` words it, the summary's `interfaces:`
/// row names the loopback relay, and the idle line a late relay prints is quoted.
#[test]
fn the_commands_page_on_samples_name_the_private_default_and_the_connect_idle_line() {
    let audiences = string_literals(&fn_body("src/repl/mesh.rs", "audience"));
    let private = audiences
        .iter()
        .find(|literal| literal.contains("relay") && !literal.contains("internet"))
        .unwrap_or_else(|| {
            panic!("audience in src/repl/mesh.rs has no private-relay literal: {audiences:?}")
        });

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Commands.md"));
    let on = section("Mesh-Commands.md", &page, "### `.mesh on`");
    for needle in [
        format!("  private 127.0.0.1:4242: {private}"),
        "  interfaces: private 127.0.0.1:4242".to_string(),
        "`[mesh] <label> connected`".to_string(),
    ] {
        assert!(
            on.contains(&needle),
            "Mesh-Commands.md's `.mesh on` section does not carry {needle:?}"
        );
    }
}

/// The Containers page quotes the image's `ENTRYPOINT` line as the Dockerfile has it; the
/// Dockerfile-side check runs on every run.
#[test]
fn the_containers_page_quotes_the_image_entrypoint() {
    let dockerfile = read(repo_root().join("Dockerfile"));
    let entrypoint = dockerfile
        .lines()
        .find(|line| line.starts_with("ENTRYPOINT "))
        .expect("Dockerfile has an ENTRYPOINT line");
    assert!(
        entrypoint.contains("coyote-entrypoint"),
        "the Dockerfile's ENTRYPOINT moved: {entrypoint}"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Containers.md"));
    assert!(
        page.contains(entrypoint),
        "Mesh-Containers.md does not quote {entrypoint:?}"
    );
}

/// The Containers page names every `COYOTE_MESH_*` variable the image entrypoint reads
/// (the set is scraped from the script's `${...}` and bare `$...` references, so a fourth
/// variable fails this until the page names it), says the AutoInterface needs
/// `--network host`, never to publish `4242`, and quotes the Dockerfile's
/// `rns==<RNS_VERSION>`; the Sandboxes and Installation pages name the opt-out and the
/// relay variable; the "from v0.10.4" qualifier is spelled the same in both READMEs and
/// every `vX.Y.Z` token on the Containers page and in the Sandboxes mesh section is that
/// release. The repo-side checks run on every run.
#[test]
fn the_containers_page_names_the_entrypoint_variables() {
    let entrypoint = read(repo_root().join("scripts/docker-entrypoint.sh"));
    let name_end = |rest: &str| {
        rest.find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(rest.len())
    };
    let mut variables: Vec<&str> = entrypoint
        .match_indices("$COYOTE_MESH_")
        .map(|(at, _)| &entrypoint[at + 1..])
        .chain(
            entrypoint
                .match_indices("${COYOTE_MESH_")
                .map(|(at, _)| &entrypoint[at + 2..]),
        )
        .map(|rest| &rest[..name_end(rest)])
        .collect();
    variables.sort_unstable();
    variables.dedup();
    assert_eq!(
        variables,
        ["COYOTE_MESH_LAN", "COYOTE_MESH_RELAY", "COYOTE_MESH_RNSD"],
        "scripts/docker-entrypoint.sh reads a different COYOTE_MESH_* set; update the wiki and this test"
    );
    let dockerfile = read(repo_root().join("Dockerfile"));
    let rns_version = dockerfile
        .lines()
        .find_map(|line| line.strip_prefix("ARG RNS_VERSION="))
        .expect("Dockerfile declares ARG RNS_VERSION=")
        .trim();
    let readme = flat(&read(repo_root().join("README.md")));
    let node_readme = flat(&read(
        repo_root().join("deployment/propagation-node/README.md"),
    ));
    for (label, prose) in [
        ("README.md", &readme),
        ("deployment/propagation-node/README.md", &node_readme),
    ] {
        assert!(
            prose.contains(IMAGE_RNSD_SINCE),
            "{label} does not say {IMAGE_RNSD_SINCE:?} about the image's rnsd"
        );
    }
    let release = image_rnsd_release();
    let docker_readme = section(
        "README.md",
        &read(repo_root().join("README.md")),
        "### Docker",
    );
    for variable in &variables {
        assert!(
            docker_readme.contains(variable),
            "README.md's `### Docker` section does not name `{variable}`"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Mesh-Containers.md"));
    let private = section(
        "Mesh-Containers.md",
        &page,
        "## Running with `type: private`",
    );
    for variable in &variables {
        assert!(
            private.contains(&format!("`{variable}")),
            "Mesh-Containers.md's `type: private` section does not name `{variable}`"
        );
    }
    let rns_spec = format!("rns=={rns_version}");
    for needle in [
        "`--network host`",
        "never publish `4242`",
        IMAGE_RNSD_SINCE,
        rns_spec.as_str(),
    ] {
        assert!(
            flat(&page).contains(needle),
            "Mesh-Containers.md does not say {needle:?}"
        );
    }
    let sandbox = section(
        "Sandboxes.md",
        &read(wiki.join("Sandboxes.md")),
        "## Mesh inside a sandbox",
    );
    for needle in [
        "`COYOTE_MESH_RNSD=0`",
        "`COYOTE_MESH_RELAY=host:port`",
        IMAGE_RNSD_SINCE,
    ] {
        assert!(
            flat(&sandbox).contains(needle),
            "Sandboxes.md's mesh section does not say {needle:?}"
        );
    }
    let docker = section(
        "Installation.md",
        &read(wiki.join("Installation.md")),
        "## Docker",
    );
    for needle in [
        "COYOTE_MESH_RNSD=0`",
        "COYOTE_MESH_RELAY=host:port`",
        IMAGE_RNSD_SINCE,
    ] {
        assert!(
            flat(&docker).contains(needle),
            "Installation.md's Docker section does not say {needle:?}"
        );
    }
    for (label, text) in [
        ("Mesh-Containers.md", &page),
        ("Sandboxes.md's mesh section", &sandbox),
        ("Installation.md's Docker section", &docker),
    ] {
        let stale: Vec<String> = release_tokens(text)
            .into_iter()
            .filter(|token| token != release)
            .collect();
        assert!(
            stale.is_empty(),
            "{label} names releases other than {release}: {stale:?}; the image's rnsd qualifier is spelled from {IMAGE_RNSD_SINCE:?}"
        );
    }
}

/// The README names the image's rnsd twice (the `## Prerequisites` line, which the
/// `from v0.10.4` needle finds, and the `### Docker` paragraph) and the propagation node's
/// README once; this sweeps the `### Docker` section and the node README so every `vX.Y.Z`
/// token there is that one release and neither site drifts from the other. Repo-side; runs
/// without the wiki.
#[test]
fn the_readme_docker_section_and_the_node_readme_name_only_the_image_rnsd_release() {
    let release = image_rnsd_release();
    let readme = read(repo_root().join("README.md"));
    let docker = section("README.md", &readme, "### Docker");
    let node_readme = read(repo_root().join("deployment/propagation-node/README.md"));
    for (label, text) in [
        ("README.md's `### Docker` section", docker.as_str()),
        (
            "deployment/propagation-node/README.md",
            node_readme.as_str(),
        ),
    ] {
        let tokens = release_tokens(text);
        assert!(
            !tokens.is_empty(),
            "{label} no longer qualifies the image's rnsd with a release"
        );
        let stale: Vec<&String> = tokens.iter().filter(|token| *token != release).collect();
        assert!(
            stale.is_empty(),
            "{label} names releases other than {release}: {stale:?}; the image's rnsd qualifier is spelled from {IMAGE_RNSD_SINCE:?}"
        );
    }
}

/// The Hooks page scopes `COYOTE_MESH_INTERFACES` to the interfaces connected when
/// `mesh.started` fired and at the ones connected when the node left; no page names the
/// retired `COYOTE_LOG_FILE`, the current `COYOTE_LOG_PATH` is the variable the code reads
/// and Unattended-Mode points at it; `src/mesh/announce.rs` says an announce is forwarded
/// undeduplicated. The code-side checks run on every run.
#[test]
fn the_hooks_page_scopes_mesh_interfaces_to_the_start_event_and_the_log_path_variable_is_current() {
    assert!(
        read(repo_root().join("src/config/paths.rs")).contains("get_env_name(\"log_path\")"),
        "src/config/paths.rs no longer reads the log_path variable; update the wiki and this test"
    );
    let announce = flat(&read(repo_root().join("src/mesh/announce.rs")));
    assert!(
        announce.contains("forwards it like any other; nothing dedups it."),
        "src/mesh/announce.rs no longer says the relay forwards an announce undeduplicated"
    );
    assert!(
        !announce.contains("dedups it by announce hash"),
        "src/mesh/announce.rs claims the relay dedups by announce hash again"
    );

    let Some(wiki) = wiki_dir() else { return };
    let hooks = read(wiki.join("Hooks.md"));
    let lines: Vec<&str> = hooks.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.starts_with("- `COYOTE_MESH_INTERFACES`"))
        .expect("Hooks.md has a `COYOTE_MESH_INTERFACES` bullet");
    let bullet = std::iter::once(lines[at])
        .chain(
            lines[at + 1..]
                .iter()
                .copied()
                .take_while(|line| line.starts_with("  ")),
        )
        .collect::<Vec<_>>()
        .join(" ");
    for needle in [
        "when `mesh.started` fired",
        "refires nothing",
        "when the node left",
    ] {
        assert!(
            bullet.contains(needle),
            "Hooks.md's COYOTE_MESH_INTERFACES bullet does not say {needle:?}: {bullet}"
        );
    }

    let stale: Vec<String> = fs::read_dir(&wiki)
        .unwrap_or_else(|e| panic!("read {}: {e}", wiki.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter(|path| read(path).contains("COYOTE_LOG_FILE"))
        .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        stale.is_empty(),
        "these pages still name COYOTE_LOG_FILE: {stale:?}"
    );
    assert!(
        read(wiki.join("Environment-Variables.md"))
            .lines()
            .any(|line| line.starts_with("| `COYOTE_LOG_PATH`")),
        "Environment-Variables.md has no `COYOTE_LOG_PATH` row"
    );
    let unattended = flat(&read(wiki.join("Unattended-Mode.md")));
    assert!(
        unattended.contains("`COYOTE_LOG_PATH` points"),
        "Unattended-Mode.md does not point at `COYOTE_LOG_PATH`"
    );
}

/// The README names the daemon prerequisite with both one-liners and the installer flags,
/// shows the shipped `mesh.interfaces` default, and says what the propagation node is and
/// is not for; the node's own README points hosts at `--relay` with all four recipes, keeps
/// the loopback default and describes the existing-config path as `write_config` in
/// `scripts/mesh-relay.sh` takes it. Reads only the repo, so it runs without the wiki.
#[test]
fn the_readme_names_the_daemon_prerequisite_and_the_private_default() {
    let readme = read(repo_root().join("README.md"));
    let prose = flat(&readme);
    for needle in [
        "Coyote mesh (`.mesh`) needs a local Reticulum daemon",
        "curl -fsSL https://raw.githubusercontent.com/Dark-Alex-17/coyote/refs/heads/main/scripts/mesh-relay.sh | bash",
        "iwr -useb https://raw.githubusercontent.com/Dark-Alex-17/coyote/refs/heads/main/scripts/mesh-relay.ps1 | iex",
        "--with-mesh",
        "-WithMesh",
    ] {
        assert!(prose.contains(needle), "README.md does not say {needle:?}");
    }
    let row = readme
        .lines()
        .find(|line| line.starts_with("| `mesh.interfaces`"))
        .expect("README.md has a `mesh.interfaces` table row");
    assert_eq!(
        table_cells(row)[1],
        INTERFACES_DEFAULT,
        "README.md's `mesh.interfaces` default is not the shipped loopback entry: {row}"
    );
    let mesh = section("README.md", &readme, "### Mesh");
    for needle in [
        "store-and-forward across devices for a team",
        "not how sessions on one host reach each other",
    ] {
        assert!(
            mesh.contains(needle),
            "README.md's `### Mesh` section does not say {needle:?}:\n{mesh}"
        );
    }
    let node_readme = read(repo_root().join("deployment/propagation-node/README.md"));
    for needle in ["scripts/mesh-relay.sh --relay", "host: 127.0.0.1"] {
        assert!(
            node_readme.contains(needle),
            "deployment/propagation-node/README.md does not say {needle:?}"
        );
    }
    let write_config = bash_fn_body("scripts/mesh-relay.sh", "write_config");
    for needle in ["-f \"$RNS_CONFIG\"", "already exists, not touched"] {
        assert!(
            write_config.contains(needle),
            "scripts/mesh-relay.sh's write_config no longer carries {needle:?}; update the propagation node README and this test"
        );
    }
    let coyote_side = flat(&section(
        "deployment/propagation-node/README.md",
        &node_readme,
        "## Coyote side",
    ));
    for needle in [
        "no `~/.reticulum/config` yet",
        "prints the stanza to add by hand",
        "systemctl --user restart coyote-rnsd",
        "launchctl bootout",
        "Start-ScheduledTask 'Coyote rnsd'",
        "--relay <node host>:4242",
        "bash -s -- --relay",
        "-ExecutionPolicy Bypass -File .\\mesh-relay.ps1",
        "host: <node host>",
    ] {
        assert!(
            coyote_side.contains(needle),
            "deployment/propagation-node/README.md's `## Coyote side` does not say {needle:?}:\n{coyote_side}"
        );
    }
}

/// `path:line: spells <needle>` for every line of `text` that carries one of `needles`,
/// matched case-insensitively.
fn label_hits(path: &str, text: &str, needles: &[String]) -> Vec<String> {
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let lower = line.to_lowercase();
        for needle in needles {
            if lower.contains(&needle.to_lowercase()) {
                hits.push(format!("{path}:{}: spells {needle}", index + 1));
            }
        }
    }
    hits
}

/// No README, this lint or Mesh page carries a planning label; the needles are assembled
/// at runtime so this test's own text cannot trip it.
#[test]
fn no_doc_or_lint_carries_a_plan_or_task_label() {
    let needles = [
        ["PLAN", "-"].concat(),
        ["TASK", "-"].concat(),
        ["rul", "ing "].concat(),
    ];
    let mut files = vec![
        repo_root().join("README.md"),
        repo_root().join("deployment/propagation-node/README.md"),
        repo_root().join("tests/mesh_wiki_docs.rs"),
    ];
    if let Some(wiki) = wiki_dir() {
        files.extend(mesh_pages(&wiki));
    }
    let mut hits = Vec::new();
    for path in &files {
        hits.extend(label_hits(
            &path.display().to_string(),
            &read(path),
            &needles,
        ));
    }
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

/// The two-sessions prose on the Deployment and Configuration pages carries the same
/// `mesh/trust.yaml` live-sharing sentence, and the Directories section keeps the split
/// rule: nothing split to run, both directories split for two identities.
#[test]
fn the_two_sessions_prose_says_the_trust_file_is_shared_live_on_both_pages() {
    let Some(wiki) = wiki_dir() else { return };
    let configuration = read(wiki.join("Mesh-Configuration.md"));
    for (label, text) in [
        ("Mesh-Deployment.md", read(wiki.join("Mesh-Deployment.md"))),
        ("Mesh-Configuration.md", configuration.clone()),
    ] {
        assert!(
            flat(&text).contains(TRUST_FILE_TWIN),
            "{label} does not carry the live-sharing sentence verbatim:\n{TRUST_FILE_TWIN}"
        );
        assert!(
            !flat(&text).contains("reaches the other only after its `.mesh off`"),
            "{label} still says a second session must restart mesh to see trust"
        );
    }
    let directories = flat(&section(
        "Mesh-Configuration.md",
        &configuration,
        "## Directories",
    ));
    for needle in [
        "need nothing split",
        "need both directories split",
        "`mesh/trust.yaml.lock`",
        "`mesh/shares.yaml.lock`",
    ] {
        assert!(
            directories.contains(needle),
            "Mesh-Configuration.md's Directories section does not say {needle:?}:\n{directories}"
        );
    }
}

/// The knock entry quotes the four lines `.mesh knock` prints for a knock that drew no
/// reply or could not be held, and both the entry and the Trust Model's Knocks section say
/// a knock is not first contact. The quoted lines are read from the source so a reworded
/// report moves the page with it.
#[test]
fn the_knock_pages_say_a_knock_is_not_first_contact_and_quote_the_reports() {
    let Some(wiki) = wiki_dir() else { return };
    let repl = read(repo_root().join("src").join("repl").join("mesh.rs"));
    let knock_rs = read(repo_root().join("src").join("mesh").join("knock.rs"));
    let declaration = &knock_rs[knock_rs
        .find("const KNOCK_IS_NOT_FIRST_CONTACT")
        .expect("src/mesh/knock.rs declares KNOCK_IS_NOT_FIRST_CONTACT")..];
    let declaration = &declaration[..declaration
        .find(";\n")
        .expect("the KNOCK_IS_NOT_FIRST_CONTACT declaration ends with `;`")];
    let first_contact = string_literals(declaration)
        .into_iter()
        .next()
        .expect("KNOCK_IS_NOT_FIRST_CONTACT is a string literal");
    assert!(first_contact.starts_with("A knock is heard only by a peer"));
    let stored = format!(
        "Knocked on {{dest8}}; no answer over the link, so a propagation node holds the knock until the peer fetches it. {first_contact}"
    );
    let unanswered = "Knocked on {dest8}; no answer. A knock is heard only by a peer that already trusts this identity (a new instance of a known identity). For first contact, exchange destination hashes and run `.mesh trust <destination>` on both sides. No propagation node is known yet to hold the knock for it.";
    let unreachable = "The peer is unreachable and no propagation node is known yet to hold the knock for it. Run `.mesh peers` to see which nodes this Coyote has heard from.";
    let preview_tail = "A knock is heard only by a peer that already trusts this identity; for first contact use `.mesh trust` on both sides.";
    assert!(
        repl.contains(&stored.replace(&first_contact, "{KNOCK_IS_NOT_FIRST_CONTACT}")),
        "src/repl/mesh.rs no longer prints the stored-after-silence report this test quotes"
    );
    assert!(
        repl.contains(preview_tail),
        "the preview no longer ends with {preview_tail:?}"
    );
    for (name, literal) in [
        ("Unanswered", unanswered),
        ("NoPropagationNode", unreachable),
    ] {
        assert!(
            knock_rs.contains(literal),
            "src/mesh/knock.rs's KnockError::{name} no longer renders {literal:?}"
        );
    }
    let commands = flat(&section(
        "Mesh-Commands.md",
        &read(wiki.join("Mesh-Commands.md")),
        "### `.mesh knock`",
    ));
    for needle in [
        "A knock is not first contact".to_string(),
        preview_tail.to_string(),
        stored.replace("{dest8}", "<dest8>"),
        unanswered.replace("{dest8}", "<dest8>"),
        unreachable.to_string(),
        "(Mesh-Deployment#two-laptops-on-one-network)".to_string(),
    ] {
        assert!(
            commands.contains(&needle),
            "Mesh-Commands.md's `.mesh knock` entry does not carry {needle:?}"
        );
    }
    let knocks = flat(&section(
        "Mesh-Trust-Model.md",
        &read(wiki.join("Mesh-Trust-Model.md")),
        "## Knocks",
    ));
    for needle in [
        "A knock is not first contact.",
        "reports the knock as unanswered rather than the peer as unreachable",
        "(Mesh-Deployment#two-laptops-on-one-network)",
    ] {
        assert!(
            knocks.contains(needle),
            "Mesh-Trust-Model.md's Knocks section does not carry {needle:?}"
        );
    }
}

/// Every page that quotes the refusal for a peer the table knows but the network could
/// not route to carries the cause `unreachable_cause` renders, with the number
/// `PATH_REQUEST_TIMEOUT` holds, followed by the tail its surface appends. The Deployment
/// and Platform pages tell the restart story as a path request, the Configuration page's
/// Timers section names the wait and the sweep that skips it, the three REPL verbs' pages
/// quote the line `route_request_line` prints before the wait, and no mesh page still
/// says a route is learned only from an announce heard after this node started.
#[test]
fn the_mesh_pages_tell_the_restart_story_as_a_path_request_with_the_timeout_the_code_waits() {
    let Some(wiki) = wiki_dir() else { return };
    let timeout = const_secs("src/mesh/announce.rs", "PATH_REQUEST_TIMEOUT");
    let cause_template = string_literals(&fn_body("src/mesh/message.rs", "unreachable_cause"))
        .into_iter()
        .next()
        .expect("unreachable_cause formats one string literal");
    assert!(
        cause_template
            .starts_with("could not be reached: this node asked the network for a route to it"),
        "src/mesh/message.rs's unreachable_cause no longer opens the way this test quotes: {cause_template:?}"
    );
    let cause = cause_template.replace("{}", &timeout.to_string());
    let head = format!("Destination <hash> {cause}");
    let route_line_template = string_literals(&fn_body("src/repl/mesh.rs", "route_request_line"))
        .into_iter()
        .next()
        .expect("route_request_line formats one string literal");
    assert!(
        route_line_template.starts_with("No route to {} yet; asking the network for one"),
        "src/repl/mesh.rs's route_request_line no longer opens the way this test quotes: {route_line_template:?}"
    );
    let route_line =
        route_line_template
            .replacen("{}", "<dest8>", 1)
            .replacen("{}", &timeout.to_string(), 1);

    let message_rs = read(repo_root().join("src").join("mesh").join("message.rs"));
    let repl = read(repo_root().join("src").join("repl").join("mesh.rs"));
    let send_tail = "Try again in a minute; nothing was queued for it, because a message can be held on a propagation node only after the peer's announce has been heard. `.mesh peers` shows when it was last heard.";
    let status_tail = "A status request is answered live or not at all, so nothing was queued; try again in a minute. `.mesh peers` shows when it was last heard.";
    let knock_tail = "Try again in a minute; nothing was queued for it, because a knock can be held on a propagation node only after the peer's announce has been heard. `.mesh peers` shows when it was last heard.";
    let fetch_tail = "A fetch runs live or not at all, so nothing was queued; try again in a minute. `.mesh peers` shows when it was last heard.";
    for (path, source, tail) in [
        ("src/mesh/message.rs", &message_rs, send_tail),
        ("src/repl/mesh.rs", &repl, status_tail),
        ("src/repl/mesh.rs", &repl, knock_tail),
        ("src/repl/mesh.rs", &repl, fetch_tail),
    ] {
        assert!(
            source.contains(&format!("\"{{}} {tail}\"")),
            "{path} no longer appends {tail:?} to unreachable_head"
        );
    }

    let commands = read(wiki.join("Mesh-Commands.md"));
    let status = flat(&section(
        "Mesh-Commands.md",
        &commands,
        "### `.mesh status`",
    ));
    assert!(
        status.contains(&format!("{head} {status_tail}")),
        "Mesh-Commands.md's `.mesh status` entry does not quote the path-request refusal"
    );
    assert!(
        status.contains(&format!("waits up to {timeout} s for the answer")),
        "Mesh-Commands.md's `.mesh status` entry does not say how long the path request waits"
    );
    assert!(
        status.contains(&route_line),
        "Mesh-Commands.md's `.mesh status` entry does not quote the route line: {route_line:?}"
    );
    assert!(
        status.contains(
            "a relay gives from its path table, or the peer itself gives by announcing again"
        ),
        "Mesh-Commands.md's `.mesh status` entry misattributes who answers a path request"
    );
    let knock = flat(&section("Mesh-Commands.md", &commands, "### `.mesh knock`"));
    assert!(
        knock.contains(&format!("{head} {knock_tail}")),
        "Mesh-Commands.md's `.mesh knock` entry does not quote the path-request refusal"
    );
    assert!(
        knock.contains(&route_line),
        "Mesh-Commands.md's `.mesh knock` entry does not quote the route line: {route_line:?}"
    );
    let gotchas = flat(&section(
        "Mesh-Deployment.md",
        &read(wiki.join("Mesh-Deployment.md")),
        "## Relay gotchas",
    ));
    for needle in [
        format!("{head} {send_tail}"),
        format!("asks the network for a path and waits up to {timeout} s for the answer"),
        "a request to one peer (`.mesh status`, `.mesh reply`, a `mesh__ask`) from a restarted node reaches a peer it already knew at once, without waiting for a heartbeat".to_string(),
        "A bulletin (`.mesh broadcast`) still goes only to peers the transport already has a route to".to_string(),
        "The answer to a path request is not a sighting: `last seen` does not move, and `mesh.peer.discovered` does not fire".to_string(),
        format!("reports such a peer unreachable at once rather than waiting {timeout} s for each"),
    ] {
        assert!(
            gotchas.contains(&needle),
            "Mesh-Deployment.md's Relay gotchas do not carry {needle:?}"
        );
    }
    let walkthrough = flat(&section(
        "Mesh-Platform-Support.md",
        &read(wiki.join("Mesh-Platform-Support.md")),
        "## A two-machine walkthrough on Windows",
    ));
    for needle in [
        format!("{head} {status_tail}"),
        "the request goes through without waiting for W1's next heartbeat".to_string(),
        route_line.clone(),
        "With W1 alone off, W2's own `rnsd` still answers the path request from its table"
            .to_string(),
        "the request fails later, at the link stage".to_string(),
        format!(
            "Only when nothing on the network holds a path for W1 (its `rnsd` gone too, or the path expired) would the request fail at the path request, and then within {timeout} s"
        ),
        "W2 can contact W1 right away, through the same path request as in step 10".to_string(),
    ] {
        assert!(
            walkthrough.contains(&needle),
            "Mesh-Platform-Support.md's walkthrough does not carry {needle:?}"
        );
    }
    let verbs = flat(&section(
        "Mesh-File-Sharing.md",
        &read(wiki.join("Mesh-File-Sharing.md")),
        "## The verbs",
    ));
    assert!(
        verbs.contains(&format!("{head} {fetch_tail}")),
        "Mesh-File-Sharing.md's verbs do not quote the fetch's path-request refusal"
    );
    assert!(
        verbs.contains(&route_line),
        "Mesh-File-Sharing.md's verbs do not quote the route line: {route_line:?}"
    );
    let timers = flat(&section(
        "Mesh-Configuration.md",
        &read(wiki.join("Mesh-Configuration.md")),
        "## Timers",
    ));
    assert!(
        timers.contains(&format!("waits up to {timeout} s for the network to answer a path request; that wait is fixed, and the status sweep skips it")),
        "Mesh-Configuration.md's Timers section does not name the path-request wait and the sweep that skips it"
    );

    for page in mesh_pages(&wiki) {
        let text = flat(&read(&page));
        let name = page.file_name().unwrap().to_string_lossy().into_owned();
        // The sync narrative still says a held message's sender "has not announced since
        // this node started"; that is the propagation node's verification, not a route.
        for retired in [
            "its announce has not been heard since this node started",
            "cannot be reached yet",
            "never requests a path",
            "needs W1's next heartbeat",
        ] {
            assert!(!text.contains(retired), "{name} still says {retired:?}");
        }
    }
}

/// `line` with each `` `...` `` code span replaced by one space; an unpaired backtick and
/// what follows it stay.
fn strip_spans(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let Some(close) = rest[open + 1..].find('`') else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push(' ');
        rest = &rest[open + close + 2..];
    }
    out.push_str(rest);
    out
}

/// `(line, text)` for every prose line of the markdown `source`: the lines outside ``` and
/// ~~~ fences, code spans stripped, those blank once stripped dropped. Twin of
/// `markdown_prose_lines` in src/hygiene_pins.rs, which this crate cannot import.
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
                None => {
                    let text = strip_spans(line);
                    let text = text.trim();
                    if !text.is_empty() {
                        out.push((index + 1, text.to_string()));
                    }
                }
            },
        }
    }
    out
}

/// The prose lines of `source`, each tagged with `label`.
fn labelled_prose(label: &str, source: &str) -> Vec<(String, usize, String)> {
    markdown_prose_lines(source)
        .into_iter()
        .map(|(line, text)| (label.to_string(), line, text))
        .collect()
}

/// The 1-based line numbers of `page` from `heading`, an `## ` line, through the line
/// before the next `## ` heading or the end of the page.
fn section_lines(label: &str, page: &str, heading: &str) -> RangeInclusive<usize> {
    let lines: Vec<&str> = page.lines().collect();
    let start = lines
        .iter()
        .position(|line| *line == heading)
        .unwrap_or_else(|| panic!("{label} has no `{heading}` heading"));
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.starts_with("## "))
        .map_or(lines.len(), |offset| start + offset);
    start + 1..=end
}

/// `(page, line, prose)` for every prose line of the wiki's mesh documentation: all of
/// every `Mesh*.md` page, the `## Mesh events` section of Hooks.md, the
/// `## Mesh inside a sandbox` section of Sandboxes.md, and the lines that mention the mesh
/// on Home.md, Installation.md and Environment-Variables.md. Line numbers are the page's.
fn mesh_doc_prose(wiki: &Path) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    for path in mesh_pages(wiki) {
        let label = path.file_name().unwrap().to_string_lossy();
        out.extend(labelled_prose(&label, &read(&path)));
    }
    for (label, heading) in [
        ("Hooks.md", "## Mesh events"),
        ("Sandboxes.md", "## Mesh inside a sandbox"),
    ] {
        let page = read(wiki.join(label));
        let lines = section_lines(label, &page, heading);
        out.extend(
            labelled_prose(label, &page)
                .into_iter()
                .filter(|(_, line, _)| lines.contains(line)),
        );
    }
    for label in ["Home.md", "Installation.md", "Environment-Variables.md"] {
        out.extend(
            labelled_prose(label, &read(wiki.join(label)))
                .into_iter()
                .filter(|(_, _, text)| text.to_lowercase().contains("mesh")),
        );
    }
    out
}

/// `page:line: [U+XXXX 'c']` for every prose line that carries a non-ASCII character,
/// naming the first one.
fn non_ascii_hits(prose: &[(String, usize, String)]) -> Vec<String> {
    prose
        .iter()
        .filter_map(|(label, line, text)| {
            text.chars()
                .find(|c| !c.is_ascii())
                .map(|c| format!("{label}:{line}: [U+{:04X} '{c}']", c as u32))
        })
        .collect()
}

/// `page:line: spells <needle>` for every prose line that carries one of `needles`
/// case-insensitively, or one of `numbered` followed by a digit.
fn vocabulary_hits(
    prose: &[(String, usize, String)],
    needles: &[String],
    numbered: &[String],
) -> Vec<String> {
    let mut hits = Vec::new();
    for (label, line, text) in prose {
        let lower = text.to_lowercase();
        for needle in needles {
            if lower.contains(needle.as_str()) {
                hits.push(format!("{label}:{line}: spells {needle}"));
            }
        }
        for needle in numbered {
            let follows_digit = lower.match_indices(needle.as_str()).any(|(at, _)| {
                lower
                    .as_bytes()
                    .get(at + needle.len())
                    .is_some_and(u8::is_ascii_digit)
            });
            if follows_digit {
                hits.push(format!("{label}:{line}: spells {needle}"));
            }
        }
    }
    hits
}

#[test]
fn every_mesh_doc_prose_line_is_ascii() {
    let Some(wiki) = wiki_dir() else { return };
    let dash = char::from_u32(0x2014).expect("an em dash");
    let fixture = format!(
        "plain prose\n```\nfenced {dash}\n```\nspan `{dash}` only\nprose {dash} here\n~~~\n{dash}\n~~~\n"
    );
    let control = non_ascii_hits(&labelled_prose("fixture", &fixture));
    assert_eq!(
        control,
        [format!("fixture:6: [U+2014 '{dash}']")],
        "only the prose line outside fences and spans is a hit"
    );
    let control = non_ascii_hits(&labelled_prose("fixture", "plain prose\n`x`\n"));
    assert!(
        control.is_empty(),
        "a clean fixture is not a hit: {control:?}"
    );

    let hits = non_ascii_hits(&mesh_doc_prose(&wiki));
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

/// The needles are assembled at runtime so the repo-wide guard against planning labels
/// never trips on this test's own text.
#[test]
fn no_mesh_doc_prose_spells_planning_vocabulary() {
    let Some(wiki) = wiki_dir() else { return };
    let needles = [
        ["bake", "-off"].concat(),
        ["bake", " off"].concat(),
        ["rul", "ing"].concat(),
        ["ta", "sk-"].concat(),
        ["pl", "an-"].concat(),
        ["the sp", "ec says"].concat(),
        ["per the sp", "ec"].concat(),
        ["usa", "ge probe"].concat(),
        ["rev", "iew round"].concat(),
        ["des", "ign doc"].concat(),
        ["pin", "ned by"].concat(),
        ["acc", "epted deviation"].concat(),
        ["the pro", "be"].concat(),
        ["the adv", "ersary"].concat(),
        ["gaunt", "let"].concat(),
        ["sp", "ec-first"].concat(),
    ];
    let numbered = [["ro", "und-"].concat()];
    let fixture = [
        "a hand-off is fine\n```\nthe rul",
        "ing in a fence\n```\n`ta",
        "sk-3` in a span\nsee ro",
        "und-2 and the des",
        "ign doc\nround-trip is fine\n",
    ]
    .concat();
    let control = vocabulary_hits(&labelled_prose("fixture", &fixture), &needles, &numbered);
    assert_eq!(
        control,
        [
            ["fixture:6: spells des", "ign doc"].concat(),
            ["fixture:6: spells ro", "und-"].concat(),
        ],
        "only the prose line outside fences and spans is a hit"
    );

    let hits = vocabulary_hits(&mesh_doc_prose(&wiki), &needles, &numbered);
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}
