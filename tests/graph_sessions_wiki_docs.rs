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
//! `src/graph/types.rs`, the denied prefixes and the pass messages in
//! `src/graph/validator.rs`, the load-time messages and the teaching sentence
//! in `src/graph/parser.rs`, the `routing:` line in `src/graph/route.rs`, the
//! record's headings in `src/graph/last_run.rs`, the `.run` command and the
//! turn hook variables in `src/repl/mod.rs`, the chat agent's engage and
//! resume texts in `src/config/request_context.rs`, `src/graph/dispatch.rs`
//! and `src/config/agent.rs`, and the `session:` block of the repo-root
//! `graph.example.yaml`.

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
const GRAPH_CHAT_AGENT: &str = "COYOTE_GRAPH_CHAT_AGENT";
const GRAPH_TURN_VARS: [&str; 3] = [GRAPH_BRANCH, GRAPH_ROUTE, GRAPH_CHAT_AGENT];
/// The lead of the sentence that lists every denied `chat.tools` entry.
const DENY_LEAD: &str = "Denied in `chat.tools`:";
/// The `# Sessions` subsection that documents `chat.agent` and `chat.variables`.
const DELEGATE_HEADING: &str = "### Handing chat turns to a config agent";
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

/// Asserts the `session:` block in `yaml` spells exactly the `SessionBlock`
/// and `RouteBlock` field sets, and a subset of `ChatBlock`'s: the inline
/// keys and `agent` are exclusive, so one example cannot carry both forms.
/// Either way the documented keys load.
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
    let documented = nested_keys(label, &block, "chat");
    assert!(
        documented.is_subset(&as_set(&chat)),
        "{label}: `session.chat` keys {documented:?} are not all ChatBlock fields {chat:?}"
    );
}

/// The delegating keys of `ChatBlock`, which an inline example cannot carry
/// live beside `instructions` and so shows commented out.
const DELEGATE_CHAT_KEYS: [&str; 2] = ["agent", "variables"];

/// Asserts the live `session.chat` keys of `yaml` plus the delegating keys
/// spell every `ChatBlock` field, so an inline example documents the whole
/// struct once the commented-out alternative is counted.
fn assert_inline_chat_keys_complete_the_schema(label: &str, yaml: &str) {
    let (_, _, chat) = session_struct_fields();
    let mut live_and_commented = nested_keys(label, &session_block(label, yaml), "chat");
    for key in DELEGATE_CHAT_KEYS {
        assert!(
            !live_and_commented.contains(key),
            "{label}: an inline example carries `{key}:` live under `session.chat`; \
             it is exclusive with `instructions` and must stay commented out"
        );
    }
    live_and_commented.extend(DELEGATE_CHAT_KEYS.map(str::to_string));
    assert_eq!(
        live_and_commented,
        chat.iter().cloned().collect::<BTreeSet<_>>(),
        "{label}: the live `session.chat` keys plus the commented-out `agent` and `variables` must spell every ChatBlock field"
    );
}

/// The first fenced yaml block of `text`.
fn first_yaml_block(label: &str, text: &str) -> String {
    text.split("```yaml\n")
        .nth(1)
        .and_then(|rest| rest.split("\n```").next())
        .unwrap_or_else(|| panic!("{label} has no ```yaml block"))
        .to_string()
}

