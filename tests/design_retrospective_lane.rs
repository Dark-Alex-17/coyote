//! Pins the design-retrospective agent and its wiring into the closing steps
//! of sisyphus and architect: the agent stays read-only with its output
//! contract, both orchestrators can spawn it, and the architect's Phase F
//! numbering survives the inserted step.

use std::fs;
use std::path::Path;

const READ_ONLY_GLOBAL_TOOLS: [&str; 6] = [
    "ast_grep.sh",
    "fs_read.sh",
    "fs_cat.sh",
    "fs_grep.sh",
    "fs_glob.sh",
    "fs_ls.sh",
];

const MUTATING_GIT_VERBS: [&str; 11] = [
    "commit", "checkout", "reset", "push", "add", "rm", "stash", "apply", "merge", "rebase",
    "clean",
];

fn agent_file(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/agents")
        .join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn agent_config(name: &str) -> serde_yaml::Value {
    let rel = format!("{name}/config.yaml");
    serde_yaml::from_str(&agent_file(&rel)).unwrap_or_else(|e| panic!("parse {rel}: {e}"))
}

fn string_list(config: &serde_yaml::Value, key: &str) -> Vec<String> {
    config[key]
        .as_sequence()
        .unwrap_or_else(|| panic!("config has no {key} list"))
        .iter()
        .map(|v| v.as_str().expect("list entries are strings").to_string())
        .collect()
}

fn section<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text
        .find(start)
        .unwrap_or_else(|| panic!("no {start:?} in the text"));
    let to = text[from..]
        .find(end)
        .map(|i| from + i)
        .unwrap_or_else(|| panic!("no {end:?} after {start:?}"));
    &text[from..to]
}

#[test]
fn design_retrospective_agent_ships_read_only_with_explore_as_its_only_child() {
    for file in ["config.yaml", "instructions.md", "README.md", "tools.sh"] {
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets/agents/design-retrospective")
                .join(file)
                .is_file(),
            "design-retrospective must ship {file}"
        );
    }
    let config = agent_config("design-retrospective");
    let variables: Vec<&str> = config["variables"]
        .as_sequence()
        .expect("config has a variables list")
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    assert!(
        variables.contains(&"project_dir"),
        "design-retrospective must declare project_dir, got {variables:?}"
    );
    assert!(
        !variables.contains(&"auto_confirm"),
        "design-retrospective runs no confirmable commands and must not declare auto_confirm"
    );
    let tools = string_list(&config, "global_tools");
    for tool in &tools {
        assert!(
            READ_ONLY_GLOBAL_TOOLS.contains(&tool.as_str()),
            "design-retrospective is read-only; global tool {tool} is not in the read-only \
             allowlist {READ_ONLY_GLOBAL_TOOLS:?}"
        );
    }
    assert_eq!(
        string_list(&config, "spawnable_agents"),
        ["explore"],
        "design-retrospective spawns explore and nothing else"
    );
    assert!(
        string_list(&config, "enabled_skills")
            .iter()
            .any(|s| s == "codebase-design"),
        "design-retrospective must enable codebase-design for its vocabulary"
    );
}

#[test]
fn design_retrospective_prompt_carries_the_contract_and_the_core_question() {
    let text = agent_file("design-retrospective/instructions.md");
    for anchor in [
        "DESIGN_RETROSPECTIVE: REFACTORS <n>",
        "DESIGN_RETROSPECTIVE: CLEAN",
        "## Refactor items",
        "## Choices I would make again",
        "If I built this again, would I make the same choices",
        "These are examples to prompt judgement, not a checklist",
        "Never modify repository files.",
        "Never ask the user",
        "Read every changed file IN FULL",
    ] {
        assert!(
            text.contains(anchor),
            "design-retrospective/instructions.md lost anchor {anchor:?}"
        );
    }
    for file in ["instructions.md", "README.md", "config.yaml", "tools.sh"] {
        assert!(
            agent_file(&format!("design-retrospective/{file}")).is_ascii(),
            "design-retrospective/{file} must stay ASCII"
        );
    }
}

