//! Opt-in doc lint: checks the `TUI.md` wiki page, the prompt protocol and
//! prompt helper sections of `Custom-Tools.md` and `Bash-Prompt-Helpers.md`,
//! the UI rows of `Environment-Variables.md`, `Getting-Started.md` and
//! `Hooks.md`, and the headless prompt section of `Unattended-Mode.md`
//! against the code they describe, plus the matching `README.md` rows.
//!
//! The wiki is a separate clone, so the wiki half of each test runs only when
//! `COYOTE_WIKI_DIR` points at it; a relative value is resolved against the
//! crate root:
//!
//! ```sh
//! COYOTE_WIKI_DIR=../coyote.wiki cargo test --test tui_wiki_docs
//! ```
//!
//! Without the variable the wiki half of every test is skipped (printed as
//! `skipping:`) and the repo-only assertions still run, so a plain `cargo test`
//! is unaffected. With it set to a directory that has no `TUI.md` the tests
//! fail rather than pass against nothing.
//!
//! There is no lib target, so the code side of each check is read from the
//! repo's text. The literal scraper reads plain `"..."` strings only.
//!
//! The `# Keys` table of `TUI.md` is checked against the key map itself by
//! a unit test of the binary, which reads the same variable:
//!
//! ```sh
//! COYOTE_WIKI_DIR=../coyote.wiki cargo test --bin coyote keys::
//! ```

mod common;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use common::source_scan::{const_str, fn_body, read, repo_root, src, string_literals};

const WIKI_DIR: &str = "COYOTE_WIKI_DIR";
const RECIPE: &str = "`COYOTE_WIKI_DIR=../coyote.wiki cargo test --test tui_wiki_docs`";
const PAGE: &str = "TUI.md";
const CHARTS_HEADING: &str = "## Charts";
const PROTOCOL_HEADING: &str = "## Prompting the user from a tool";
const CHANNEL_HEADING: &str = "# The Prompt Channel";
const ENV_HEADING: &str = "# UI and Prompt Related Variables";
const HEADLESS_HEADING: &str = "## Prompts without a human";
const IN_PROCESS_LEAD: &str = "- **Also published in-process.**";
/// The settings whose README and Getting-Started rows this lint checks.
const SETTINGS: [&str; 3] = ["ui", "editor", "theme"];
const WIKI_URL: &str = "https://github.com/Dark-Alex-17/coyote/wiki/";
/// Words that narrate a plan or a release rather than describe the product.
const PLANNING_WORDS: [&str; 8] = [
    "phase", "phases", "round", "rounds", "wave", "waves", "ruling", "rulings",
];
const CHANGELOG_WORDS: [&str; 2] = ["new", "now"];

fn asset(parts: &[&str]) -> String {
    let mut path = repo_root().join("assets");
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
        dir.join(PAGE).is_file(),
        "{WIKI_DIR}={} has no {PAGE}; point it at a coyote.wiki checkout",
        dir.display()
    );
    Some(dir)
}

/// The part of a source file before its unit test module.
fn production(source: &str) -> &str {
    source
        .find("\n#[cfg(test)]\nmod tests")
        .map_or(source, |at| &source[..at])
}

/// The right-hand side of `const {name}: <type> = <value>;` in `source`.
fn const_expr(source: &str, name: &str) -> String {
    let head = format!("const {name}: ");
    let start = source
        .find(&head)
        .unwrap_or_else(|| panic!("the source declares {name}"));
    let rest = &source[start..];
    let value = &rest[rest.find(" = ").expect("a const has a value") + 3..];
    value[..value.find(';').expect("a const ends with `;`")]
        .trim()
        .to_string()
}

/// The one string literal in `source` that contains `needle`. Only the code
/// lines that hold `needle` are scanned, so a raw string or a `'"'` char
/// elsewhere in the file cannot desync the scan.
fn literal_with(source: &str, needle: &str) -> String {
    let found: Vec<String> = source
        .lines()
        .filter(|line| line.contains(needle) && !line.trim_start().starts_with("//"))
        .flat_map(string_literals)
        .filter(|literal| literal.contains(needle))
        .collect();
    assert_eq!(found.len(), 1, "one literal holds {needle:?}: {found:?}");
    found.into_iter().next().unwrap()
}

/// The text under `heading`, a heading line of `page`, up to the next
/// heading of the same or a higher level. Lines inside fenced code blocks are
/// never taken for headings.
fn section(label: &str, page: &str, heading: &str) -> String {
    let level = heading.bytes().take_while(|b| *b == b'#').count();
    let closes = |line: &str| {
        let depth = line.bytes().take_while(|b| *b == b'#').count();
        (1..=level).contains(&depth) && line[depth..].starts_with(' ')
    };
    let mut fenced = false;
    let mut body: Option<Vec<&str>> = None;
    for line in page.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if !fenced && body.is_none() && line == heading {
            body = Some(Vec::new());
            continue;
        } else if !fenced && body.is_some() && closes(line) {
            break;
        }
        if let Some(lines) = body.as_mut() {
            lines.push(line);
        }
    }
    let lines = body.unwrap_or_else(|| panic!("{label} has no `{heading}` heading"));
    lines.join("\n")
}

/// The paragraph or list item of `page` that starts with `lead`, up to the
/// next blank line or list item.
fn paragraph(label: &str, page: &str, lead: &str) -> String {
    let at = page
        .find(lead)
        .unwrap_or_else(|| panic!("{label} has no paragraph starting {lead:?}"));
    let rest = &page[at..];
    let end = rest[lead.len()..]
        .find("\n\n")
        .into_iter()
        .chain(rest[lead.len()..].find("\n- "))
        .min()
        .map_or(rest.len(), |end| end + lead.len());
    rest[..end].to_string()
}

/// `text` with every whitespace run collapsed to one space, so a sentence the
/// page wraps is matched as prose.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// The body rows of every table in `text`: header and separator rows left
/// out.
fn table_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut in_table = false;
    let mut header_seen = false;
    for line in text.lines() {
        if !line.starts_with('|') {
            in_table = false;
            continue;
        }
        if !in_table {
            in_table = true;
            header_seen = false;
        }
        if !header_seen {
            header_seen = true;
            continue;
        }
        if line.starts_with("|-") || line.starts_with("| -") {
            continue;
        }
        rows.push(table_cells(line));
    }
    rows
}

/// The table row of `text` whose first cell is `first`.
fn table_row(label: &str, text: &str, first: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.starts_with('|'))
        .map(table_cells)
        .find(|cells| cells.first().is_some_and(|cell| cell == first))
        .unwrap_or_else(|| panic!("{label} has no table row whose first cell is {first}"))
}

/// Every `` `...` `` span of `text`, in order.
fn code_spans(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The body of every fenced block of `text` whose fence names `lang`.
fn fenced_blocks(text: &str, lang: &str) -> Vec<String> {
    let open = format!("```{lang}");
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        match current.as_mut() {
            None if trimmed == open => current = Some(Vec::new()),
            None => {}
            Some(_) if trimmed.starts_with("```") => {
                blocks.push(current.take().unwrap().join("\n"));
            }
            Some(lines) => lines.push(trimmed),
        }
    }
    blocks
}

