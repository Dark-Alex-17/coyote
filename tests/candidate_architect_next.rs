//! Wrapper around the `architect-next` candidate's own batteries.
//!
//! The batteries ship inside `assets/agents/architect-next/scripts/` and run
//! from that path: `audit_graph.py` (static edges, script `_next` edges,
//! cycles against their bounds), `smoke_test.py` (every gate, both sides of
//! every bound, the fail-open paths, the run-dir resolution),
//! `check_session_block.sh` (session block rules plus the variable mirror
//! against the prose `architect/config.yaml`) and `mode_matrix.py` (the
//! marker map against the prose `architect/instructions.md` plus all 48 mode
//! combinations walked to `end_complete`; several minutes, so opt-in).
//! `audit_graph.py`, `check_session_block.sh` and `mode_matrix.py` need the
//! Go `yq` by mikefarah and skip without it. Every run points `HOME`,
//! `LLM_ROOT_DIR`, `COYOTE_CACHE_DIR`, `TMPDIR` and `ARCHITECT_RUNS_ROOT` at a
//! scratch directory and sets `PYTHONDONTWRITEBYTECODE=1`, so nothing under
//! `assets/` or `~/.config/coyote` is read or written.
//!
//! The reachability sweep that keeps both candidates out of every bundled
//! agent lives in `candidate_sisyphus_next.rs` and already covers this name.

#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CANDIDATE: &str = "architect-next";
const SMOKE_FLOOR: usize = 304;
const MATRIX_FLOOR: usize = 53;

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
        .env("ARCHITECT_RUNS_ROOT", scratch.0.join("runs"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("GRAPH_STATE")
        .env_remove("GRAPH_STATE_FILE");
    scrub_agent_vars(&mut cmd);
    cmd
}

/// Drops every inherited `LLM_AGENT_VAR_*` variable from `cmd`, so a test run
/// started inside an agent session does not hand that session's variables to
/// the batteries, the gates or the binary.
fn scrub_agent_vars(cmd: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LLM_AGENT_VAR_") {
            cmd.env_remove(key);
        }
    }
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

/// The `(passed, total)` pair from the battery's final `N/N checks passed` line.
fn summary_counts(stdout: &str, report: &str) -> (usize, usize) {
    let summary = stdout
        .lines()
        .rev()
        .find(|l| l.ends_with(" checks passed"))
        .unwrap_or_else(|| panic!("no `N/N checks passed` line\n{report}"));
    let (passed, total) = summary
        .trim_end_matches(" checks passed")
        .split_once('/')
        .unwrap_or_else(|| panic!("malformed summary {summary:?}\n{report}"));
    (passed.parse().unwrap(), total.parse().unwrap())
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
    let (passed, total) = summary_counts(&stdout, &report);
    assert_eq!(passed, total, "{report}");
    assert!(
        total >= SMOKE_FLOOR,
        "the battery shrank below {SMOKE_FLOOR} checks: {total}\n{report}"
    );
    // The run-dir resolution pins and the utils.sh copy pin live in the
    // battery; these are the lines that tie the wrapper to them.
    assert!(
        stdout.contains(
            "ok   runs_root: COYOTE_CACHE_DIR/architect-next/runs when no override is set"
        ),
        "{report}"
    );
    assert!(
        stdout.contains("ok   utils.sh: the repo asset is copied, not a stub"),
        "{report}"
    );
    assert_no_bytecode_under_candidate();
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
    let report = transcript(&pristine);
    assert!(pristine.status.success(), "{report}");
    let (passed, total) = summary_counts(&String::from_utf8_lossy(&pristine.stdout), &report);
    assert_eq!(passed, total, "{report}");

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
        summary_counts(&stdout, &report),
        (total, total + 1),
        "the summary must count the check recorded last:\n{report}"
    );
    assert!(
        !broken.status.success(),
        "a failure recorded last must fail the battery:\n{report}"
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
            .any(|l| l.starts_with("ok variables mirror architect")),
        "{report}"
    );
}