/// The subcommand of every `git` invocation in `script`: the first word
/// after `git` that is not an option, skipping the argument `-C` takes.
fn git_subcommands(script: &str) -> Vec<String> {
    let mut subcommands = Vec::new();
    for line in script.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let words: Vec<&str> = line.split_whitespace().collect();
        for (i, _) in words.iter().enumerate().filter(|(_, w)| **w == "git") {
            let mut rest = words[i + 1..].iter();
            while let Some(word) = rest.next() {
                if *word == "-C" {
                    rest.next();
                } else if !word.starts_with('-') {
                    subcommands.push(word.trim_matches(['"', '\'']).to_string());
                    break;
                }
            }
        }
    }
    subcommands
}

#[test]
fn design_retrospective_git_tools_run_only_read_only_subcommands() {
    let tools = agent_file("design-retrospective/tools.sh");
    for function in [
        "get_changed_files()",
        "get_diff()",
        "get_log()",
        "_instructions()",
    ] {
        assert!(
            tools.contains(function),
            "design-retrospective/tools.sh must define {function}"
        );
    }
    assert!(
        tools.contains("resolve_diff_base "),
        "design-retrospective/tools.sh must resolve --base through the shared resolve_diff_base"
    );
    let subcommands = git_subcommands(&tools);
    assert!(
        !subcommands.is_empty(),
        "design-retrospective/tools.sh should run git; found no git invocations"
    );
    for verb in MUTATING_GIT_VERBS {
        assert!(
            !subcommands.iter().any(|s| s == verb),
            "design-retrospective/tools.sh must not run `git {verb}`, got {subcommands:?}"
        );
    }
}

#[test]
fn both_orchestrators_can_spawn_the_design_retrospective() {
    for orchestrator in ["sisyphus", "architect"] {
        let spawnable = string_list(&agent_config(orchestrator), "spawnable_agents");
        assert!(
            spawnable.iter().any(|s| s == "design-retrospective"),
            "{orchestrator} spawnable_agents must include design-retrospective, got {spawnable:?}"
        );
    }
}

#[test]
fn sisyphus_runs_the_retrospective_in_both_cadences_before_whetstone() {
    let text = agent_file("sisyphus/instructions.md");
    let retro = section(
        &text,
        "### Design retrospective (after a large body of work)",
        "### Continuous improvement (whetstone)",
    );
    assert!(
        !retro.contains("<!-- when:") && !retro.contains("<!-- end -->"),
        "the design retrospective section must sit outside every conditional block"
    );
    for anchor in [
        "\"Large\" is your judgement, not a threshold",
        "`Design retrospective:`",
        "`retro-declined: <item> -- <reason>`",
        "agent__spawn --agent design-retrospective",
        "`Design question:`",
        "Run it ONCE per run",
    ] {
        assert!(
            retro.contains(anchor),
            "sisyphus design retrospective section lost anchor {anchor:?}"
        );
    }
    let batch = section(
        &text,
        "### Review cadence: batch (set by the parent orchestrator)",
        "<!-- end -->",
    );
    assert!(
        batch.contains("design retrospective"),
        "the batch cadence must hand the design retrospective to the parent"
    );
}

#[test]
fn architect_runs_the_retrospective_before_the_hygiene_pass() {
    let text = agent_file("architect/instructions.md");
    let retro = section(
        &text,
        "2. **Run-closing design retrospective.**",
        "3. **Run-closing hygiene pass.**",
    );
    for anchor in [
        "`TASK-NNN-run-refactor`",
        "agent__spawn --agent design-retrospective",
        "`retro-declined: <item> -- <reason>`",
    ] {
        assert!(
            retro.contains(anchor),
            "architect design retrospective step lost anchor {anchor:?}"
        );
    }
    let hygiene = section(
        &text,
        "3. **Run-closing hygiene pass.**",
        "4. **Roll up follow-ups",
    );
    assert!(
        hygiene.contains("`TASK-NNN-run-refactor`"),
        "the hygiene task must also be blocked by the refactor task"
    );
    let brief = section(&text, "   ## MUST NOT DO", "   ## CONTEXT");
    assert!(
        brief.contains("Do NOT run the design retrospective"),
        "the Phase E Sisyphus brief must hand the design retrospective to the orchestrator"
    );
}