/// The lowercase words of `text`, punctuation trimmed.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// The `--long` flag clap derives for the bool field `field` of the CLI:
/// its `#[arg(long, ...)]` line names no explicit long, so the field name.
fn clap_long(field: &str) -> String {
    let cli = src(&["cli", "mod.rs"]);
    let decl = format!("\n    pub {field}: bool,");
    let at = cli
        .find(&decl)
        .unwrap_or_else(|| panic!("the CLI declares {field}"));
    let attr = cli[..at].rsplit('\n').next().unwrap().trim();
    assert!(
        attr.starts_with("#[arg(long,") && !attr.contains("long ="),
        "{field} is a plain long flag: {attr}"
    );
    format!("--{}", field.replace('_', "-"))
}

/// The value the config template gives `key`, without its comment.
fn template_value(key: &str) -> String {
    yaml_value(
        "config-template.yaml",
        &asset(&["config-template.yaml"]),
        key,
    )
}

/// The value a top-level `key:` line of `yaml` sets, without its comment.
fn yaml_value(label: &str, yaml: &str, key: &str) -> String {
    let head = format!("{key}: ");
    let line = yaml
        .lines()
        .find(|line| line.starts_with(&head))
        .unwrap_or_else(|| panic!("{label} sets {key}"));
    line[head.len()..]
        .split(" #")
        .next()
        .unwrap()
        .trim()
        .to_string()
}

/// `UiMode::NAMES`, the values the `ui` setting takes.
fn ui_mode_names() -> Vec<String> {
    let source = src(&["config", "ui_mode.rs"]);
    let line = source
        .lines()
        .find(|line| line.contains("const NAMES:"))
        .expect("UiMode declares NAMES");
    string_literals(&line[line.find('=').unwrap()..])
}

/// What `UiKind::as_str` exports as `COYOTE_UI`.
fn ui_kind_names() -> Vec<String> {
    string_literals(&fn_body(
        &src(&["config", "ui_kind.rs"]),
        "pub(crate) fn as_str(",
    ))
}

/// `PromptAction::name()`: the `action` values the protocol accepts.
fn prompt_action_names() -> Vec<String> {
    string_literals(&fn_body(
        &src(&["prompt", "mod.rs"]),
        "pub(crate) fn name(self)",
    ))
}

/// The fields of `struct WireRequest`, each with whether a request must
/// carry it: an `Option` field or one marked `#[serde(default)]` may be left
/// out.
fn wire_request_fields() -> Vec<(String, bool)> {
    let source = src(&["function", "prompt_channel", "mod.rs"]);
    let body = fn_body(&source, "struct WireRequest {");
    let mut fields = Vec::new();
    let mut defaulted = false;
    for line in body.lines().skip(1).map(str::trim) {
        if line == "#[serde(default)]" {
            defaulted = true;
            continue;
        }
        let Some((name, ty)) = line.split_once(": ") else {
            continue;
        };
        let required = !defaulted && !ty.starts_with("Option<");
        fields.push((name.to_string(), required));
        defaulted = false;
    }
    fields
}

/// The bytes a request line starts with: `MARKER` in prompt_channel/mod.rs.
fn request_marker() -> String {
    let source = src(&["function", "prompt_channel", "mod.rs"]);
    let head = "const MARKER: &[u8] = b";
    let at = source
        .find(head)
        .expect("prompt_channel/mod.rs declares MARKER");
    let rest = &source[at + head.len()..];
    let mut literals = string_literals(&rest[..rest.find(';').unwrap()]);
    assert_eq!(literals.len(), 1, "MARKER is one byte string");
    literals.remove(0)
}

/// The keys of the reply line `Server::reply` writes, checked on both the
/// unix and the windows half.
fn reply_keys() -> Vec<String> {
    let source = src(&["function", "prompt_channel", "server.rs"]);
    let shape = "json!({\"id\": id, \"answer\": answer})";
    let head = "pub(crate) fn reply(&self, id: &str, answer: &Value)";
    let halves: Vec<String> = source
        .match_indices(head)
        .map(|(at, _)| {
            let line_start = source[..at].rfind('\n').map_or(0, |nl| nl + 1);
            fn_body(&source[line_start..], head)
        })
        .collect();
    assert_eq!(halves.len(), 2, "a unix and a windows Server::reply");
    for half in halves {
        assert!(half.contains(shape), "Server::reply writes {shape}: {half}");
    }
    vec!["id".to_string(), "answer".to_string()]
}

/// The dotted names of the `tool.*` hook events with their `HookEvent`
/// variants, from `HookEvent::as_str`.
fn tool_events() -> Vec<(String, String)> {
    let body = fn_body(&src(&["hooks.rs"]), "pub fn as_str(self)");
    body.lines()
        .filter_map(|line| {
            let (variant, name) = line.trim().split_once(" => ")?;
            let name = string_literals(name).into_iter().next()?;
            name.starts_with("tool.")
                .then(|| (variant.trim_start_matches("HookEvent::").to_string(), name))
        })
        .collect()
}

#[test]
fn config_rows_match_the_template() {
    let names = ui_mode_names();
    assert_eq!(names, ["repl", "tui"]);
    let flags = [clap_long("tui"), clap_long("repl")];
    let readme = read(repo_root().join("README.md"));
    let example = read(repo_root().join("config.example.yaml"));
    assert_eq!(
        yaml_value("config.example.yaml", &example, "theme"),
        template_value("theme"),
        "config.example.yaml `theme` default"
    );
    for (label, text) in [("config.example.yaml", &example), ("README.md", &readme)] {
        assert!(!names_light_theme(text), "{label} names light_theme");
    }
    let mut tables = vec![("README.md".to_string(), readme)];

    let wiki = wiki_dir();
    if let Some(wiki) = &wiki {
        tables.push((
            "Getting-Started.md".to_string(),
            read(wiki.join("Getting-Started.md")),
        ));
    }
    for (label, text) in &tables {
        for key in SETTINGS {
            let row = table_row(label, text, &format!("`{key}`"));
            assert_eq!(
                row[1],
                format!("`{}`", template_value(key)),
                "{label} `{key}` default"
            );
        }
        let ui = table_row(label, text, "`ui`").join(" ");
        for needle in names
            .iter()
            .map(|name| format!("`{name}`"))
            .chain([format!("`{}`", flags[0]), format!("`{}`", flags[1])])
        {
            assert!(ui.contains(&needle), "{label} `ui` row names {needle}");
        }
        assert!(
            !ui.contains("COYOTE_UI"),
            "{label} `ui` row names COYOTE_UI, which coyote does not read"
        );
        let theme = table_row(label, text, "`theme`").join(" ");
        for needle in [
            "`light`",
            "`dark`",
            "`COYOTE_THEME`",
            "while `theme` is null",
        ] {
            assert!(theme.contains(needle), "{label} `theme` row says {needle}");
        }
        let editor = table_row(label, text, "`editor`").join(" ");
        let visual = editor.find("`$VISUAL`");
        let fallback = editor.find("`$EDITOR`");
        assert!(
            visual.is_some() && fallback.is_some() && visual < fallback,
            "{label} `editor` row names `$VISUAL` before `$EDITOR`: {editor}"
        );
    }
    if let [(_, readme), (_, started)] = &tables[..] {
        for key in SETTINGS {
            let first = format!("`{key}`");
            assert_eq!(
                table_row("README.md", readme, &first)[2].replace(WIKI_URL, ""),
                table_row("Getting-Started.md", started, &first)[2],
                "README.md and Getting-Started.md describe `{key}` alike"
            );
        }
    }

    let Some(wiki) = wiki else { return };
    let tui = read(wiki.join(PAGE));
    let row = table_row(PAGE, &tui, "`ui`");
    assert_eq!(row[1], format!("`{}`", template_value("ui")), "{PAGE} `ui`");
    for page in wiki_pages(&wiki) {
        let text = read(&page);
        assert!(
            !names_light_theme(&text),
            "{} names light_theme",
            page.display()
        );
    }
}

