//! Opt-in doc lint: checks the wiki pages that describe a graph agent's
//! `session:` block against the code they describe.
//!
//! The wiki is a separate clone, so these tests run only when `COYOTE_WIKI_DIR`
//! points at it; a relative value is resolved against the crate root, so from
//! a `.worktrees/<name>` checkout the sibling clone is `../../../coyote.wiki`:
//!
//! ```sh
//! COYOTE_WIKI_DIR=../coyote.wiki cargo test --test graph_sessions_wiki_docs
//! ```
//!
//! Without the variable the wiki half of every test is skipped (printed as
//! `skipping:`) and the repo-only assertions still run, so a plain `cargo test`
//! is unaffected. With it set to a directory that has no `Graph-Agents.md` the
//! tests fail rather than pass against nothing.
//!
//! There is no lib target, so the code side of each check is read from the
//! repo's text: the `SessionBlock` / `RouteBlock` / `ChatBlock` fields in
//! `src/graph/types.rs`, the deny list and the pass messages in
//! `src/graph/validator.rs`, the load-time messages and the teaching sentence
//! in `src/graph/parser.rs`, the `routing:` line in `src/graph/route.rs`, the
//! record's headings in `src/graph/last_run.rs`, the `.run` command and the
//! turn hook variables in `src/repl/mod.rs`, and the `session:` block of the
//! repo-root `graph.example.yaml`.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const WIKI_DIR: &str = "COYOTE_WIKI_DIR";
const RECIPE: &str = "`COYOTE_WIKI_DIR=../coyote.wiki cargo test --test graph_sessions_wiki_docs`";
const SENTINEL_PAGE: &str = "Graph-Agents.md";
/// The pages the block is documented on; every one is swept for the wording
/// that described graph agents without sessions.
const PAGES: [&str; 5] = [
    "Graph-Agents.md",
    "REPL.md",
    "Sessions.md",
    "Hooks.md",
    "Environment-Variables.md",
];
const GRAPH_BRANCH: &str = "COYOTE_GRAPH_BRANCH";
const GRAPH_ROUTE: &str = "COYOTE_GRAPH_ROUTE";
/// The lead of the sentence that lists every denied `chat.tools` entry.
const DENY_LEAD: &str = "Denied in `chat.tools`:";
/// The caveat the Sessions section carries about a mistyped block name,
/// whitespace flattened.
const TYPO_CAVEAT: &str = "an unrecognised top-level key such as `sesion:` is ignored by the loader like any other, so a mistyped block name silently disables sessions";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every edited section and row together.
fn edited_texts(wiki: &Path) -> Vec<(String, String)> {
    let mut texts: Vec<(String, String)> = edited_sections(wiki)
        .into_iter()
        .map(|(label, text)| (label.to_string(), text))
        .collect();
    texts.extend(edited_rows(wiki));
    texts
}

fn read(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    // Line endings are normalised so the `\n`-anchored block and heading
    // searches below hold on a CRLF checkout.
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn src(parts: &[&str]) -> String {
    let mut path = repo_root().join("src");
    for part in parts {
        path = path.join(part);
    }
    read(path)
}

/// The wiki checkout, or `None` after printing the conventional `skipping:`
/// line. A set variable that does not point at the wiki is a failure, not a
/// skip.
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
        dir.join(SENTINEL_PAGE).is_file(),
        "{WIKI_DIR}={} has no {SENTINEL_PAGE}; point it at a coyote.wiki checkout",
        dir.display()
    );
    Some(dir)
}

