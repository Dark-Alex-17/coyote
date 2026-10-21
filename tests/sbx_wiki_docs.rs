//! Opt-in doc lint: checks the `Sandboxes.md` wiki page and its sidebar
//! entries against the sandbox code they describe.
//!
//! The wiki is a separate clone, so these tests run only when `COYOTE_WIKI_DIR`
//! points at it; a relative value is resolved against the crate root:
//!
//! ```sh
//! COYOTE_WIKI_DIR=../coyote.wiki cargo test --test sbx_wiki_docs
//! ```
//!
//! Without the variable the wiki half of every test is skipped (printed as
//! `skipping:`) and the repo-only assertions still run, so a plain `cargo test`
//! is unaffected. With it set to a directory that has no `Sandboxes.md` the
//! tests fail rather than pass against nothing.
//!
//! There is no lib target, so the code side of each check is read from the
//! repo's text: the kit literals in `src/sandbox/kit.rs`, the discovery roots
//! in `src/sandbox/mixins.rs`, the version floor and the `sbx` argv builders
//! in `src/sandbox/mod.rs`, the directory names in `src/config/mod.rs`, the
//! generated kit name in `src/sandbox/mcp_credentials.rs`, the override env
//! var in `src/config/paths.rs` (through `get_env_name` in `src/utils/mod.rs`
//! and the binary name in `Cargo.toml`) and the shipped kits under `assets/`.
//! The literal scraper reads plain `"..."` strings only: a `"` inside a line
//! comment, a `'"'` char or a raw string in a scraped body would desync it, so
//! keep those out of the bodies named here.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const WIKI_DIR: &str = "COYOTE_WIKI_DIR";
const RECIPE: &str = "`COYOTE_WIKI_DIR=../coyote.wiki cargo test --test sbx_wiki_docs`";
const PAGE: &str = "Sandboxes.md";
const SIDEBAR: &str = "_Sidebar.md";
/// Wording of the kit format the page no longer describes. The two entries
/// that are also ordinary words (`extends`, `files/`) are matched only as the
/// code spans the old page used for them.
const RETIRED: [&str; 14] = [
    "sbx-mixin.yaml",
    "spec.yaml",
    "schemaVersion: \"2\"",
    "schemaVersion: '2'",
    "permissions.network.allow",
    "setup.install",
    "setup.startup",
    "--no-mixins",
    "--profile headless",
    "sbx-mixin-kits",
    "sbx_kit_dir",
    "`extends:`",
    "agentInstructions",
    "`files/`",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
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
        dir.join(PAGE).is_file(),
        "{WIKI_DIR}={} has no {PAGE}; point it at a coyote.wiki checkout",
        dir.display()
    );
    Some(dir)
}

/// Every `"..."` literal in `source`, in order, paired with the text that
/// follows its closing quote. `\"`, `\\` and `\n` are unescaped and a `\` line
/// continuation is dropped the way rustc drops it.
fn string_literals(source: &str) -> Vec<(String, &str)> {
    let mut literals = Vec::new();
    let mut chars = source.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut literal = String::new();
        loop {
            match chars.next() {
                Some((at, '"')) => {
                    literals.push((literal, &source[at + 1..]));
                    break;
                }
                Some((_, '\\')) => match chars.next() {
                    Some((_, escaped @ ('"' | '\\'))) => literal.push(escaped),
                    Some((_, 'n')) => literal.push('\n'),
                    Some((_, '\n')) => {
                        while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
                            chars.next();
                        }
                    }
                    other => panic!(
                        "unexpected escape {other:?} in {literal:?}; extend string_literals in tests/sbx_wiki_docs.rs"
                    ),
                },
                Some((_, c)) => literal.push(c),
                None => panic!("unterminated string literal: {literal:?}"),
            }
        }
    }
    literals
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
    literals.remove(0).0
}

/// The source of the function whose signature starts with `head` in
/// `source`, up to the first `}` standing alone on a line at the function's
/// own indentation.
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

/// The text under `heading`, a `## ` or `### ` line of `page`, up to the next
/// heading of the same or a higher level. Lines inside fenced code blocks are
/// never taken for headings.
fn section(page: &str, heading: &str) -> String {
    let level = heading.bytes().take_while(|b| *b == b'#').count();
    let closes = |line: &str| {
        let depth = line.bytes().take_while(|b| *b == b'#').count();
        (1..=level).contains(&depth) && line[depth..].starts_with(' ')
    };
    let mut fenced = false;
    let mut body: Option<Vec<&str>> = None;
    for line in page.lines() {
        if line.starts_with("```") {
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
    let lines = body.unwrap_or_else(|| panic!("{PAGE} has no `{heading}` heading"));
    lines.join("\n")
}

/// The first cell of every table row of `text`, in order.
fn first_cells(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.starts_with('|'))
        .map(|line| line.split('|').nth(1).unwrap().trim().to_string())
        .collect()
}

/// The heading lines of `page` outside fenced code blocks, without their `#`
/// prefix.
fn headings(page: &str) -> Vec<String> {
    let mut fenced = false;
    let mut out = Vec::new();
    for line in page.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if !fenced && line.starts_with('#') {
            out.push(line.trim_start_matches('#').trim().to_string());
        }
    }
    out
}

/// A heading as GitHub anchors it: lowercased, spaces to `-`, backticks and
/// every punctuation mark other than `-` and `_` dropped.
fn slug(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .collect()
}

/// Every anchor of a `](<prefix><anchor>)` link in `text`.
fn anchors<'a>(text: &'a str, prefix: &str) -> Vec<&'a str> {
    let open = format!("]({prefix}");
    text.match_indices(&open)
        .map(|(at, _)| {
            let rest = &text[at + open.len()..];
            &rest[..rest.find(')').expect("a closed link")]
        })
        .collect()
}

/// `MIN_SBX_VERSION` from `src/sandbox/mod.rs`, rendered `vX.Y.Z`.
fn min_sbx_version() -> String {
    let source = src(&["sandbox", "mod.rs"]);
    let head = "const MIN_SBX_VERSION: (u64, u64, u64) = (";
    let start = source.find(head).expect("mod.rs declares MIN_SBX_VERSION");
    let rest = &source[start + head.len()..];
    let parts: Vec<u64> = rest[..rest.find(')').expect("a closed tuple")]
        .split(',')
        .map(|part| part.trim().parse().expect("a numeric version part"))
        .collect();
    assert_eq!(parts.len(), 3, "MIN_SBX_VERSION has three parts");
    format!("v{}.{}.{}", parts[0], parts[1], parts[2])
}

/// The refusal sentence of `check_sbx_version` with the found version spelled
/// `<found>` and the floor filled in.
fn refusal_sentence() -> String {
    let body = fn_body(&src(&["sandbox", "mod.rs"]), "fn check_sbx_version(");
    let (template, _) = string_literals(&body)
        .into_iter()
        .find(|(literal, _)| literal.starts_with("sbx v{major}"))
        .expect("check_sbx_version bails with the too-old sentence");
    template
        .replace("v{major}.{minor}.{patch}", "v<found>")
        .replace("v{min_major}.{min_minor}.{min_patch}", &min_sbx_version())
}