/// Whether `text` names the retired `light_theme` setting in any case, its
/// `COYOTE_LIGHT_THEME` override included.
fn names_light_theme(text: &str) -> bool {
    text.to_ascii_lowercase().contains("light_theme")
}

/// Every `*.md` under `wiki`, sorted by path.
fn wiki_pages(wiki: &Path) -> Vec<PathBuf> {
    let mut pages: Vec<PathBuf> = fs::read_dir(wiki)
        .unwrap_or_else(|e| panic!("read {}: {e}", wiki.display()))
        .map(|entry| entry.expect("a readable wiki entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    pages.sort();
    pages
}

#[test]
fn protocol_examples_parse_against_the_wire_types() {
    let fields = wire_request_fields();
    let names: Vec<&str> = fields.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["id", "action", "question", "options", "default"]);
    let actions = prompt_action_names();
    assert_eq!(actions.len(), 5, "{actions:?}");
    let marker = request_marker();
    assert_eq!(marker, "{\"coyote_prompt\"");
    let wrapper = marker.trim_start_matches("{\"").trim_end_matches('"');
    let replies = reply_keys();

    let Some(wiki) = wiki_dir() else { return };
    let custom = read(wiki.join("Custom-Tools.md"));
    let protocol = section("Custom-Tools.md", &custom, PROTOCOL_HEADING);
    let mut requests = 0;
    let mut answers = 0;
    for (label, text) in [
        ("Custom-Tools.md", protocol.clone()),
        (
            "Bash-Prompt-Helpers.md",
            read(wiki.join("Bash-Prompt-Helpers.md")),
        ),
    ] {
        for block in fenced_blocks(&text, "json") {
            let value: serde_json::Value = serde_json::from_str(&block)
                .unwrap_or_else(|e| panic!("{label} example does not parse: {e}\n{block}"));
            let object = value.as_object().expect("an example is an object");
            if block.contains(wrapper) {
                requests += 1;
                assert!(block.starts_with(&marker), "{label}: {block}");
                assert_eq!(object.len(), 1, "{label}: one key, {wrapper}");
                let request = object[wrapper]
                    .as_object()
                    .expect("the request is an object");
                for key in request.keys() {
                    assert!(
                        names.contains(&key.as_str()),
                        "{label}: unknown field {key}"
                    );
                }
                for (name, required) in &fields {
                    assert!(
                        !required || request.contains_key(name),
                        "{label}: the example lacks {name}"
                    );
                }
                let action = request["action"].as_str().expect("action is a string");
                assert!(actions.iter().any(|a| a == action), "{label}: {action}");
            } else {
                answers += 1;
                let keys: Vec<&String> = object.keys().collect();
                assert_eq!(keys, replies.iter().collect::<Vec<_>>(), "{label} reply");
            }
        }
    }
    assert!(requests > 0 && answers > 0, "a request and a reply example");

    let documented: Vec<(String, bool)> = table_rows(&protocol)
        .into_iter()
        .filter(|row| row.len() == 3)
        .map(|row| (row[0].trim_matches('`').to_string(), row[1] == "yes"))
        .collect();
    assert_eq!(documented, fields, "Custom-Tools.md field table");
    let action_row = table_row("Custom-Tools.md", &protocol, "`action`").join(" ");
    let answer_rows = table_rows(&protocol)
        .into_iter()
        .filter(|row| row.len() == 2)
        .map(|row| row[0].clone())
        .collect::<Vec<_>>()
        .join(" ");
    for action in &actions {
        assert!(action_row.contains(&format!("`{action}`")), "{action}");
        assert!(answer_rows.contains(&format!("`{action}`")), "{action}");
    }
}

#[test]
fn prompt_helpers_are_described_as_they_ship() {
    let sh = asset(&["functions", "utils", "prompt-utils.sh"]);
    let py = asset(&["functions", "utils", "prompt-utils.py"]);
    let ts = asset(&["functions", "utils", "prompt-utils.ts"]);
    let refusal = string_literals(
        sh.lines()
            .find(|line| line.starts_with("_COYOTE_TUI_NO_CHANNEL="))
            .expect("the bash helper names its refusal"),
    )
    .remove(0);
    assert_eq!(
        refusal,
        string_literals(
            py.lines()
                .find(|line| line.starts_with("TUI_NO_CHANNEL = "))
                .unwrap()
        )[0]
    );
    let prompt = fn_body(&sh, "_coyote_prompt() {");
    assert!(
        prompt.contains("    return 2\n"),
        "list exits 2 for typed text"
    );
    assert!(prompt.contains("[[ -n \"${answer//[[:space:]]/}\" ]] || return 1"));
    assert!(sh.contains("exec 9>&1"), "the helper keeps stdout on fd 9");
    assert!(sh.contains("if [[ \"${COYOTE_UI:-}\" == \"tui\" ]]; then"));
    let py_helpers: Vec<String> = py
        .lines()
        .filter_map(|line| line.strip_prefix("def "))
        .filter(|rest| !rest.starts_with('_'))
        .map(|rest| rest[..rest.find('(').unwrap()].to_string())
        .collect();
    let ts_helpers: Vec<String> = ts
        .lines()
        .filter_map(|line| line.strip_prefix("export function "))
        .map(|rest| rest[..rest.find('(').unwrap()].to_string())
        .collect();
    assert_eq!(py_helpers.len(), 7, "{py_helpers:?}");
    assert_eq!(ts_helpers.len(), 7, "{ts_helpers:?}");
    for (shim, var, present) in [
        ("run-tool.py", "LLM_PROMPT_UTILS_PY", true),
        ("run-tool.ts", "LLM_PROMPT_UTILS_TS", true),
        ("run-agent.py", "LLM_PROMPT_UTILS_PY", false),
        ("run-agent.ts", "LLM_PROMPT_UTILS_TS", false),
    ] {
        let text = asset(&["functions", "scripts", shim]);
        assert_eq!(text.contains(var), present, "{shim} and {var}");
    }

    let Some(wiki) = wiki_dir() else { return };
    let bash = flat(&section(
        "Bash-Prompt-Helpers.md",
        &read(wiki.join("Bash-Prompt-Helpers.md")),
        CHANNEL_HEADING,
    ));
    for needle in [
        refusal.as_str(),
        "exit `2`",
        "File descriptor 9",
        "`Other (custom)`",
        "`COYOTE_PROMPT_REPLY`",
        "`COYOTE_UI`",
    ] {
        assert!(
            bash.contains(needle),
            "Bash-Prompt-Helpers.md says {needle}"
        );
    }
    let list = table_rows(&section(
        "Bash-Prompt-Helpers.md",
        &read(wiki.join("Bash-Prompt-Helpers.md")),
        CHANNEL_HEADING,
    ))
    .into_iter()
    .filter(|row| row[0] == "`list`")
    .map(|row| row[2].clone())
    .collect::<Vec<_>>();
    assert_eq!(list, ["`0`", "`2`"], "the list rows' exit statuses");

    let protocol = section(
        "Custom-Tools.md",
        &read(wiki.join("Custom-Tools.md")),
        PROTOCOL_HEADING,
    );
    let spans = code_spans(&protocol);
    for helper in py_helpers.iter().chain(&ts_helpers) {
        assert!(spans.contains(helper), "Custom-Tools.md names {helper}");
    }
    for var in ["LLM_PROMPT_UTILS_PY", "LLM_PROMPT_UTILS_TS"] {
        assert!(protocol.contains(var), "Custom-Tools.md names {var}");
    }
}

#[test]
fn hooks_page_lists_the_call_id_on_every_tool_event() {
    let events = tool_events();
    assert_eq!(events.len(), 3, "{events:?}");
    let typed_tool_events = ["Event::ToolStarted {", "Event::ToolFinished {"];
    let function = src(&["function", "mod.rs"]);
    let function = production(&function);
    for typed in typed_tool_events {
        assert!(function.contains(typed), "function/mod.rs emits {typed}");
    }
    let hooks = src(&["hooks.rs"]);
    let spec = fn_body(&hooks, "fn hook_spec(");
    for (variant, name) in &events {
        assert!(
            spec.contains(&format!("HookEvent::{variant}")),
            "hook_spec maps a typed event to {name}"
        );
    }
    for typed in typed_tool_events {
        assert!(spec.contains(typed), "hook_spec maps {typed}");
    }
    assert_eq!(
        spec.matches("tool_extras(").count(),
        typed_tool_events.len(),
        "every tool arm of hook_spec takes its env from tool_extras"
    );
    let tool_extras = fn_body(&hooks, "fn tool_extras(");
    assert!(
        tool_extras.contains("(\"COYOTE_TOOL_CALL_ID\","),
        "{tool_extras}"
    );
    let dispatch = fn_body(&hooks, "fn dispatch(");
    assert!(
        dispatch.find("publish_hook_event(").unwrap() < dispatch.find("fire_resolved(").unwrap(),
        "a typed event is published before its shell hooks run"
    );
    let fire = fn_body(&hooks, "pub fn fire(");
    assert!(
        fire.find("publish_hook_event(").unwrap() < fire.find("if resolved.is_empty()").unwrap(),
        "fire publishes before it looks for shell hooks"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Hooks.md"));
    for (_, name) in &events {
        let row = table_row("Hooks.md", &page, &format!("`{name}`"));
        assert!(
            row.last().unwrap().contains("`COYOTE_TOOL_CALL_ID`"),
            "the Hooks.md `{name}` row lists COYOTE_TOOL_CALL_ID: {row:?}"
        );
    }
    let in_process = flat(&paragraph("Hooks.md", &page, IN_PROCESS_LEAD));
    assert!(in_process.contains("`COYOTE_HOOK_NAME`"), "{in_process}");
    let env = read(wiki.join("Environment-Variables.md"));
    let row = table_row("Environment-Variables.md", &env, "`COYOTE_TOOL_CALL_ID`");
    assert!(row[1].contains("`tool.*`"), "{row:?}");
}

#[test]
fn environment_page_documents_the_ui_and_reply_variables() {
    let ui_env = const_str(&src(&["config", "ui_kind.rs"]), "ENV");
    let reply_env = const_str(&src(&["function", "prompt_channel", "mod.rs"]), "REPLY_ENV");
    assert_eq!(
        (ui_env.as_str(), reply_env.as_str()),
        ("COYOTE_UI", "COYOTE_PROMPT_REPLY")
    );
    let kinds = ui_kind_names();
    assert_eq!(kinds, ["repl", "tui", "headless"]);

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join("Environment-Variables.md"));
    let vars = section("Environment-Variables.md", &page, ENV_HEADING);
    let ui = table_row("Environment-Variables.md", &vars, &format!("`{ui_env}`")).join(" ");
    for kind in &kinds {
        assert!(
            ui.contains(&format!("`{kind}`")),
            "the {ui_env} row names {kind}"
        );
    }
    let reply = table_row("Environment-Variables.md", &vars, &format!("`{reply_env}`")).join(" ");
    assert!(reply.contains("`/dev/fd/<n>`") && reply.contains("named pipe"));
    for var in ["`LLM_PROMPT_UTILS_PY`", "`LLM_PROMPT_UTILS_TS`"] {
        table_row("Environment-Variables.md", &vars, var);
    }
    assert!(
        table_rows(&page)
            .iter()
            .all(|row| row.first().is_none_or(|cell| cell != "`ui`")),
        "Environment-Variables.md lists `ui` as a setting {ui_env} overrides"
    );
}

/// The chart section of the TUI page names every chart type and spec
/// field the parser reads, the setting that sizes a chart and the key
/// that shows it as source.
#[test]
fn tui_page_documents_every_chart_spec_name() {
    let chart = src(&["tui", "rich", "chart.rs"]);
    let names = string_literals(&const_expr(&chart, "CHART_SPEC_NAMES"));
    for name in ["line", "scatter", "bar", "sparkline", "series", "data"] {
        assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
    }
    assert_eq!(template_value("tui_chart_height"), "12");

    let Some(wiki) = wiki_dir() else { return };
    let charts = flat(&section(PAGE, &read(wiki.join(PAGE)), CHARTS_HEADING));
    let needles = names
        .iter()
        .map(|name| format!("`{name}`"))
        .chain(["`tui_chart_height`".to_string(), "`Alt+M`".to_string()]);
    for needle in needles {
        assert!(
            charts.contains(&needle),
            "{PAGE} `{CHARTS_HEADING}` names {needle}"
        );
    }
}

#[test]
fn tui_page_quotes_the_code() {
    let tui_source = src(&["tui", "mod.rs"]);
    let tui = production(&tui_source);
    let run_loop_source = src(&["tui", "run_loop.rs"]);
    let run_loop = production(&run_loop_source);
    let rewind_source = src(&["tui", "rewind.rs"]);
    let rewind_code = production(&rewind_source);
    let main_source = src(&["main.rs"]);
    let main = production(&main_source);
    let no_tty = string_literals(&fn_body(run_loop, "pub fn ensure_tty("));
    assert_eq!(no_tty, ["No TTY for TUI"]);
    let flags = [clap_long("tui"), clap_long("repl")];
    let interactive =
        literal_with(main, "requires the interactive mode").replace("{flag}", &flags[0]);
    assert!(main.contains(&format!(
        "let flag = if cli.tui {{ \"{}\" }} else {{ \"{}\" }};",
        flags[0], flags[1]
    )));
    let custom_row = const_str(
        &src(&["prompt", "mod.rs"]),
        "CUSTOM_MULTI_CHOICE_ANSWER_OPTION",
    );
    let cells = src(&["tui", "cells.rs"]);
    let mid_turn = literal_with(production(&cells), "(sent mid-turn)").replace("{text}", "<text>");
    let queued = src(&["tui", "queued.rs"]);
    let pending = literal_with(production(&queued), "pending: ").replace("{}", "<n>");
    let dropped = literal_with(tui, "queued line(s)").replace("{dropped}", "<n>");
    let running = literal_with(tui, " . {tool}")
        .trim_start()
        .replace("{}", "<time>")
        .replace("{tool}", "<tool>");
    let thinking = literal_with(tui, "Thinking for ")
        .trim_start()
        .replace("{}", "<time>");
    assert_eq!(
        const_expr(&src(&["tui", "throbber.rs"]), "THROBBER_INTERVAL"),
        "Duration::from_millis(100)"
    );
    let interrupt = literal_with(tui, "Esc to interrupt")
        .trim_start()
        .to_string();
    let footer_text = fn_body(
        tui,
        "fn parts(&self, (agents, jobs): (usize, usize)) -> Vec<String> {",
    ) + &fn_body(tui, "fn footer_right(usage: &str) -> Line<'static> {");
    let mut footer: Vec<String> = string_literals(&footer_text)
        .iter()
        .filter(|literal| literal.chars().any(char::is_alphabetic))
        .map(|literal| literal.replace("{agents}", "<n>").replace("{jobs}", "<n>"))
        .collect();
    footer.sort();
    assert_eq!(footer, ["agents <n>", "jobs <n>", "no session"]);
    let bottom = src(&["tui", "bottom_pane.rs"]);
    let search = const_str(&bottom, "SEARCH_LABEL");
    let rewind = literal_with(rewind_code, "Rewind stops at the last ");
    let barriers = string_literals(&fn_body(&cells, "pub(crate) fn kind_name("));
    assert_eq!(barriers.len(), 2);
    let roster_empty = const_str(&src(&["tui", "roster.rs"]), "EMPTY");
    let idle_status = flat(&const_str(tui, "IDLE_STATUS"));
    let idle_status_with_help = flat(&const_str(tui, "IDLE_STATUS_WITH_HELP"));
    let keys_command = const_str(tui, "KEYS_COMMAND");
    let threads_command = const_str(tui, "THREADS_COMMAND");
    let normal = const_str(&src(&["tui", "composer.rs"]), "NORMAL_TAG");
    for (source, gap) in [(tui, "HOTKEY_IDLE"), (rewind_code, "REWIND_ESC_GAP")] {
        assert_eq!(const_expr(source, gap), "Duration::from_secs(1)", "{gap}");
    }
    assert_eq!(const_expr(tui, "FULL_OUTPUT_LIMIT"), "4 * 1024 * 1024");
    assert_eq!(const_expr(tui, "FULL_OUTPUT_BUDGET"), "32 * 1024 * 1024");
    let suspend = run_loop
        .lines()
        .skip_while(|line| line.trim() != "Some(Command::Suspend) => {")
        .nth(1)
        .map(str::trim);
    assert_eq!(suspend, Some("#[cfg(unix)]"), "suspend is unix-only");
    assert!(src(&["tui", "terminal.rs"]).contains("WriteConsoleW"));
    assert!(src(&["tui", "terminal.rs"]).contains("\\x1b[?1000h\\x1b[?1002h\\x1b[?1006h"));
    assert!(src(&["config", "input.rs"]).contains("left the group with setsid"));
    let editor = fn_body(&src(&["config", "app_config.rs"]), "pub fn editor(&self)");
    assert!(editor.contains("env::var(\"VISUAL\")") && editor.contains("env::var(\"EDITOR\")"));
    assert!(src(&["utils", "mod.rs"]).contains("env::var(\"NO_COLOR\")"));

    let Some(wiki) = wiki_dir() else { return };
    let page = flat(&read(wiki.join(PAGE)));
    let mut needles = vec![
        format!("`{}`", no_tty[0]),
        format!("`{interactive}`"),
        format!("`{}`", flags[0]),
        format!("`{}`", flags[1]),
        "`ui: tui`".to_string(),
        format!("`{custom_row}`"),
        format!("`{mid_turn}`"),
        format!("`{pending}`"),
        format!("`{dropped}`"),
        format!("`{running}`"),
        format!("`{thinking}`"),
        format!("`{interrupt}`"),
        format!("`{search}`"),
        format!("`{roster_empty}`"),
        format!("`{idle_status}`"),
        format!("`{idle_status_with_help}`"),
        format!("`.{keys_command}`"),
        format!("`.{threads_command}`"),
        format!("`{}`", normal.trim()),
        "1 second".to_string(),
        "100 ms".to_string(),
        "within a second".to_string(),
        "4 MiB".to_string(),
        "32 MiB".to_string(),
        "`$VISUAL`".to_string(),
        "`$EDITOR`".to_string(),
        "`NO_COLOR`".to_string(),
        "`WriteConsoleW`".to_string(),
        "`setsid`".to_string(),
        "`/dev/tty`".to_string(),
        "use the REPL".to_string(),
    ];
    needles.extend(footer.iter().map(|text| format!("`{text}`")));
    needles.extend(
        barriers
            .iter()
            .map(|kind| format!("`{}`", rewind.replace("{}", kind))),
    );
    for needle in needles {
        assert!(page.contains(&needle), "{PAGE} says {needle}");
    }
    assert!(
        !page.contains("`COYOTE_UI=tui`"),
        "{PAGE} shows COYOTE_UI choosing the shell"
    );

    let custom = flat(&section(
        "Custom-Tools.md",
        &read(wiki.join("Custom-Tools.md")),
        PROTOCOL_HEADING,
    ));
    let source = src(&["function", "prompt_channel", "server.rs"]);
    assert!(source.contains(r#"r"\\.\pipe\coyote-prompt-{}-{}""#));
    assert!(source.contains("format!(\"/dev/fd/{}\", self.child_fd)"));
    for needle in [r"`\\.\pipe\coyote-prompt-...`", "`/dev/fd/<n>`"] {
        assert!(custom.contains(needle), "Custom-Tools.md says {needle}");
    }
}

/// Every entry of the TUI's command timing table as the page's table shows
/// it: the command, its class and, for a queued one, the reason.
fn command_timings() -> Vec<(String, String, String)> {
    let source = src(&["tui", "command_timing.rs"]);
    let start = source
        .find("pub(super) const COMMAND_TIMINGS")
        .expect("the source declares COMMAND_TIMINGS");
    let table = &source[start..];
    let table = flat(&table[..table.find("\n];").expect("the table ends with `];`")])
        .replace("( \"", "(\"");
    table
        .split("(\".")
        .skip(1)
        .map(|entry| {
            let (name, rest) = entry.split_once('"').expect("a command name ends");
            let timing = rest.trim_start_matches(", ");
            let (class, reason) = if timing.starts_with("Timing::Now)") {
                ("At once", String::new())
            } else if timing.starts_with("Timing::Deferred)") {
                ("Next turn", String::new())
            } else if timing.starts_with("Timing::Queue(") {
                ("Queued", string_literals(timing).remove(0))
            } else {
                let constant = &timing[..timing.find(')').expect("an entry ends")];
                (
                    "Queued",
                    string_literals(&const_expr(&source, constant)).remove(0),
                )
            };
            (format!(".{name}"), class.to_string(), reason)
        })
        .collect()
}

/// The page's table of what a command typed during a turn does is the
/// timing table, row for row, so a command added, moved or reworded in
/// the code fails here until the page follows.
#[test]
fn tui_page_lists_every_command_timing() {
    let timings = command_timings();
    assert!(timings.len() > 60, "the table scraper read {timings:?}");
    let control = src(&["tui", "control.rs"]);
    let applies = literal_with(production(&control), "applies from the next turn")
        .replace("{line}", "<line>");

    let Some(wiki) = wiki_dir() else { return };
    let queued = section(PAGE, &read(wiki.join(PAGE)), "## Queued input");
    let needle = format!("`{}`", applies.trim_end());
    assert!(flat(&queued).contains(&needle), "{PAGE} says {needle}");
    let rows: Vec<(String, String, String)> = table_rows(&queued)
        .into_iter()
        .map(|row| {
            let [name, class, reason] = <[String; 3]>::try_from(row).expect("three cells a row");
            (name.trim_matches('`').to_string(), class, reason)
        })
        .collect();
    assert_eq!(rows, timings);
}

/// What the page says about how the UI is drawn holds for the code that
/// draws it: each pair is a fact in the source and the words the page
/// uses for it, so changing one without the other fails here. The page
/// is ASCII, so the fold arrows are described rather than quoted.
#[test]
fn tui_page_describes_the_drawn_chrome() {
    let cells = src(&["tui", "cells.rs"]);
    let composer = src(&["tui", "composer.rs"]);
    let modal = src(&["tui", "modal.rs"]);
    let theme = src(&["tui", "theme.rs"]);
    let tui = src(&["tui", "mod.rs"]);
    let facts: [(&str, &str, &str); 7] = [
        (
            &cells,
            "COLLAPSED_ARROW: &str = \"\\u{25B8} \"",
            "behind an arrow pointing right",
        ),
        (
            &cells,
            "EXPANDED_ARROW: &str = \"\\u{25BE} \"",
            "Opened, the arrow points down",
        ),
        (
            &modal,
            "prompt.req.action == PromptAction::Confirm && prompt.asked_by_call(call_id)",
            "its row is held open so what you approve is on screen",
        ),
        (
            &composer,
            ".borders(Borders::TOP | Borders::BOTTOM)",
            "It sits between two horizontal rules",
        ),
        (
            &modal,
            "if self.req.tool.is_some() {",
            "a tool's question stays plain text",
        ),
        (
            &theme,
            "const HIGHLIGHT: Color = Color::LightCyan;",
            "bold in the REPL's highlight color (light cyan)",
        ),
        (
            &theme,
            "const HEADING_FALLBACK: Color = Color::Yellow;",
            "the REPL's yellow when no theme gives one",
        ),
    ];
    for (code, fact, _) in facts {
        assert!(production(code).contains(fact), "the source lost {fact}");
    }
    assert!(
        production(&tui).contains("Borders::TOP"),
        "mod.rs draws no rule"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = flat(&read(wiki.join(PAGE)));
    for (_, _, words) in facts {
        assert!(page.contains(words), "{PAGE} says {words}");
    }
    let rule = "A horizontal line under the transcript";
    assert!(page.contains(rule), "{PAGE} says {rule}");
}

#[test]
fn the_word_cut_keys_on_the_page_match_the_key_help() {
    let keys = src(&["tui", "keys.rs"]);
    let label = "\"Ctrl+W / Alt+Backspace\",";
    assert!(production(&keys).contains(label), "keys.rs lost {label}");
    assert!(
        !production(&keys).contains("Alt+H\""),
        "keys.rs offers Alt+H"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = flat(&read(wiki.join(PAGE)));
    let sentence = "`Ctrl+W`, `Alt+Backspace`: cut the word before the cursor";
    assert!(page.contains(sentence), "{PAGE} says {sentence}");
    assert!(!page.contains("`Alt+H`"), "{PAGE} still offers Alt+H");
}

#[test]
fn tool_prompts_are_documented_with_their_label() {
    let modal_source = src(&["tui", "modal.rs"]);
    let asks = literal_with(production(&modal_source), " asks:").replace("{}", "<tool>");
    assert_eq!(asks, "<tool> asks:");
    let prompt_source = src(&["prompt", "mod.rs"]);
    assert!(
        production(&prompt_source)
            .contains("format!(\"{tool} asks: {}\", trim_leading_blank_lines(&req.question))")
    );

    let Some(wiki) = wiki_dir() else { return };
    let custom = flat(&section(
        "Custom-Tools.md",
        &read(wiki.join("Custom-Tools.md")),
        PROTOCOL_HEADING,
    ));
    for needle in [
        format!("labels every question a tool asks `{asks}`"),
        "with the name taken from the tool it ran".to_string(),
        "never from the request line".to_string(),
        "any tool may ask any kind of question, `password` included".to_string(),
        "a newline is a line break".to_string(),
        "every other control or escape character is written out as a visible escape such as `\\x1b`"
            .to_string(),
        "\"Yes for this session\" is kept for the exact question the tool sent".to_string(),
    ] {
        assert!(custom.contains(&needle), "Custom-Tools.md says {needle}");
    }
    let page = flat(&read(wiki.join(PAGE)));
    assert!(
        page.contains(&format!("`{asks}` for a tool")),
        "{PAGE} says {asks}"
    );
}

#[test]
fn session_yes_is_documented_as_covering_one_operation_or_path() {
    let sh = asset(&["functions", "utils", "prompt-utils.sh"]);
    let py = asset(&["functions", "utils", "prompt-utils.py"]);
    let ts = asset(&["functions", "utils", "prompt-utils.ts"]);
    assert!(fn_body(&sh, "guard_operation() {").contains("question=\"$question\"$'\\n'\"$2\""));
    assert!(
        fn_body(&sh, "guard_path() {")
            .contains("confirmation_prompt=\"$confirmation_prompt\"$'\\n'\"$path\"")
    );
    assert!(py.contains(
        "def guard_operation(question=\"Are you sure you want to continue?\", operation=None):"
    ));
    assert!(fn_body(&ts, "export function guardOperation(").contains("operation?: string,"));
    for (tool, call) in [
        (
            "execute_command.sh",
            "guard_operation \"Run this command?\" \"$argc_command\"",
        ),
        (
            "fs_write.sh",
            "guard_operation \"Apply changes?\" \"$(_to_real_path \"$argc_path\")\"",
        ),
        (
            "fs_patch.sh",
            "guard_operation \"Apply changes?\" \"$(_to_real_path \"$argc_path\")\"",
        ),
        (
            "execute_sql_code.sh",
            "guard_operation \"Execute SQL?\" \"$argc_code\"",
        ),
    ] {
        assert!(
            asset(&["functions", "tools", tool]).contains(call),
            "{tool} names what it guards"
        );
    }
    assert!(
        asset(&["agents", "sql", "tools.sh"])
            .contains("guard_operation \"Execute SQL?\" \"$argc_query\""),
        "the sql agent's write_query names the query it guards"
    );

    let Some(wiki) = wiki_dir() else { return };
    let bash = read(wiki.join("Bash-Prompt-Helpers.md"));
    let operation = flat(&section(
        "Bash-Prompt-Helpers.md",
        &bash,
        "## guard_operation",
    ));
    for needle in [
        "Usage: `guard_operation [question] [operation]`",
        "\"Yes for this session\" covers only that exact operation; a different operation asks again",
    ] {
        assert!(
            operation.contains(needle),
            "Bash-Prompt-Helpers.md says {needle}"
        );
    }
    let path = flat(&section("Bash-Prompt-Helpers.md", &bash, "## guard_path"));
    for needle in [
        "Usage: `guard_path <path> <question>`",
        "The resolved path is added on the line after the question unless the question already names it",
        "\"Yes for this session\" covers only that exact path; a different path asks again",
    ] {
        assert!(
            path.contains(needle),
            "Bash-Prompt-Helpers.md says {needle}"
        );
    }
    let custom = flat(&section(
        "Custom-Tools.md",
        &read(wiki.join("Custom-Tools.md")),
        PROTOCOL_HEADING,
    ));
    for needle in [
        "`guard_operation(question, operation)` (`guardOperation(question, operation)` in TypeScript) takes an optional operation",
        "unless the question already names that resolved path",
        "so the answer covers only that exact path. Anything different asks again.",
        "An empty question is the default one",
    ] {
        assert!(custom.contains(needle), "Custom-Tools.md says {needle}");
    }
    let page = flat(&read(wiki.join(PAGE)));
    for needle in [
        "for a guarded tool it covers only that exact command or path, and anything different asks again",
        "for a guarded tool only for that exact command or path; anything different asks again",
    ] {
        assert!(page.contains(needle), "{PAGE} says {needle}");
    }
}

/// Every `require_human(` call in `source`: its action format and its
/// remedy, when the remedy is a literal.
fn refusal_calls(source: &str) -> Vec<(String, Option<String>)> {
    source
        .match_indices("require_human(")
        .filter(|(at, _)| !source[..*at].ends_with("fn "))
        .map(|(at, call)| {
            let rest = &source[at + call.len()..];
            let mut literals = string_literals(&rest[..rest.find(")?;").unwrap()]).into_iter();
            (literals.next().unwrap(), literals.next())
        })
        .collect()
}

/// Whether `text` fits `format`, every `{...}` placeholder matching any text.
fn fits(format: &str, text: &str) -> bool {
    let pieces: Vec<&str> = format
        .split('{')
        .enumerate()
        .map(|(i, piece)| {
            if i == 0 {
                piece
            } else {
                &piece[piece.find('}').unwrap() + 1..]
            }
        })
        .collect();
    let mut rest = text;
    for (i, piece) in pieces.iter().enumerate() {
        if i == 0 {
            let Some(after) = rest.strip_prefix(piece) else {
                return false;
            };
            rest = after;
        } else if i == pieces.len() - 1 {
            return rest.ends_with(piece) && rest.len() > piece.len();
        } else {
            let skip = rest.char_indices().nth(1).map_or(rest.len(), |(at, _)| at);
            let Some(at) = rest[skip..].find(piece) else {
                return false;
            };
            rest = &rest[skip + at + piece.len()..];
        }
    }
    rest.is_empty()
}

#[test]
fn unattended_page_quotes_the_refusals() {
    let prompt_source = src(&["prompt", "mod.rs"]);
    let prompt = production(&prompt_source);
    let attached = const_str(prompt, "NO_HUMAN_ATTACHED");
    let oauth = const_str(prompt, "NO_HUMAN_FOR_OAUTH");
    let needs =
        literal_with(prompt, "prompt needs a human").replace("{NO_HUMAN_ATTACHED}", &attached);
    let creating = literal_with(prompt, "needs a human to answer the prompt")
        .replace("{NO_HUMAN_ATTACHED}", &attached);
    let headless = fn_body(prompt, "impl Prompter for HeadlessPrompter");
    assert!(headless.contains("PromptAction::Confirm => Ok(PromptAnswer::NeedsHuman)"));
    assert!(headless.contains("action => Err(NeedsHuman::Prompt(action).into())"));
    let shape = string_literals(&fn_body(prompt, "pub(crate) fn require_human(")).remove(0);
    let (lead, tail) = shape.split_once("{action}").unwrap();
    let (middle, _) = tail.split_once("{remedy}").unwrap();
    let mut calls = Vec::new();
    for file in [
        &["config", "install_remote.rs"][..],
        &["vault", "mod.rs"],
        &["mcp", "oauth.rs"],
        &["client", "oauth.rs"],
    ] {
        calls.extend(refusal_calls(production(&src(file))));
    }
    assert!(calls.len() >= 5, "{calls:?}");

    let Some(wiki) = wiki_dir() else { return };
    let section = section(
        "Unattended-Mode.md",
        &read(wiki.join("Unattended-Mode.md")),
        HEADLESS_HEADING,
    );
    let prose = flat(&section);
    let select = needs.replace("{}", "select");
    let role = creating
        .replace("{kind}", "role")
        .replace("{name}", "reviewer");
    for needle in [
        format!("`{select}`"),
        format!("`{role}`"),
        format!("`{oauth}`"),
        format!("`{lead}<action>{middle}`"),
        "`COYOTE_UI=headless`".to_string(),
    ] {
        assert!(prose.contains(&needle), "Unattended-Mode.md says {needle}");
    }
    let examples = fenced_blocks(&section, "");
    assert_eq!(examples.len(), 1, "one block of refusal examples");
    for line in examples[0].lines() {
        let action_and_remedy = line
            .strip_prefix(lead)
            .unwrap_or_else(|| panic!("{line:?} starts with {lead:?}"));
        let (action, remedy) = action_and_remedy
            .split_once(middle)
            .unwrap_or_else(|| panic!("{line:?} has {middle:?}"));
        assert!(
            calls
                .iter()
                .any(|(format, literal)| fits(format, action) && literal.as_deref() == Some(remedy)),
            "{line:?} is a refusal the code builds: {calls:?}"
        );
    }
}

#[test]
fn fits_matches_placeholders_anywhere_but_literals_exactly() {
    assert!(fits("uninstall bundle '{name}'", "uninstall bundle 'x'"));
    assert!(fits(
        "add secret '{secret_name}'",
        "add secret 'GITHUB_TOKEN'"
    ));
    assert!(fits(
        "sign in to '{server_name}' with OAuth",
        "sign in to 'github' with OAuth"
    ));
    assert!(!fits(
        "sign in to '{server_name}' with OAuth",
        "sign in to 'github'"
    ));
    assert!(!fits("add secret '{secret_name}'", "add a secret 'x'"));
    assert!(fits("plain", "plain"));
    assert!(!fits("plain", "plainer"));
    assert!(fits("a '{x}' b '{y}'", "a '\u{e9}' b 'z'"));
    assert!(!fits("a '{x}' b '{y}'", "a '\u{e9}' c 'z'"));
}

/// The sections and rows this lint covers on each wiki page, labelled.
fn edited_texts(wiki: &Path) -> Vec<(String, String)> {
    let custom = read(wiki.join("Custom-Tools.md"));
    let bash = read(wiki.join("Bash-Prompt-Helpers.md"));
    let env = read(wiki.join("Environment-Variables.md"));
    let hooks = read(wiki.join("Hooks.md"));
    let unattended = read(wiki.join("Unattended-Mode.md"));
    let started = read(wiki.join("Getting-Started.md"));
    let sidebar = read(wiki.join("_Sidebar.md"));
    let mut texts = vec![
        (PAGE.to_string(), read(wiki.join(PAGE))),
        (
            format!("Custom-Tools.md {PROTOCOL_HEADING}"),
            section("Custom-Tools.md", &custom, PROTOCOL_HEADING),
        ),
        (
            format!("Bash-Prompt-Helpers.md {CHANNEL_HEADING}"),
            section("Bash-Prompt-Helpers.md", &bash, CHANNEL_HEADING),
        ),
        (
            "Bash-Prompt-Helpers.md ## guard_operation".to_string(),
            section("Bash-Prompt-Helpers.md", &bash, "## guard_operation"),
        ),
        (
            "Bash-Prompt-Helpers.md ## guard_path".to_string(),
            section("Bash-Prompt-Helpers.md", &bash, "## guard_path"),
        ),
        (
            format!("Environment-Variables.md {ENV_HEADING}"),
            section("Environment-Variables.md", &env, ENV_HEADING),
        ),
        (
            "Hooks.md tool events".to_string(),
            section("Hooks.md", &hooks, "## Tool events"),
        ),
        (
            "Hooks.md in-process paragraph".to_string(),
            paragraph("Hooks.md", &hooks, IN_PROCESS_LEAD),
        ),
        (
            format!("Unattended-Mode.md {HEADLESS_HEADING}"),
            section("Unattended-Mode.md", &unattended, HEADLESS_HEADING),
        ),
        (
            "_Sidebar.md TUI entries".to_string(),
            sidebar
                .lines()
                .filter(|line| line.contains("(TUI"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    ];
    let rows = [
        ("Environment-Variables.md", &env, "`COYOTE_TOOL_CALL_ID`"),
        ("Hooks.md", &hooks, "`tool.started`"),
        ("Hooks.md", &hooks, "`tool.completed`"),
        ("Hooks.md", &hooks, "`tool.failed`"),
    ];
    for (label, text, first) in rows {
        texts.push((
            format!("{label} {first} row"),
            table_row(label, text, first).join(" | "),
        ));
    }
    for key in SETTINGS {
        let first = format!("`{key}`");
        texts.push((
            format!("Getting-Started.md {first} row"),
            table_row("Getting-Started.md", &started, &first).join(" | "),
        ));
    }
    texts
}

/// Non-ASCII characters, task or plan ids and planning vocabulary in `text`,
/// each as one line naming `label`.
fn planning_hits(label: &str, text: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if let Some(c) = line.chars().find(|c| !c.is_ascii()) {
            hits.push(format!("{label} line {}: U+{:04X}", i + 1, c as u32));
        }
        for id in ["TASK-", "PLAN-"] {
            if line.contains(id) {
                hits.push(format!("{label} line {}: {id}", i + 1));
            }
        }
    }
    for word in words(text) {
        if PLANNING_WORDS.contains(&word.as_str()) {
            hits.push(format!("{label}: the word {word:?}"));
        }
    }
    hits
}

/// [`planning_hits`] plus release narration in `text`.
fn voice_hits(label: &str, text: &str) -> Vec<String> {
    let mut hits = planning_hits(label, text);
    for word in words(text) {
        if CHANGELOG_WORDS.contains(&word.as_str()) {
            hits.push(format!("{label}: the word {word:?}"));
        }
    }
    hits
}

#[test]
fn edited_prose_is_ascii_and_plain() {
    let readme = read(repo_root().join("README.md"));
    let mut hits = Vec::new();
    for key in SETTINGS {
        let row = table_row("README.md", &readme, &format!("`{key}`")).join(" | ");
        hits.extend(voice_hits(&format!("README.md `{key}` row"), &row));
    }
    assert!(hits.is_empty(), "{}", hits.join("\n"));
    assert!(!voice_hits("probe", "a new wave of rulings").is_empty());

    let Some(wiki) = wiki_dir() else { return };
    let hits: Vec<String> = edited_texts(&wiki)
        .iter()
        .flat_map(|(label, text)| voice_hits(label, text))
        .collect();
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

/// The `#` comments of the top-level `key:` lines of `yaml` that `keys`
/// name, each followed by the comment-only lines that continue it.
fn setting_comments(yaml: &str, keys: &[&str]) -> String {
    let mut out = Vec::new();
    let mut continuing = false;
    for line in yaml.lines() {
        if continuing && line.starts_with(' ') && line.trim_start().starts_with('#') {
            out.push(line.trim());
            continue;
        }
        continuing = keys.iter().any(|key| line.starts_with(&format!("{key}: ")));
        if continuing {
            let (_, comment) = line
                .split_once(" #")
                .unwrap_or_else(|| panic!("the `{line}` setting has a comment"));
            out.push(comment.trim());
        }
    }
    out.join("\n")
}

#[test]
fn comments_carry_no_planning_vocabulary() {
    let mut hits = Vec::new();
    for file in ["tui_wiki_docs.rs", "theme_setting_usage.rs"] {
        let source = read(repo_root().join("tests").join(file));
        let comments: Vec<&str> = source
            .lines()
            .filter(|line| line.trim_start().starts_with("//"))
            .collect();
        assert!(!comments.is_empty(), "tests/{file} has comments");
        hits.extend(planning_hits(
            &format!("tests/{file} comments"),
            &comments.join("\n"),
        ));
    }
    for (label, yaml) in [
        (
            "config.example.yaml",
            read(repo_root().join("config.example.yaml")),
        ),
        (
            "assets/config-template.yaml",
            asset(&["config-template.yaml"]),
        ),
    ] {
        let comments = setting_comments(&yaml, &SETTINGS);
        assert!(
            comments.lines().count() > SETTINGS.len(),
            "{label} comments the UI settings: {comments}"
        );
        hits.extend(planning_hits(&format!("{label} comments"), &comments));
    }
    assert!(hits.is_empty(), "{}", hits.join("\n"));
    assert!(!planning_hits("probe", "the next wave").is_empty());
    assert!(planning_hits("probe", "now a new setting").is_empty());
}
