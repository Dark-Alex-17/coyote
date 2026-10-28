//! Cold-start probes of the injected MEMORY.md section through the real
//! binary. The dry-run model echoes the whole system prompt, so the rendered
//! `<memory>` block is observable on stdout and the once-per-process notice
//! in the log file (never on the terminal). Each test boots a fresh temp config dir holding a config.yaml,
//! a global `memory/MEMORY.md` and a workspace `.coyote/memory/MEMORY.md`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const GLOBAL_INDEX: &str =
    "PLACEHOLDER_CHECK\n- [[g-one]]: global drill one\n- [[g-two]]: global drill two\n";

const WORKSPACE_INDEX: &str = "# Workspace Memory Index\n\n## Always-on facts\n- **Fact A.** alpha\n\
                               - **Fact B.** bravo\n- **Fact C.** charlie\n- **Fact D.** delta\n\n\
                               ## Drill files\n- [[w-one]]: workspace drill one\n- [[w-two]]: workspace drill two\n";

/// Every non-catalogue line of both indexes, global first; 160 chars.
const FACTS: &str = "PLACEHOLDER_CHECK\n# Workspace Memory Index\n\n## Always-on facts\n\
                     - **Fact A.** alpha\n- **Fact B.** bravo\n- **Fact C.** charlie\n\
                     - **Fact D.** delta\n\n## Drill files";

/// Every catalogue line of both indexes, workspace first; 125 chars.
const CATALOGUE: &str = "- [[w-one]]: workspace drill one\n- [[w-two]]: workspace drill two\n\
                         - [[g-one]]: global drill one\n- [[g-two]]: global drill two";