/// The root labels `fn roots` in `src/sandbox/mixins.rs` pairs with each
/// `paths::kits_dir(...)` push, in push order: the literal a label is built
/// from is the one followed by `.to_string()` or by the `format!` arguments.
fn root_labels() -> Vec<String> {
    let body = fn_body(&src(&["sandbox", "mixins.rs"]), "fn roots(");
    let literals = string_literals(&body);
    let labels: Vec<String> = literals
        .iter()
        .filter(|(_, after)| after.starts_with(".to_string()") || after.starts_with(','))
        .map(|(literal, _)| literal.clone())
        .collect();
    // Every literal in the walk is either a root label or a joined `hooks`
    // segment; a root pushed in a third shape would escape both scrapes, so
    // the two partitions have to add up to the whole.
    let joined = literals
        .into_iter()
        .filter(|(_, after)| after.starts_with(')'))
        .count();
    assert_eq!(
        labels.len() + joined,
        string_literals(&body).len(),
        "every literal in `fn roots` is a label or a joined segment"
    );
    let last_push = body.rfind("roots.push((paths::kits_dir(").unwrap();
    let workspace = body.find("workspace_kit_roots(").unwrap();
    assert!(
        workspace > last_push,
        "the workspace roots come after every config-dir root"
    );
    labels
}

/// The `kits/` dir a root label stands for, as the page's discovery table
/// spells it.
fn table_path(config: &str, label: &str) -> String {
    let kits = const_str(config, "KITS_DIR_NAME");
    // A top-level label segment names a config-dir child whose real name is a
    // constant, so the row is spelled from the constant and a renamed
    // directory reds the lint even while the display label stays put. The
    // nested `hooks` under an agent or the roles dir is the literal the walk
    // joins (`agent.join("hooks")`, `roles_dir().join("hooks")` in
    // `mixins::roots`), so that segment is scraped from the walk itself.
    let nested_hooks = {
        let roots = fn_body(&src(&["sandbox", "mixins.rs"]), "fn roots(");
        let joined: Vec<String> = string_literals(&roots)
            .into_iter()
            .filter(|(_, after)| after.starts_with(')'))
            .map(|(literal, _)| literal)
            .collect();
        assert_eq!(
            joined,
            ["hooks", "hooks"],
            "both nested hook roots join one literal"
        );
        joined[0].clone()
    };
    let dir_const = |segment: &str, nested: bool| -> String {
        let name = match segment {
            "functions" => "FUNCTIONS_DIR_NAME",
            "agents" => "AGENTS_DIR_NAME",
            "rags" => "RAGS_DIR_NAME",
            "roles" => "ROLES_DIR_NAME",
            "hooks" if nested => return nested_hooks.clone(),
            "hooks" => "HOOKS_DIR_NAME",
            "{}" => return "<agent>".to_string(),
            other => panic!("no directory constant is known for the root label segment '{other}'"),
        };
        const_str(config, name)
    };
    let dir = match label {
        "config" => "<config-dir>".to_string(),
        label if label.starts_with("workspace ") => {
            format!(
                "<ancestor>/{}",
                const_str(config, "WORKSPACE_COYOTE_DIR_NAME")
            )
        }
        label => {
            let segments: Vec<String> = label
                .split('/')
                .enumerate()
                .map(|(index, segment)| dir_const(segment, index > 0))
                .collect();
            format!("<config-dir>/{}", segments.join("/"))
        }
    };
    format!("`{dir}/{kits}/<handle>/<handle>.yaml`")
}

/// The name `paths::sandbox_kit_override` reads, built the way `get_env_name`
/// builds it from the bin crate's name.
fn override_env_name() -> String {
    let body = fn_body(
        &src(&["config", "paths.rs"]),
        "pub fn sandbox_kit_override(",
    );
    let (key, _) = string_literals(&body)
        .into_iter()
        .find(|(literal, _)| body.contains(&format!("get_env_name(\"{literal}\")")))
        .expect("sandbox_kit_override reads one get_env_name key");
    let get_env_name = fn_body(&src(&["utils", "mod.rs"]), "pub fn get_env_name(");
    assert!(
        get_env_name.contains("CARGO_CRATE_NAME") && get_env_name.contains("to_ascii_uppercase"),
        "get_env_name prefixes the crate name and uppercases:\n{get_env_name}"
    );
    let manifest = read(repo_root().join("Cargo.toml"));
    let bin = manifest
        .find("[[bin]]")
        .expect("Cargo.toml declares the bin");
    let (crate_name, _) = string_literals(&manifest[bin..])
        .into_iter()
        .next()
        .expect("the bin table names the crate");
    format!("{crate_name}_{key}").to_ascii_uppercase()
}

#[test]
fn slug_follows_the_github_anchor_rule() {
    assert_eq!(
        slug("What `--sandbox` actually does"),
        "what---sandbox-actually-does"
    );
    assert_eq!(slug("Credentials & Secrets"), "credentials--secrets");
    assert_eq!(
        slug("Extending the Sandbox: Auto-Discovered Mixins"),
        "extending-the-sandbox-auto-discovered-mixins"
    );
}

#[test]
fn discovery_table_lists_the_roots_in_walk_order() {
    let labels = root_labels();
    assert_eq!(
        labels,
        [
            "config",
            "functions",
            "agents/{}",
            "agents/{}/hooks",
            "rags",
            "roles",
            "roles/hooks",
            "hooks",
            "workspace {}"
        ]
    );
    let config = src(&["config", "mod.rs"]);
    let expected: Vec<String> = labels
        .iter()
        .map(|label| table_path(&config, label))
        .collect();

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let discovery = section(&page, "## Extending the Sandbox: Auto-Discovered Mixins");
    let rows: Vec<String> = first_cells(&discovery)
        .into_iter()
        .filter(|cell| cell.starts_with("`<"))
        .collect();
    assert_eq!(rows, expected, "the discovery table rows, in order");
}

#[test]
fn kit_format_literals_appear_on_the_page() {
    let kit = src(&["sandbox", "kit.rs"]);
    let syntax_line = const_str(&kit, "SYNTAX_LINE");
    let schema = format!("schemaVersion: \"{}\"", const_str(&kit, "SCHEMA_VERSION"));
    assert!(
        kit.contains("const RESERVED_HANDLES: [&str; 2] = [BASE_KIT_HANDLE, MCP_MIXIN_NAME];"),
        "RESERVED_HANDLES is the base kit handle and the generated kit name"
    );
    let reserved = [
        const_str(&src(&["config", "mod.rs"]), "BASE_KIT_HANDLE"),
        const_str(&src(&["sandbox", "mcp_credentials.rs"]), "MCP_MIXIN_NAME"),
    ];
    let capabilities: Vec<String> = [
        "CAP_NETWORK_POLICY",
        "CAP_CREDENTIAL",
        "CAP_LIFECYCLE",
        "CAP_AGENT_CONTEXT",
    ]
    .iter()
    .map(|name| const_str(&kit, name))
    .collect();

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    for needle in [syntax_line, schema]
        .into_iter()
        .chain(reserved.iter().map(|handle| format!("`{handle}`")))
    {
        assert!(page.contains(&needle), "{PAGE} does not mention {needle}");
    }
    let rows = first_cells(&section(&page, "### What a mixin can declare"));
    for cap in &capabilities {
        assert!(
            rows.contains(&format!("`{cap}`")),
            "the capability table has no `{cap}` row"
        );
    }
}

#[test]
fn requirements_quote_the_version_floor_and_the_refusal_sentence() {
    let floor = min_sbx_version();
    let sentence = refusal_sentence();
    assert!(sentence.contains(&floor), "{sentence}");

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let requirements = section(&page, "## Requirements");
    assert!(
        requirements.contains(&format!("`sbx` {floor} or newer")),
        "the Requirements section names the floor {floor}"
    );
    assert!(
        requirements.lines().any(|line| line == sentence),
        "the Requirements section quotes the refusal sentence verbatim:\n{sentence}"
    );
}

