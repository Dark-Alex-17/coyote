//! Opt-in doc lint: checks the wiki pages that document native structured
//! output, `stream_options` and `--json` (Clients.md, Graph-Agents.md,
//! Unattended-Mode.md, _Sidebar.md) against the built binary.
//!
//! The wiki is a separate clone, so these tests run only when `COYOTE_WIKI_DIR`
//! points at it; a relative value is resolved against the crate root, so from
//! a `.worktrees/<name>` checkout the sibling clone is `../../../coyote.wiki`:
//!
//! ```sh
//! COYOTE_WIKI_DIR=../coyote.wiki cargo test --test structured_output_wiki_docs
//! ```
//!
//! Without the variable every test is skipped (printed as `skipping:`), so a
//! plain `cargo test` is unaffected. With it set to a directory that has no
//! `Unattended-Mode.md` the tests fail rather than pass against nothing.
//!
//! The checks read the pages as a consumer would and replay their claims
//! against the binary: the documented conflicting flags, the quoted error
//! sentences, the example object's keys and the exit codes.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const WIKI_DIR: &str = "COYOTE_WIKI_DIR";
const RECIPE: &str =
    "`COYOTE_WIKI_DIR=../coyote.wiki cargo test --test structured_output_wiki_docs`";
const SENTINEL_PAGE: &str = "Unattended-Mode.md";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

/// The text from the heading line that equals `heading` up to the next
/// heading of the same or a higher level.
fn section(page: &str, heading: &str) -> String {
    let level = heading.chars().take_while(|c| *c == '#').count();
    let mut out = String::new();
    let mut inside = false;
    let mut fenced = false;
    for line in page.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        let is_heading = !fenced && hashes > 0 && line[hashes..].starts_with(' ');
        if inside && is_heading && hashes <= level {
            break;
        }
        if line == heading {
            inside = true;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    assert!(!out.is_empty(), "heading {heading:?} not found");
    out
}

/// GitHub's heading anchor: lowercase, keep alphanumerics, `-` and `_`,
/// spaces become `-`, everything else is dropped.
fn anchor(heading_text: &str) -> String {
    heading_text
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

fn anchors_of(page: &str) -> Vec<String> {
    let mut fenced = false;
    let mut out = Vec::new();
    for line in page.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if !fenced && hashes > 0 && line[hashes..].starts_with(' ') {
            out.push(anchor(line[hashes..].trim()));
        }
    }
    out
}

fn code_spans(text: &str) -> Vec<String> {
    text.split('`')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, s)| s.to_string())
        .collect()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "coyote-so-wiki-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("home")).unwrap();
        fs::write(dir.join("vault-pass"), "test-password\n").unwrap();
        // A port nothing listens on: a model call fails fast after the run starts.
        fs::write(
            dir.join("config.yaml"),
            format!(
                "model: stub:stub-model\n\
                 save_session: false\n\
                 save: false\n\
                 memory: false\n\
                 vault_password_file: '{}'\n\
                 clients:\n\
                 \x20 - type: openai-compatible\n\
                 \x20   name: stub\n\
                 \x20   api_key: unused\n\
                 \x20   api_base: http://127.0.0.1:9/v1\n\
                 \x20   models:\n\
                 \x20     - name: stub-model\n\
                 \x20       max_input_tokens: 100000\n",
                dir.join("vault-pass").display()
            ),
        )
        .unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coyote"));
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            command.env_remove(key);
        }
    }
    command
        .args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", dir.join("home"))
        .env("USERPROFILE", dir.join("home"))
        .env("NO_COLOR", "1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("IS_SANDBOX")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Flags that take a value, with a value clap accepts, so the conflict (not
/// a bad value) is what argument parsing reports.
fn value_for(flag: &str) -> Option<&'static str> {
    match flag {
        "--memory-migrate" | "--init-memory" => Some("workspace"),
        "--completions" => Some("bash"),
        "--install-builtins" => Some("agents"),
        _ => None,
    }
}