/// Opt-in: the walk runs all 48 mode combinations through the real gates
/// inside scratch repos and takes three to five minutes, so it is ignored by
/// default and run with `cargo test --test candidate_architect_next -- --ignored`.
/// `mode_matrix.py` loads the graph through `yq -o=json`, so like the audit
/// and the session check it needs the mikefarah build and skips otherwise.
#[test]
#[ignore]
fn mode_matrix_walks_every_combination() {
    let Some(python) = python3() else { return };
    let Some(yq) = mikefarah_yq() else { return };
    let scratch = scratch_dir("matrix");
    let output = battery(&python, &scratch)
        .env("PATH", path_with(yq.parent().unwrap()))
        .arg("scripts/mode_matrix.py")
        .output()
        .unwrap();
    let report = transcript(&output);
    assert!(output.status.success(), "{report}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.lines().any(|l| l.starts_with("FAIL ")), "{report}");
    // The walk copies the repo's `.shared/utils.sh` into the scratch root and
    // exits before any combination runs if that file is missing or lacks
    // `set -euo pipefail`; the confirmation line proves the real file was used.
    assert!(
        stdout.lines().any(|l| l == "utils.sh: real asset"),
        "{report}"
    );
    let (passed, total) = summary_counts(&stdout, &report);
    assert_eq!(passed, total, "{report}");
    assert!(
        total >= MATRIX_FLOOR,
        "the matrix shrank below {MATRIX_FLOOR} checks: {total}\n{report}"
    );
    assert_no_bytecode_under_candidate();
}

/// The matrix has no stub branch left: when the repo asset `.shared/utils.sh`
/// is missing, or is present but lacks `set -euo pipefail`, `mode_matrix.py`
/// exits non-zero with a loud message before the first combination runs and
/// never prints the `utils.sh: real asset` confirmation. Exercised on a
/// scratch copy of the three agent dirs the matrix reaches (the candidate,
/// the prose `architect` it copies, and a doctored `.shared`), so the shipped
/// tree is never touched. The exit sits before `load_graph()`, so this path
/// needs no `yq`.
#[test]
fn mode_matrix_exits_before_the_walk_when_the_shared_utils_sh_is_missing_or_a_stub() {
    let Some(python) = python3() else { return };
    for (case, doctor) in [("stub", Some("# stub\n")), ("missing", None::<&str>)] {
        let scratch = scratch_dir(&format!("matrix-{case}"));
        let agents = scratch.0.join("agents");
        for name in [".shared", "architect", CANDIDATE] {
            copy_tree(&agents_dir().join(name), &agents.join(name));
        }
        let utils = agents.join(".shared").join("utils.sh");
        match doctor {
            Some(body) => fs::write(&utils, body).unwrap(),
            None => fs::remove_file(&utils).unwrap(),
        }
        let output = battery(&python, &scratch)
            .current_dir(agents.join(CANDIDATE))
            .arg("scripts/mode_matrix.py")
            .output()
            .unwrap();
        let report = transcript(&output);
        assert!(
            !output.status.success(),
            "{case}: the matrix must fail loud without the real utils.sh\n{report}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("utils.sh: the repo asset is missing or is not the real file"),
            "{case}: the failure must name utils.sh\n{report}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains("utils.sh: real asset"),
            "{case}: a stubbed or missing utils.sh was reported as the real asset\n{report}"
        );
        assert!(
            !stdout.contains("combination walk"),
            "{case}: the walk started despite the missing utils.sh\n{report}"
        );
        assert!(
            !stdout.lines().any(|l| l.starts_with("FAIL ")),
            "{case}: the marker-map checks that run before the exit must still pass\n{report}"
        );
        assert!(
            fs::read_to_string(agents_dir().join(".shared").join("utils.sh"))
                .unwrap()
                .contains("set -euo pipefail"),
            "{case}: the shipped .shared/utils.sh was altered"
        );
    }
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
/// `config.yaml`, a prose `architect/config.yaml` whose variable list has
/// drifted, and an `architect-next/graph.yaml` that is not YAML at all.
/// Returns the planted (relative path, bytes) pairs.
fn poison_user_config(scratch: &TempDirGuard) -> Vec<(String, Vec<u8>)> {
    let home = scratch.0.join("home");
    let planted = [
        (".config/coyote/agents/.shared/utils.sh", "exit 97\n"),
        (".config/coyote/config.yaml", "not: [valid\n"),
        (
            ".config/coyote/agents/architect/config.yaml",
            "name: architect\nvariables:\n  - name: drifted\n    default: 'x'\n",
        ),
        (
            ".config/coyote/agents/architect-next/graph.yaml",
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
/// nothing, and nothing is written under `HOME` either. The audit and the
/// session check are the legs that prove the poison is ignored, so the test
/// skips without the mikefarah `yq` or `bash`. `smoke_test.py` re-points
/// `HOME` at its own temp dir before importing anything and never sees the
/// poison; its leg only adds the check that nothing lands under the scratch
/// `HOME`.
#[test]
fn batteries_never_consult_the_user_config_dir() {
    let Some(python) = python3() else { return };
    let Some(yq) = mikefarah_yq() else { return };
    let Ok(bash) = which::which("bash") else {
        eprintln!("skipping: bash not available");
        return;
    };
    let scratch = scratch_dir("poison");
    let planted = poison_user_config(&scratch);
    let home = scratch.0.join("home");

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
            .any(|l| l.starts_with("ok variables mirror architect")),
        "{}",
        transcript(&session)
    );

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
        .env_remove("ARCHITECT_RUNS_ROOT")
        .env_remove("COYOTE_CACHE_DIR")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("LOCALAPPDATA");
    scrub_agent_vars(&mut cmd);
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
            ("ARCHITECT_RUNS_ROOT", override_root.as_path()),
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
        cache.join("architect-next").join("runs")
    );
    assert_eq!(
        PathBuf::from(&run),
        cache.join("architect-next").join("runs").join("r1")
    );

    let (root, _) = runs_root_under(&python, &scratch, &[("XDG_CACHE_HOME", xdg.as_path())]);
    assert_eq!(
        PathBuf::from(&root),
        xdg.join("coyote").join("architect-next").join("runs")
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
            .join("architect-next")
            .join("runs"),
        "platform tier"
    );
    assert_no_bytecode_under_candidate();
}

/// An empty `ARCHITECT_RUNS_ROOT` is no override: a consumer who clears it
/// with `ARCHITECT_RUNS_ROOT=` gets the `COYOTE_CACHE_DIR` tier, not a run
/// root at the current directory.
#[test]
fn empty_runs_root_override_falls_back_to_the_cache_dir_tier() {
    let Some(python) = python3() else { return };
    let scratch = scratch_dir("emptyoverride");
    let cache = scratch.0.join("cache");
    let (root, run) = runs_root_under(
        &python,
        &scratch,
        &[
            ("ARCHITECT_RUNS_ROOT", Path::new("")),
            ("COYOTE_CACHE_DIR", cache.as_path()),
        ],
    );
    let expected = cache.join(CANDIDATE).join("runs");
    assert_eq!(
        PathBuf::from(&root),
        expected,
        "an empty override falls back to the cache dir tier"
    );
    assert_eq!(PathBuf::from(&run), expected.join("r1"));
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

fn description_of<'a>(vars: &'a [serde_yaml::Value], name: &str) -> &'a str {
    vars.iter()
        .find(|v| v["name"].as_str() == Some(name))
        .and_then(|v| v["description"].as_str())
        .unwrap_or_else(|| panic!("no {name} variable with a description"))
}

/// `build_lock` is declared where the prose twin declares it (right after
/// `pr_strategy`, right before `task_granularity`) with an empty default,
/// and the candidate's description is the prose twin's byte for byte.
#[test]
fn build_lock_is_declared_at_the_prose_twins_position() {
    let candidate = variable_list(&candidate_dir().join("graph.yaml"));
    let prose = variable_list(&agents_dir().join("architect/config.yaml"));
    for (label, vars) in [("architect-next", &candidate), ("architect", &prose)] {
        let names = names_of(vars);
        let at = names
            .iter()
            .position(|n| *n == "build_lock")
            .unwrap_or_else(|| panic!("{label} declares no build_lock: {names:?}"));
        assert_eq!(
            at.checked_sub(1).and_then(|i| names.get(i)),
            Some(&"pr_strategy"),
            "{label}: {names:?}"
        );
        assert_eq!(
            names.get(at + 1),
            Some(&"task_granularity"),
            "{label}: {names:?}"
        );
        assert_eq!(vars[at]["default"].as_str(), Some(""), "{label} default");
    }
    assert_eq!(
        description_of(&candidate, "build_lock"),
        description_of(&prose, "build_lock"),
        "the candidate's build_lock description must equal the prose twin's"
    );
}

/// The candidate declares the prose twin's variables in the same order, name
/// for name and `default` for `default`, with a missing default counted as
/// null on both sides. `check_session_block.sh` runs the same comparison but
/// needs the mikefarah `yq`; this one runs without it.
#[test]
fn variables_mirror_the_prose_twin_name_for_name_and_default_for_default() {
    let candidate = variable_list(&candidate_dir().join("graph.yaml"));
    let prose = variable_list(&agents_dir().join("architect/config.yaml"));
    assert_eq!(
        names_of(&candidate),
        names_of(&prose),
        "the candidate's variable names drifted from the prose twin's"
    );
    let defaults = |vars: &[serde_yaml::Value]| -> Vec<serde_yaml::Value> {
        vars.iter()
            .map(|v| v.get("default").cloned().unwrap_or(serde_yaml::Value::Null))
            .collect()
    };
    for ((name, ours), theirs) in names_of(&candidate)
        .iter()
        .zip(defaults(&candidate))
        .zip(defaults(&prose))
    {
        assert_eq!(
            ours, theirs,
            "{name}: the default drifted from the prose twin's"
        );
    }
}

/// Lines the toolchain pattern matches without naming a toolchain command,
/// as (file name, substring the line must carry): the executable-criterion
/// classifier regex in `_lib.py` (its alternation lists toolchain words to
/// recognise one in an acceptance criterion, it runs none), the
/// dependency-manifest regex in `gates.py` (the hit is `yarn`/`pnpm` inside
/// the lockfile names), and the English adverb `just` in a `gates.py`
/// comment and in a `gates.py` prompt sentence.
const EXEMPT: [(&str, &str); 4] = [
    ("_lib.py", "(go|cargo|npm|pnpm|yarn|make|pytest|"),
    ("gates.py", "yarn\\.lock$|pnpm-lock\\.yaml$"),
    ("gates.py", "An amendment round just ran"),
    ("gates.py", "just do not write to them"),
];

/// The gates stay project-agnostic: no script or graph prompt names a
/// toolchain command (verification commands come from the brief). The smoke
/// battery's fixtures are the one place such words may appear freely; the
/// four lines in `EXEMPT` match the pattern for a documented non-command
/// reason, and each must still be present exactly once so a deleted or moved
/// line surfaces here.
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
    assert!(files.len() > 60, "the sweep saw only {} files", files.len());
    let mut hits = Vec::new();
    let mut exemptions_used = [0usize; EXEMPT.len()];
    for path in files {
        let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).unwrap();
        for (idx, line) in text.lines().enumerate() {
            if !re.is_match(line).unwrap() {
                continue;
            }
            let exempt = EXEMPT
                .iter()
                .position(|(file, needle)| *file == file_name && line.contains(needle));
            match exempt {
                Some(i) => exemptions_used[i] += 1,
                None => hits.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim())),
            }
        }
    }
    assert!(
        hits.is_empty(),
        "toolchain commands named outside fixtures:\n{}",
        hits.join("\n")
    );
    for (i, (file, needle)) in EXEMPT.iter().enumerate() {
        assert_eq!(
            exemptions_used[i], 1,
            "exemption {file}: {needle:?} must match exactly one line"
        );
    }
}

