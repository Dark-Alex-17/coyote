//! Wrapper around the `sisyphus-next` candidate's own batteries and the
//! reachability sweep that keeps it a candidate.
//!
//! The batteries ship inside `assets/agents/sisyphus-next/scripts/` and run
//! from that path: `audit_graph.py` (static edges, script `_next` edges,
//! cycles against their bounds), `smoke_test.py` (every gate, both sides of
//! every bound, the fail-open paths, the run-dir resolution) and
//! `check_session_block.sh` (session block rules plus the variable mirror
//! against the prose `sisyphus/config.yaml`). Both `audit_graph.py` and
//! `check_session_block.sh` need the Go `yq` by mikefarah and skip without it.
//! Every run points `HOME`, `LLM_ROOT_DIR`, `COYOTE_CACHE_DIR`, `TMPDIR` and
//! `SISYPHUS_RUNS_ROOT` at a scratch directory and sets
//! `PYTHONDONTWRITEBYTECODE=1`, so nothing under `assets/` or `~/.config/coyote`
//! is read or written.
//!
//! The sweep is key-based over every bundled agent: no `spawnable_agents`
//! entry, no `session.chat.agent` value and no `agent:` node value names a
//! candidate, and no instructions or config prose mentions one. Both
//! candidate names are swept, including the one that has not landed yet.

#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const CANDIDATE: &str = "sisyphus-next";
const CANDIDATE_NAMES: [&str; 2] = ["sisyphus-next", "architect-next"];
const SMOKE_FLOOR: usize = 190;

fn agents_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/agents")
}

fn candidate_dir() -> PathBuf {
    agents_dir().join(CANDIDATE)
}

/// Removes the scratch dir even when an assertion panics mid-run.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch_dir(label: &str) -> TempDirGuard {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "coyote-{CANDIDATE}-{label}-{}-{unique}",
        std::process::id()
    ));
    for sub in ["home", "root", "cache", "tmp", "runs"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    TempDirGuard(dir)
}

fn python3() -> Option<PathBuf> {
    match which::which("python3") {
        Ok(path) => Some(path),
        Err(_) => {
            eprintln!("skipping: python3 not available");
            None
        }
    }
}

/// `audit_graph.py` and `check_session_block.sh` are written against the Go
/// `yq` by mikefarah; python-yq has a different syntax, so anything else
/// skips.
fn mikefarah_yq() -> Option<PathBuf> {
    let Ok(yq) = which::which("yq") else {
        eprintln!("skipping: yq not available");
        return None;
    };
    let version = Command::new(&yq).arg("--version").output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );
    if text.contains("mikefarah") {
        Some(yq)
    } else {
        eprintln!("skipping: yq is not the mikefarah build");
        None
    }
}

/// The current `PATH` with `yq_dir` in front, so the battery resolves the
/// same `yq` the gate just inspected.
fn path_with(yq_dir: &Path) -> OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let entries = std::iter::once(yq_dir.to_path_buf()).chain(std::env::split_paths(&path));
    std::env::join_paths(entries).unwrap()
}