#[test]
fn new_sections_are_ascii_and_free_of_planning_labels() {
    let Some(wiki) = wiki_dir() else { return };
    let sections = [
        (
            "Clients.md",
            "### Structured output and `supports_structured_output`",
        ),
        (
            "Clients.md",
            "## Token usage on the chat wire (`stream_options`)",
        ),
        ("Graph-Agents.md", "### Native structured output"),
        ("Unattended-Mode.md", "# `--json`"),
    ];
    let banned = [
        "TASK-",               // debris-exempt
        "Phase ",              // debris-exempt
        "phase ",              // debris-exempt
        "the plan",            // debris-exempt
        "acceptance criteria", // debris-exempt
        "gauntlet",            // debris-exempt
    ];
    for (page, heading) in sections {
        let text = section(&read(&wiki.join(page)), heading);
        for (n, line) in text.lines().enumerate() {
            assert!(
                line.is_ascii(),
                "{page} {heading} line {n}: non-ASCII: {line}"
            );
            for word in banned {
                assert!(
                    !line.contains(word),
                    "{page} {heading} line {n}: {word:?}: {line}"
                );
            }
        }
    }
}

#[test]
fn wiki_new_links_resolve_to_headings() {
    let Some(wiki) = wiki_dir() else { return };
    let pages = [
        "Clients.md",
        "Graph-Agents.md",
        "Unattended-Mode.md",
        "_Sidebar.md",
    ];
    let wanted = [
        "Graph-Agents#native-structured-output",
        "Graph-Agents#routing",
        "Clients#structured-output-and-supports_structured_output",
        "Clients#token-usage-on-the-chat-wire-stream_options",
        "Clients#extending-vs-replacing-the-catalog",
        "Unattended-Mode#--json",
        "Unattended-Mode#errors-and-exit-codes",
        "Model-Catalog#quirksyaml-the-hand-owned-overlay",
        "Patches#model-specific-patches",
        "Patches#client-configuration-patches",
    ];
    let all: String = pages.iter().map(|p| read(&wiki.join(p))).collect();
    for link in wanted {
        let (page, frag) = link.split_once('#').unwrap();
        let used = all.contains(&format!("({link})")) || all.contains(&format!("(#{frag})"));
        assert!(used, "the pages no longer link {link}; update this test");
        let anchors = anchors_of(&read(&wiki.join(format!("{page}.md"))));
        assert!(
            anchors.iter().any(|a| a == frag),
            "{link}: no such heading on {page}.md"
        );
    }
}

#[test]
fn wiki_sidebar_lists_json_right_after_headless() {
    let Some(wiki) = wiki_dir() else { return };
    let sidebar = read(&wiki.join("_Sidebar.md"));
    let lines: Vec<&str> = sidebar.lines().map(str::trim).collect();
    let at = lines
        .iter()
        .position(|l| *l == "- [--headless](Unattended-Mode#--headless)")
        .expect("sidebar has the --headless entry");
    assert_eq!(lines[at + 1], "- [--json](Unattended-Mode#--json)");
}