#[test]
fn architect_phase_f_steps_are_numbered_one_to_nine() {
    let text = agent_file("architect/instructions.md");
    let phase_f = section(&text, "### Phase F", "\n## Durable state");
    let numbers: Vec<u32> = phase_f
        .lines()
        .filter_map(|l| l.split_once(". ").and_then(|(n, _)| n.parse().ok()))
        .collect();
    assert_eq!(
        numbers,
        (1..=9).collect::<Vec<_>>(),
        "Phase F top-level steps must run 1..9 without gaps"
    );
    for anchor in [
        "Steps 1-5 run at EVERY group's close-out",
        "steps 6-9 run ONCE",
        "- Steps 6-9 run once, at the end",
        "for steps 6-9, after the last group's",
    ] {
        assert!(
            phase_f.contains(anchor),
            "multi-strategy Phase F text lost renumbered anchor {anchor:?}"
        );
    }
    for stale in ["Steps 1-4", "steps 5-8", "Steps 5-8"] {
        assert!(
            !phase_f.contains(stale),
            "Phase F still carries the pre-renumber range {stale:?}"
        );
    }
}

/// Runs the design-retrospective git tools against a scratch repository.
#[cfg(unix)]
mod git_tools {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct ScratchDir(PathBuf);

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// Runs one tool with the argc variables in `envs`, returning
    /// (exit code, stdout, stderr).
    fn run_tool(project: &Path, function: &str, envs: &[(&str, &str)]) -> (i32, String, String) {
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        let agent = assets.join("agents/design-retrospective");
        let output = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "source '{}'; {function}",
                agent.join("tools.sh").display()
            ))
            .current_dir(project)
            .env("LLM_ROOT_DIR", &assets)
            .env("LLM_AGENT_ROOT_DIR", &agent)
            .env("LLM_OUTPUT", "/dev/stdout")
            .env("LLM_AGENT_VAR_PROJECT_DIR", project)
            .envs(envs.iter().copied())
            .output()
            .expect("spawn bash");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// With one base commit and one change commit, the tools report exactly
    /// the change, and an option-shaped `--base` that git would read as
    /// `--output=<file>` is refused by every tool without writing.
    #[test]
    fn design_retrospective_git_tools_read_the_range_and_refuse_option_bases() {
        for binary in ["bash", "git"] {
            if which::which(binary).is_err() {
                eprintln!("skipping: {binary} not available");
                return;
            }
        }
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let scratch =
            ScratchDir(std::env::temp_dir().join(format!("coyote-design-retro-{unique}")));
        let repo = scratch.0.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&repo, &["add", "base.txt"]);
        git(&repo, &["commit", "-q", "-m", "seed the base"]);
        git(&repo, &["tag", "start"]);
        fs::write(repo.join("feature.txt"), "feature body\n").unwrap();
        git(&repo, &["add", "feature.txt"]);
        git(&repo, &["commit", "-q", "-m", "add the feature"]);

        let (code, out, err) = run_tool(&repo, "get_changed_files", &[("argc_base", "start")]);
        assert_eq!(code, 0, "get_changed_files failed: {err}");
        assert!(
            out.contains("feature.txt") && !out.contains("base.txt"),
            "get_changed_files --base start must list only the change: {out}"
        );

        let (code, out, err) = run_tool(&repo, "get_log", &[("argc_base", "start")]);
        assert_eq!(code, 0, "get_log failed: {err}");
        assert!(
            out.contains("add the feature") && !out.contains("seed the base"),
            "get_log --base start must list only the change's commits: {out}"
        );

        let (code, out, err) = run_tool(
            &repo,
            "get_diff",
            &[("argc_base", "start"), ("argc_path", "feature.txt")],
        );
        assert_eq!(code, 0, "get_diff failed: {err}");
        assert!(
            out.contains("+feature body"),
            "get_diff --path feature.txt must show that file's change: {out}"
        );

        let target = scratch.0.join("written-by-git");
        let option_base = format!("--output={}", target.display());
        for function in ["get_changed_files", "get_diff", "get_log"] {
            let (code, out, _) = run_tool(&repo, function, &[("argc_base", &option_base)]);
            assert_ne!(
                code, 0,
                "{function} must refuse the option-shaped base {option_base:?}: {out}"
            );
            assert!(
                !target.exists(),
                "{function} let git write {} through an option-shaped base",
                target.display()
            );
        }
    }
}