/// A command that runs inside the candidate dir with every path-bearing
/// variable pointed at the scratch dir.
fn battery(program: &Path, scratch: &TempDirGuard) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(candidate_dir())
        .env("HOME", scratch.0.join("home"))
        .env("LLM_ROOT_DIR", scratch.0.join("root"))
        .env("COYOTE_CACHE_DIR", scratch.0.join("cache"))
        .env("TMPDIR", scratch.0.join("tmp"))
        .env("SISYPHUS_RUNS_ROOT", scratch.0.join("runs"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("GRAPH_STATE")
        .env_remove("GRAPH_STATE_FILE");
    cmd
}

fn transcript(output: &Output) -> String {
    format!(
        "status: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_no_bytecode_under_candidate() {
    let mut pending = vec![candidate_dir()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(
                name != "__pycache__" && !name.ends_with(".pyc"),
                "battery wrote bytecode under the asset dir: {}",
                path.display()
            );
            if path.is_dir() {
                pending.push(path);
            }
        }
    }
}

#[test]
fn audit_graph_reports_no_problems() {
    let Some(python) = python3() else { return };
    let Some(yq) = mikefarah_yq() else { return };
    let scratch = scratch_dir("audit");
    let output = battery(&python, &scratch)
        .env("PATH", path_with(yq.parent().unwrap()))
        .args(["scripts/audit_graph.py", "."])
        .output()
        .unwrap();
    let report = transcript(&output);
    assert!(output.status.success(), "{report}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|l| l.trim() == "PROBLEMS: none"),
        "{report}"
    );
    assert_no_bytecode_under_candidate();
}

#[test]
fn smoke_battery_passes_every_check() {
    let Some(python) = python3() else { return };
    let scratch = scratch_dir("smoke");
    let output = battery(&python, &scratch)
        .arg("scripts/smoke_test.py")
        .output()
        .unwrap();
    let report = transcript(&output);
    assert!(output.status.success(), "{report}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.lines().any(|l| l.starts_with("FAIL ")), "{report}");
    let summary = stdout
        .lines()
        .rev()
        .find(|l| l.ends_with(" checks passed"))
        .unwrap_or_else(|| panic!("no `N/N checks passed` line\n{report}"));
    let counts = summary.trim_end_matches(" checks passed");
    let (passed, total) = counts
        .split_once('/')
        .unwrap_or_else(|| panic!("malformed summary {summary:?}\n{report}"));
    let passed: usize = passed.parse().unwrap();
    let total: usize = total.parse().unwrap();
    assert_eq!(passed, total, "{summary}\n{report}");
    assert!(
        total >= SMOKE_FLOOR,
        "the battery shrank below {SMOKE_FLOOR} checks: {summary}\n{report}"
    );
    // The run-dir resolution pins live in the battery; this is the line that
    // ties the wrapper to them.
    assert!(
        stdout.contains(
            "ok   runs_root: COYOTE_CACHE_DIR/sisyphus-next/runs when no override is set"
        ),
        "{report}"
    );
    assert_no_bytecode_under_candidate();
}

#[test]
fn session_block_check_passes_under_mikefarah_yq() {
    let Some(yq) = mikefarah_yq() else { return };
    let Ok(bash) = which::which("bash") else {
        eprintln!("skipping: bash not available");
        return;
    };
    let scratch = scratch_dir("session");
    let output = battery(&bash, &scratch)
        .env("PATH", path_with(yq.parent().unwrap()))
        .arg("scripts/check_session_block.sh")
        .output()
        .unwrap();
    let report = transcript(&output);
    assert!(output.status.success(), "{report}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.lines().any(|l| l.starts_with("FAIL ")), "{report}");
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("ok variables mirror sisyphus")),
        "{report}"
    );
}

/// The engine forwards a script-node crash only along `fallback:`, so the
/// fail-open shim needs that edge declared next to `next:` and pointing at
/// the same node.
#[test]
fn debt_scan_declares_its_fallback_next_to_next() {
    let text = fs::read_to_string(candidate_dir().join("graph.yaml")).unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    let node = &doc["nodes"]["debt_scan"];
    assert_eq!(
        node["next"].as_str(),
        Some("bookkeeping_scan"),
        "debt_scan next"
    );
    assert_eq!(
        node["fallback"].as_str(),
        Some("bookkeeping_scan"),
        "debt_scan has no fallback edge: a script crash forwards only on fallback, so the fail-open default would never reach bookkeeping_scan"
    );
}

/// Every string value under a key named `agent` anywhere in the document
/// (agent nodes, map branches, `session.chat.agent`), plus every entry of a
/// `spawnable_agents` sequence.
fn agent_references(value: &serde_yaml::Value, found: &mut Vec<(String, String)>) {
    match value {
        serde_yaml::Value::Mapping(map) => {
            for (key, inner) in map {
                let key = key.as_str().unwrap_or_default();
                match (key, inner) {
                    ("agent", serde_yaml::Value::String(name)) => {
                        found.push(("agent".to_string(), name.clone()));
                    }
                    ("spawnable_agents", serde_yaml::Value::Sequence(names)) => {
                        for name in names.iter().filter_map(serde_yaml::Value::as_str) {
                            found.push(("spawnable_agents".to_string(), name.to_string()));
                        }
                    }
                    _ => {}
                }
                agent_references(inner, found);
            }
        }
        serde_yaml::Value::Sequence(items) => {
            for item in items {
                agent_references(item, found);
            }
        }
        _ => {}
    }
}

#[test]
fn no_bundled_agent_reaches_a_candidate() {
    let mut agents: Vec<PathBuf> = fs::read_dir(agents_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    agents.sort();
    assert!(
        agents.iter().any(|p| p.ends_with(CANDIDATE)),
        "{CANDIDATE} is bundled"
    );

    let mut hits = Vec::new();
    let mut references_checked = 0usize;
    for agent in &agents {
        for file in ["graph.yaml", "config.yaml"] {
            let path = agent.join(file);
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let doc: serde_yaml::Value = serde_yaml::from_str(&text)
                .unwrap_or_else(|e| panic!("{} failed to parse: {e}", path.display()));
            let mut found = Vec::new();
            agent_references(&doc, &mut found);
            if let Some(chat_agent) = doc
                .get("session")
                .and_then(|s| s.get("chat"))
                .and_then(|c| c.get("agent"))
                .and_then(serde_yaml::Value::as_str)
            {
                found.push(("session.chat.agent".to_string(), chat_agent.to_string()));
            }
            references_checked += found.len();
            for (key, name) in found {
                if CANDIDATE_NAMES.contains(&name.as_str()) {
                    hits.push(format!("{}: {key} = {name}", path.display()));
                }
            }
        }
        for file in ["instructions.md", "config.yaml"] {
            let path = agent.join(file);
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for name in CANDIDATE_NAMES {
                if text.contains(name) {
                    hits.push(format!("{}: prose names {name}", path.display()));
                }
            }
        }
    }
    assert!(
        references_checked >= 20,
        "the sweep saw only {references_checked} agent references; the bundled graphs name far more"
    );
    assert!(
        hits.is_empty(),
        "a bundled agent reaches a candidate:\n{}",
        hits.join("\n")
    );
}

/// Lists every regular file under `root` as a path relative to it, sorted.
fn files_under(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    found.sort();
    found
}

/// Plants a user config dir under the scratch `HOME` that would break every
/// battery if it were read: a `utils.sh` that exits, an unparseable
/// `config.yaml`, a prose `sisyphus/config.yaml` whose variable list has
/// drifted, and a `sisyphus-next/graph.yaml` that is not YAML at all.
/// Returns the planted (relative path, bytes) pairs.
fn poison_user_config(scratch: &TempDirGuard) -> Vec<(String, Vec<u8>)> {
    let home = scratch.0.join("home");
    let planted = [
        (".config/coyote/agents/.shared/utils.sh", "exit 97\n"),
        (".config/coyote/config.yaml", "not: [valid\n"),
        (
            ".config/coyote/agents/sisyphus/config.yaml",
            "name: sisyphus\nvariables:\n  - name: drifted\n    default: 'x'\n",
        ),
        (
            ".config/coyote/agents/sisyphus-next/graph.yaml",
            "{{ not yaml",
        ),
    ];
    planted
        .iter()
        .map(|(rel, body)| {
            let path = home.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            (rel.to_string(), body.as_bytes().to_vec())
        })
        .collect()
}

/// The batteries resolve everything from the asset tree they run in: a user
/// config dir under `HOME` that would break them if consulted changes
/// nothing, and nothing is written under `HOME` either.
#[test]
fn batteries_never_consult_the_user_config_dir() {
    let Some(python) = python3() else { return };
    let scratch = scratch_dir("poison");
    let planted = poison_user_config(&scratch);
    let home = scratch.0.join("home");

    let yq = mikefarah_yq();
    if let Some(yq) = &yq {
        let audit = battery(&python, &scratch)
            .env("PATH", path_with(yq.parent().unwrap()))
            .args(["scripts/audit_graph.py", "."])
            .output()
            .unwrap();
        assert!(audit.status.success(), "{}", transcript(&audit));
        assert!(
            String::from_utf8_lossy(&audit.stdout)
                .lines()
                .any(|l| l.trim() == "PROBLEMS: none"),
            "{}",
            transcript(&audit)
        );
    }

    let smoke = battery(&python, &scratch)
        .arg("scripts/smoke_test.py")
        .output()
        .unwrap();
    let smoke_out = String::from_utf8_lossy(&smoke.stdout);
    assert!(smoke.status.success(), "{}", transcript(&smoke));
    assert!(
        !smoke_out.lines().any(|l| l.starts_with("FAIL ")),
        "{}",
        transcript(&smoke)
    );

    if let Some(yq) = &yq {
        let bash = which::which("bash").expect("bash");
        let session = battery(&bash, &scratch)
            .env("PATH", path_with(yq.parent().unwrap()))
            .arg("scripts/check_session_block.sh")
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&session.stdout);
        assert!(session.status.success(), "{}", transcript(&session));
        // The mirror is taken against the bundled prose config, not the
        // drifted one planted under HOME.
        assert!(
            out.lines()
                .any(|l| l.starts_with("ok variables mirror sisyphus")),
            "{}",
            transcript(&session)
        );
    }

    let mut expected: Vec<String> = planted.iter().map(|(rel, _)| rel.clone()).collect();
    expected.sort();
    assert_eq!(
        files_under(&home),
        expected,
        "a battery wrote under HOME beyond the planted poison"
    );
    for (rel, body) in &planted {
        assert_eq!(
            fs::read(home.join(rel)).unwrap(),
            *body,
            "a battery rewrote {rel}"
        );
    }
    assert_no_bytecode_under_candidate();
}

/// Asks the candidate's own `_lib.py` for `runs_root()` and `run_dir_for()`
/// in a fresh process with exactly the given run-dir variables set.
fn runs_root_under(
    python: &Path,
    scratch: &TempDirGuard,
    vars: &[(&str, &Path)],
) -> (String, String) {
    let mut cmd = Command::new(python);
    cmd.current_dir(candidate_dir().join("scripts"))
        .env("HOME", scratch.0.join("home"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("SISYPHUS_RUNS_ROOT")
        .env_remove("COYOTE_CACHE_DIR")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("LOCALAPPDATA");
    for (key, value) in vars {
        cmd.env(key, value);
    }
    let output = cmd
        .args([
            "-c",
            "import _lib; print(_lib.runs_root()); print(_lib.run_dir_for('r1'))",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", transcript(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    (
        lines.next().unwrap().to_string(),
        lines.next().unwrap().to_string(),
    )
}

/// The run dir follows the engine's cache-dir precedence, tier by tier, from
/// a fresh process: the explicit override, then `COYOTE_CACHE_DIR`, then
/// `$XDG_CACHE_HOME/coyote`, then the platform cache dir; every run lands
/// directly under that root.
#[test]
fn runs_root_follows_the_cache_dir_precedence_from_a_fresh_process() {
    let Some(python) = python3() else { return };
    let scratch = scratch_dir("runsroot");
    let home = scratch.0.join("home");
    let cache = scratch.0.join("cache");
    let xdg = scratch.0.join("xdg");
    let override_root = scratch.0.join("runs");
    assert_no_bytecode_under_candidate();

    let (root, run) = runs_root_under(
        &python,
        &scratch,
        &[
            ("SISYPHUS_RUNS_ROOT", override_root.as_path()),
            ("COYOTE_CACHE_DIR", cache.as_path()),
            ("XDG_CACHE_HOME", xdg.as_path()),
        ],
    );
    assert_eq!(
        PathBuf::from(&root),
        override_root,
        "the override wins over every cache dir"
    );
    assert_eq!(PathBuf::from(&run), override_root.join("r1"));

    let (root, run) = runs_root_under(
        &python,
        &scratch,
        &[
            ("COYOTE_CACHE_DIR", cache.as_path()),
            ("XDG_CACHE_HOME", xdg.as_path()),
        ],
    );
    assert_eq!(
        PathBuf::from(&root),
        cache.join("sisyphus-next").join("runs")
    );
    assert_eq!(
        PathBuf::from(&run),
        cache.join("sisyphus-next").join("runs").join("r1")
    );

    let (root, _) = runs_root_under(&python, &scratch, &[("XDG_CACHE_HOME", xdg.as_path())]);
    assert_eq!(
        PathBuf::from(&root),
        xdg.join("coyote").join("sisyphus-next").join("runs")
    );

    let (root, _) = runs_root_under(&python, &scratch, &[]);
    let platform_cache = if cfg!(target_os = "macos") {
        home.join("Library").join("Caches")
    } else {
        home.join(".cache")
    };
    assert_eq!(
        PathBuf::from(&root),
        platform_cache
            .join("coyote")
            .join("sisyphus-next")
            .join("runs"),
        "platform tier"
    );
    assert_no_bytecode_under_candidate();
}

fn variable_list(path: &Path) -> Vec<serde_yaml::Value> {
    let text = fs::read_to_string(path).unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    doc["variables"].as_sequence().unwrap().clone()
}

fn names_of(vars: &[serde_yaml::Value]) -> Vec<&str> {
    vars.iter().map(|v| v["name"].as_str().unwrap()).collect()
}

/// `build_lock` is declared where the prose twin declares it (right after
/// `escalation_policy`, right before `review_cadence`) with an empty
/// default, and its description is the gauntlet's sentence plus the
/// value-constraint sentence the prose sisyphus carries.
#[test]
fn build_lock_is_declared_at_the_prose_twins_position() {
    let candidate = variable_list(&candidate_dir().join("graph.yaml"));
    let prose = variable_list(&agents_dir().join("sisyphus/config.yaml"));
    for (label, vars) in [("sisyphus-next", &candidate), ("sisyphus", &prose)] {
        let names = names_of(vars);
        let at = names
            .iter()
            .position(|n| *n == "build_lock")
            .unwrap_or_else(|| panic!("{label} declares no build_lock: {names:?}"));
        assert_eq!(
            names.get(at - 1),
            Some(&"escalation_policy"),
            "{label}: {names:?}"
        );
        assert_eq!(
            names.get(at + 1),
            Some(&"review_cadence"),
            "{label}: {names:?}"
        );
        assert_eq!(vars[at]["default"].as_str(), Some(""), "{label} default");
    }

    let gauntlet = variable_list(&agents_dir().join("review-gauntlet/graph.yaml"));
    let gauntlet_text = gauntlet
        .iter()
        .find(|v| v["name"].as_str() == Some("build_lock"))
        .and_then(|v| v["description"].as_str())
        .expect("review-gauntlet declares build_lock")
        .trim_end_matches('.')
        .to_string();
    let constraint = "It is substituted raw into a JSON string on every hop, so it must be free of double quotes and backslashes; quote paths with single quotes";
    let prose_text = prose
        .iter()
        .find(|v| v["name"].as_str() == Some("build_lock"))
        .and_then(|v| v["description"].as_str())
        .unwrap();
    assert!(
        prose_text.contains(constraint),
        "prose sisyphus carries the constraint sentence"
    );
    let description = candidate
        .iter()
        .find(|v| v["name"].as_str() == Some("build_lock"))
        .and_then(|v| v["description"].as_str())
        .unwrap();
    assert!(
        description.starts_with(&gauntlet_text),
        "candidate description does not open with the gauntlet's sentence:\n{description}\n{gauntlet_text}"
    );
    assert!(
        description.contains(constraint),
        "candidate description lacks the constraint sentence"
    );
}

/// The gates stay project-agnostic: no script or graph prompt names a
/// toolchain command (verification commands come from the brief). The smoke
/// battery's fixtures are the one place such words may appear.
#[test]
fn no_gate_names_a_toolchain_command() {
    let re = fancy_regex::Regex::new(
        r"\b(cargo|rustc|npm|pnpm|yarn|pytest|mvn|gradle|just)\b|\bgo (test|build|run)\b",
    )
    .unwrap();
    let mut files = vec![candidate_dir().join("graph.yaml")];
    for entry in fs::read_dir(candidate_dir().join("scripts")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name.ends_with(".py") && name != "smoke_test.py" {
            files.push(path);
        }
    }
    assert!(files.len() > 40, "the sweep saw only {} files", files.len());
    let mut hits = Vec::new();
    for path in files {
        let text = fs::read_to_string(&path).unwrap();
        for (idx, line) in text.lines().enumerate() {
            if re.is_match(line).unwrap() {
                hits.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "toolchain commands named outside fixtures:\n{}",
        hits.join("\n")
    );
}

/// Copies `src` into `dst` recursively (regular files and directories only).
fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A scratch copy of the candidate dir beside the shared scripts it reaches
/// through `../../.shared`, so a battery can be edited without touching the
/// asset tree. Returns the copied candidate dir.
fn scratch_candidate_copy(scratch: &TempDirGuard) -> PathBuf {
    let agents = scratch.0.join("tree").join("agents");
    copy_tree(&agents_dir().join(".shared"), &agents.join(".shared"));
    let copy = agents.join(CANDIDATE);
    copy_tree(&candidate_dir(), &copy);
    copy
}

fn smoke_summary(stdout: &str) -> (usize, usize) {
    let summary = stdout
        .lines()
        .rev()
        .find(|l| l.ends_with(" checks passed"))
        .unwrap_or_else(|| panic!("no `N/N checks passed` line in:\n{stdout}"));
    let (passed, total) = summary
        .trim_end_matches(" checks passed")
        .split_once('/')
        .unwrap();
    (passed.parse().unwrap(), total.parse().unwrap())
}

/// The summary line and the exit code are taken after the last check, so a
/// failure recorded by the final check of the battery is neither hidden from
/// the `N/N` tally nor from the exit code. Driven on a scratch copy with one
/// failing check appended after the battery's last `check(` call.
#[test]
fn smoke_summary_and_exit_code_cover_a_check_recorded_last() {
    let Some(python) = python3() else { return };
    let scratch = scratch_dir("tally");
    let copy = scratch_candidate_copy(&scratch);
    let run = |copy: &Path| {
        let mut cmd = battery(&python, &scratch);
        cmd.current_dir(copy).arg("scripts/smoke_test.py");
        cmd.output().unwrap()
    };

    let pristine = run(&copy);
    assert!(pristine.status.success(), "{}", transcript(&pristine));
    let (passed, total) = smoke_summary(&String::from_utf8_lossy(&pristine.stdout));
    assert_eq!(passed, total, "{}", transcript(&pristine));

    let script = copy.join("scripts").join("smoke_test.py");
    let text = fs::read_to_string(&script).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    let last_check = lines
        .iter()
        .rposition(|l| l.starts_with("check("))
        .expect("the battery has a check( call");
    let forced = "check(\"probe: a failure recorded after the last battery check\", False)";
    lines.insert(last_check + 1, forced);
    fs::write(&script, lines.join("\n") + "\n").unwrap();

    let broken = run(&copy);
    let report = transcript(&broken);
    let stdout = String::from_utf8_lossy(&broken.stdout);
    assert!(
        stdout.lines().any(|l| l.starts_with("FAIL probe:")),
        "the forced check did not run:\n{report}"
    );
    assert_eq!(
        smoke_summary(&stdout),
        (total, total + 1),
        "the summary must count the check recorded last:\n{report}"
    );
    assert!(
        !broken.status.success(),
        "a failure recorded last must fail the battery:\n{report}"
    );
}

/// The shipped `debt_scan` node lifted verbatim into a two-node graph with
/// the shipped `bookkeeping_scan`, run through the real binary. The binary
/// installs the bundled `agents/.shared/` into the config dir but leaves a
/// file that is already there alone, so a planted scanner that exits
/// non-zero is the hard crash of the shim: it falls forward on the
/// `fallback:` edge, `bookkeeping_scan` still runs with the initial_state
/// `DEBT_SCAN_FAILED` default in `debt_hits` and records the hop in the
/// decisions ledger; with the shipped scanner the same graph takes `next:`
/// and the default is replaced by the scanner's report.
#[test]
fn a_debt_scan_crash_falls_forward_to_bookkeeping_scan_with_the_default_report() {
    let Some(_python) = python3() else { return };
    let Ok(git) = which::which("git") else {
        eprintln!("skipping: git not available");
        return;
    };
    let text = fs::read_to_string(candidate_dir().join("graph.yaml")).unwrap();
    let shipped: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    let default_report = shipped["initial_state"]["debt_hits"]
        .as_str()
        .expect("shipped debt_hits default")
        .to_string();
    assert!(default_report.starts_with("DEBT_SCAN_FAILED"));

    let run_leg = |label: &str, scanner_crashes: bool| -> (serde_json::Value, String) {
        let scratch = scratch_dir(label);
        let dir = scratch.0.clone();
        let project = dir.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("note.txt"), "clean\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=probe",
                "-c",
                "user.email=probe@example.invalid",
                "add",
                ".",
            ],
            vec![
                "-c",
                "user.name=probe",
                "-c",
                "user.email=probe@example.invalid",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        ] {
            let status = Command::new(&git)
                .args(&args)
                .current_dir(&project)
                .env("HOME", dir.join("home"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }

        let vault_pass = dir.join("vault-pass");
        fs::write(&vault_pass, "test-password\n").unwrap();
        fs::write(
            dir.join("config.yaml"),
            format!(
                "model: dryrun:dry-model\ndry_run: true\nvault_password_file: '{}'\nclients:\n  - type: openai\n    name: dryrun\n    auth: none\n    api_key: 'unused'\n    models:\n      - name: dry-model\n        max_input_tokens: 100000\n        supports_function_calling: true\nsave: false\nmemory: false\nstream: false\n",
                vault_pass.display()
            ),
        )
        .unwrap();

        let agents = dir.join("agents");
        copy_tree(&agents_dir().join(".shared"), &agents.join(".shared"));
        if scanner_crashes {
            fs::write(
                agents.join(".shared").join("debt_scan.py"),
                "raise SystemExit('probe: scanner crashed')\n",
            )
            .unwrap();
        }
        let agent = agents.join("probe-debt");
        copy_tree(&candidate_dir().join("scripts"), &agent.join("scripts"));
        let dump = dir.join("dump.json");
        fs::write(
            agent.join("scripts").join("probe_dump.py"),
            format!(
                "import json, os, sys\n\
                 sys.dont_write_bytecode = True\n\
                 path = os.environ.get('GRAPH_STATE_FILE')\n\
                 state = json.load(open(path)) if path else json.loads(os.environ.get('GRAPH_STATE', '{{}}'))\n\
                 keys = ('debt_hits', 'decisions', 'last_script_error', 'debt_items')\n\
                 open({:?}, 'w').write(json.dumps({{k: state.get(k) for k in keys}}))\n\
                 print('{{}}')\n",
                dump.display()
            ),
        )
        .unwrap();

        let mut nodes = serde_yaml::Mapping::new();
        for id in ["debt_scan", "bookkeeping_scan"] {
            nodes.insert(id.into(), shipped["nodes"][id].clone());
        }
        let mut judge = serde_yaml::Mapping::new();
        judge.insert("id".into(), "bookkeeping_judge".into());
        judge.insert("type".into(), "script".into());
        judge.insert("script".into(), "scripts/probe_dump.py".into());
        judge.insert("next".into(), "done".into());
        nodes.insert(
            "bookkeeping_judge".into(),
            serde_yaml::Value::Mapping(judge),
        );
        for end in ["done", "end_failure"] {
            let mut node = serde_yaml::Mapping::new();
            node.insert("id".into(), end.into());
            node.insert("type".into(), "end".into());
            node.insert("output".into(), format!("PROBE_ROUTE={end}").into());
            nodes.insert(end.into(), serde_yaml::Value::Mapping(node));
        }
        let mut initial = serde_yaml::Mapping::new();
        initial.insert("debt_hits".into(), default_report.clone().into());
        initial.insert(
            "project_dir".into(),
            project.to_string_lossy().into_owned().into(),
        );
        initial.insert("debt_base".into(), "HEAD".into());
        initial.insert(
            "run_dir".into(),
            dir.join("run").to_string_lossy().into_owned().into(),
        );
        initial.insert("decisions".into(), serde_yaml::Value::Sequence(vec![]));
        initial.insert("last_script_error".into(), "".into());
        let mut graph = serde_yaml::Mapping::new();
        graph.insert("name".into(), "probe_debt".into());
        graph.insert("start".into(), "debt_scan".into());
        graph.insert("initial_state".into(), serde_yaml::Value::Mapping(initial));
        graph.insert("nodes".into(), serde_yaml::Value::Mapping(nodes));
        fs::write(
            agent.join("graph.yaml"),
            serde_yaml::to_string(&serde_yaml::Value::Mapping(graph)).unwrap(),
        )
        .unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
            .args(["--agent", "probe-debt", "probe"])
            .current_dir(&dir)
            .env("HOME", dir.join("home"))
            .env("COYOTE_CONFIG_DIR", &dir)
            .env("COYOTE_CACHE_DIR", dir.join("cache"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("NO_COLOR", "1")
            .env_remove("COYOTE_LOG_PATH")
            .env_remove("IS_SANDBOX")
            .env_remove("GRAPH_STATE")
            .env_remove("GRAPH_STATE_FILE")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let report = transcript(&output);
        assert!(output.status.success(), "{label}: {report}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("PROBE_ROUTE=done"), "{label}: {report}");
        let dumped: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&dump).unwrap_or_else(|e| {
                panic!("{label}: bookkeeping_judge never ran ({e}): {report}")
            }))
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
        let trace = stderr
            .lines()
            .find(|l| l.contains("debt_scan (script) done in"))
            .unwrap_or_else(|| panic!("{label}: no debt_scan trace line in:\n{report}"))
            .to_string();
        assert_no_bytecode_under_candidate();
        (dumped, trace)
    };

    let (crashed, trace) = run_leg("debtcrash", true);
    assert!(
        trace.contains("-> bookkeeping_scan (fallback)"),
        "a shim crash must take the fallback edge: {trace}"
    );
    assert_eq!(
        crashed["debt_hits"].as_str(),
        Some(default_report.as_str()),
        "the initial_state default must survive the crash: {crashed}"
    );
    let decisions = crashed["decisions"].as_array().unwrap();
    assert!(
        decisions.iter().any(|d| d
            .as_str()
            .is_some_and(|s| s.starts_with("fallback: debt_scan:"))),
        "bookkeeping_scan must record the hop: {decisions:?}"
    );
    assert_eq!(
        crashed["last_script_error"].as_str(),
        Some(""),
        "the engine's crash note is consumed by bookkeeping_scan: {crashed}"
    );

    let (clean, trace) = run_leg("debtclean", false);
    assert!(
        !trace.contains("(fallback)"),
        "with the scanner present debt_scan routes on next: {trace}"
    );
    assert!(
        !clean["debt_hits"]
            .as_str()
            .unwrap()
            .starts_with("DEBT_SCAN_FAILED"),
        "the scanner's report must replace the default: {clean}"
    );
    assert!(
        !clean["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().is_some_and(|s| s.starts_with("fallback:"))),
        "no fallback hop on the clean run: {clean}"
    );
}