#[test]
fn json_section_every_documented_conflicting_flag_is_rejected_by_argument_parsing() {
    let Some(wiki) = wiki_dir() else { return };
    let page = section(
        &read(&wiki.join("Unattended-Mode.md")),
        "## Prompt and flags",
    );
    let list = &page[page
        .find("Argument parsing rejects these")
        .expect("the conflict list")..];
    let flags: Vec<String> = code_spans(list)
        .iter()
        .flat_map(|span| span.split('/').map(str::to_string).collect::<Vec<_>>())
        .filter(|f| f.starts_with('-'))
        .collect();
    let pinned = [
        "--execute",
        "--code",
        "--dry-run",
        "--macro",
        "--acp-server",
        "--memory-migrate",
        "--rebuild-rag",
        "--info",
        "--list-models",
        "--delete-secret",
    ];
    for flag in pinned {
        assert!(
            flags.iter().any(|f| f == flag),
            "{flag} missing from the conflict list: {flags:?}"
        );
    }
    let dir = TempDir::new("conflicts");
    for flag in &flags {
        let mut args = vec!["--json", flag.as_str()];
        if let Some(v) = value_for(flag) {
            args.push(v);
        }
        args.push("hi");
        let output = run(&dir.0, &args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{flag}: {stderr}");
        assert!(output.stdout.is_empty(), "{flag}: stdout not empty");
        assert!(stderr.contains("cannot be used with"), "{flag}: {stderr}");
    }
}

#[test]
fn json_section_quoted_errors_before_the_run_are_plain_stderr_with_exit_1() {
    let Some(wiki) = wiki_dir() else { return };
    let page = section(
        &read(&wiki.join("Unattended-Mode.md")),
        "## Prompt and flags",
    );
    let dir = TempDir::new("pre-run");

    let missing = "--json requires a prompt (argument, -f, or stdin)";
    assert!(
        page.contains(&format!("`{missing}`")),
        "page no longer quotes {missing:?}"
    );
    let output = run(&dir.0, &["--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains(missing));

    assert!(page.contains("`Skill '<name>' is not installed`"));
    let output = run(&dir.0, &["--json", "--skill", "no-such-skill", "hi"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Skill 'no-such-skill' is not installed")
    );
}

#[test]
fn json_section_failed_run_prints_the_documented_object_with_exit_1() {
    let Some(wiki) = wiki_dir() else { return };
    let page = section(&read(&wiki.join("Unattended-Mode.md")), "# `--json`");
    let example = &page[page.find("```json\n").expect("example object") + 8..];
    let example: Value = serde_json::from_str(&example[..example.find("```").unwrap()]).unwrap();
    let dir = TempDir::new("failed");

    // Nothing listens on the client's port: the model call fails after the run starts.
    let output = run(&dir.0, &["--json", "hi"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout.lines().count(),
        1,
        "stdout holds exactly one line: {stdout}"
    );
    let report: Value = serde_json::from_str(stdout.trim()).unwrap();
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        keys(&report),
        keys(&example),
        "every documented key, and only those"
    );
    assert_eq!(keys(&report["usage"]), keys(&example["usage"]));
    assert_eq!(report["is_error"], Value::Bool(true));
    assert_eq!(report["result"], Value::Null);
    assert!(report["error"].is_string());
    assert!(
        !output.stderr.is_empty(),
        "the usual error line goes to stderr"
    );
}

#[test]
fn graph_agents_native_section_pins_the_call_sites_fallback_and_parse_order() {
    let Some(wiki) = wiki_dir() else { return };
    let text = section(
        &read(&wiki.join("Graph-Agents.md")),
        "### Native structured output",
    );
    // Quotes wrap across lines, inside the NOTE block too, so match them on
    // the text with blockquote markers dropped and whitespace collapsed.
    let flat = text
        .lines()
        .map(|line| line.trim_start().trim_start_matches('>'))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");

    for call_site in [
        "`llm` node",
        "the extraction step",
        "[session router](#routing)",
    ] {
        assert!(text.contains(call_site), "call site {call_site:?} missing");
    }
    assert!(text.contains("tools and functions are sent exactly as"));

    let source = read(&repo_root().join("src/graph/structured.rs"));
    let open = "warn!(\"";
    let start = source
        .find(&format!("{open}provider rejected the native"))
        .expect("src/graph/structured.rs no longer has the native-format rejection warn! literal")
        + open.len();
    let warning = &source[start..start + source[start..].find('"').unwrap()];
    let (prefix, suffix) = warning
        .split_once("({reason})")
        .expect("the rejection warning no longer interpolates ({reason})");
    let quoted = format!("{prefix}(<reason>){suffix}");
    assert!(
        flat.contains(&quoted),
        "section does not quote the warning as logged: {quoted:?}"
    );

    assert!(text.contains("once more without the format"));
    assert!(flat.contains("sticks for the rest of that node run"));

    let parsing = &flat[flat
        .find("**Parsing the reply.**")
        .expect("the parsing paragraph")..];
    let positions: Vec<usize> = ["JSON fast path", "extractor", "repair retry"]
        .iter()
        .map(|step| {
            parsing
                .find(step)
                .unwrap_or_else(|| panic!("parsing paragraph lacks {step:?}"))
        })
        .collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "parse chain out of order: {positions:?}"
    );

    assert!(text.contains("declares no"));
    assert!(text.contains("`additionalProperties: false`"));
    assert!(flat.contains("Declare `properties` on every object"));
}