/// A code format string as the wiki quotes it: the agent, session and key
/// holes as `<name>` and `<key>`, the chat agent's and the graph agent's as
/// `<chat-agent>` and `<graph-agent>` (or `<agent>` where the session's
/// previous chat agent is meant), the session hole of a typed-back command
/// as `<session>`, the input placeholder spelled out, and the
/// declared-variables hole as its three-name example.
fn with_placeholders(literal: &str) -> String {
    let role = src(&["config", "role.rs"]);
    literal
        .replace("{key}", "<key>")
        .replace("{name}", "<name>")
        .replace("{session_name}", "<name>")
        .replace("{session}", "<session>")
        .replace("{previous}", "<agent>")
        .replace("{delegate_name}", "<chat-agent>")
        .replace("{graph_agent}", "<graph-agent>")
        .replace("{agent_name}", "<graph-agent>")
        .replace(
            "{INPUT_PLACEHOLDER}",
            &const_str(&role, "INPUT_PLACEHOLDER"),
        )
        .replace("{declared_text}", "declared: a, b, c")
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

/// The comment text on the `  chat:` line of `yaml` and on the comment-only
/// lines under it, up to the first live key of the block, flattened.
fn chat_block_lead_comment(yaml: &str) -> String {
    let text = format!("\n{yaml}");
    let at = text
        .find("\n  chat:")
        .expect("a `  chat:` line under `session:`");
    let comments: Vec<&str> = text[at + 1..]
        .lines()
        .enumerate()
        .take_while(|(i, line)| {
            *i == 0 || line.trim().is_empty() || line.trim_start().starts_with('#')
        })
        .filter_map(|(_, line)| line.split_once('#').map(|(_, comment)| comment))
        .collect();
    flat(&comments.join(" "))
}

fn denied_prefixes() -> Vec<String> {
    let validator = src(&["graph", "validator.rs"]);
    let prefixes = const_str_array(&validator, "CHAT_TOOL_DENY_PREFIXES");
    assert!(
        prefixes.iter().all(|p| p.ends_with("__")),
        "every denied prefix ends with `__`: {prefixes:?}"
    );
    prefixes
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
    let prefixes = denied_prefixes();
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
    assert_eq!(
        chat,
        [
            "model",
            "instructions",
            "memory",
            "tools",
            "agent",
            "variables"
        ]
    );
    let example = read(repo_root().join("graph.example.yaml"));
    assert_block_matches_schema("graph.example.yaml", &example);
    assert_inline_chat_keys_complete_the_schema("graph.example.yaml", &example);
    assert!(
        example.contains("\n    # agent: research-helper\n"),
        "graph.example.yaml shows `agent:` commented out under `session.chat`"
    );
    assert!(
        example.contains("\n    # variables: { tone: terse }\n"),
        "graph.example.yaml shows `variables:` commented out under `session.chat`"
    );
    assert!(
        example.contains("Required unless `agent:` is set"),
        "graph.example.yaml says `instructions:` is required unless `agent:` is set"
    );
    let comments = chat_block_lead_comment(&example);
    for needle in [
        "`agent` is exclusive with `instructions`, `model` and any non-default `memory`/`tools` below",
        "`variables` pre-sets variables that agent declares and is refused without `agent`",
    ] {
        assert!(
            comments.contains(needle),
            "the graph.example.yaml `chat:` comment does not say {needle:?}"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    let sessions = section(SENTINEL_PAGE, &page, "# Sessions");
    let yaml = first_yaml_block("Graph-Agents.md # Sessions", &sessions);
    assert_block_matches_schema("Graph-Agents.md # Sessions", &yaml);
    assert_inline_chat_keys_complete_the_schema("Graph-Agents.md # Sessions", &yaml);
    for line in [
        "\n    # agent: research-helper\n",
        "\n    # variables: { tone: terse }\n",
    ] {
        assert!(
            yaml.contains(line),
            "the Graph-Agents.md # Sessions yaml does not show {line:?} commented out under `chat:`"
        );
    }
    assert_eq!(
        page.matches("\nsession:").count(),
        3,
        "Graph-Agents.md spells the block three times: in the top-level yaml, under # Sessions and in the delegating example"
    );
    let top = page
        .split("```yaml\nname: my-graph-agent\n")
        .nth(1)
        .and_then(|rest| rest.split("\n```").next())
        .expect("the top-level fields yaml block");
    assert_block_matches_schema("Graph-Agents.md top-level yaml", top);
    assert_inline_chat_keys_complete_the_schema("Graph-Agents.md top-level yaml", top);
    let delegating = first_yaml_block(
        "Graph-Agents.md delegate subsection",
        &section(SENTINEL_PAGE, &page, DELEGATE_HEADING),
    );
    assert_eq!(
        nested_keys(
            "Graph-Agents.md delegate subsection",
            &session_block("Graph-Agents.md delegate subsection", &delegating),
            "chat"
        ),
        DELEGATE_CHAT_KEYS.map(str::to_string).into_iter().collect(),
        "the delegating example's `session.chat` carries exactly `agent` and `variables`"
    );
    assert!(
        flat(&page).contains("- **`session`:**"),
        "Graph-Agents.md lists `session` among the top-level field bullets"
    );
}

#[test]
fn deny_sentence_names_exactly_the_denied_prefixes() {
    let prefixes = denied_prefixes();
    let expected: BTreeSet<String> = prefixes.iter().cloned().collect();
    assert_eq!(expected.len(), prefixes.len());

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
        "the deny sentence differs from CHAT_TOOL_DENY_PREFIXES"
    );
}

/// Nothing is denied in `chat.tools` by NAME any more, so the by-name
/// vocabulary the rule used to carry must be gone from every place a reader
/// learns the rule: the wiki's chat-tools section (incl. the dropped
/// `fetch_url_via_*` carve-out), the example's `tools:` comment, and the two
/// refusal messages themselves. Only the prefix families may be named.
#[test]
fn usage_probe_no_tool_is_named_as_denied_anywhere_the_rule_is_taught() {
    const RETIRED_NAMES: &[&str] = &[
        "fs_write",
        "fs_patch",
        "fs_rm",
        "fs_mkdir",
        "execute_command",
        "git_command",
        "execute_py_code",
        "execute_sql_code",
        "send_mail",
        "send_twilio",
        "fetch_url_via",
    ];
    const RETIRED_PHRASES: &[&str] = &["write/exec/send", "write/spawn", "read-class egress"];
    let prefixes = denied_prefixes();

    let check = |label: &str, text: &str| {
        let flat = flat(text);
        for needle in RETIRED_NAMES.iter().chain(RETIRED_PHRASES) {
            assert!(
                !flat.contains(needle),
                "{label} still names {needle:?} as part of the chat.tools rule: {flat}"
            );
        }
        for prefix in &prefixes {
            assert!(
                flat.contains(prefix.as_str()),
                "{label} must still name the refused family {prefix}: {flat}"
            );
        }
    };

    // The example's comment and the two messages are repo text: ungated.
    check(
        "the graph.example.yaml `tools:` comment",
        &session_tools_comment(&read(repo_root().join("graph.example.yaml"))),
    );
    let validator = src(&["graph", "validator.rs"]);
    let message = string_literals(&fn_body(&validator, "pub fn denied_chat_tool_message("))
        .into_iter()
        .find(|l| l.contains("is not allowed in the chat turn ("))
        .expect("denied_chat_tool_message spells the denied-tool sentence");
    check("the denied-tool message", &message);
    let parser = src(&["graph", "parser.rs"]);
    assert!(
        !flat(&parser).contains("write/spawn"),
        "parser.rs still carries the old write/spawn wording"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    check(
        "the wiki `### Tools in the chat turn` section",
        &section(SENTINEL_PAGE, &page, "### Tools in the chat turn"),
    );
    let sessions = section(SENTINEL_PAGE, &page, "# Sessions");
    check("the wiki validation bullet", &flat(&sessions));
}

#[test]
fn hook_pages_carry_the_graph_turn_variables() {
    let finish = fn_body(&src(&["repl", "mod.rs"]), "fn finish");
    for var in GRAPH_TURN_VARS {
        assert!(
            finish.contains(&format!("\"{var}\"")),
            "TurnBracket::finish in src/repl/mod.rs pushes {var} onto the turn extras"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let hooks = read(wiki.join("Hooks.md"));
    for event in ["turn.completed", "turn.interrupted", "turn.failed"] {
        let row = table_row("Hooks.md", &hooks, &format!("`{event}`"));
        let extras = &row[2];
        for var in GRAPH_TURN_VARS {
            assert!(
                extras.contains(&format!("`{var}`")),
                "the Hooks.md `{event}` row does not list {var}: {extras}"
            );
        }
    }
    let started = table_row("Hooks.md", &hooks, "`turn.started`");
    for var in GRAPH_TURN_VARS {
        assert!(
            !started[2].contains(var),
            "the Hooks.md `turn.started` row must not list {var}"
        );
    }
    let footnote = flat(&hooks);
    for needle in [
        "`turn.started` fires before the branch is decided and never carries them",
        "`classified` (the router decided) or `forced` (`.run <prompt>`)",
        &format!(
            "A `chat` turn under a graph agent engaged with a `chat.agent` adds `{GRAPH_CHAT_AGENT}`, the name of the chat agent engaged for the block; run turns and `turn.started` never carry it"
        ),
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
    let chat_agent = table_row(
        "Environment-Variables.md",
        &env,
        &format!("`{GRAPH_CHAT_AGENT}`"),
    );
    for needle in [
        "The config agent engaged for the block's `chat.agent`",
        "`turn.completed`",
        "`turn.interrupted`",
        "`turn.failed`",
        "branch was `chat`",
    ] {
        assert!(
            chat_agent[1].contains(needle),
            "the {GRAPH_CHAT_AGENT} row does not say {needle:?}: {}",
            chat_agent[1]
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
    let hint = fn_body(&parser, "fn enhance_yaml_error");
    assert!(
        hint.contains("Optional top-level block: `session` (keys: expose, route, chat)."),
        "the unknown-field hint names the session block"
    );
    assert!(
        hint.contains(
            "Optional top-level list: `carry` (state keys remembered into the next run of a session)."
        ),
        "the unknown-field hint names the carry list"
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
        string_literals(&fn_body(&validator, "fn validate_session_chat_agent")),
    ]
    .concat();
    let fragments = [
        "session.chat.instructions must be non-empty",
        "is not declared anywhere (add it to initial_state, or have a node write it via state_updates / output_schema)",
        "is not allowed in the chat turn (the agent__, job__, todo__, user__ and memory__ built-in families are refused)",
        "must name a GLOBAL tool declared for this agent (agent-local tools.* subcommands are not allowed)",
        "references unknown model",
        "tools unavailable on",
        "(no function calling)",
        "; the MEMORY.md section is inlined with the without-tools cap",
        "function calling is disabled (function_calling_support: false), so these tools will not be declared",
        "memory is disabled (memory: false), so no MEMORY.md section and no memory tools",
    ];
    // The delegating messages are quoted twice: in the Validation bullets
    // and in the Sessions subsection that teaches `chat.agent`.
    let delegate_fragments: Vec<String> = [
        "session.chat.agent must be non-empty",
        "session.chat.agent must be a plain agent name (no path separators or reserved names)",
        "session.chat.agent is exclusive with instructions, model, memory and tools",
        "session.chat.agent must not name the graph agent itself",
        "session.chat.variables value for '{key}' must not contain '{INPUT_PLACEHOLDER}'",
        "session.chat.variables requires session.chat.agent",
        "session.chat.agent must name a config agent (config.yaml); '{name}' is a graph agent",
        "session.chat.agent must name a config agent (config.yaml); '{name}' has no config.yaml",
        "session.chat.variables names '{key}', which agent '{name}' does not declare ({declared_text})",
    ]
    .into_iter()
    .map(|fragment| {
        assert!(
            messages.iter().any(|l| l.contains(fragment)),
            "no session message in the code carries {fragment:?}"
        );
        with_placeholders(fragment)
    })
    .collect();
    for fragment in fragments {
        assert!(
            messages.iter().any(|l| l.contains(fragment)),
            "no session message in the code carries {fragment:?}"
        );
    }
    for fragment in &delegate_fragments {
        assert!(
            !fragment.contains(['{', '}']),
            "{fragment:?} still has a format hole; extend with_placeholders"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    let validation = flat(&section(SENTINEL_PAGE, &page, "# Validation"));
    let delegate = flat(&section(SENTINEL_PAGE, &page, DELEGATE_HEADING));
    for fragment in fragments {
        assert!(
            validation.contains(fragment),
            "the Validation section does not quote {fragment:?}"
        );
    }
    for fragment in &delegate_fragments {
        assert!(
            validation.contains(fragment),
            "the Validation section does not quote {fragment:?}"
        );
        assert!(
            delegate.contains(fragment),
            "the delegate subsection does not quote {fragment:?}"
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

/// The delegate subsection quotes the engage failure, the two resume
/// warnings, the stale-block refusal and the required-variables lead the way
/// the code prints them, and states the exclusivity tolerance and the
/// chat-agent hook variable in the words the other pages use. Its prose
/// claims about what the chat agent keeps (tools, model, variables, the
/// session it is born into) are pinned in their own words.
#[test]
fn delegate_subsection_quotes_the_engage_and_resume_texts() {
    let request_context = src(&["config", "request_context.rs"]);
    let warnings: Vec<String> =
        string_literals(&fn_body(&request_context, "fn chat_agent_mismatch_warning"))
            .into_iter()
            .filter(|l| l.starts_with("Warning: session '{session_name}' was born with "))
            .collect();
    assert_eq!(
        warnings.len(),
        2,
        "chat_agent_mismatch_warning has an inline form and a chat-agent form: {warnings:?}"
    );
    let failure = string_literals(&fn_body(&request_context, "fn chat_agent_engage_failure"));
    assert!(
        failure.len() == 1
            && failure[0].contains("'{delegate_name}'")
            && failure[0].contains("'{graph_agent}'"),
        "chat_agent_engage_failure is one sentence naming both agents: {failure:?}"
    );
    let dispatch = src(&["graph", "dispatch.rs"]);
    let delegated = string_literals(&fn_body(&dispatch, "fn delegated_chat_input"));
    let hint = |head: &str| {
        delegated
            .iter()
            .find(|l| l.starts_with(head))
            .map(|l| with_placeholders(l))
            .unwrap_or_else(|| panic!("delegated_chat_input spells a hint opening {head:?}"))
    };
    let stale = hint("session.chat.agent changed since the agent was engaged")
        .replacen("{}", &hint(".exit agent"), 1)
        .replacen("{}", &hint(".agent {agent_name} {session}"), 1);
    let agent = src(&["config", "agent.rs"]);
    let required = string_literals(&fn_body(&agent, "pub fn init_agent_variables"))
        .into_iter()
        .find(|l| l.starts_with("The following agent variables are required:"))
        .expect("init_agent_variables names the variables it could not seat")
        .lines()
        .next()
        .unwrap()
        .to_string();

    let mut texts: Vec<String> = warnings
        .iter()
        .chain(&failure)
        .map(|l| with_placeholders(l))
        .collect();
    texts.extend([
        stale,
        required,
        "`memory: false` and `tools: []` are the defaults and may stay beside `agent:`".to_string(),
        format!("`{GRAPH_CHAT_AGENT}=<chat-agent>`"),
        "`turn.started` and run turns never carry it".to_string(),
        "none of the `chat.tools` denials apply".to_string(),
        "MCP servers, skills, memory setting".to_string(),
        "The `run` branch and the router keep the graph agent's and the session's model"
            .to_string(),
        "`chat.variables` is the only way to pre-set the chat agent's variables".to_string(),
        "reach the graph agent's own variables only".to_string(),
        "The chat agent is built once, when the graph agent is engaged".to_string(),
        "exactly as `.agent <chat-agent>` would build it: its required variables without a default are prompted for on a terminal"
            .to_string(),
        "(`--info` runs the same pass)".to_string(),
        "the session name is quoted when it has spaces; the command sigil follows `command_prefix`"
            .to_string(),
        "The session is born from the chat agent: the recorded system prompt is the chat agent's rendered instructions"
            .to_string(),
        "its compression settings govern the session".to_string(),
        "its model and tools apply per request only and are never written into the session"
            .to_string(),
        "`COYOTE_AGENT_NAME=<chat-agent>`".to_string(),
    ]);
    for text in &texts {
        assert!(
            !text.contains(['{', '}']),
            "{text:?} still has a format hole; extend with_placeholders"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    let delegate = flat(&section(SENTINEL_PAGE, &page, DELEGATE_HEADING));
    for text in &texts {
        assert!(
            delegate.contains(text),
            "the delegate subsection does not quote {text:?}"
        );
    }
    // The chat agent keeps its whole tool pool, so the subsection must not
    // carry any refused family over from `### Tools in the chat turn`.
    for prefix in denied_prefixes() {
        assert!(
            !delegate.contains(&prefix),
            "the delegate subsection names the refused family {prefix}, which does not apply to a chat agent"
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
/// The lead of the Sessions.md paragraph about what a persisted session
/// carries back, which names the chat agent's recorded fields.
const PERSISTED_LEAD: &str = "Sessions you persist and then load again later";

/// The `chat_agent*` fields of `Session`, in declaration order. The struct's
/// fields are private (no `pub `), so `struct_fields` cannot read them; a
/// `#[serde(rename)]` on any of them would make the wiki's spelling drift, so
/// the attribute lines above each field are checked too.
fn session_chat_agent_fields() -> Vec<String> {
    let source = src(&["config", "session.rs"]);
    let head = "pub struct Session {";
    let start = source
        .find(head)
        .expect("src/config/session.rs declares Session");
    let body = &source[start + head.len()..];
    let end = body.find("\n}").expect("Session ends with a `}` line");
    let lines: Vec<&str> = body[..end].lines().map(str::trim).collect();
    let mut fields = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(name) = line
            .strip_suffix(',')
            .and_then(|l| l.split_once(':'))
            .map(|(name, _)| name.trim())
            .filter(|name| name.starts_with("chat_agent"))
        else {
            continue;
        };
        let attributes: Vec<&&str> = lines[..i]
            .iter()
            .rev()
            .take_while(|l| l.starts_with("#[") || l.starts_with("///"))
            .collect();
        assert!(
            !attributes.iter().any(|a| a.contains("rename")),
            "Session.{name} is renamed for serde; this lint reads Rust names as yaml keys"
        );
        fields.push(name.to_string());
    }
    fields
}

/// A GitHub-wiki anchor for `heading`: lowercase, spaces to hyphens, the
/// `#` marks and punctuation dropped.
fn anchor_of(heading: &str) -> String {
    heading
        .trim_start_matches('#')
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// Sessions.md says which fields a session engaged under a delegating block
/// carries back, spelled as the `Session` struct serialises them, and points
/// at the Graph-Agents.md subsection that teaches the block; every in-page
/// link to that subsection resolves to its heading.
#[test]
fn sessions_page_names_the_persisted_chat_agent_fields() {
    let fields = session_chat_agent_fields();
    assert_eq!(
        fields,
        [
            "chat_agent",
            "chat_agent_variables",
            "chat_agent_instructions"
        ],
        "Session persists exactly the chat agent's name, variables and rendered prompt"
    );
    let anchor = anchor_of(DELEGATE_HEADING);
    assert_eq!(anchor, "handing-chat-turns-to-a-config-agent");

    let Some(wiki) = wiki_dir() else { return };
    let sessions = read(wiki.join("Sessions.md"));
    let persisted = flat(&paragraph("Sessions.md", &sessions, PERSISTED_LEAD));
    for field in &fields {
        assert!(
            persisted.contains(&format!("`{field}`")),
            "the Sessions.md persisted-configuration paragraph does not name `{field}`: {persisted}"
        );
    }
    for needle in [
        "`chat.agent` names a config agent",
        "in the session yaml",
        &format!("](Graph-Agents#{anchor})"),
    ] {
        assert!(
            persisted.contains(needle),
            "the Sessions.md persisted-configuration paragraph does not say {needle:?}: {persisted}"
        );
    }
    let graph = read(wiki.join(SENTINEL_PAGE));
    assert!(
        graph.contains(&format!("\n{DELEGATE_HEADING}\n")),
        "Graph-Agents.md has no `{DELEGATE_HEADING}` heading for the anchor to resolve to"
    );
    let in_page = graph.matches(&format!("](#{anchor})")).count();
    assert!(
        in_page >= 2,
        "Graph-Agents.md links the delegate subsection from the Validation bullets and the Sessions prose; found {in_page} link(s)"
    );
}

/// The undeclared-variable error's "no variables" form is quoted where the
/// rule is taught, with the parentheses the code wraps it in.
#[test]
fn delegate_texts_quote_the_declares_no_variables_form() {
    let validator = src(&["graph", "validator.rs"]);
    let pass = string_literals(&fn_body(&validator, "fn validate_session_chat_agent"));
    let none = pass
        .iter()
        .find(|l| l.as_str() == "declares no variables")
        .expect("validate_session_chat_agent spells the no-variables form as one literal");
    let quoted = format!("({none})");

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    for heading in ["# Validation", DELEGATE_HEADING] {
        let text = flat(&section(SENTINEL_PAGE, &page, heading));
        assert!(
            text.contains(&format!("`{quoted}`")),
            "the {heading} section does not quote `{quoted}`"
        );
    }
}

/// `chat.agent` dropped from the block after engage: the next chat turn is
/// inline, answered by the graph agent (session model, its own tool pool,
/// the block's inline keys) while the chat agent stays parked (its memory
/// preference still governs and `COYOTE_GRAPH_CHAT_AGENT` still names it),
/// as `usage_probe_delegated_block_dropping_its_chat_agent_runs_inline_with_the_delegate_parked`
/// in `src/repl/mod.rs` pins. The subsection's dropped-key sentence has to
/// say who answers: the graph agent, not the parked chat agent; and every
/// claim it makes about the parked chat agent has to be one that test
/// asserts (session model on the request, the chat agent parked, its memory
/// setting still applied, `COYOTE_GRAPH_CHAT_AGENT` still naming it).
#[test]
fn usage_probe_delegate_subsection_says_the_graph_agent_answers_once_the_key_is_dropped() {
    let repl = src(&["repl", "mod.rs"]);
    let pin = fn_body(
        &repl,
        "fn usage_probe_delegated_block_dropping_its_chat_agent_runs_inline_with_the_delegate_parked",
    );
    // The code pin asserts each half the sentence is allowed to claim.
    for needle in [
        "format!(\"openai:{SESSION_MODEL}\")",
        ".get(\"COYOTE_GRAPH_CHAT_AGENT\")",
        "ctx.memory_config().enabled",
        "parked.is_none()",
    ] {
        assert!(
            pin.contains(needle),
            "the dropped-key code pin no longer asserts {needle:?}"
        );
    }

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(SENTINEL_PAGE));
    let delegate = flat(&section(SENTINEL_PAGE, &page, DELEGATE_HEADING));
    let lead = "Dropping the key";
    let start = delegate.find(lead).unwrap_or_else(|| {
        panic!("the delegate subsection does not teach what dropping `chat.agent` does")
    });
    let rest = &delegate[start..];
    let end = rest.find(". ").map(|i| i + 1).unwrap_or(rest.len());
    let sentence = &rest[..end];
    for wrong in [
        "chat agent answering",
        "chat agent answers",
        "chat agent keeps answering",
    ] {
        assert!(
            !sentence.contains(wrong),
            "the dropped-key sentence promises the parked chat agent answers the turn, \
             but the graph agent answers inline once the key is gone: {sentence:?}"
        );
    }
    assert!(
        sentence.contains("graph agent"),
        "the dropped-key sentence does not name the graph agent as the one answering: {sentence:?}"
    );
    // Both halves of the pinned behaviour: who answers with what, and what
    // the parked chat agent still does.
    for needle in [
        "session model",
        "parked",
        "memory setting",
        &format!("`{GRAPH_CHAT_AGENT}`"),
    ] {
        assert!(
            sentence.contains(needle),
            "the dropped-key sentence does not say {needle:?}: {sentence:?}"
        );
    }
    let claim_start = sentence.find(&format!("`{GRAPH_CHAT_AGENT}`")).unwrap();
    assert!(
        sentence[claim_start..].contains("names it"),
        "the sentence must say the env var still names the parked chat agent: {sentence:?}"
    );
    assert!(
        delegate.contains("re-engaged") || delegate.contains("re-engage"),
        "the subsection does not say the parked chat agent lasts until the agent is re-engaged"
    );
}

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
            "Sessions.md persisted configuration",
            paragraph("Sessions.md", &sessions, PERSISTED_LEAD),
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

/// The three Hooks.md turn rows that gained the graph variables and the
/// three Environment-Variables.md graph rows, one string each.
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
    rows.extend(GRAPH_TURN_VARS.iter().map(|var| {
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