struct Fixture {
    dir: PathBuf,
    workspace: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A config dir with a dry-run model and the two indexes above. `extra`
/// is appended verbatim to config.yaml.
fn fixture(label: &str, extra: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "coyote-memory-section-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    let workspace = dir.join("ws");
    fs::create_dir_all(dir.join("memory")).unwrap();
    fs::create_dir_all(dir.join("macros")).unwrap();
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(workspace.join(".coyote").join("memory")).unwrap();
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    // Paths land in single-quoted YAML scalars so a Windows temp path keeps
    // its backslashes.
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: dryrun\n\
             \x20   auth: none\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: dry-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n\
             stream: false\n\
             {extra}",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
    fs::write(dir.join("memory").join("MEMORY.md"), GLOBAL_INDEX).unwrap();
    fs::write(
        workspace.join(".coyote").join("memory").join("MEMORY.md"),
        WORKSPACE_INDEX,
    )
    .unwrap();
    Fixture { dir, workspace }
}

/// A drill file in `scope_dir` (the global `memory/` dir or the workspace
/// `.coyote/memory/` dir) whose body is `body`.
fn write_drill(scope_dir: &Path, name: &str, body: &str) {
    fs::write(
        scope_dir.join(format!("{name}.md")),
        format!("---\nname: {name}\ndescription: d\ntype: project\n---\n{body}\n"),
    )
    .unwrap();
}

fn run(fx: &Fixture, args: &[&str]) -> Output {
    let home = fx.dir.join("home");
    Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(args)
        .current_dir(&fx.workspace)
        .env("COYOTE_CONFIG_DIR", &fx.dir)
        .env("COYOTE_CACHE_DIR", fx.dir.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .env("COYOTE_LOG_PATH", log_path(fx))
        .env("COYOTE_LOG_LEVEL", "info")
        .env_remove("COYOTE_LOG_FILTER")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n")
}

/// Every `<memory>...</memory>` block the dry-run model echoed, in order.
fn memory_sections(stdout: &str) -> Vec<&str> {
    let mut rest = stdout;
    let mut out = Vec::new();
    while let Some(start) = rest.find("<memory>\n") {
        let after = &rest[start..];
        let end = after.find("</memory>").expect("closing memory tag") + "</memory>".len();
        out.push(&after[..end]);
        rest = &after[end..];
    }
    out
}

fn log_path(fx: &Fixture) -> PathBuf {
    fx.dir.join("coyote.log")
}

/// The memory notices the runs so far wrote to the fixture''s log file, with
/// the log line prefix (timestamp, level, source location) stripped. Also
/// asserts the notice never reached the terminal.
fn notice_lines(fx: &Fixture, output: &Output) -> Vec<String> {
    let stderr = stderr_of(output);
    assert!(
        !stderr.lines().any(|l| l.starts_with("memory: ")),
        "the memory notice is log-only, but stderr carried it:\n{stderr}"
    );
    let stdout = stdout_of(output);
    assert!(
        !stdout.lines().any(|l| l.starts_with("memory: ")),
        "the memory notice is log-only, but stdout carried it:\n{stdout}"
    );
    let log = fs::read_to_string(log_path(fx)).unwrap_or_default();
    log.lines()
        .filter_map(|l| l.split_once(" - memory: "))
        .map(|(_, rest)| format!("memory: {rest}"))
        .collect()
}

fn info_row<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout
        .lines()
        .find(|l| l.split_whitespace().next() == Some(key))
        .map(|l| l.split_whitespace().nth(1).unwrap_or(""))
}

#[test]
fn old_cap_keys_load_into_the_renamed_info_rows_without_a_warning() {
    let fx = fixture(
        "aliases",
        "memory_cap_with_tools: 123\nmemory_cap_without_tools: 456\n",
    );
    let output = run(&fx, &["--info"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert_eq!(
        info_row(&stdout, "memory_facts_cap"),
        Some("123"),
        "{stdout}"
    );
    assert_eq!(
        info_row(&stdout, "memory_drill_cap"),
        Some("456"),
        "{stdout}"
    );
    assert!(
        info_row(&stdout, "memory_cap_with_tools").is_none(),
        "{stdout}"
    );
    assert!(
        info_row(&stdout, "memory_cap_without_tools").is_none(),
        "{stdout}"
    );
    // The old names are silent aliases: nothing reaches stderr, and the
    // logged memory notice already carries the renamed cap and the alias's
    // value.
    let stderr = stderr_of(&output);
    assert!(stderr.trim().is_empty(), "{stderr}");
    assert_eq!(
        notice_lines(&fx, &output),
        vec![
            "memory: 160 fact chars + 125 catalogue chars; 8 lines / 184 chars truncated at \
             memory_facts_cap=123 (global 2, workspace 6); run memory__lint"
        ],
    );
}

#[test]
fn a_dry_run_prompt_carries_facts_first_then_the_catalogue_under_the_default_cap() {
    let fx = fixture("default-cap", "");
    let output = run(&fx, &["-e", "hello"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    let sections = memory_sections(&stdout);
    assert_eq!(
        sections,
        vec![format!(
            "<memory>\n<facts>\n{FACTS}\n</facts>\n<catalogue>\n{CATALOGUE}\n</catalogue>\n</memory>"
        )],
        "{stdout}"
    );
    // The with-tools arm keeps the read-write memory instructions.
    assert!(stdout.contains("\n## Memory\n"), "{stdout}");
    assert!(!stdout.contains("## Memory (read-only)"), "{stdout}");
    assert_eq!(
        notice_lines(&fx, &output),
        vec!["memory: 160 fact chars + 125 catalogue chars injected under memory_facts_cap=4000"]
    );
}

/// Writes `global` as the global index and `workspace` as the workspace
/// index of `fx`.
fn both_indexes(fx: &Fixture, global: &str, workspace: &str) {
    fs::write(fx.dir.join("memory").join("MEMORY.md"), global).unwrap();
    fs::write(
        fx.workspace
            .join(".coyote")
            .join("memory")
            .join("MEMORY.md"),
        workspace,
    )
    .unwrap();
}

#[test]
fn under_the_default_cap_a_workspace_index_too_big_for_it_loses_only_its_own_facts() {
    // A repo-local index whose facts alone exceed the 4000-char default cap
    // must not evict a single global fact: the global facts come first, the
    // workspace facts fill what is left, and every line the cut drops is
    // counted against the scope it came from.
    let global_facts = ["- **G one.** keep me", "- **G two.** keep me"];
    let global_row = "- [[g-one]]: global drill one";
    let workspace_facts: Vec<String> = (0..120)
        .map(|i| format!("- **W {i:03}.** {}", "w".repeat(40)))
        .collect();
    let fx = fixture("workspace-overflow", "");
    both_indexes(
        &fx,
        &format!("{}\n{global_row}\n", global_facts.join("\n")),
        &format!("{}\n", workspace_facts.join("\n")),
    );
    let output = run(&fx, &["-e", "hello"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);

    let cap = 4000;
    let mut used = global_facts.join("\n").len();
    let mut kept = Vec::new();
    for line in &workspace_facts {
        if used + 1 + line.len() > cap {
            break;
        }
        used += 1 + line.len();
        kept.push(line.as_str());
    }
    assert!(kept.len() < workspace_facts.len(), "fixture must overflow");
    let workspace_lost = workspace_facts.len() - kept.len();
    let fact_chars = global_facts.join("\n").len() + 1 + workspace_facts.join("\n").len();
    let omitted_chars = fact_chars + 1 + global_row.len() - used;
    let facts_body = format!("{}\n{}", global_facts.join("\n"), kept.join("\n"));
    let expected = format!(
        "<memory>\n<facts>\n{facts_body}\n</facts>\n<catalogue>\n</catalogue>\n\
         <!-- memory facts truncated: {} lines / {omitted_chars} chars omitted \
         (global 1, workspace {workspace_lost}); run memory__lint -->\n</memory>",
        workspace_lost + 1,
    );
    assert_eq!(
        memory_sections(&stdout),
        vec![expected.as_str()],
        "{stdout}"
    );
    assert_eq!(
        notice_lines(&fx, &output),
        vec![format!(
            "memory: {fact_chars} fact chars + {} catalogue chars; {} lines / {omitted_chars} \
             chars truncated at memory_facts_cap=4000 (global 1, workspace {workspace_lost}); \
             run memory__lint",
            global_row.len(),
            workspace_lost + 1,
        )],
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn a_global_fact_longer_than_the_cap_is_cut_at_the_head_and_counted_as_global() {
    // The first line of the joined text is now a GLOBAL fact. It does not
    // fit the cap, so its first cap chars are kept, it counts as one omitted
    // global line, and every workspace line counts against the workspace.
    let global_fact = "- **G one.** keep me";
    let workspace_fact = "# W";
    let workspace_row = "- [[w-one]]: d";
    let fx = fixture("global-head-cut", "memory_facts_cap: 15\n");
    both_indexes(
        &fx,
        &format!("{global_fact}\n"),
        &format!("{workspace_fact}\n{workspace_row}\n"),
    );
    let output = run(&fx, &["-e", "hello"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    let fact_chars = global_fact.len() + 1 + workspace_fact.len();
    let omitted_chars = fact_chars + 1 + workspace_row.len() - 15;
    let expected = format!(
        "<memory>\n<facts>\n{}\n</facts>\n<catalogue>\n</catalogue>\n\
         <!-- memory facts truncated: 3 lines / {omitted_chars} chars omitted \
         (global 1, workspace 2); run memory__lint -->\n</memory>",
        &global_fact[..15],
    );
    assert_eq!(
        memory_sections(&stdout),
        vec![expected.as_str()],
        "{stdout}"
    );
    assert_eq!(
        notice_lines(&fx, &output),
        vec![format!(
            "memory: {fact_chars} fact chars + {} catalogue chars; 3 lines / {omitted_chars} \
             chars truncated at memory_facts_cap=15 (global 1, workspace 2); run memory__lint",
            workspace_row.len(),
        )],
        "{}",
        stderr_of(&output)
    );
}

/// Replaces both indexes with a single workspace index; the global one is
/// removed so the section holds exactly `index`.
fn only_workspace_index(fx: &Fixture, index: &str) {
    fs::write(
        fx.workspace
            .join(".coyote")
            .join("memory")
            .join("MEMORY.md"),
        index,
    )
    .unwrap();
    fs::remove_file(fx.dir.join("memory").join("MEMORY.md")).unwrap();
}

#[test]
fn a_catalogue_only_index_whose_first_line_exceeds_the_new_facts_cap_keeps_its_first_cap_chars() {
    // The facts cap arrives under its NEW name. With no facts at all the
    // first catalogue line is the first line of the joined text; it does not
    // fit, so it is cut at the cap and counted as one omitted line.
    let row = format!("- [[w]]: {}", "d".repeat(60));
    let fx = fixture("catalogue-first-cut", "memory_facts_cap: 20\n");
    only_workspace_index(&fx, &format!("{row}\n"));
    let output = run(&fx, &["-e", "hello"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    let expected = format!(
        "<memory>\n<facts>\n</facts>\n<catalogue>\n{}\n</catalogue>\n\
         <!-- memory facts truncated: 1 lines / {} chars omitted (global 0, workspace 1); \
         run memory__lint -->\n</memory>",
        &row[..20],
        row.len() - 20,
    );
    assert_eq!(
        memory_sections(&stdout),
        vec![expected.as_str()],
        "{stdout}"
    );
    assert_eq!(
        notice_lines(&fx, &output),
        vec![format!(
            "memory: 0 fact chars + {} catalogue chars; 1 lines / {} chars truncated at \
             memory_facts_cap=20 (global 0, workspace 1); run memory__lint",
            row.len(),
            row.len() - 20,
        )],
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn a_facts_heading_longer_than_the_new_facts_cap_is_cut_at_the_cap_and_counts_as_one_omitted_line()
{
    // Facts-first twin of the catalogue-first cut: the 24-char heading is the
    // first line and the cap is 10, so the facts block is its first ten chars
    // and the omitted count is the heading (as one line) plus the whole
    // catalogue line, over the joined text (24 + newline + 32 = 57 chars).
    let fx = fixture("facts-first-cut", "memory_facts_cap: 10\n");
    only_workspace_index(
        &fx,
        "# Workspace Memory Index\n- [[w-one]]: workspace drill one\n",
    );
    let output = run(&fx, &["-e", "hello"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert_eq!(
        memory_sections(&stdout),
        vec![
            "<memory>\n<facts>\n# Workspac\n</facts>\n<catalogue>\n</catalogue>\n\
             <!-- memory facts truncated: 2 lines / 47 chars omitted (global 0, workspace 2); \
             run memory__lint -->\n</memory>"
        ],
        "{stdout}"
    );
    assert_eq!(
        notice_lines(&fx, &output),
        vec![
            "memory: 24 fact chars + 32 catalogue chars; 2 lines / 47 chars truncated at \
             memory_facts_cap=10 (global 0, workspace 2); run memory__lint"
        ],
        "{}",
        stderr_of(&output)
    );
}

#[test]
fn the_configured_facts_cap_governs_the_cut_and_the_notice_fires_once_per_process() {
    // The caps arrive under their OLD names and differ, so a caller that
    // hands the renderer the drill cap (456) instead of the facts cap (123)
    // changes the cut and the notice.
    let fx = fixture(
        "once",
        "memory_cap_with_tools: 123\nmemory_cap_without_tools: 456\n",
    );
    fs::write(
        fx.dir.join("macros").join("twice.yaml"),
        "isolated: false\nsteps:\n  - \"hello one\"\n  - \"hello two\"\n",
    )
    .unwrap();
    let output = run(&fx, &["--macro", "twice"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    let sections = memory_sections(&stdout);
    // 286 chars of facts+catalogue cut at the last line boundary under 123:
    // the global line and five workspace facts lines (102 chars) survive, the
    // cut lands inside the facts, so the catalogue block is empty and one
    // notice follows it. The omitted lines are four workspace facts and the
    // four catalogue lines, two per scope.
    let expected = "<memory>\n<facts>\nPLACEHOLDER_CHECK\n# Workspace Memory Index\n\n\
                    ## Always-on facts\n- **Fact A.** alpha\n- **Fact B.** bravo\n</facts>\n\
                    <catalogue>\n</catalogue>\n\
                    <!-- memory facts truncated: 8 lines / 184 chars omitted \
                    (global 2, workspace 6); run memory__lint -->\n\
                    </memory>";
    assert_eq!(sections, vec![expected, expected], "{stdout}");
    assert_eq!(
        notice_lines(&fx, &output),
        vec![
            "memory: 160 fact chars + 125 catalogue chars; 8 lines / 184 chars truncated at \
             memory_facts_cap=123 (global 2, workspace 6); run memory__lint"
        ],
        "two injections, one notice:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn a_facts_only_index_renders_an_empty_catalogue_block_and_zero_catalogue_chars() {
    let fx = fixture("facts-only", "");
    fs::write(
        fx.workspace
            .join(".coyote")
            .join("memory")
            .join("MEMORY.md"),
        "# Index\n- **Only fact.** here\n",
    )
    .unwrap();
    fs::remove_file(fx.dir.join("memory").join("MEMORY.md")).unwrap();
    let output = run(&fx, &["-e", "hello"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    assert_eq!(
        memory_sections(&stdout),
        vec![
            "<memory>\n<facts>\n# Index\n- **Only fact.** here\n</facts>\n<catalogue>\n</catalogue>\n</memory>"
        ],
        "{stdout}"
    );
    assert_eq!(
        notice_lines(&fx, &output),
        vec!["memory: 29 fact chars + 0 catalogue chars injected under memory_facts_cap=4000"]
    );
}

#[test]
fn without_function_calling_the_drill_cap_minus_the_section_bounds_the_bodies() {
    // The section is 348 chars, so a 700-char drill cap leaves 352 for
    // bodies: the two short global files fit, the two 300-char workspace
    // files do not, and the pre-existing omission comment names them.
    let fx = fixture(
        "no-tools",
        "function_calling_support: false\nmemory_drill_cap: 700\n",
    );
    let global = fx.dir.join("memory");
    let workspace = fx.workspace.join(".coyote").join("memory");
    write_drill(&global, "g-one", "body of g-one");
    write_drill(&global, "g-two", "body of g-two");
    write_drill(
        &workspace,
        "w-one",
        &format!("body of w-one {}", "x".repeat(300)),
    );
    write_drill(
        &workspace,
        "w-two",
        &format!("body of w-two {}", "x".repeat(300)),
    );
    let output = run(&fx, &["-e", "hello"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = stdout_of(&output);
    let sections = memory_sections(&stdout);
    assert_eq!(sections.len(), 1, "{stdout}");
    let section = sections[0];
    assert!(
        section.starts_with(&format!(
            "<memory>\n<facts>\n{FACTS}\n</facts>\n<catalogue>\n{CATALOGUE}\n</catalogue>\n"
        )),
        "{section}"
    );
    assert!(
        section.contains("<file name=\"g-one\">\nbody of g-one\n"),
        "{section}"
    );
    assert!(
        section.contains("<file name=\"g-two\">\nbody of g-two\n"),
        "{section}"
    );
    assert!(!section.contains("body of w-one"), "{section}");
    assert!(
        section.ends_with(
            "<!-- 2 memory file(s) omitted; enable function calling for full access -->\n</memory>"
        ),
        "{section}"
    );
    assert!(
        section.chars().count() <= 700,
        "{}",
        section.chars().count()
    );
    assert!(stdout.contains("## Memory (read-only)"), "{stdout}");
    // The facts cap is untouched on this arm; the notice reports the section
    // the way the with-tools arm would.
    assert_eq!(
        notice_lines(&fx, &output),
        vec!["memory: 160 fact chars + 125 catalogue chars injected under memory_facts_cap=4000"]
    );
}