/// Every `"..."` literal in `source`, in order, with `\"`, `\\` and `\n`
/// unescaped and a `\` line continuation (the backslash, the newline and the
/// next line's indentation) dropped the way rustc drops it.
fn string_literals(source: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut chars = source.chars().peekable();
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
                    Some('n') => literal.push('\n'),
                    Some('\n') => {
                        while chars.peek().is_some_and(|c| c.is_whitespace()) {
                            chars.next();
                        }
                    }
                    other => panic!(
                        "unexpected escape {other:?} in {literal:?}; extend string_literals in tests/graph_sessions_wiki_docs.rs"
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

/// The text under `heading`, a `# `, `## ` or `### ` line of `page`, up to the
/// next heading of the same or a higher level.
fn section(label: &str, page: &str, heading: &str) -> String {
    let marker = format!("\n{heading}\n");
    let at = page
        .find(&marker)
        .unwrap_or_else(|| panic!("{label} has no `{heading}` heading"));
    let body = &page[at + marker.len()..];
    let level = heading.bytes().take_while(|b| *b == b'#').count();
    let end = (1..=level)
        .filter_map(|depth| body.find(&format!("\n{} ", "#".repeat(depth))))
        .min()
        .unwrap_or(body.len());
    body[..end].to_string()
}

/// `text` with every whitespace run collapsed to one space, so a sentence the
/// page wraps is matched as prose.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` with the `> ` of every block-quoted line removed, then flattened.
fn flat_unquoted(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(|line| {
            line.strip_prefix("> ")
                .or(line.strip_prefix('>'))
                .unwrap_or(line)
        })
        .collect();
    flat(&lines.join("\n"))
}

/// Every `` `...` `` span of `text`, in order.
fn code_spans(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The table row of `page` whose first cell is `first`.
fn table_row(label: &str, page: &str, first: &str) -> Vec<String> {
    page.lines()
        .filter(|line| line.starts_with('|'))
        .map(table_cells)
        .find(|cells| cells.first().is_some_and(|cell| cell == first))
        .unwrap_or_else(|| panic!("{label} has no table row whose first cell is {first}"))
}

/// The field names of `pub struct {name}` in `source`, in declaration order.
/// None of the session structs renames a field, in the body or in a struct
/// attribute, so these are the serde names.
fn struct_fields(source: &str, name: &str) -> Vec<String> {
    let head = format!("pub struct {name} {{");
    let start = source
        .find(&head)
        .unwrap_or_else(|| panic!("src/graph/types.rs declares {name}"));
    let body = &source[start + head.len()..];
    let end = body
        .find("\n}")
        .unwrap_or_else(|| panic!("{name} ends with a `}}` line"));
    let body = &body[..end];
    assert!(
        !body.contains("rename")
            && !struct_attributes(source, name)
                .iter()
                .any(|a| a.contains("rename")),
        "{name} renames its fields; this lint reads Rust names as serde names"
    );
    body.lines()
        .filter_map(|line| line.trim().strip_prefix("pub "))
        .map(|rest| rest.split(':').next().unwrap().trim().to_string())
        .collect()
}

/// The attribute lines directly above `pub struct {name}`.
fn struct_attributes(source: &str, name: &str) -> Vec<String> {
    let head = format!("pub struct {name} {{");
    let start = source
        .find(&head)
        .unwrap_or_else(|| panic!("src/graph/types.rs declares {name}"));
    source[..start]
        .lines()
        .rev()
        .take_while(|line| line.trim_start().starts_with("#["))
        .map(|line| line.trim().to_string())
        .collect()
}

fn session_struct_fields() -> (Vec<String>, Vec<String>, Vec<String>) {
    let types = src(&["graph", "types.rs"]);
    (
        struct_fields(&types, "SessionBlock"),
        struct_fields(&types, "RouteBlock"),
        struct_fields(&types, "ChatBlock"),
    )
}

/// The col-0 `session:` block of a yaml document or snippet as a mapping:
/// from the `session:` line through every indented or blank line after it.
fn session_block(label: &str, yaml: &str) -> serde_yaml::Mapping {
    let text = format!("\n{yaml}");
    let at = text
        .find("\nsession:")
        .unwrap_or_else(|| panic!("{label} has no col-0 `session:` line"));
    let mut lines = vec!["session:"];
    for line in text[at + "\nsession:".len()..].lines().skip(1) {
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            lines.push(line);
        } else {
            break;
        }
    }
    let value: serde_yaml::Value = serde_yaml::from_str(&lines.join("\n"))
        .unwrap_or_else(|e| panic!("{label}: the `session:` block is not valid YAML: {e}"));
    value
        .get("session")
        .and_then(serde_yaml::Value::as_mapping)
        .cloned()
        .unwrap_or_else(|| panic!("{label}: `session:` is not a mapping"))
}

fn mapping_keys(label: &str, mapping: &serde_yaml::Mapping) -> BTreeSet<String> {
    mapping
        .keys()
        .map(|key| {
            key.as_str()
                .unwrap_or_else(|| panic!("{label} has a non-string key {key:?}"))
                .to_string()
        })
        .collect()
}

fn nested_keys(label: &str, mapping: &serde_yaml::Mapping, key: &str) -> BTreeSet<String> {
    let nested = mapping
        .get(key)
        .and_then(serde_yaml::Value::as_mapping)
        .unwrap_or_else(|| panic!("{label}: `session.{key}` is not a mapping"));
    mapping_keys(&format!("{label} session.{key}"), nested)
}

/// Asserts the `session:` block in `yaml` spells exactly the three structs'
/// field sets, so the documented keys load.
fn assert_block_matches_schema(label: &str, yaml: &str) {
    let (session, route, chat) = session_struct_fields();
    let block = session_block(label, yaml);
    let as_set = |fields: &[String]| fields.iter().cloned().collect::<BTreeSet<_>>();
    assert_eq!(
        mapping_keys(label, &block),
        as_set(&session),
        "{label}: `session:` keys differ from SessionBlock"
    );
    assert_eq!(
        nested_keys(label, &block, "route"),
        as_set(&route),
        "{label}: `session.route` keys differ from RouteBlock"
    );
    assert_eq!(
        nested_keys(label, &block, "chat"),
        as_set(&chat),
        "{label}: `session.chat` keys differ from ChatBlock"
    );
}

/// The literals of a `pub const NAME: [&str; N] = [...]` in `source`.
fn const_str_array(source: &str, name: &str) -> Vec<String> {
    let head = format!("pub const {name}: [&str; ");
    let start = source
        .find(&head)
        .unwrap_or_else(|| panic!("src/graph/validator.rs declares {name}"));
    let body = &source[start..];
    let end = body
        .find("];")
        .unwrap_or_else(|| panic!("{name} ends with `];`"));
    let literals = string_literals(&body[..end]);
    let declared: usize = body[head.len()..]
        .split(']')
        .next()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("{name} declares its length"));
    assert_eq!(literals.len(), declared, "{name} holds {declared} entries");
    literals
}

/// The source of the function whose signature starts with `head` in
/// `source`, up to its closing brace: the first `}` standing alone on a line
/// at the function's own indentation.
fn fn_body(source: &str, head: &str) -> String {
    let start = source
        .find(head)
        .unwrap_or_else(|| panic!("the source defines {head}"));
    let indent: String = source[..start]
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    let body = &source[start..];
    let close = format!("\n{indent}}}");
    let end = body
        .find(&close)
        .unwrap_or_else(|| panic!("{head} closes with `}}` on its own line"));
    body[..end].to_string()
}

/// The value of `const {name}: &str = "...";` in `source`, whatever its
/// visibility.
fn const_str(source: &str, name: &str) -> String {
    let head = format!("const {name}: &str = ");
    let start = source
        .find(&head)
        .unwrap_or_else(|| panic!("the source declares {name}"));
    let rest = &source[start + head.len()..];
    let end = rest
        .find(';')
        .unwrap_or_else(|| panic!("{name} ends with `;`"));
    let mut literals = string_literals(&rest[..=end]);
    assert_eq!(literals.len(), 1, "{name} is one string literal");
    literals.remove(0)
}

/// The directory a workspace session for an agent lives in, as the wiki
/// spells it: the path `paths::workspace_agent_sessions_dir` builds under the
/// workspace config dir, with `<agent>` for the name.
fn workspace_agent_session_dir_literal() -> String {
    let paths = src(&["config", "paths.rs"]);
    let body = fn_body(&paths, "pub fn workspace_agent_sessions_dir");
    let agents = body
        .find("AGENTS_DIR_NAME")
        .expect("the agent session dir joins AGENTS_DIR_NAME");
    let sessions = body
        .find("SESSIONS_DIR_NAME")
        .expect("the agent session dir joins SESSIONS_DIR_NAME");
    assert!(
        agents < sessions,
        "agents/<name>/sessions, in that order:\n{body}"
    );
    let config = src(&["config", "mod.rs"]);
    format!(
        "`{}/{}/<agent>/{}/`",
        const_str(&config, "WORKSPACE_COYOTE_DIR_NAME"),
        const_str(&config, "AGENTS_DIR_NAME"),
        const_str(&config, "SESSIONS_DIR_NAME")
    )
}

/// The paragraph of `page` whose first line starts with `lead`: that line
/// and every following line up to the next blank one.
fn paragraph(label: &str, page: &str, lead: &str) -> String {
    let mut lines = page.lines().skip_while(|line| !line.starts_with(lead));
    let first = lines
        .next()
        .unwrap_or_else(|| panic!("{label} has no paragraph opening {lead:?}"));
    let mut out = vec![first];
    out.extend(lines.take_while(|line| !line.trim().is_empty()));
    out.join("\n")
}

/// The comment text on the `tools:` entry of the col-0 `session:` block of
/// `yaml` and on the lines under it (its list items and their continuation
/// comments), flattened. The block is delimited the way `session_block`
/// delimits it.
fn session_tools_comment(yaml: &str) -> String {
    let text = format!("\n{yaml}");
    let at = text.find("\nsession:").expect("a col-0 `session:` line");
    let block: Vec<&str> = text[at + 1..]
        .lines()
        .enumerate()
        .take_while(|(i, line)| *i == 0 || line.is_empty() || line.starts_with([' ', '\t']))
        .map(|(_, line)| line)
        .collect();
    let indent_of = |line: &str| line.len() - line.trim_start().len();
    let tools = block
        .iter()
        .position(|line| line.trim_start().starts_with("tools:"))
        .expect("the session block has a `tools:` entry");
    let indent = indent_of(block[tools]);
    let comments: Vec<&str> = block[tools..]
        .iter()
        .enumerate()
        .take_while(|(i, line)| *i == 0 || line.trim().is_empty() || indent_of(line) > indent)
        .filter_map(|(_, line)| line.split_once('#').map(|(_, comment)| comment))
        .collect();
    flat(&comments.join(" "))
}

fn deny_list() -> (Vec<String>, Vec<String>) {
    let validator = src(&["graph", "validator.rs"]);
    let names = const_str_array(&validator, "CHAT_TOOL_DENY_LIST");
    let prefixes = const_str_array(&validator, "CHAT_TOOL_DENY_PREFIXES");
    assert!(
        prefixes.iter().all(|p| p.ends_with("__")),
        "every denied prefix ends with `__`: {prefixes:?}"
    );
    (names, prefixes)
}

/// The description of `.run` as `REPL_COMMANDS` in `src/repl/mod.rs` spells it.
fn run_command_description() -> String {
    let source = src(&["repl", "mod.rs"]);
    let start = source
        .find("static REPL_COMMANDS")
        .expect("src/repl/mod.rs declares REPL_COMMANDS");
    let block = &source[start..];
    let end = block
        .find("\n});")
        .expect("the REPL_COMMANDS block ends with a `});` line");
    let literals = string_literals(&block[..end]);
    let at = literals
        .iter()
        .position(|l| l == ".run")
        .expect("REPL_COMMANDS has a `.run` entry");
    literals
        .get(at + 1)
        .cloned()
        .expect("the `.run` entry carries a description")
}

/// The teaching sentence `no_session_block_text` formats, with `<name>` in
/// the agent's place.
fn teaching_sentence() -> String {
    let parser = src(&["graph", "parser.rs"]);
    let literals = string_literals(&fn_body(&parser, "pub fn no_session_block_text"));
    assert_eq!(
        literals.len(),
        1,
        "no_session_block_text has one format string"
    );
    literals[0].replace("{agent_name}", "<name>")
}

/// The words of `text` in lowercase with punctuation stripped, for
/// word-bounded phrase checks.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

fn phrase_hits(text: &str) -> Vec<String> {
    let words = words(text);
    let mut hits = Vec::new();
    for pair in words.windows(2) {
        if (pair[0] == "as" && pair[1] == "of") || (pair[0] == "new" && pair[1] == "in") {
            hits.push(pair.join(" "));
        }
    }
    if words.iter().any(|w| w == "previously") {
        hits.push("previously".to_string());
    }
    hits
}

/// The `tools:` comment in the example's `session:` block enumerates the
/// refused tool families; it must name every denied prefix.
#[test]
fn example_tools_comment_names_every_denied_prefix() {
    let (_, prefixes) = deny_list();
    let comment = session_tools_comment(&read(repo_root().join("graph.example.yaml")));
    for prefix in prefixes {
        assert!(
            comment.contains(&prefix),
            "the graph.example.yaml `tools:` comment does not name {prefix}: {comment}"
        );
    }
}

#[test]
fn string_literals_drops_line_continuations_like_rustc() {
    let source = "format!(\n    \"one \\\n     two\"\n)";
    assert_eq!(string_literals(source), ["one two"]);
    assert_eq!(string_literals(r#"a "x\"y" b "\\n" c"#), ["x\"y", "\\n"]);
}

#[test]
fn section_scopes_level_one_and_level_two_headings() {
    let page = "intro\n\n# A\n\na body\n\n## A1\n\na1 body\n\n# B\n\nb body\n";
    assert_eq!(
        section("fixture", page, "# A"),
        "\na body\n\n## A1\n\na1 body\n"
    );
    assert_eq!(section("fixture", page, "## A1"), "\na1 body\n");
    assert_eq!(section("fixture", page, "# B"), "\nb body\n");
}

#[test]
fn wording_for_graph_agents_without_sessions_is_gone() {
    let Some(wiki) = wiki_dir() else { return };
    for page in PAGES {
        assert!(
            wiki.join(page).is_file(),
            "{} has no {page}",
            wiki.display()
        );
    }
    let retired = [
        "do not support sessions",
        "does not support sessions",
        "runs the graph fresh",
    ];
    let mut hits = Vec::new();
    for entry in fs::read_dir(&wiki).expect("read the wiki checkout") {
        let path = entry.expect("wiki entry").path();
        if path.extension().is_some_and(|ext| ext == "md") {
            let text = flat(&read(&path));
            for needle in retired {
                if text.contains(needle) {
                    hits.push(format!("{}: {needle:?}", path.display()));
                }
            }
        }
    }
    assert!(hits.is_empty(), "{}", hits.join("\n"));

    let repl = flat(&read(wiki.join("REPL.md")));
    assert!(
        !repl.contains("is not supported inside a graph-based agent"),
        "REPL.md still carries the `.undo` note for graph agents"
    );
}

#[test]
fn repl_run_row_and_section_match_the_command_literal() {
    let description = run_command_description();
    assert!(
        description.starts_with("Run the graph"),
        "the `.run` description reads {description:?}"
    );

    let Some(wiki) = wiki_dir() else { return };
    let repl = read(wiki.join("REPL.md"));
    let row = table_row("REPL.md", &repl, "`.run`");
    assert_eq!(
        row.get(1).map(String::as_str),
        Some(description.as_str()),
        "the REPL.md `.run` row does not read the command's description"
    );
    let heading = format!("## `.run` - {description}");
    let run = section("REPL.md", &repl, &heading);
    let run_flat = flat(&run);
    for needle in [
        ".run <prompt>",
        "`.macro run`",
        "`{{initial_prompt}}`",
        "`COYOTE_GRAPH_ROUTE=forced`",
        "`.undo` restores the bare prompt",
    ] {
        assert!(
            run_flat.contains(needle),
            "the `.run` section does not say {needle:?}"
        );
    }
    let agent_section = section("REPL.md", &repl, "## `.agent` - Chat with an AI agent");
    assert!(
        agent_section.contains("| `.run`"),
        "the `.run` row sits in the `.agent` command table"
    );
    for heading in [
        "## `.session` - Session management",
        "## `.agent` - Chat with an AI agent",
    ] {
        let text = flat(&section("REPL.md", &repl, heading));
        assert!(
            text.contains("`session:` block"),
            "{heading} does not mention the `session:` block"
        );
    }
}

#[test]
fn documented_session_block_keys_are_the_schema_fields() {
    let (session, route, chat) = session_struct_fields();
    assert_eq!(session, ["expose", "route", "chat"]);
    assert_eq!(route, ["model", "instructions"]);
    assert_eq!(chat, ["model", "instructions", "memory", "tools"]);
    let example = read(repo_root().join("graph.example.yaml"));
    assert_block_matches_schema("graph.example.yaml", &example);

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    let sessions = section(SENTINEL_PAGE, &page, "# Sessions");
    let yaml = sessions
        .split("```yaml\n")
        .nth(1)
        .and_then(|rest| rest.split("\n```").next())
        .expect("the Sessions section opens with a ```yaml block");
    assert_block_matches_schema("Graph-Agents.md # Sessions", yaml);
    assert_eq!(
        page.matches("\nsession:").count(),
        2,
        "Graph-Agents.md spells the block twice: in the top-level yaml and under # Sessions"
    );
    let top = page
        .split("```yaml\nname: my-graph-agent\n")
        .nth(1)
        .and_then(|rest| rest.split("\n```").next())
        .expect("the top-level fields yaml block");
    assert_block_matches_schema("Graph-Agents.md top-level yaml", top);
    assert!(
        flat(&page).contains("- **`session`:**"),
        "Graph-Agents.md lists `session` among the top-level field bullets"
    );
}

#[test]
fn deny_list_sentence_names_exactly_the_denied_tools() {
    let (names, prefixes) = deny_list();
    let expected: BTreeSet<String> = names.iter().chain(&prefixes).cloned().collect();
    assert_eq!(expected.len(), names.len() + prefixes.len());

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    let sessions = section(SENTINEL_PAGE, &page, "# Sessions");
    let paragraph = sessions
        .split("\n\n")
        .find(|p| p.trim_start().starts_with(DENY_LEAD))
        .unwrap_or_else(|| panic!("the Sessions section has a paragraph opening {DENY_LEAD:?}"));
    let mut spans = code_spans(paragraph);
    assert_eq!(spans.first().map(String::as_str), Some("chat.tools"));
    spans.remove(0);
    let documented: BTreeSet<String> = spans
        .iter()
        .map(|span| span.trim_end_matches('*').to_string())
        .collect();
    assert_eq!(
        documented.len(),
        spans.len(),
        "the deny sentence names a tool twice: {spans:?}"
    );
    assert_eq!(
        documented, expected,
        "the deny sentence differs from CHAT_TOOL_DENY_LIST and CHAT_TOOL_DENY_PREFIXES"
    );
}

#[test]
fn hook_pages_carry_the_graph_turn_variables() {
    let repl = src(&["repl", "mod.rs"]);
    for var in [GRAPH_BRANCH, GRAPH_ROUTE] {
        assert!(
            repl.contains(&format!("\"{var}\"")),
            "src/repl/mod.rs pushes {var} onto the turn extras"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let hooks = read(wiki.join("Hooks.md"));
    for event in ["turn.completed", "turn.interrupted", "turn.failed"] {
        let row = table_row("Hooks.md", &hooks, &format!("`{event}`"));
        let extras = &row[2];
        for var in [GRAPH_BRANCH, GRAPH_ROUTE] {
            assert!(
                extras.contains(&format!("`{var}`")),
                "the Hooks.md `{event}` row does not list {var}: {extras}"
            );
        }
    }
    let started = table_row("Hooks.md", &hooks, "`turn.started`");
    for var in [GRAPH_BRANCH, GRAPH_ROUTE] {
        assert!(
            !started[2].contains(var),
            "the Hooks.md `turn.started` row must not list {var}"
        );
    }
    let footnote = flat(&hooks);
    for needle in [
        "`turn.started` fires before the branch is decided and never carries them",
        "`classified` (the router decided) or `forced` (`.run <prompt>`)",
    ] {
        assert!(
            footnote.contains(needle),
            "Hooks.md does not say {needle:?}"
        );
    }

    let env = read(wiki.join("Environment-Variables.md"));
    let branch = table_row(
        "Environment-Variables.md",
        &env,
        &format!("`{GRAPH_BRANCH}`"),
    );
    for needle in [
        "`run` or `chat`",
        "`turn.completed`",
        "`turn.interrupted`",
        "`turn.failed`",
    ] {
        assert!(
            branch[1].contains(needle),
            "the {GRAPH_BRANCH} row does not say {needle:?}: {}",
            branch[1]
        );
    }
    let route = table_row(
        "Environment-Variables.md",
        &env,
        &format!("`{GRAPH_ROUTE}`"),
    );
    for needle in ["`classified`", "`forced`", &format!("`{GRAPH_BRANCH}`")] {
        assert!(
            route[1].contains(needle),
            "the {GRAPH_ROUTE} row does not say {needle:?}: {}",
            route[1]
        );
    }
}

#[test]
fn sessions_section_quotes_the_teaching_sentence_and_the_routing_line() {
    let sentence = teaching_sentence();
    assert!(sentence.contains("`session:` block"), "{sentence}");
    let route = src(&["graph", "route.rs"]);
    let last_run = src(&["graph", "last_run.rs"]);
    let code_needles = [
        (&route, "routing: run"),
        (&route, "routing: chat"),
        (&last_run, "Last graph run (completed "),
        (&last_run, "### Output"),
        (&last_run, "### Exposed state"),
        (&last_run, "### Trace"),
    ];
    for (source, needle) in code_needles {
        assert!(
            source.contains(needle),
            "the code no longer spells {needle:?}"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    let sessions = flat_unquoted(&section(SENTINEL_PAGE, &page, "# Sessions"));
    assert!(
        sessions.contains(&flat(&sentence)),
        "the Sessions section does not quote the teaching sentence {sentence:?}"
    );
    for (_, needle) in code_needles {
        assert!(
            sessions.contains(needle),
            "the Sessions section does not show {needle:?}"
        );
    }
    for needle in [
        TYPO_CAVEAT,
        "`.run <prompt>`",
        "`.macro run`",
        "`last_run:`",
        &workspace_agent_session_dir_literal(),
        "`.undo`",
        "`agent__spawn`",
        "without `last_run:`",
        "`llm.request.*`",
        &format!("`{GRAPH_BRANCH}`"),
        &format!("`{GRAPH_ROUTE}`"),
    ] {
        assert!(
            sessions.contains(needle),
            "the Sessions section does not say {needle:?}"
        );
    }
    let limitations = flat(&section(SENTINEL_PAGE, &page, "# Limitations / Gotchas"));
    assert!(
        limitations.contains("`sesion:`"),
        "the Limitations list does not carry the mistyped-block-name gotcha"
    );
}

/// The run record's trace lines end in the route tail the console narration
/// prints, so the fallback mark the page documents is the one constant the
/// code spells it with.
#[test]
fn sessions_section_describes_the_trace_line_tails_with_the_code_suffix() {
    let logging = src(&["graph", "logging.rs"]);
    let suffix = const_str(&logging, "FALLBACK_SUFFIX");
    assert!(
        suffix.starts_with(' '),
        "the suffix follows the target: {suffix:?}"
    );
    let route_tail = fn_body(&logging, "pub(super) fn route_suffix");
    assert!(
        route_tail.contains("FALLBACK_SUFFIX"),
        "route_suffix marks a fallback hop with FALLBACK_SUFFIX:\n{route_tail}"
    );
    let last_run = src(&["graph", "last_run.rs"]);
    assert!(
        fn_body(&last_run, "pub fn render(&self)").contains("route_suffix("),
        "the stored trace renders its tails through route_suffix"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    let sessions = flat_unquoted(&section(SENTINEL_PAGE, &page, "# Sessions"));
    for needle in [
        "`-> <node>`".to_string(),
        "`-> END`".to_string(),
        format!("`-> <node>{suffix}`"),
    ] {
        assert!(
            sessions.contains(&needle),
            "the Sessions section does not describe the trace tail {needle}"
        );
    }
    assert!(
        sessions.contains(&format!("`fallback:` edge renders as `-> <node>{suffix}`")),
        "the Sessions section does not tie the suffix to the `fallback:` edge"
    );
}

#[test]
fn only_the_nested_session_keys_are_rejected() {
    let types = src(&["graph", "types.rs"]);
    let deny = "#[serde(deny_unknown_fields)]";
    assert!(
        !struct_attributes(&types, "Graph").iter().any(|a| a == deny),
        "Graph rejects unknown top-level keys; the typo caveat on the wiki is stale"
    );
    for name in ["SessionBlock", "RouteBlock", "ChatBlock"] {
        assert!(
            struct_attributes(&types, name).iter().any(|a| a == deny),
            "{name} accepts unknown keys; the wiki says the nested keys are checked"
        );
    }
    let parser = src(&["graph", "parser.rs"]);
    assert!(
        parser.contains("Optional top-level block: `session` (keys: expose, route, chat)."),
        "the unknown-field hint names the session block"
    );
}

#[test]
fn validation_bullets_quote_the_messages() {
    let validator = src(&["graph", "validator.rs"]);
    let parser = src(&["graph", "parser.rs"]);
    let pass = fn_body(&validator, "fn validate_session_block(&self");
    let tail_start = validator
        .find("pub const CHAT_MEMORY_INLINED_TAIL")
        .expect("validator.rs declares CHAT_MEMORY_INLINED_TAIL");
    let tail = &validator[tail_start..];
    let tail_end = tail
        .find(";\n")
        .expect("the const ends with `;` at the end of a line");
    let tail = tail[..tail_end].to_string();
    let messages = [
        string_literals(&fn_body(&validator, "pub fn denied_chat_tool_message")),
        string_literals(&pass),
        string_literals(&tail),
        string_literals(&fn_body(&parser, "fn validate_session_block(graph")),
    ]
    .concat();
    let fragments = [
        "session.chat.instructions must be non-empty",
        "is not declared anywhere (add it to initial_state, or have a node write it via state_updates / output_schema)",
        "is not allowed in the chat turn (write/spawn/builtin tools are denied)",
        "must name a GLOBAL tool declared for this agent (agent-local tools.* subcommands are not allowed)",
        "references unknown model",
        "tools unavailable on",
        "(no function calling)",
        "; the MEMORY.md section is inlined with the without-tools cap",
        "function calling is disabled (function_calling_support: false), so these tools will not be declared",
        "memory is disabled (memory: false), so no MEMORY.md section and no memory tools",
    ];
    for fragment in fragments {
        assert!(
            messages.iter().any(|l| l.contains(fragment)),
            "no session message in the code carries {fragment:?}"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    let validation = flat(&section(SENTINEL_PAGE, &page, "# Validation"));
    for fragment in fragments {
        assert!(
            validation.contains(fragment),
            "the Validation section does not quote {fragment:?}"
        );
    }
    for needle in [
        "**Errors (abort startup)**",
        "**Warnings (printed, execution continues)**",
    ] {
        assert!(
            validation.contains(needle),
            "the Validation section lost {needle:?}"
        );
    }
}

#[test]
fn sessions_page_describes_completion_for_graph_agents() {
    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Sessions.md"));
    let listing = flat(&section("Sessions.md", &page, "## Listing and completion"));
    assert!(
        listing.contains("only when its `graph.yaml` carries a `session:` block"),
        "Sessions.md does not say when a graph agent's sessions are offered"
    );
}

/// The lead of the Hooks.md footnote paragraph about the graph turn variables.
const ROUTED_FOOTNOTE_LEAD: &str = "(routed graph turns)";

/// `(label, text)` for every prose section the `session:` block is
/// documented in; the table rows it touched are `edited_rows`.
fn edited_sections(wiki: &Path) -> Vec<(&'static str, String)> {
    let graph = read(wiki.join(SENTINEL_PAGE));
    let repl = read(wiki.join("REPL.md"));
    let sessions = read(wiki.join("Sessions.md"));
    let hooks = read(wiki.join("Hooks.md"));
    let run_heading = format!("## `.run` - {}", run_command_description());
    vec![
        (
            "Graph-Agents.md # Sessions",
            section(SENTINEL_PAGE, &graph, "# Sessions"),
        ),
        (
            "Graph-Agents.md # Validation",
            section(SENTINEL_PAGE, &graph, "# Validation"),
        ),
        (
            "Graph-Agents.md # Invocation Entry Points",
            section(SENTINEL_PAGE, &graph, "# Invocation Entry Points"),
        ),
        (
            "Graph-Agents.md # Limitations / Gotchas",
            section(SENTINEL_PAGE, &graph, "# Limitations / Gotchas"),
        ),
        ("REPL.md .run", section("REPL.md", &repl, &run_heading)),
        (
            "REPL.md .session",
            section("REPL.md", &repl, "## `.session` - Session management"),
        ),
        (
            "REPL.md .agent",
            section("REPL.md", &repl, "## `.agent` - Chat with an AI agent"),
        ),
        (
            "REPL.md .undo",
            section("REPL.md", &repl, "## `.undo` - Undo the last exchange"),
        ),
        (
            "Sessions.md listing",
            section("Sessions.md", &sessions, "## Listing and completion"),
        ),
        (
            "Hooks.md routed graph turns footnote",
            paragraph("Hooks.md", &hooks, ROUTED_FOOTNOTE_LEAD),
        ),
    ]
}

/// The label prefix of the Hooks.md rows in `edited_rows`; the ASCII sweep
/// treats those rows' footnote marker specially.
const HOOKS_ROWS_LABEL: &str = "Hooks.md turn row";

/// The three Hooks.md turn rows that gained the graph variables and the two
/// Environment-Variables.md graph rows, one string each.
fn edited_rows(wiki: &Path) -> Vec<(String, String)> {
    let hooks = read(wiki.join("Hooks.md"));
    let env = read(wiki.join("Environment-Variables.md"));
    let mut rows: Vec<(String, String)> = ["turn.completed", "turn.interrupted", "turn.failed"]
        .iter()
        .map(|event| {
            (
                format!("{HOOKS_ROWS_LABEL} `{event}`"),
                table_row("Hooks.md", &hooks, &format!("`{event}`")).join(" "),
            )
        })
        .collect();
    rows.extend([GRAPH_BRANCH, GRAPH_ROUTE].iter().map(|var| {
        (
            format!("Environment-Variables.md `{var}` row"),
            table_row("Environment-Variables.md", &env, &format!("`{var}`")).join(" "),
        )
    }));
    rows
}

#[test]
fn edited_sections_read_as_if_the_feature_always_existed() {
    let control = phrase_hits("As of today this was previously new in 1.0; areas of it are new.");
    assert_eq!(control, ["as of", "new in", "previously"]);
    assert!(!run_command_description().is_empty());

    let Some(wiki) = wiki_dir() else { return };
    let hits: Vec<String> = edited_texts(&wiki)
        .iter()
        .flat_map(|(label, text)| {
            phrase_hits(text)
                .into_iter()
                .map(move |hit| format!("{label}: {hit:?}"))
        })
        .collect();
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

/// The needles are assembled at runtime so a sweep of this file never trips
/// on its own text. Each is checked case-insensitively.
fn planning_label_needles() -> Vec<String> {
    vec![
        ["ta", "sk-"].concat(),
        ["pl", "an-"].concat(),
        ["rul", "ing"].concat(),
        ["acc", "eptance criter"].concat(),
    ]
}

/// `label: needle` for every needle `text` carries.
fn planning_label_hits(label: &str, text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    planning_label_needles()
        .into_iter()
        .filter(|needle| lower.contains(needle.as_str()))
        .map(|needle| format!("{label}: {needle:?}"))
        .collect()
}

#[test]
fn no_edited_doc_or_this_lint_carries_a_planning_label() {
    let control = planning_label_hits("fixture", &["see TA", "SK-7 and the rul", "ing"].concat());
    assert_eq!(control.len(), 2, "{control:?}");
    let mut hits = Vec::new();
    for path in ["graph.example.yaml", "tests/graph_sessions_wiki_docs.rs"] {
        hits.extend(planning_label_hits(path, &read(repo_root().join(path))));
    }
    assert!(hits.is_empty(), "{}", hits.join("\n"));

    let Some(wiki) = wiki_dir() else { return };
    let hits: Vec<String> = edited_texts(&wiki)
        .iter()
        .flat_map(|(label, text)| planning_label_hits(label, text))
        .collect();
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

/// Every edited section and row is ASCII. The Hooks.md turn rows are checked
/// with their one pre-existing U+2020 footnote marker removed; the marker
/// predates the rows' graph variables and is counted so a second one trips.
#[test]
fn edited_prose_is_ascii() {
    assert!(!run_command_description().is_empty());

    let Some(wiki) = wiki_dir() else { return };
    let mut hits = Vec::new();
    for (label, mut text) in edited_texts(&wiki) {
        if label.starts_with(HOOKS_ROWS_LABEL) {
            assert_eq!(
                text.matches('\u{2020}').count(),
                1,
                "{label} carries exactly one footnote dagger"
            );
            text = text.replace('\u{2020}', "");
        }
        for (i, line) in text.lines().enumerate() {
            if let Some(c) = line.chars().find(|c| !c.is_ascii()) {
                hits.push(format!("{label} line {}: U+{:04X}", i + 1, c as u32));
            }
        }
    }
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

/// The `# Sessions` section says where a workspace session for an agent
/// keeps its `last_run:`; that directory is whatever
/// `paths::workspace_agent_sessions_dir` builds, which the Sessions page's
/// layout tree draws too.
#[test]
fn sessions_section_names_the_real_workspace_session_directory() {
    let real = workspace_agent_session_dir_literal();
    assert_eq!(real, "`.coyote/agents/<agent>/sessions/`");
    // The agent-less workspace session directory with the agent name below
    // it: a shape that does not exist on disk.
    let config = src(&["config", "mod.rs"]);
    let wrong = format!(
        "`{}/{}/<agent>/`",
        const_str(&config, "WORKSPACE_COYOTE_DIR_NAME"),
        const_str(&config, "SESSIONS_DIR_NAME")
    );

    let Some(wiki) = wiki_dir() else { return };
    let tree = section(
        "Sessions.md",
        &read(wiki.join("Sessions.md")),
        "# Workspace Sessions",
    );
    let agents_line = tree.find("agents/").expect("the layout tree draws agents/");
    let agent_sessions_line = tree[agents_line..]
        .find("sessions/")
        .expect("the layout tree draws sessions/ under the agent");
    assert!(
        tree[agents_line..agents_line + agent_sessions_line].contains("<agent-name>/"),
        "the tree nests sessions/ under agents/<agent-name>/:\n{tree}"
    );

    let page = read(wiki.join(SENTINEL_PAGE));
    let text = flat(&section(SENTINEL_PAGE, &page, "# Sessions"));
    assert!(
        text.contains(&real),
        "the Sessions section does not name the agent's workspace session directory {real}"
    );
    assert!(
        !text.contains(&wrong),
        "the Sessions section names a directory that does not exist: {wrong}"
    );
}