#[test]
fn argv_blocks_match_the_create_and_run_builders() {
    let source = src(&["sandbox", "mod.rs"]);
    let flags = |head: &str| -> Vec<String> {
        string_literals(&fn_body(&source, head))
            .into_iter()
            .filter(|(literal, after)| after.starts_with(".to_string()") && !literal.contains(' '))
            .map(|(literal, _)| literal)
            .collect()
    };
    let create = flags("fn build_create_args(");
    assert_eq!(create, ["create", "--name", "--kit", "--kit", "."]);
    // The positional operands sit between the flags: the opening vec! carries
    // the name and the base kit right after `--name`, and the `.` is the last
    // push, which is the shape the page's create line spells.
    let create_body = fn_body(&src(&["sandbox", "mod.rs"]), "fn build_create_args(");
    // Folded and stripped of the bracket spacing rustfmt's layout adds, so the
    // pin reads the operand order whether the vec! spans one line or five.
    let opening = flat(&create_body).replace("[ ", "[").replace(", ]", "]");
    assert!(
        opening.contains(
            "vec![\"create\".to_string(), \"--name\".to_string(), name.to_string(), kit_str.to_string()];"
        ),
        "build_create_args opens with create, --name, the name and the base kit"
    );
    assert!(
        flat(&create_body).ends_with("args.push(\".\".to_string()); Ok(args)"),
        "the trailing `.` is the last operand build_create_args pushes"
    );
    let run = flags("fn run_args(");
    assert_eq!(run, ["run", "--name"]);
    let generated = format!(
        "<cache>/{}/{}",
        const_str(&src(&["config", "mod.rs"]), "SBX_KITS_DIR_NAME"),
        const_str(&src(&["sandbox", "mcp_credentials.rs"]), "MCP_MIXIN_NAME")
    );
    let create_line = format!(
        "sbx {} {} <NAME> <base-kit-dir> {} <kit-dir> ... {} {generated} {}",
        create[0], create[1], create[2], create[3], create[4]
    );
    let run_line = format!("sbx {} {} <NAME>", run[0], run[1]);

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    for line in [&create_line, &run_line] {
        assert!(
            page.lines().any(|l| l == line),
            "{PAGE} shows the line {line}"
        );
    }
    for line in page.lines().filter(|line| line.contains("sbx run")) {
        assert!(
            !line.contains("--kit"),
            "kits are bound at create only: {line}"
        );
    }
}