/// Every `scripts/*.py` shaped like a script-node shim (docstring naming the
/// node, `from gates import run`, a single `run("<name>")` call) names its
/// own file stem in both places, and the shims map one-to-one onto the
/// `type: script` nodes of `graph.yaml`, and every `run()` literal is a key of
/// the `GATES` dict in `gates.py`. `audit_graph.py` only extracts the first
/// `run()` literal of a shim and never checks it against `GATES`, so a shim
/// that called `run("other")` would pass the audit and silently run the wrong
/// gate, and a misspelt literal would pass it too and raise `KeyError` in
/// `gates.run` at runtime; this is the pin that catches both.
#[test]
fn every_script_shim_runs_the_gate_named_by_its_file_and_graph_node() {
    let scripts = candidate_dir().join("scripts");
    let run_re = fancy_regex::Regex::new(r#"(?m)^run\("([a-z_0-9]+)"\)$"#).unwrap();
    let doc_re =
        fancy_regex::Regex::new(r#"^"""Entry point for the `([a-z_0-9]+)` script node"#).unwrap();
    let mut shims = Vec::new();
    for entry in fs::read_dir(&scripts).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "py") {
            continue;
        }
        let body = fs::read_to_string(&path).unwrap();
        if !body.contains("from gates import run") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let calls: Vec<String> = run_re
            .captures_iter(&body)
            .map(|c| c.unwrap()[1].to_string())
            .collect();
        assert_eq!(
            calls,
            vec![stem.clone()],
            "{stem}.py must call run(\"{stem}\") exactly once"
        );
        let doc_line = body.lines().nth(1).unwrap_or_default();
        let documented = doc_re.captures(doc_line).unwrap().unwrap_or_else(|| {
            panic!("{stem}.py: the docstring does not name a script node: {doc_line:?}")
        });
        assert_eq!(
            &documented[1], stem,
            "{stem}.py: the docstring names another node"
        );
        shims.push(stem);
    }
    shims.sort();
    assert!(
        shims.len() >= 70,
        "the shim sweep found only {} shims: {shims:?}",
        shims.len()
    );

    let text = fs::read_to_string(candidate_dir().join("graph.yaml")).unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    let mut script_nodes = Vec::new();
    for (id, node) in doc["nodes"]
        .as_mapping()
        .expect("graph.yaml has a nodes mapping")
    {
        if node["type"].as_str() != Some("script") {
            continue;
        }
        let id = id.as_str().unwrap().to_string();
        assert_eq!(
            node["script"].as_str(),
            Some(format!("scripts/{id}.py").as_str()),
            "script node {id} does not run scripts/{id}.py"
        );
        script_nodes.push(id);
    }
    script_nodes.sort();
    assert_eq!(
        shims, script_nodes,
        "shims and script nodes are not one-to-one"
    );

    let gates = fs::read_to_string(scripts.join("gates.py")).unwrap();
    let dict = gates
        .split_once("\nGATES = {\n")
        .and_then(|(_, rest)| rest.split_once("\n}\n"))
        .expect("gates.py has a `GATES = {` dict closed by a `}` line")
        .0;
    let key_re = fancy_regex::Regex::new(r#""([a-z_0-9]+)":"#).unwrap();
    let keys: Vec<String> = key_re
        .captures_iter(dict)
        .map(|c| c.unwrap()[1].to_string())
        .collect();
    assert!(
        keys.len() >= 70,
        "the GATES parse found only {} keys: {keys:?}",
        keys.len()
    );
    let missing: Vec<&String> = shims.iter().filter(|s| !keys.contains(s)).collect();
    assert!(
        missing.is_empty(),
        "shims call run() with a name that is not a GATES key: {missing:?}"
    );
}

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

/// A scratch tree holding the three agent dirs the batteries reach: the
/// candidate, the prose `architect` it reads core tools from, and `.shared`.
/// Returns the candidate copy.
fn scratch_candidate_copy(scratch: &TempDirGuard) -> PathBuf {
    let agents = scratch.0.join("tree").join("agents");
    for name in [".shared", "architect"] {
        copy_tree(&agents_dir().join(name), &agents.join(name));
    }
    let copy = agents.join(CANDIDATE);
    copy_tree(&candidate_dir(), &copy);
    copy
}

/// The node trace of one real-binary dry run of the candidate, in order,
/// from `graph: architect-next` to the moment `ask_user` starts.
const EXPECTED_WALK: [&str; 7] = [
    "graph: architect-next (start: intake)",
    "intake (script) done",
    "orient_read (llm) done",
    "orient_fault (script) done",
    "escalate_gate (script) done",
    "ask_dispatch (script) done",
    "ask_user (input) start",
];

/// A scratch `COYOTE_CONFIG_DIR` holding the three agent dirs the candidate
/// reaches at run time (`.shared`, the prose `architect` whose `tools.sh` the
/// gates call, and the candidate itself), a dry-run `config.yaml`, and a
/// project dir with one readable design doc. Returns the project dir.
fn dry_run_config_dir(scratch: &TempDirGuard) -> PathBuf {
    dry_run_config_dir_with(scratch, true)
}

/// `dry_run_config_dir` with the agent copies optional: a consumer's first
/// run starts from a config dir that holds nothing but `config.yaml`, and the
/// engine installs the bundled agents (the candidate included) on that run.
fn dry_run_config_dir_with(scratch: &TempDirGuard, copy_agents: bool) -> PathBuf {
    let dir = &scratch.0;
    if copy_agents {
        let agents = dir.join("agents");
        for name in [".shared", "architect", CANDIDATE] {
            copy_tree(&agents_dir().join(name), &agents.join(name));
        }
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
    let project = dir.join("project");
    fs::create_dir_all(project.join("plans")).unwrap();
    fs::write(
        project.join("plans").join("PLAN-x.md"),
        "# Design: probe\n\n## Goal\nA one-task probe design.\n",
    )
    .unwrap();
    project
}

/// The brief every dry run below starts from unless it re-enters a run.
const DRY_RUN_PROMPT: &str = "Implement the plan at plans/PLAN-x.md";

/// Dry-runs the candidate by name through the real binary until the trace
/// reaches `ask_user` (an input node that waits on a terminal, so the child
/// is killed there) and returns the trace lines seen. `extra_env` lays
/// run-dir variables over the scratch defaults.
fn dry_run_until_ask_user(
    scratch: &TempDirGuard,
    project: &Path,
    extra_env: &[(&str, PathBuf)],
    label: &str,
) -> Vec<String> {
    dry_run_until_ask_user_with_prompt(scratch, project, extra_env, label, DRY_RUN_PROMPT)
}

/// `dry_run_until_ask_user` with the prompt chosen by the caller.
fn dry_run_until_ask_user_with_prompt(
    scratch: &TempDirGuard,
    project: &Path,
    extra_env: &[(&str, PathBuf)],
    label: &str,
    prompt: &str,
) -> Vec<String> {
    let dir = &scratch.0;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coyote"));
    cmd.args(["--agent", CANDIDATE, "--dry-run", prompt])
        .current_dir(project)
        .env("HOME", dir.join("home"))
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("TMPDIR", dir.join("tmp"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("NO_COLOR", "1")
        .env_remove("ARCHITECT_RUNS_ROOT")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("IS_SANDBOX")
        .env_remove("GRAPH_STATE")
        .env_remove("GRAPH_STATE_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_agent_vars(&mut cmd);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    run_until_ask_user(cmd, label)
}

/// Spawns a fully configured dry-run command and reads its trace until
/// `ask_user` starts (or the binary exits / the deadline passes), then kills
/// the child and returns the lines seen.
fn run_until_ask_user(mut cmd: Command, label: &str) -> Vec<String> {
    let mut child = cmd.spawn().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    for stream in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut lines = Vec::new();
    let mut reached_ask_user = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                let done = line.contains("ask_user (input) start");
                lines.push(line);
                if done {
                    reached_ask_user = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait().unwrap() {
                    lines.extend(rx.try_iter());
                    panic!(
                        "{label}: the binary exited ({status}) before ask_user:\n{}",
                        lines.join("\n")
                    );
                }
            }
            // Both pipes closed: the binary is gone, whatever its status. The
            // kill is a no-op on a child that has already exited and keeps one
            // that closed its output while still running from hanging the
            // test; `wait` then reports the real exit status.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let status = child.wait().unwrap();
                panic!(
                    "{label}: the binary exited ({status}) before ask_user:\n{}",
                    lines.join("\n")
                );
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        reached_ask_user,
        "{label}: ask_user never started within the deadline:\n{}",
        lines.join("\n")
    );
    lines
}

/// Asserts `root` holds exactly one run dir carrying the gates' durable
/// state and the brief the run was started with; returns the run dir.
fn single_run_under(root: &Path, label: &str) -> PathBuf {
    let mut runs: Vec<PathBuf> = fs::read_dir(root)
        .unwrap_or_else(|e| panic!("{label}: runs root {} unreadable: {e}", root.display()))
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        runs.len(),
        1,
        "{label}: exactly one run dir under {}: {runs:?}",
        root.display()
    );
    let run = runs.pop().unwrap();
    for file in ["state.json", "brief.md", "decisions.md"] {
        assert!(
            run.join(file).is_file(),
            "{label}: {file} missing under {}",
            run.display()
        );
    }
    let brief = fs::read_to_string(run.join("brief.md")).unwrap();
    assert!(
        brief.contains(DRY_RUN_PROMPT),
        "{label}: brief.md does not carry the prompt: {brief:?}"
    );
    run
}

/// The candidate is reachable only as `coyote -a architect-next`: run that
/// way from a scratch config dir against a readable design doc, the real
/// binary walks `intake -> orient_read -> orient_fault -> escalate_gate ->
/// ask_dispatch -> ask_user` in that order (the dry-run model answers no
/// schema, so `orient_read` falls back to `orient_fault`), and the gates put
/// the run dir at `$COYOTE_CACHE_DIR/architect-next/runs/<run_id>/`, not
/// anywhere under `HOME`. With `ARCHITECT_RUNS_ROOT` set the run lands
/// directly under that root and the cache dir gets no `architect-next` at all.
#[test]
fn dry_run_by_name_walks_to_ask_user_and_pins_the_run_dir() {
    if python3().is_none() {
        return;
    }
    let scratch = scratch_dir("dryrun");
    let project = dry_run_config_dir(&scratch);
    let dir = scratch.0.clone();

    let lines = dry_run_until_ask_user(&scratch, &project, &[], "cache dir");
    let mut cursor = 0;
    for needle in EXPECTED_WALK {
        let hit = lines[cursor..].iter().position(|l| l.contains(needle));
        let idx = hit.unwrap_or_else(|| {
            panic!(
                "cache dir: {needle:?} missing after line {cursor}:\n{}",
                lines.join("\n")
            )
        });
        cursor += idx + 1;
    }
    let runs_root = dir.join("cache").join(CANDIDATE).join("runs");
    single_run_under(&runs_root, "cache dir");
    // The engine's own tooling may populate `HOME` (uv caches and the like);
    // what must not appear is the candidate's former run root or any run
    // state named after it.
    let legacy = dir.join("home").join(".local").join("state").join("coyote");
    assert!(
        !legacy.exists(),
        "cache dir: legacy run root created at {}",
        legacy.display()
    );
    let leaked: Vec<String> = files_under(&dir.join("home"))
        .into_iter()
        .filter(|rel| rel.contains("architect"))
        .collect();
    assert!(
        leaked.is_empty(),
        "cache dir: run state leaked under HOME: {leaked:?}"
    );

    let override_root = dir.join("override");
    dry_run_until_ask_user(
        &scratch,
        &project,
        &[("ARCHITECT_RUNS_ROOT", override_root.clone())],
        "override",
    );
    single_run_under(&override_root, "override");
    assert_eq!(
        fs::read_dir(&runs_root).unwrap().count(),
        1,
        "override: the cache-dir root must not gain a second run"
    );
    assert_no_bytecode_under_candidate();
}

/// A consumer's first run: a config dir holding nothing but `config.yaml`
/// (no agent copied in), `coyote -a architect-next --dry-run ...`. The
/// engine installs the bundled agents on that run (the candidate among
/// them), walks the same `intake -> ... -> ask_user` path and puts the run
/// dir under `$COYOTE_CACHE_DIR/architect-next/runs/`.
#[test]
fn dry_run_by_name_from_an_empty_config_dir_installs_the_candidate_and_walks_to_ask_user() {
    if python3().is_none() {
        return;
    }
    let scratch = scratch_dir("coldstart");
    let project = dry_run_config_dir_with(&scratch, false);
    let dir = scratch.0.clone();
    assert!(
        !dir.join("agents").exists(),
        "cold start: the config dir must hold no agents before the run"
    );

    let lines = dry_run_until_ask_user(&scratch, &project, &[], "cold start");
    let mut cursor = 0;
    for needle in EXPECTED_WALK {
        let hit = lines[cursor..].iter().position(|l| l.contains(needle));
        let idx = hit.unwrap_or_else(|| {
            panic!(
                "cold start: {needle:?} missing after line {cursor}:\n{}",
                lines.join("\n")
            )
        });
        cursor += idx + 1;
    }
    // What the engine installs is the shipped candidate, file for file and
    // byte for byte; the engine-managed `bin/` functions dir is its own.
    let installed = dir.join("agents").join(CANDIDATE);
    let shipped_files = files_under(&candidate_dir());
    let installed_files: Vec<String> = files_under(&installed)
        .into_iter()
        .filter(|rel| !rel.starts_with("bin/"))
        .collect();
    assert_eq!(
        installed_files, shipped_files,
        "cold start: the installed {CANDIDATE} file set differs from the shipped one"
    );
    for rel in &shipped_files {
        assert_eq!(
            fs::read(installed.join(rel)).unwrap(),
            fs::read(candidate_dir().join(rel)).unwrap(),
            "cold start: installed {CANDIDATE}/{rel} differs from the shipped file"
        );
    }
    single_run_under(
        &dir.join("cache").join(CANDIDATE).join("runs"),
        "cold start",
    );
    assert_no_bytecode_under_candidate();
}

/// The README's run-state bullet, middle tier, through the real binary: with
/// `COYOTE_CACHE_DIR` and `ARCHITECT_RUNS_ROOT` both unset and
/// `XDG_CACHE_HOME` set, a cold-start dry run walks to `ask_user` and puts
/// the run dir under `$XDG_CACHE_HOME/coyote/architect-next/runs/`, the same
/// `coyote` cache dir the engine itself writes to, so the candidate and the
/// engine never disagree about where the cache is. Nothing architect-shaped
/// lands under `HOME`.
#[test]
fn dry_run_with_only_xdg_cache_home_puts_the_run_under_the_engines_cache_dir() {
    if python3().is_none() {
        return;
    }
    let scratch = scratch_dir("xdgtier");
    let project = dry_run_config_dir_with(&scratch, false);
    let dir = scratch.0.clone();
    let xdg = dir.join("xdg");
    fs::create_dir_all(&xdg).unwrap();

    // `dry_run_until_ask_user` pins `COYOTE_CACHE_DIR` to the scratch dir;
    // this tier needs it absent, so drive the same command line by hand.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coyote"));
    cmd.args(["--agent", CANDIDATE, "--dry-run", DRY_RUN_PROMPT])
        .current_dir(&project)
        .env("HOME", dir.join("home"))
        .env("COYOTE_CONFIG_DIR", &dir)
        .env("XDG_CACHE_HOME", &xdg)
        .env("TMPDIR", dir.join("tmp"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_CACHE_DIR")
        .env_remove("ARCHITECT_RUNS_ROOT")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("IS_SANDBOX")
        .env_remove("GRAPH_STATE")
        .env_remove("GRAPH_STATE_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_agent_vars(&mut cmd);
    let lines = run_until_ask_user(cmd, "xdg tier");
    let mut cursor = 0;
    for needle in EXPECTED_WALK {
        let hit = lines[cursor..].iter().position(|l| l.contains(needle));
        let idx = hit.unwrap_or_else(|| {
            panic!(
                "xdg tier: {needle:?} missing after line {cursor}:\n{}",
                lines.join("\n")
            )
        });
        cursor += idx + 1;
    }

    let engine_cache = xdg.join("coyote");
    single_run_under(&engine_cache.join(CANDIDATE).join("runs"), "xdg tier");
    // `scratch_dir` pre-creates `cache/`; the run must not have used it.
    assert!(
        fs::read_dir(dir.join("cache")).unwrap().next().is_none(),
        "xdg tier: a run landed in the scratch cache dir although COYOTE_CACHE_DIR was unset"
    );
    let leaked: Vec<String> = files_under(&dir.join("home"))
        .into_iter()
        .filter(|rel| rel.contains("architect"))
        .collect();
    assert!(
        leaked.is_empty(),
        "xdg tier: run state leaked under HOME: {leaked:?}"
    );
    assert_no_bytecode_under_candidate();
}

/// The README's run-state bullet: "A prompt containing `Architect run id:
/// <id>` re-enters that run." After a first dry run has created
/// `<cache>/architect-next/runs/<id>/`, a second dry run whose prompt names
/// that id walks `intake -> resume_route -> ...` (no fresh `orient_read`),
/// reaches `ask_user`, and leaves exactly the one run dir in place.
#[test]
fn dry_run_with_the_run_id_in_the_prompt_re_enters_the_run_instead_of_starting_another() {
    if python3().is_none() {
        return;
    }
    let scratch = scratch_dir("reenter");
    let project = dry_run_config_dir(&scratch);
    let runs_root = scratch.0.join("cache").join(CANDIDATE).join("runs");

    dry_run_until_ask_user(&scratch, &project, &[], "first run");
    let run = single_run_under(&runs_root, "first run");
    let run_id = run.file_name().unwrap().to_string_lossy().into_owned();

    let lines = dry_run_until_ask_user_with_prompt(
        &scratch,
        &project,
        &[],
        "re-entry",
        &format!("Architect run id: {run_id} -- continue"),
    );
    let mut cursor = 0;
    for needle in [
        "graph: architect-next (start: intake)",
        "intake (script) done",
        "resume_route (script) done",
        "ask_user (input) start",
    ] {
        let hit = lines[cursor..].iter().position(|l| l.contains(needle));
        let idx = hit.unwrap_or_else(|| {
            panic!(
                "re-entry: {needle:?} missing after line {cursor}:\n{}",
                lines.join("\n")
            )
        });
        cursor += idx + 1;
    }
    assert!(
        !lines.iter().any(|l| l.contains("orient_read (llm) start")),
        "re-entry: the run was re-oriented from scratch instead of resumed:\n{}",
        lines.join("\n")
    );
    let runs: Vec<String> = fs::read_dir(&runs_root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        runs,
        vec![run_id.clone()],
        "re-entry: naming the run id must not start a second run"
    );
    assert_no_bytecode_under_candidate();
}

/// The converse of re-entry: the README promises that a prompt naming a run
/// id re-enters *that* run, so an id with no durable state behind it must
/// not be treated as one. A first dry run whose prompt names an id nobody
/// minted still reaches `ask_user` (the binary neither exits nor hangs on
/// the missing state), takes the fresh-run path (`resume_route` never
/// runs), and leaves exactly one run dir whose id is the engine's own, not
/// the one the prompt named.
#[test]
fn dry_run_with_an_unknown_run_id_in_the_prompt_starts_a_fresh_run() {
    if python3().is_none() {
        return;
    }
    let scratch = scratch_dir("unknownid");
    let project = dry_run_config_dir(&scratch);
    let runs_root = scratch.0.join("cache").join(CANDIDATE).join("runs");
    let bogus = "20991231T235959-deadbeef";

    let lines = dry_run_until_ask_user_with_prompt(
        &scratch,
        &project,
        &[],
        "unknown id",
        &format!("Architect run id: {bogus} -- continue"),
    );
    let mut cursor = 0;
    for needle in [
        "graph: architect-next (start: intake)",
        "intake (script) done",
        "ask_user (input) start",
    ] {
        let hit = lines[cursor..].iter().position(|l| l.contains(needle));
        let idx = hit.unwrap_or_else(|| {
            panic!(
                "unknown id: {needle:?} missing after line {cursor}:\n{}",
                lines.join("\n")
            )
        });
        cursor += idx + 1;
    }
    assert!(
        !lines.iter().any(|l| l.contains("resume_route (script)")),
        "unknown id: an id with no durable state was resumed:\n{}",
        lines.join("\n")
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("Traceback") || l.contains("panicked")),
        "unknown id: the gates crashed instead of starting a fresh run:\n{}",
        lines.join("\n")
    );

    let runs: Vec<String> = fs::read_dir(&runs_root)
        .unwrap_or_else(|e| {
            panic!(
                "unknown id: runs root {} unreadable: {e}",
                runs_root.display()
            )
        })
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        runs.len(),
        1,
        "unknown id: exactly one fresh run dir under {}: {runs:?}",
        runs_root.display()
    );
    assert_ne!(
        runs[0], bogus,
        "unknown id: the run dir took the prompt's id instead of a minted one"
    );
    let state = fs::read_to_string(runs_root.join(&runs[0]).join("state.json")).unwrap();
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert_eq!(state["run_id"].as_str(), Some(runs[0].as_str()));
    assert_ne!(
        state["resume_mode"].as_bool(),
        Some(true),
        "unknown id: state.json records a resume that never happened"
    );
    assert_no_bytecode_under_candidate();
}

/// A brief that names no readable design doc is the protocol's first
/// escalation, not a crash and not an orientation attempt. Whether the brief
/// points at a `.md` path that does not exist or carries no path at all, the
/// real binary walks `intake -> escalate_gate -> ask_dispatch -> ask_user`
/// (never `orient_read`: there is nothing to orient), the run dir still
/// lands under `$COYOTE_CACHE_DIR/architect-next/runs/<run_id>/` with the
/// brief, `state.json` and the decisions ledger, and `state.json` records the
/// run as parked on a `need_design_doc` escalation whose question asks for
/// the path and whose only fixed option aborts. The ledger carries the mode
/// line and the escalation, so a consumer who re-enters the run by id finds
/// the question that stopped it.
#[test]
fn dry_run_without_a_readable_design_doc_parks_on_need_design_doc_before_orienting() {
    if python3().is_none() {
        return;
    }
    for (label, prompt) in [
        ("missing doc", "Implement the plan at plans/PLAN-missing.md"),
        ("no path", "Implement the plan"),
    ] {
        let scratch = scratch_dir(&format!("nodoc-{}", label.replace(' ', "-")));
        let project = dry_run_config_dir(&scratch);
        // The fixture seeds one readable design doc under `plans/`; neither
        // prompt names it, so the project holds no doc the brief resolves to.
        let runs_root = scratch.0.join("cache").join(CANDIDATE).join("runs");

        let lines = dry_run_until_ask_user_with_prompt(&scratch, &project, &[], label, prompt);
        let mut cursor = 0;
        for needle in [
            "graph: architect-next (start: intake)",
            "intake (script) done",
            "escalate_gate (script) done",
            "ask_dispatch (script) done",
            "ask_user (input) start",
        ] {
            let hit = lines[cursor..].iter().position(|l| l.contains(needle));
            let idx = hit.unwrap_or_else(|| {
                panic!(
                    "{label}: {needle:?} missing after line {cursor}:\n{}",
                    lines.join("\n")
                )
            });
            cursor += idx + 1;
        }
        assert!(
            !lines.iter().any(|l| l.contains("orient_read (llm)")),
            "{label}: the graph tried to orient a design doc it never had:\n{}",
            lines.join("\n")
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("Traceback") || l.contains("panicked")),
            "{label}: the gates crashed instead of parking the run:\n{}",
            lines.join("\n")
        );

        let runs: Vec<PathBuf> = fs::read_dir(&runs_root)
            .unwrap_or_else(|e| {
                panic!("{label}: runs root {} unreadable: {e}", runs_root.display())
            })
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(
            runs.len(),
            1,
            "{label}: exactly one run dir under {}: {runs:?}",
            runs_root.display()
        );
        let run = &runs[0];
        for file in ["state.json", "brief.md", "decisions.md"] {
            assert!(
                run.join(file).is_file(),
                "{label}: {file} missing under {}",
                run.display()
            );
        }
        let brief = fs::read_to_string(run.join("brief.md")).unwrap();
        assert!(
            brief.contains(prompt),
            "{label}: brief.md does not carry the prompt: {brief:?}"
        );

        let state = fs::read_to_string(run.join("state.json")).unwrap();
        let state: serde_json::Value = serde_json::from_str(&state).unwrap();
        assert_eq!(state["phase"].as_str(), Some("parked"), "{label}: phase");
        let escalation = &state["escalation"];
        assert_eq!(
            escalation["kind"].as_str(),
            Some("need_design_doc"),
            "{label}: escalation kind: {escalation}"
        );
        let question = escalation["question"].as_str().unwrap_or_default();
        assert!(
            question.contains("design doc") && question.contains("path"),
            "{label}: the question does not ask for the design doc path: {question:?}"
        );
        let options: Vec<&str> = escalation["options"]
            .as_array()
            .map(|o| o.iter().filter_map(|v| v["text"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(options, ["Abort the run"], "{label}: fixed options");

        let ledger = fs::read_to_string(run.join("decisions.md")).unwrap();
        assert!(
            ledger.contains("escalation: need_design_doc:"),
            "{label}: the decisions ledger does not record the escalation:\n{ledger}"
        );
        assert!(
            ledger.contains("mode[pr_strategy=single parallel_tasks=0 task_granularity=standard autonomy=full escalation_policy=interactive]"),
            "{label}: the decisions ledger does not record the default mode line:\n{ledger}"
        );
    }
    assert_no_bytecode_under_candidate();
}

// ---------------------------------------------------------------------------
// README ground truth. The candidate's README is user documentation: every
// file:line it cites, every count it states and every node, agent and pin it
// names must match the shipped files, and the one gap it documents must still
// be open. These flip the moment a later landing moves a line or closes the
// gap without updating the README.
// ---------------------------------------------------------------------------

fn readme() -> String {
    fs::read_to_string(candidate_dir().join("README.md")).unwrap()
}

/// The first integer captured by `pattern` in the README.
fn readme_number(pattern: &str) -> usize {
    let re = fancy_regex::Regex::new(pattern).unwrap();
    let text = readme();
    let caps = re
        .captures(&text)
        .unwrap()
        .unwrap_or_else(|| panic!("README no longer states {pattern:?}"));
    caps[1].parse().unwrap()
}

/// The README's `description provenance` row: the shipped `description:`
/// names no file that lives outside this repo. Every path-shaped token in it
/// (a `dir/name` or a `name.ext` for the extensions the planning store uses)
/// resolves under `assets/agents/`, and the planning-store prefix `plans/`
/// does not appear at all.
#[test]
fn description_names_no_file_outside_the_repo() {
    let text = fs::read_to_string(candidate_dir().join("graph.yaml")).unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    assert!(
        readme().contains("| description provenance |"),
        "the README no longer carries the description provenance row"
    );
    let description = doc["description"]
        .as_str()
        .expect("graph.yaml carries a string description");
    assert!(
        !description.contains("plans/"),
        "description points into the planning store:\n{description}"
    );
    for dropped in ["architect-graph-design", "suite-performance-draft"] {
        assert!(
            !description.contains(dropped),
            "description still cites the out-of-repo file {dropped}"
        );
    }
    let token_re = fancy_regex::Regex::new(
        r"\b[A-Za-z0-9_.-]+/[A-Za-z0-9_./-]+\b|\b[A-Za-z0-9_-]+\.(?:md|ya?ml|py|sh|json)\b",
    )
    .unwrap();
    let mut checked = 0;
    for cap in token_re.captures_iter(description) {
        let token = cap.unwrap()[0].to_string();
        // Prose slashes like `author/amend` or `oracle / explore` are not paths;
        // only tokens whose first segment is a shipped agent dir or `.shared`
        // count, and those must resolve.
        let first = token.split('/').next().unwrap();
        let base = if first == "agents" {
            agents_dir().join(token.trim_start_matches("agents/"))
        } else if agents_dir().join(first).is_dir() || token.contains('.') {
            agents_dir().join(&token)
        } else {
            continue;
        };
        checked += 1;
        assert!(
            base.exists(),
            "description cites {token}, which is not under assets/agents/"
        );
    }
    assert!(
        checked >= 1,
        "the description no longer names the facade agents/architect; update this pin"
    );
}

/// Every shipped file is ASCII on disk (the README's em-dash row), and the
/// marker map keeps exactly the `\u2014` JSON escapes the README counts, all
/// of them inside `first` lines.
#[test]
fn shipped_candidate_is_pure_ascii_and_the_marker_map_keeps_its_escapes() {
    let mut offenders = Vec::new();
    let mut seen = 0;
    for rel in files_under(&candidate_dir()) {
        if rel.contains("__pycache__") {
            continue;
        }
        seen += 1;
        let bytes = fs::read(candidate_dir().join(&rel)).unwrap();
        if let Some(pos) = bytes.iter().position(|b| !b.is_ascii()) {
            let line = bytes[..pos].iter().filter(|b| **b == b'\n').count() + 1;
            offenders.push(format!("{rel}:{line}: byte 0x{:02x}", bytes[pos]));
        }
    }
    assert!(seen > 80, "the ASCII sweep saw only {seen} files");
    assert!(
        offenders.is_empty(),
        "non-ASCII bytes in shipped files:\n{}",
        offenders.join("\n")
    );

    let expected = readme_number(r"JSON escapes for (\d+) lines");
    let raw = fs::read_to_string(candidate_dir().join("scripts/marker_map.json")).unwrap();
    assert_eq!(
        raw.matches("\\u2014").count(),
        expected,
        "marker_map.json carries a different number of \\u2014 escapes than the README states"
    );
    let map: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
    let in_first = map
        .iter()
        .filter(|e| e["first"].as_str().unwrap_or("").contains('\u{2014}'))
        .count();
    assert_eq!(
        in_first, expected,
        "every escape must sit in a `first` line"
    );
}

/// The README's toolchain census block is the live output of the grep it
/// shows: the same file:line set, no more and no fewer, and the `gates.py`
/// hits are exactly the lockfile regex plus the two prose lines it cites.
#[test]
fn readme_toolchain_census_matches_the_live_grep() {
    let re = fancy_regex::Regex::new(
        r"\b(cargo|npm|pnpm|yarn|go (test|build)|pytest|mvn|gradle|make)\b",
    )
    .unwrap();
    let mut live = Vec::new();
    let mut gates_lines = Vec::new();
    let mut paths: Vec<PathBuf> = fs::read_dir(candidate_dir().join("scripts"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "py"))
        .collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).unwrap();
        for (idx, line) in text.lines().enumerate() {
            if re.is_match(line).unwrap() {
                live.push(format!("scripts/{name}:{}", idx + 1));
                if name == "gates.py" {
                    gates_lines.push(idx + 1);
                }
            }
        }
    }

    let cited_re = fancy_regex::Regex::new(r"(?m)^(scripts/[\w.]+\.py:\d+):").unwrap();
    let text = readme();
    let cited: Vec<String> = cited_re
        .captures_iter(&text)
        .map(|c| c.unwrap()[1].to_string())
        .collect();
    assert!(!cited.is_empty(), "the README census block is gone");
    assert_eq!(
        live, cited,
        "the README census block no longer matches the live grep (left: live, right: README)"
    );
    assert_eq!(
        gates_lines,
        vec![41, 2568, 2577],
        "the gates.py toolchain hits moved; update the README census and its prose"
    );
    for line in [2568, 2577] {
        assert!(
            text.contains(&format!("`gates.py:{line}`")) || text.contains(&format!("`:{line}`")),
            "the README prose must cite gates.py:{line}"
        );
    }
}

/// The README's counts hold against the graph and the prose twin: two
/// `agent: sisyphus` lines, the `chat_agent_for: architect` chat lane, and
/// one marker region in the map per `<!-- when:` region in the landed
/// `architect/instructions.md`.
#[test]
fn readme_counts_match_the_graph_and_the_prose_twin() {
    let graph = fs::read_to_string(candidate_dir().join("graph.yaml")).unwrap();
    assert_eq!(
        graph
            .lines()
            .filter(|l| l.trim() == "agent: sisyphus")
            .count(),
        2,
        "the README promises two `agent: sisyphus` lines"
    );
    assert!(
        graph
            .lines()
            .any(|l| l.trim() == "chat_agent_for: architect"),
        "the README promises `chat_agent_for: architect` in the session block"
    );

    let regions = readme_number(r"carries (\d+) marker regions");
    let prose = fs::read_to_string(agents_dir().join("architect/instructions.md")).unwrap();
    assert_eq!(
        prose.matches("<!-- when:").count(),
        regions,
        "architect/instructions.md carries a different number of marker regions than the README states"
    );
    let map: Vec<serde_json::Value> = serde_json::from_str(
        &fs::read_to_string(candidate_dir().join("scripts/marker_map.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        map.len(),
        regions,
        "marker_map.json maps a different number of regions"
    );
    let mut ordinals: Vec<u64> = map.iter().map(|e| e["ordinal"].as_u64().unwrap()).collect();
    ordinals.sort_unstable();
    let first = ordinals[0];
    assert_eq!(
        ordinals,
        (first..first + regions as u64).collect::<Vec<_>>(),
        "marker_map.json ordinals must be contiguous"
    );
}

/// The README's `build_lock` row names five agent nodes, the agent each runs,
/// that each of those agents declares `build_lock` itself, and the engine
/// paths and test pin that make inheritance a fact. All of them must exist as
/// named.
#[test]
fn readme_build_lock_row_names_shipped_nodes_agents_and_pins() {
    let text = fs::read_to_string(candidate_dir().join("graph.yaml")).unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    let nodes = &doc["nodes"];
    let readme = readme();
    let row = readme
        .lines()
        .find(|l| l.starts_with("| `build_lock` variable |"))
        .expect("the README lost its build_lock row");
    for (node, agent) in [
        ("delegate", "sisyphus"),
        ("wave_delegate", "sisyphus"),
        ("run_gauntlet", "review-gauntlet"),
        ("wave_gauntlet", "review-gauntlet"),
        ("run_adversary", "adversary"),
    ] {
        assert!(
            row.contains(&format!("`{node}`")) && row.contains(&format!("`{agent}`")),
            "the README build_lock row no longer names {node} running {agent}"
        );
        assert_eq!(
            nodes[node]["type"].as_str(),
            Some("agent"),
            "README names {node} as an agent node"
        );
        assert_eq!(
            nodes[node]["agent"].as_str(),
            Some(agent),
            "README says {node} runs {agent}"
        );
        assert!(
            nodes[node].get("inputs").is_none()
                || !nodes[node]["inputs"]
                    .as_mapping()
                    .is_some_and(|m| m.contains_key(serde_yaml::Value::from("build_lock"))),
            "README says no `inputs: build_lock` is needed on {node}; one appeared"
        );
    }
    for (agent, file) in [
        ("sisyphus", "config.yaml"),
        ("review-gauntlet", "graph.yaml"),
        ("adversary", "graph.yaml"),
    ] {
        let vars = variable_list(&agents_dir().join(agent).join(file));
        assert!(
            names_of(&vars).contains(&"build_lock"),
            "README says {agent} declares build_lock itself"
        );
    }

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for (file, needle) in [
        (
            "function/agents.rs",
            "fn run_agent_for_graph_child_seats_the_parents_build_lock_over_its_default",
        ),
        ("graph/agent.rs", "run_agent_for_graph"),
        ("graph/executor.rs", "fn seed_variables"),
        ("config/request_context.rs", "fn new_for_child"),
    ] {
        assert!(
            row.contains(needle.trim_start_matches("fn ")),
            "the README build_lock row no longer cites {needle} from `{file}`"
        );
        let source = fs::read_to_string(src.join(file)).unwrap();
        assert!(
            source.contains(needle),
            "README cites `{file}` {needle}; it is not there"
        );
    }
}

/// `pr_body` hands the group members to `assemble_pr_body` as `tasks=`, and
/// the README no longer records a missing `--tasks` gap for that call.
#[test]
fn pr_body_passes_tasks_and_the_readme_records_no_gap() {
    let gates = fs::read_to_string(candidate_dir().join("scripts/gates.py")).unwrap();
    let lines: Vec<&str> = gates.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.contains("core_tool(\"assemble_pr_body\""))
        .expect("gates.py calls assemble_pr_body through core_tool");
    let mut call = String::new();
    for line in &lines[start..] {
        call.push_str(line);
        call.push('\n');
        if line.trim_end().ends_with(')') {
            break;
        }
    }
    let passes_tasks = fancy_regex::Regex::new(r"\btasks=")
        .unwrap()
        .is_match(&call)
        .unwrap();
    let readme_owns_gap = readme().contains("without `--tasks`");
    assert!(
        passes_tasks && !readme_owns_gap,
        "README gap row and gates.py disagree: call passes tasks = {passes_tasks}, \
         README documents the gap = {readme_owns_gap}\n{call}"
    );
}

/// The README's `dead hygiene_pending_for_chain guard` row and its
/// `gates.py 1 hunk` census line describe one deletion: `stack_gate` no
/// longer reads a state key nothing writes. The staging tree is not in this
/// repo, so this pins the shape the row describes: the key appears nowhere
/// in the shipped scripts or graph (no reader was kept, no writer was
/// invented), and the guard that survives in `stack_gate` is the
/// `closeout_scope != "run"` one the row says stays. The README row itself
/// must still be present while the deletion is the recorded divergence.
#[test]
fn readme_census_row_dead_hygiene_guard_matches_the_shipped_gates() {
    let key = "hygiene_pending_for_chain";
    let mut carriers = Vec::new();
    let mut pending = vec![
        candidate_dir().join("scripts"),
        candidate_dir().join("graph.yaml"),
    ];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            continue;
        }
        if fs::read_to_string(&path).is_ok_and(|text| text.contains(key)) {
            carriers.push(path.strip_prefix(candidate_dir()).unwrap().to_path_buf());
        }
    }
    assert!(
        carriers.is_empty(),
        "README says `{key}` has no reader and no writer; found in {carriers:?}"
    );

    let gates = fs::read_to_string(candidate_dir().join("scripts/gates.py")).unwrap();
    let start = gates
        .find("def stack_gate(")
        .expect("gates.py defines stack_gate");
    let end = gates[start..]
        .find("\ndef ")
        .map_or(gates.len(), |i| start + i);
    let body = &gates[start..end];
    assert!(
        body.contains("state.get(\"closeout_scope\") != \"run\""),
        "stack_gate lost the closeout_scope guard the README row says survives:\n{body}"
    );
    assert!(
        readme().contains("| dead `hygiene_pending_for_chain` guard |"),
        "the README lost its dead-guard census row while gates.py still carries the deletion"
    );
}

/// Runs `coyote --agent architect-next [extra..] --info` from a scratch
/// config dir and returns its stdout, asserting a clean, error-free exit.
fn info_by_name(scratch: &TempDirGuard, project: &Path, extra: &[&str], label: &str) -> String {
    let dir = &scratch.0;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coyote"));
    cmd.args(["--agent", CANDIDATE])
        .args(extra)
        .arg("--info")
        .current_dir(project)
        .env("HOME", dir.join("home"))
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("TMPDIR", dir.join("tmp"))
        .env("NO_COLOR", "1")
        .env_remove("ARCHITECT_RUNS_ROOT")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("GRAPH_STATE")
        .env_remove("GRAPH_STATE_FILE")
        .stdin(Stdio::null());
    scrub_agent_vars(&mut cmd);
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{label}: --info failed\n{}",
        transcript(&output)
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        !stderr.contains("error"),
        "{label}: --info reported an error\n{}",
        transcript(&output)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The `name: value` lines of the `variables:` block `--info` prints.
fn info_variables(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .skip_while(|l| *l != "variables:")
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .map(str::trim)
        .collect()
}

/// `coyote -a architect-next --info` lists the graph with zero errors, the
/// variables block carries `build_lock` with its empty default between
/// `pr_strategy` and `task_granularity`, and a consumer's
/// `--agent-variable build_lock <cmd>` is seated over that default (the value
/// every agent-node child then inherits).
#[test]
fn info_by_name_lists_the_graph_and_seats_a_build_lock_override() {
    let scratch = scratch_dir("info");
    let project = dry_run_config_dir(&scratch);

    let stdout = info_by_name(&scratch, &project, &[], "default");
    assert!(
        stdout.contains("name: architect-next"),
        "default: no graph name:\n{stdout}"
    );
    let vars = info_variables(&stdout);
    let at = |p: &str| vars.iter().position(|l| l.starts_with(p));
    let lock = at("build_lock:").unwrap_or_else(|| panic!("no build_lock in {vars:?}"));
    assert_eq!(
        vars[lock], "build_lock: ''",
        "default: build_lock default must be empty"
    );
    assert_eq!(
        at("pr_strategy:"),
        Some(lock - 1),
        "build_lock must follow pr_strategy: {vars:?}"
    );
    assert_eq!(
        at("task_granularity:"),
        Some(lock + 1),
        "build_lock must precede task_granularity: {vars:?}"
    );

    let lock_cmd = "flock -w 240 /tmp/probe.lock";
    let stdout = info_by_name(
        &scratch,
        &project,
        &["--agent-variable", "build_lock", lock_cmd],
        "override",
    );
    assert!(
        stdout
            .lines()
            .any(|l| l.trim() == format!("build_lock: {lock_cmd}")),
        "override: build_lock not seated over the default:\n{stdout}"
    );
}

/// The variables block the binary renders for `coyote -a architect-next
/// --info` is exactly the prose twin's variable list, name for name and in
/// order (eleven entries, `build_lock` included), so a consumer sees no
/// variable the twin lacks and misses none it has.
#[test]
fn info_by_name_lists_exactly_the_prose_twins_variables_in_order() {
    let scratch = scratch_dir("info-vars");
    let project = dry_run_config_dir(&scratch);
    let stdout = info_by_name(&scratch, &project, &[], "variables");
    let listed: Vec<&str> = info_variables(&stdout)
        .into_iter()
        .map(|l| l.split_once(':').map_or(l, |(name, _)| name))
        .collect();
    let prose = variable_list(&agents_dir().join("architect/config.yaml"));
    let twin = names_of(&prose);
    assert_eq!(
        listed, twin,
        "--info variables must be the prose twin's list in order:\n{stdout}"
    );
    assert_eq!(
        listed.len(),
        11,
        "--info must list eleven variables: {listed:?}"
    );
}