#[test]
fn kit_override_names_the_env_var_and_requires_a_workload_kit() {
    let env_name = override_env_name();
    let resolve = fn_body(&src(&["sandbox", "mod.rs"]), "fn resolve_kit_path(");
    assert!(
        resolve.contains("sandbox_kit_override()") && resolve.contains("check_workload()"),
        "the override is loaded as a workload kit:\n{resolve}"
    );
    let workload = format!(
        "kind: {}",
        const_str(&src(&["sandbox", "kit.rs"]), "KIND_WORKLOAD")
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let override_section = section(&page, "## Custom Kit Override");
    for needle in [
        format!("`{env_name}`"),
        format!("`{workload}`"),
        "workload kit".to_string(),
    ] {
        assert!(
            override_section.contains(&needle),
            "the Custom Kit Override section does not say {needle}"
        );
    }
}

#[test]
fn wording_for_the_retired_kit_format_is_gone() {
    let Some(wiki) = wiki_dir() else { return };
    for file in [PAGE, SIDEBAR] {
        let text = read(wiki.join(file));
        for needle in RETIRED {
            let hit = text.lines().find(|line| line.contains(needle));
            assert!(hit.is_none(), "{file} still says {needle:?}: {hit:?}");
        }
    }
}

/// Every `Sandboxes#<anchor>` link on the other wiki pages, and every `#<anchor>`
/// link on the page itself, names a heading of the page. The whole wiki is
/// swept because a renamed heading breaks its inbound links, not the page.
#[test]
fn inbound_anchors_resolve_to_page_headings() {
    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let slugs: Vec<String> = headings(&page).iter().map(|h| slug(h)).collect();
    let mut pages: Vec<PathBuf> = fs::read_dir(&wiki)
        .unwrap_or_else(|e| panic!("read {}: {e}", wiki.display()))
        .map(|entry| entry.expect("a readable wiki entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    pages.sort();
    let mut from_sidebar = 0;
    for path in pages {
        let text = read(&path);
        let name = path.file_name().unwrap().to_string_lossy();
        let found = if *name == *PAGE {
            anchors(&text, "#")
        } else {
            anchors(&text, "Sandboxes#")
        };
        if *name == *SIDEBAR {
            from_sidebar = found.len();
        }
        for anchor in found {
            assert!(
                slugs.iter().any(|s| s == anchor),
                "{name}: #{anchor} matches no heading of {PAGE}: {slugs:?}"
            );
        }
    }
    assert!(from_sidebar > 0, "{SIDEBAR} links into {PAGE}");
}

// ---- usage-probe additions -------------------------------------------------
//
// Spec-first pins for the page claims the tests above leave unpinned: the
// base kit's dockerfile and yaml, the three built-in kits, the launch order
// behind "What `--sandbox` actually does", the refusal and log sentences the
// discovery section quotes, and the page's own voice rules.

fn asset(parts: &[&str]) -> PathBuf {
    let mut path = repo_root().join("assets");
    for part in parts {
        path = path.join(part);
    }
    path
}

/// `text` with every run of whitespace folded to one space, so a sentence the
/// page wraps across lines compares as one line.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `(install hooks, runtime allow entries, install allow entries)` of one kit
/// yaml, counted the way `KitDir::summary` counts them; the capability type
/// strings and phase names are scraped from `kit.rs`.
fn kit_counts(kit_src: &str, yaml: &str) -> (usize, usize, usize) {
    let doc: serde_yaml::Value = serde_yaml::from_str(yaml).expect("the kit yaml parses");
    let lifecycle = const_str(kit_src, "CAP_LIFECYCLE");
    let network = const_str(kit_src, "CAP_NETWORK_POLICY");
    let runtime = const_str(kit_src, "PHASE_RUNTIME");
    let install = const_str(kit_src, "PHASE_INSTALL");
    let install_key = const_str(kit_src, "LIFECYCLE_INSTALL_KEY");
    let seq_len = |value: Option<&serde_yaml::Value>| {
        value.and_then(|v| v.as_sequence()).map_or(0, |s| s.len())
    };
    let mut counts = (0, 0, 0);
    for cap in doc["capabilities"].as_sequence().expect("capabilities") {
        let config = cap.get("config");
        let cap_type = cap["type"].as_str().unwrap_or_default();
        if cap_type == lifecycle {
            counts.0 += seq_len(config.and_then(|c| c.get(&install_key)));
        } else if cap_type == network {
            counts.1 += seq_len(
                config
                    .and_then(|c| c.get(&runtime))
                    .and_then(|p| p.get("allow")),
            );
            counts.2 += seq_len(
                config
                    .and_then(|c| c.get(&install))
                    .and_then(|p| p.get("allow")),
            );
        }
    }
    counts
}

/// The `rustfmt`-style `format!("...")` template that opens with `prefix`
/// inside `source`, with each `{...}` placeholder replaced in order by the
/// matching entry of `fill`.
fn format_template(source: &str, prefix: &str, fill: &[&str]) -> String {
    let (template, _) = string_literals(source)
        .into_iter()
        .find(|(literal, _)| literal.starts_with(prefix))
        .unwrap_or_else(|| panic!("the source formats a string starting {prefix:?}"));
    let mut out = String::new();
    let mut fills = fill.iter();
    let mut rest = template.as_str();
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = rest[open..].find('}').expect("a closed placeholder") + open;
        out.push_str(fills.next().expect("a fill for every placeholder"));
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    assert!(fills.next().is_none(), "every fill used");
    out
}

/// Every directory under `assets/functions/kits/` and `assets/agents/*/kits/`,
/// spelled the way the page spells a config-dir path, in page order
/// (functions first, then agents alphabetically).
fn shipped_kit_paths() -> Vec<String> {
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    let functions = asset(&["functions", "kits"]);
    for entry in fs::read_dir(&functions).expect("assets/functions/kits exists") {
        let path = entry.unwrap().path();
        if path.is_dir() {
            let handle = path.file_name().unwrap().to_string_lossy().into_owned();
            dirs.push((format!("functions/kits/{handle}"), path));
        }
    }
    let mut agents: Vec<PathBuf> = fs::read_dir(asset(&["agents"]))
        .expect("assets/agents exists")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("kits").is_dir())
        .collect();
    agents.sort();
    for agent in agents {
        let name = agent.file_name().unwrap().to_string_lossy().into_owned();
        let mut kits: Vec<PathBuf> = fs::read_dir(agent.join("kits"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        kits.sort();
        for kit in kits {
            let handle = kit.file_name().unwrap().to_string_lossy().into_owned();
            dirs.push((format!("agents/{name}/kits/{handle}"), kit));
        }
    }
    for (label, dir) in &dirs {
        let handle = dir.file_name().unwrap().to_string_lossy();
        assert!(
            dir.join(format!("{handle}.yaml")).is_file(),
            "{label} holds {handle}.yaml"
        );
    }
    dirs.into_iter()
        .map(|(label, _)| format!("`<config-dir>/{label}/`"))
        .collect()
}

#[test]
fn usage_probe_base_kit_section_describes_the_shipped_workload_kit() {
    let base = const_str(&src(&["config", "mod.rs"]), "BASE_KIT_HANDLE");
    let dockerfile = read(asset(&["sbx-kit", &base, &format!("{base}.dockerfile")]));
    let env_line = dockerfile
        .lines()
        .find(|line| line.starts_with("ENV "))
        .expect("the dockerfile sets its environment on one ENV line");
    let env: Vec<&str> = env_line["ENV ".len()..].split_whitespace().collect();
    let image = dockerfile
        .lines()
        .find_map(|line| line.strip_prefix("FROM "))
        .expect("the dockerfile builds on an image");
    let (repo, tag) = image.split_once(':').expect("the base image is tagged");
    assert!(
        tag.starts_with('v'),
        "the tag is a release version: {image}"
    );

    let kit = src(&["sandbox", "kit.rs"]);
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&read(asset(&["sbx-kit", &base, &format!("{base}.yaml")])))
            .expect("the base kit yaml parses");
    let caps = doc["capabilities"].as_sequence().expect("capabilities");
    let of = |name: &str| -> Vec<&serde_yaml::Value> {
        let wanted = const_str(&kit, name);
        caps.iter()
            .filter(|cap| cap["type"].as_str() == Some(wanted.as_str()))
            .collect()
    };
    let services: Vec<String> = of("CAP_CREDENTIAL")
        .iter()
        .map(|cap| cap["config"]["service"].as_str().unwrap().to_string())
        .collect();
    let lifecycle = of("CAP_LIFECYCLE");
    assert_eq!(lifecycle.len(), 1, "one lifecycle capability");
    let lifecycle = &lifecycle[0]["config"];
    let install = lifecycle["install"].as_sequence().expect("install hooks");
    let files = lifecycle["files"].as_sequence().expect("lifecycle files");
    assert!(
        lifecycle.get("startup").is_none(),
        "the base kit has no startup hook"
    );
    assert_eq!((install.len(), files.len()), (1, 1));
    let file_path = files[0]["path"].as_str().unwrap();
    let file_mode = files[0]["mode"].as_str().unwrap();
    let context_file = of("CAP_AGENT_CONTEXT")[0]["config"]["filename"]
        .as_str()
        .unwrap()
        .to_string();

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let base_section = flat(&section(&page, "## The base kit"));
    assert!(
        base_section.contains(&format!("`FROM {repo}:v<version>`")),
        "the base kit section names the release image {repo}"
    );
    // Every variable the dockerfile sets is quoted in full by the base kit
    // section and named by the override rule, so a fork learns the whole set.
    assert!(!env.is_empty(), "the ENV line sets at least one variable");
    let override_section = flat(&section(&page, "## Custom Kit Override"));
    for assignment in &env {
        assert!(
            base_section.contains(&format!("`{assignment}`")),
            "the base kit section quotes `{assignment}`"
        );
        let (var, _) = assignment.split_once('=').expect("KEY=value");
        assert!(
            override_section.contains(&format!("`{var}`"))
                || override_section.contains(&format!("`{assignment}`")),
            "the override rule names `{var}` among the variables a fork keeps"
        );
    }
    assert!(
        base_section.contains(&format!(
            "- {} optional proxy-managed credentials",
            services.len()
        )),
        "the base kit section counts {} credentials",
        services.len()
    );
    for service in &services {
        assert!(
            base_section.contains(&format!("`{service}`")),
            "the base kit section lists the `{service}` provider"
        );
    }
    assert!(
        base_section.contains(&format!("`{file_path}` (mode `{file_mode}`)")),
        "the base kit section names the lifecycle file and its mode"
    );
    assert!(
        base_section.contains("one lifecycle install hook")
            && base_section.contains("no startup hook"),
        "the base kit section says one install hook and no startup hook"
    );
    assert!(
        base_section.contains(&format!("`{context_file}`")),
        "the base kit section names the agent-context file"
    );
    // The same env fact is what Nesting and the override rule rest on.
    let is_sandbox = env.iter().find(|e| e.starts_with("IS_SANDBOX=")).unwrap();
    for heading in ["## Nesting", "## Custom Kit Override"] {
        assert!(
            section(&page, heading).contains(&format!("`{is_sandbox}`")),
            "{heading} quotes `{is_sandbox}` as coming from the kit dockerfile"
        );
    }
}

#[test]
fn usage_probe_built_in_kits_listed_are_exactly_the_shipped_ones() {
    let shipped = shipped_kit_paths();
    assert_eq!(shipped.len(), 3, "{shipped:?}");

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let built_in = section(&page, "### Built-in kits Coyote ships");
    let mut listed: Vec<String> = built_in
        .lines()
        .filter_map(|line| line.strip_prefix("- `<config-dir>/"))
        .map(|rest| format!("`<config-dir>/{}", &rest[..rest.find('`').unwrap() + 1]))
        .collect();
    listed.sort();
    let mut shipped = shipped;
    shipped.sort();
    assert_eq!(listed, shipped, "the bullet list names every shipped kit");
    assert!(
        built_in.contains("Coyote ships three mixin kits"),
        "the count word matches the shipped set"
    );
}

#[test]
fn usage_probe_launch_steps_follow_the_order_of_launch() {
    let source = src(&["sandbox", "mod.rs"]);
    let launch = fn_body(&source, "pub fn launch(");
    let at = |needle: &str| {
        launch
            .find(needle)
            .unwrap_or_else(|| panic!("launch calls {needle}"))
    };
    let order = [
        "ensure_sbx_installed()",
        "resolve_kit_path()",
        "plan_mcp_mixin()",
        "sandbox_exists(",
        "check_composition(",
        "Vault::init(",
        "inject_llm_secret(",
        "log_discovery(",
        "create_sandbox(",
        "copy_host_files(",
        "warn_if_mixin_drifted(",
        "exec_run(",
    ];
    for pair in order.windows(2) {
        assert!(
            at(pair[0]) < at(pair[1]),
            "{} runs before {}",
            pair[0],
            pair[1]
        );
    }
    let config = src(&["config", "mod.rs"]);
    let cache_base = format!(
        "<cache>/{}/{}/",
        const_str(&config, "SBX_KITS_DIR_NAME"),
        const_str(&config, "BASE_KIT_HANDLE")
    );
    let info_row = "sbx_kits_dir";
    assert!(
        src(&["config", "request_context.rs"]).contains(&format!(
            "(\"{info_row}\", display_path(&paths::sbx_kits_dir()))"
        )),
        "`--info` reports the cache root as {info_row}"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let steps = section(&page, "## What `--sandbox` actually does");
    let step_at = |needle: &str| {
        steps
            .find(needle)
            .unwrap_or_else(|| panic!("the steps block mentions {needle:?}"))
    };
    let page_order = [
        "sbx version",
        &cache_base,
        "sbx ls",
        "composition checks",
        "before any secret is registered",
        "sbx secret set ",
        "sbx create --name",
        "sbx cp ",
        "mcp.json changed",
        "sbx run --name",
    ];
    for pair in page_order.windows(2) {
        assert!(
            step_at(pair[0]) < step_at(pair[1]),
            "{:?} is described before {:?}",
            pair[0],
            pair[1]
        );
    }
    assert!(
        steps.contains(&format!(
            "`coyote --info` prints the cache root as {info_row}"
        )),
        "the steps block names the --info row"
    );
}

#[test]
fn usage_probe_discovery_section_quotes_the_refusal_and_log_sentences() {
    let mixins = src(&["sandbox", "mixins.rs"]);
    let discover = fn_body(&mixins, "pub(crate) fn discover_from(");
    let twice = format_template(&discover, "sbx kit handle '", &["<h>", "<a>", "<b>"]);
    let header = format_template(
        &fn_body(&mixins, "fn discovery_header("),
        "Applying ",
        &["2"],
    );
    let none = string_literals(&fn_body(&mixins, "pub(crate) fn log_discovery("))
        .into_iter()
        .map(|(literal, _)| literal)
        .find(|literal| literal.starts_with("No sbx kits"))
        .expect("log_discovery names the empty case");
    // The two example lines show the built-in-tools and sisyphus-ddg kits as
    // shipped, so their counts come from those yamls the way `summary()`
    // derives them: lifecycle install entries, runtime and install allow
    // entries.
    let kit_src = src(&["sandbox", "kit.rs"]);
    let example_lines: Vec<String> = [
        (
            "functions",
            asset(&["functions", "kits", "built-in-tools", "built-in-tools.yaml"]),
        ),
        (
            "agents/sisyphus",
            asset(&[
                "agents",
                "sisyphus",
                "kits",
                "sisyphus-ddg",
                "sisyphus-ddg.yaml",
            ]),
        ),
    ]
    .into_iter()
    .map(|(label, yaml)| {
        let (hooks, runtime, install) = kit_counts(&kit_src, &read(yaml));
        format_template(
            &fn_body(&mixins, "fn discovery_line("),
            "  {} (adds:",
            &[
                label,
                &hooks.to_string(),
                &runtime.to_string(),
                &install.to_string(),
            ],
        )
    })
    .collect();
    assert!(
        fn_body(&mixins, "fn subdirs_of(").contains("sort_by_key(|e| e.file_name())"),
        "kit dirs under one root apply in name order"
    );
    let roots = fn_body(&mixins, "fn roots(");
    assert_eq!(
        roots.matches("for agent in &agents").count(),
        2,
        "agent kits/ roots are pushed in one pass and agent hooks/kits/ roots in a second"
    );
    let kit = src(&["sandbox", "kit.rs"]);
    let check_mixin = fn_body(&kit, "pub(crate) fn check_mixin(");
    for needle in [
        "'interactive', which a mixin kit may not",
        "'filename' without 'directory'",
    ] {
        assert!(check_mixin.contains(needle), "check_mixin refuses {needle}");
    }
    let sbx_cap = const_str(&kit, "CAP_SBX");
    let v2_start = kit
        .find("const V2_KEYS:")
        .expect("kit.rs lists the v2 keys");
    let v2_keys: Vec<String> = string_literals(
        &kit[v2_start..v2_start + kit[v2_start..].find("];").expect("a closed list")],
    )
    .into_iter()
    .map(|(literal, _)| literal)
    .collect();
    assert!(v2_keys.len() >= 5, "{v2_keys:?}");

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let discovery = flat(&section(
        &page,
        "## Extending the Sandbox: Auto-Discovered Mixins",
    ));
    assert!(
        discovery.contains(&format!("`{twice}`")),
        "the rules quote: {twice}"
    );
    assert!(
        discovery.contains("Within one root, kit directories apply in name order")
            && discovery.contains("All agent `kits/` roots come before all agent `hooks/kits/`"),
        "the ordering prose matches roots()/subdirs_of()"
    );
    let verbose = section(&page, "### Verbose mixin log");
    assert!(
        verbose.lines().any(|l| l == header),
        "the log example opens with {header}"
    );
    for line_shape in &example_lines {
        assert!(
            verbose.lines().any(|l| l == line_shape),
            "the log example shows {line_shape}"
        );
    }
    assert!(
        verbose.contains(&format!("`{none}`")),
        "the empty case `{none}` is quoted"
    );
    let declare = flat(&section(&page, "### What a mixin can declare"));
    assert!(
        declare.contains(&format!(
            "`{sbx_cap}` belongs to the base kit and is refused in a mixin"
        )),
        "the capability section refuses {sbx_cap} for mixins"
    );
    assert!(declare.contains("A mixin may not set `interactive`"));
    assert!(declare.contains("`filename` is allowed only together with `directory`"));
    let layout = flat(&section(&page, "### Kit layout"));
    assert!(layout.contains("A mixin may not carry a `<handle>.dockerfile`"));
    let refused_keys = layout
        .split("from the earlier kit grammar (")
        .nth(1)
        .expect("the layout section names refused top-level keys");
    let refused_keys = &refused_keys[..refused_keys.find(')').unwrap()];
    let examples: Vec<&str> = refused_keys
        .split(',')
        .map(str::trim)
        .filter(|k| k.starts_with('`'))
        .map(|k| k.trim_matches('`'))
        .collect();
    assert!(!examples.is_empty());
    for key in examples {
        assert!(
            v2_keys.iter().any(|k| k == key),
            "`{key}` is one of the keys kit::load refuses by name"
        );
    }
}

#[test]
fn usage_probe_page_voice_has_no_upgrade_story_and_stays_ascii() {
    let Some(wiki) = wiki_dir() else { return };
    for file in [PAGE, SIDEBAR] {
        let text = read(wiki.join(file));
        if let Some(bad) = text.lines().find(|l| !l.is_ascii()) {
            panic!("{file} has a non-ASCII line: {bad:?}");
        }
        for needle in ["pre-v3", "Upgrading from", "no longer", "previously"] {
            let hit = text.lines().find(|line| line.contains(needle));
            assert!(hit.is_none(), "{file} narrates a release: {hit:?}");
        }
    }
    let page = read(wiki.join(PAGE));
    assert!(
        !headings(&page)
            .iter()
            .any(|h| h.to_lowercase().contains("upgrad")),
        "{PAGE} has no upgrade subsection: {:?}",
        headings(&page)
    );
}

/// Every `KEY=value` pair on the base kit dockerfile's one `ENV` line.
fn base_kit_env() -> (String, Vec<(String, String)>) {
    let base = const_str(&src(&["config", "mod.rs"]), "BASE_KIT_HANDLE");
    let dockerfile = read(asset(&["sbx-kit", &base, &format!("{base}.dockerfile")]));
    let env = dockerfile
        .lines()
        .find_map(|line| line.strip_prefix("ENV "))
        .expect("the dockerfile sets its environment on one ENV line")
        .split_whitespace()
        .map(|pair| {
            let (key, value) = pair.split_once('=').expect("KEY=value");
            (key.to_string(), value.to_string())
        })
        .collect();
    (dockerfile, env)
}

/// The `apiKey.name` of every credential the base kit declares.
fn base_kit_credential_vars() -> Vec<String> {
    let base = const_str(&src(&["config", "mod.rs"]), "BASE_KIT_HANDLE");
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&read(asset(&["sbx-kit", &base, &format!("{base}.yaml")])))
            .expect("the base kit yaml parses");
    let credential = const_str(&src(&["sandbox", "kit.rs"]), "CAP_CREDENTIAL");
    doc["capabilities"]
        .as_sequence()
        .expect("capabilities")
        .iter()
        .filter(|cap| cap["type"].as_str() == Some(credential.as_str()))
        .map(|cap| {
            cap["config"]["apiKey"]["name"]
                .as_str()
                .expect("a credential names its env var")
                .to_string()
        })
        .collect()
}

/// The base kit sets `GOOGLE_API_KEY=proxy-managed` for sandbox tools that read
/// Google's conventional variable; Coyote's own Gemini client reads
/// `GEMINI_API_KEY`, the variable the kit's `gemini` credential binds
/// (`{NAME}_API_KEY` per `src/client/macros.rs`). The base kit section must
/// attribute each sentinel variable to its real reader: a sentinel variable
/// that no credential binds may not be presented as the Coyote client's own,
/// and the sentence that quotes it names the credential variable instead.
#[test]
fn usage_probe_base_kit_sentinel_vars_name_their_real_reader() {
    let (dockerfile, env) = base_kit_env();
    let bound = base_kit_credential_vars();
    let macros = src(&["client", "macros.rs"]);
    assert!(
        macros.contains("format!(\"{}_API_KEY\", Self::NAME).to_ascii_uppercase()"),
        "a client's API key env var is <NAME>_API_KEY"
    );
    let clients = src(&["client", "mod.rs"]);
    let is_client = |provider: &str| clients.contains(&format!(", \"{provider}\", "));
    // Code side: every sentinel the dockerfile sets but no credential binds is
    // explained by a comment that names the variable Coyote actually reads,
    // and that variable is a bound credential of a registered client.
    let sentinel = env
        .iter()
        .find(|(_, value)| value == "proxy-managed")
        .map(|(_, value)| value.clone())
        .expect("at least one proxy-managed sentinel on the ENV line");
    let unbound: Vec<&String> = env
        .iter()
        .filter(|(key, value)| *value == sentinel && !bound.contains(key))
        .map(|(key, _)| key)
        .collect();
    assert!(
        !unbound.is_empty(),
        "the dockerfile sets a tool-facing sentinel"
    );
    let mut coyote_vars = Vec::new();
    for key in &unbound {
        let comment = dockerfile
            .lines()
            .find(|line| line.starts_with('#') && line.contains(key.as_str()))
            .unwrap_or_else(|| panic!("the dockerfile explains {key} in a comment"));
        let coyote_var = comment
            .split(|c: char| !(c.is_ascii_uppercase() || c == '_'))
            .rfind(|word| word.ends_with("_API_KEY") && word != key)
            .unwrap_or_else(|| panic!("the comment on {key} names the variable Coyote reads"));
        assert!(
            bound.contains(&coyote_var.to_string()),
            "{coyote_var} is bound by a base kit credential"
        );
        let provider = coyote_var.trim_end_matches("_API_KEY").to_ascii_lowercase();
        assert!(is_client(&provider), "{provider} is a registered client");
        coyote_vars.push((key.to_string(), coyote_var.to_string()));
    }

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let base_section = flat(&section(&page, "## The base kit"));
    for (tool_var, coyote_var) in &coyote_vars {
        let quoted = format!("`{tool_var}={sentinel}`");
        let at = base_section
            .find(&quoted)
            .unwrap_or_else(|| panic!("the base kit section quotes {quoted}"));
        let rest = &base_section[at + quoted.len()..];
        let sentence = &rest[..rest.find(". ").unwrap_or(rest.len())];
        assert!(
            sentence.contains(&format!("`{coyote_var}`")),
            "the sentence on {tool_var} names `{coyote_var}` as the variable Coyote reads, \
             not {tool_var}; found: {sentence:?}"
        );
    }
    // The provider list form quotes a bound credential variable, never the
    // tool-facing sentinel.
    let example = base_section
        .split("each provider's env var (`")
        .nth(1)
        .and_then(|rest| rest.split('`').next())
        .expect("the base kit section gives an example provider variable");
    assert!(
        bound.iter().any(|var| var == example),
        "`{example}` is a credential the kit binds"
    );
}

/// The capability table of "What a mixin can declare" lists exactly the
/// capabilities `kit.rs` knows, minus the one `check_mixin` refuses, each on
/// one row, and the lead-in counts them.
#[test]
fn usage_probe_capability_table_covers_every_mixin_capability_once() {
    let kit = src(&["sandbox", "kit.rs"]);
    let refused = const_str(&kit, "CAP_SBX");
    let known: Vec<String> = kit
        .lines()
        .filter_map(|line| {
            line.split_once("const CAP_")?
                .1
                .split_once(':')
                .map(|(n, _)| n)
        })
        .map(|name| const_str(&kit, &format!("CAP_{name}")))
        .collect();
    let available: Vec<&String> = known.iter().filter(|cap| **cap != refused).collect();
    assert!(available.len() >= 3, "{known:?}");
    let base = const_str(&src(&["config", "mod.rs"]), "BASE_KIT_HANDLE");
    assert!(
        read(asset(&["sbx-kit", &base, &format!("{base}.yaml")])).contains(&refused),
        "the refused capability is the one the base kit itself declares"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let declare = section(&page, "### What a mixin can declare");
    let rows: Vec<String> = first_cells(&declare)
        .into_iter()
        .skip(2) // header + separator
        .map(|cell| cell.trim_matches('`').to_string())
        .collect();
    let mut expected: Vec<String> = available.iter().map(|c| c.to_string()).collect();
    let mut found = rows.clone();
    expected.sort();
    found.sort();
    assert_eq!(
        found, expected,
        "one table row per mixin capability, none for {refused}"
    );
    let words = ["One", "Two", "Three", "Four", "Five", "Six"];
    let count = words[available.len() - 1];
    assert!(
        flat(&declare).contains(&format!("{count} are available to it")),
        "the lead-in counts {count} capabilities"
    );
}

/// Every `](Page#anchor)` link the page makes into ANOTHER wiki page, as
/// `(page, anchor)`, plus every bare `](Page)` link as `(page, "")`. Links
/// into the page itself (`](#...)`) are covered by
/// `inbound_anchors_resolve_to_page_headings`; external `http` links are not
/// wiki pages.
fn cross_page_links(page: &str) -> Vec<(String, String)> {
    page.match_indices("](")
        .filter_map(|(at, _)| {
            let rest = &page[at + 2..];
            let target = &rest[..rest.find(')')?];
            if target.starts_with('#') || target.contains("://") || target.is_empty() {
                return None;
            }
            let (name, anchor) = target.split_once('#').unwrap_or((target, ""));
            Some((name.to_string(), anchor.to_string()))
        })
        .collect()
}

/// A first-time reader follows the page's cross-references: every wiki page
/// it links to exists, and every `Page#anchor` it uses matches a heading of
/// that page under the same GitHub anchor rule the inbound sweep applies.
#[test]
fn usage_probe_outbound_links_resolve_on_their_target_pages() {
    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let links = cross_page_links(&page);
    assert!(
        links.iter().any(|(_, anchor)| !anchor.is_empty()),
        "{PAGE} links into other pages with anchors"
    );
    for (name, anchor) in links {
        let target = wiki.join(format!("{name}.md"));
        assert!(
            target.is_file(),
            "{PAGE} links to {name}, which is not a wiki page"
        );
        if anchor.is_empty() {
            continue;
        }
        let slugs: Vec<String> = headings(&read(&target)).iter().map(|h| slug(h)).collect();
        assert!(
            slugs.contains(&anchor),
            "{PAGE} links to {name}#{anchor}, which matches no heading of {name}.md: {slugs:?}"
        );
    }
}

/// The Requirements section promises two things about the version gate that
/// the refusal-sentence pin does not cover: an unreadable `sbx version`
/// (unparseable output, a non-zero exit, a spawn failure) is let through
/// with a warning instead of a refusal, and the gate runs before anything
/// else the launch does. Both are read from `src/sandbox/mod.rs`.
#[test]
fn usage_probe_requirements_describe_the_lenient_version_gate() {
    let source = src(&["sandbox", "mod.rs"]);
    let check = fn_body(&source, "fn check_sbx_version(");
    let installed = fn_body(&source, "fn ensure_sbx_installed(");
    let lenient_arms = [&check, &installed]
        .iter()
        .flat_map(|body| string_literals(body))
        .filter(|(literal, _)| literal.ends_with("; proceeding"))
        .count();
    assert_eq!(
        lenient_arms, 3,
        "unparseable output, a failed exit and a spawn failure each warn and proceed"
    );
    assert!(
        check.contains("warn!") && !check.contains("bail!(\"Could not"),
        "an unreadable version is a warning, not a refusal:\n{check}"
    );
    let launch = fn_body(&source, "pub fn launch(");
    let gate_at = launch
        .find("ensure_sbx_installed()")
        .expect("launch runs the version gate");
    let first_sbx_call = launch
        .find("sandbox_exists(")
        .expect("launch asks sbx whether the sandbox exists");
    assert!(
        gate_at < first_sbx_call,
        "the version gate precedes every other sbx call in launch"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let requirements = flat(&section(&page, "## Requirements"));
    assert!(
        requirements.contains("Coyote runs `sbx version` before anything else"),
        "Requirements places the gate first"
    );
    assert!(
        requirements.contains("whose output Coyote cannot read is let through with a warning"),
        "Requirements states the lenient path for an unreadable version"
    );
    assert!(
        !requirements.contains("cannot read is refused"),
        "the lenient path is not described as a refusal"
    );
}

/// The three bullets under "Built-in kits Coyote ships" describe what each
/// kit grants. Read the shipped yamls and hold the prose to them: the
/// capability kinds a bullet implies are declared, the services it names
/// resolve to allow entries, and (for the tools kit) every allow entry is
/// accounted for by a named service so a new domain cannot slip in unlisted.
#[test]
fn usage_probe_built_in_kit_bullets_describe_the_kits_they_name() {
    let kit_src = src(&["sandbox", "kit.rs"]);
    let network = const_str(&kit_src, "CAP_NETWORK_POLICY");
    let lifecycle = const_str(&kit_src, "CAP_LIFECYCLE");
    let context = const_str(&kit_src, "CAP_AGENT_CONTEXT");
    let runtime = const_str(&kit_src, "PHASE_RUNTIME");
    let install = const_str(&kit_src, "PHASE_INSTALL");
    let load = |parts: &[&str]| -> serde_yaml::Value {
        serde_yaml::from_str(&read(asset(parts))).expect("the kit yaml parses")
    };
    let caps = |doc: &serde_yaml::Value, kind: &str| -> Vec<serde_yaml::Value> {
        doc["capabilities"]
            .as_sequence()
            .expect("capabilities")
            .iter()
            .filter(|cap| cap["type"].as_str() == Some(kind))
            .cloned()
            .collect()
    };
    let allow = |doc: &serde_yaml::Value, phase: &str| -> Vec<String> {
        caps(doc, &network)
            .iter()
            .filter_map(|cap| cap["config"][phase]["allow"].as_sequence().cloned())
            .flatten()
            .map(|host| host.as_str().expect("a host string").to_lowercase())
            .collect()
    };
    let hooks = |doc: &serde_yaml::Value| -> String {
        caps(doc, &lifecycle)
            .iter()
            .filter_map(|cap| cap["config"]["install"].as_sequence().cloned())
            .flatten()
            .filter_map(|hook| hook["command"].as_str().map(str::to_lowercase))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let tools = load(&["functions", "kits", "built-in-tools", "built-in-tools.yaml"]);
    let ddg = load(&[
        "agents",
        "sisyphus",
        "kits",
        "sisyphus-ddg",
        "sisyphus-ddg.yaml",
    ]);
    let probe = load(&["agents", "probe", "kits", "agent-probe", "agent-probe.yaml"]);

    // Code-side shape the bullets lean on, checked without the wiki too.
    assert!(caps(&tools, &lifecycle).is_empty() && caps(&tools, &context).is_empty());
    assert!(
        !caps(&ddg, &context).is_empty(),
        "sisyphus-ddg carries agent context"
    );
    assert!(
        allow(&ddg, &runtime)
            .iter()
            .any(|h| h.contains("duckduckgo"))
    );
    let probe_hooks = hooks(&probe);
    assert!(probe_hooks.contains("hurl") && probe_hooks.contains("grpcurl"));
    assert!(allow(&probe, &install).iter().any(|h| h.contains("github")));
    assert!(allow(&probe, &install).iter().any(|h| h.contains("ubuntu")));

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let built_in = section(&page, "### Built-in kits Coyote ships");
    let bullet = |dir: &str| -> String {
        flat(
            built_in
                .split("\n- ")
                .find(|b| b.contains(dir))
                .unwrap_or_else(|| panic!("a bullet for {dir}")),
        )
        .to_lowercase()
    };

    // built-in-tools: the prose names services; each resolves to a runtime
    // allow entry, and each allow entry is one of the named services. The
    // few hosts spelled differently from their service are mapped by hand.
    let tools_bullet = bullet("functions/kits/built-in-tools/");
    let named = [
        ("wikipedia", "wikipedia"),
        ("arxiv", "arxiv"),
        ("jina", "jina"),
        ("wttr", "wttr"),
        ("wolframalpha", "wolframalpha"),
        ("perplexity", "perplexity"),
        ("tavily", "tavily"),
        ("twilio", "twilio"),
        ("github mcp", "githubcopilot"),
        ("atlassian mcp", "atlassian"),
        ("ddg-search mcp", "duckduckgo"),
        ("npm registry", "npmjs"),
        ("docker registries", "docker.io"),
    ];
    let tools_hosts = allow(&tools, &runtime);
    for (word, host_part) in named {
        assert!(tools_bullet.contains(word), "the tools bullet names {word}");
        assert!(
            tools_hosts.iter().any(|h| h.contains(host_part)),
            "the tools kit allows a host for {word}: {tools_hosts:?}"
        );
    }
    let also_covered = [("doi.org", "arxiv"), ("ghcr.io", "docker registries")];
    for host in &tools_hosts {
        let covered = named.iter().any(|(_, part)| host.contains(part))
            || also_covered.iter().any(|(h, _)| host.starts_with(h));
        assert!(
            covered,
            "allow entry {host} is behind no service the tools bullet names"
        );
    }
    assert!(
        allow(&tools, &install).is_empty() && tools_bullet.contains("allows the domains"),
        "the tools kit grants egress only"
    );

    // sisyphus-ddg: DuckDuckGo egress plus instructions for the agent.
    let ddg_bullet = bullet("agents/sisyphus/kits/sisyphus-ddg/");
    assert!(ddg_bullet.contains("duckduckgo") && ddg_bullet.contains("tells the agent"));

    // agent-probe: install hooks for the two named tools, with the hosts
    // they download from in the install allow list.
    let probe_bullet = bullet("agents/probe/kits/agent-probe/");
    for word in [
        "hurl",
        "grpcurl",
        "install hooks",
        "github release",
        "ubuntu mirrors",
        "install allow list",
    ] {
        assert!(probe_bullet.contains(word), "the probe bullet says {word}");
    }
    assert!(
        caps(&probe, &context).is_empty() && !probe_bullet.contains("tells the agent"),
        "the probe kit carries no agent context and the bullet claims none"
    );
}

/// The "Common failures" bullets promise that a malformed kit, a reserved or
/// repeated handle and a colliding credential service all stop the launch
/// before `sbx create`, and they list the causes. Hold every listed cause to a
/// refusal arm in the kit loader, the walk or the composition check, hold
/// every refusal arm of those three to a cause on the page, and hold the
/// "before `sbx create`" claim to the order of `launch`.
#[test]
fn usage_probe_common_failures_name_real_refusals_that_precede_sbx_create() {
    let kit_src = src(&["sandbox", "kit.rs"]);
    let mixins_src = src(&["sandbox", "mixins.rs"]);
    let preflight_src = src(&["sandbox", "preflight.rs"]);
    let mod_src = src(&["sandbox", "mod.rs"]);

    // Every bail arm the loader, the mixin check, the walk and the
    // composition check carry, keyed by the fragment that names its cause.
    let bails = |source: &str, head: &str| -> Vec<String> {
        let body = fn_body(source, head);
        body.match_indices("bail!(")
            .map(|(at, _)| {
                let rest = &body[at..];
                string_literals(rest)
                    .into_iter()
                    .next()
                    .map(|(literal, _)| literal)
                    .expect("every bail! carries a format string")
            })
            .collect()
    };
    let load_arms = bails(&kit_src, "pub(crate) fn load(");
    let mixin_arms = bails(&kit_src, "pub(crate) fn check_mixin(");
    let walk_arms = bails(&mixins_src, "pub(crate) fn discover_from(");
    let composition_arms = bails(&preflight_src, "fn check_credential_phases(");
    let schema = const_str(&kit_src, "SCHEMA_VERSION");

    // (page cause fragment, code fragment of the arm that raises it)
    let causes: [(&str, &str, &Vec<String>); 7] = [
        (
            "missing or misnamed `<handle>.yaml`",
            "must hold exactly one",
            &load_arms,
        ),
        ("a wrong first line", "first line must be", &load_arms),
        (
            "a `schemaVersion` other than",
            "schemaVersion must be the string",
            &load_arms,
        ),
        (
            "a top-level key from the earlier kit grammar",
            "is not valid in a v3 kit",
            &load_arms,
        ),
        (
            "a dockerfile beside a mixin",
            "which a mixin kit may not",
            &mixin_arms,
        ),
        (
            "a reserved or repeated handle",
            "uses the reserved handle",
            &walk_arms,
        ),
        (
            "an inject domain missing from the allow list",
            "is missing from the",
            &mixin_arms,
        ),
    ];
    for (cause, fragment, arms) in &causes {
        assert!(
            arms.iter().any(|arm| arm.contains(fragment)),
            "the loader refuses `{cause}` with an arm saying `{fragment}`:\n{arms:#?}"
        );
    }
    assert!(
        walk_arms
            .iter()
            .any(|arm| arm.contains("is declared twice")),
        "a repeated handle is its own refusal in the walk"
    );
    assert!(
        composition_arms
            .iter()
            .any(|arm| arm.starts_with("kit dir '{}' declares credential service")),
        "the composition check refuses a credential service declared twice"
    );
    let remedy = "(or its env placeholder)";
    assert!(
        fn_body(&preflight_src, "fn check_credential_phases(").contains(remedy),
        "the credential-collision message tells the user to rename the vault secret"
    );

    // "before `sbx create` runs": the walk and the composition check both sit
    // ahead of the create call inside launch.
    let launch = fn_body(&mod_src, "pub fn launch(");
    let at = |needle: &str| {
        launch
            .find(needle)
            .unwrap_or_else(|| panic!("launch calls {needle}"))
    };
    assert!(
        at("check_composition(") < at("create_sandbox("),
        "the composition check precedes sbx create in launch"
    );
    assert!(
        fn_body(&mod_src, "fn check_composition(").contains("mixins::discover()"),
        "check_composition is where the kit walk runs, so the walk precedes sbx create too"
    );

    let Some(wiki) = wiki_dir() else { return };
    let page = read(wiki.join(PAGE));
    let failures = flat(&section(&page, "### Common failures"));

    // The too-old bullet quotes the gate's sentence shape and sends the
    // reader to Requirements, which the inbound-anchor test resolves.
    let refusal = refusal_sentence();
    assert!(
        refusal.starts_with("sbx v") && refusal.contains(" is too old"),
        "the version gate's sentence opens `sbx v<found> is too old`: {refusal}"
    );
    assert!(
        failures.contains("- **`sbx v... is too old`.**") && failures.contains("](#requirements)"),
        "the too-old bullet quotes the gate and links Requirements"
    );

    // The malformed-kit bullet names every refusal class and claims the
    // message names the kit dir, which every arm above spells as `kit dir`.
    let malformed = failures
        .split("- **")
        .find(|bullet| bullet.starts_with("A malformed kit aborts launch before `sbx create` runs"))
        .expect("a malformed-kit bullet");
    assert!(
        malformed.contains("a message naming the kit dir"),
        "the bullet says the message names the kit dir"
    );
    for (cause, _, _) in &causes {
        assert!(
            malformed.contains(cause),
            "the malformed-kit bullet names `{cause}`"
        );
    }
    assert!(
        malformed.contains(&format!("other than `\"{schema}\"`")),
        "the schemaVersion cause spells the accepted string"
    );
    for arm in load_arms.iter().chain(&mixin_arms).chain(&walk_arms) {
        assert!(
            arm.contains("kit dir") || arm.contains("kit file") || arm.starts_with("sbx kit"),
            "every loader/walk refusal names the kit path: {arm}"
        );
    }
    assert!(
        !malformed.contains("parse error"),
        "the bullet no longer describes the refusal as a parse error"
    );

    // The credential-collision bullet carries the remedy the message gives.
    let collision = failures
        .split("- **")
        .find(|bullet| bullet.starts_with("The launch refuses a credential service declared twice"))
        .expect("a credential-collision bullet");
    assert!(
        collision.contains("Rename the vault secret (or its env placeholder) as the message says"),
        "the collision bullet repeats the message's remedy"
    );
}
