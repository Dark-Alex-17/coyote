//! Wrapper around the `architect-next` candidate's own batteries.
//!
//! The batteries ship inside `assets/agents/architect-next/scripts/` and run
//! from that path: `audit_graph.py` (static edges, script `_next` edges,
//! cycles against their bounds), `smoke_test.py` (every gate, both sides of
//! every bound, the fail-open paths, the run-dir resolution),
//! `check_session_block.sh` (session block rules plus the variable mirror
//! against the prose `architect/config.yaml`) and `mode_matrix.py` (the
//! marker map against the prose `architect/instructions.md` plus all 48 mode
//! combinations walked to `end_complete`; several minutes, so opt-in). Both
//! `audit_graph.py` and `check_session_block.sh` need the Go `yq` by mikefarah
//! and skip without it. Every run points `HOME`, `LLM_ROOT_DIR`,
//! `COYOTE_CACHE_DIR`, `TMPDIR` and `ARCHITECT_RUNS_ROOT` at a scratch
//! directory and sets `PYTHONDONTWRITEBYTECODE=1`, so nothing under `assets/`
//! or `~/.config/coyote` is read or written.
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
        let output = Command::new(&python)
            .current_dir(agents.join(CANDIDATE))
            .env("HOME", scratch.0.join("home"))
            .env("LLM_ROOT_DIR", scratch.0.join("root"))
            .env("COYOTE_CACHE_DIR", scratch.0.join("cache"))
            .env("TMPDIR", scratch.0.join("tmp"))
            .env("ARCHITECT_RUNS_ROOT", scratch.0.join("runs"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env_remove("GRAPH_STATE")
            .env_remove("GRAPH_STATE_FILE")
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
/// nothing, and nothing is written under `HOME` either.
#[test]
fn batteries_never_consult_the_user_config_dir() {
    let Some(python) = python3() else { return };
    let scratch = scratch_dir("poison");
    let planted = poison_user_config(&scratch);
    let home = scratch.0.join("home");

    let yq = mikefarah_yq();
    if yq.is_none() {
        eprintln!("skipping: the audit and session legs need the mikefarah yq");
    }
    let bash = which::which("bash").ok();
    if bash.is_none() {
        eprintln!("skipping: bash not available");
    }
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

    if let (Some(yq), Some(bash)) = (&yq, &bash) {
        let session = battery(bash, &scratch)
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
        .env_remove("ARCHITECT_RUNS_ROOT")
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
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
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
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
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
        brief.contains("Implement the plan at plans/PLAN-x.md"),
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
        eprintln!("skipping: python3 not available");
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
        eprintln!("skipping: python3 not available");
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

/// The README's run-state bullet: "A prompt containing `Architect run id:
/// <id>` re-enters that run." After a first dry run has created
/// `<cache>/architect-next/runs/<id>/`, a second dry run whose prompt names
/// that id walks `intake -> resume_route -> ...` (no fresh `orient_read`),
/// reaches `ask_user`, and leaves exactly the one run dir in place.
#[test]
fn dry_run_with_the_run_id_in_the_prompt_re_enters_the_run_instead_of_starting_another() {
    if python3().is_none() {
        eprintln!("skipping: python3 not available");
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
        vec![42, 2511, 2520],
        "the gates.py toolchain hits moved; update the README census and its prose"
    );
    for line in [2511, 2520] {
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
    for (node, agent) in [
        ("delegate", "sisyphus"),
        ("wave_delegate", "sisyphus"),
        ("run_gauntlet", "review-gauntlet"),
        ("wave_gauntlet", "review-gauntlet"),
        ("run_adversary", "adversary"),
    ] {
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
                    .as_sequence()
                    .is_some_and(|s| s.iter().any(|i| i.as_str() == Some("build_lock"))),
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
        let source = fs::read_to_string(src.join(file)).unwrap();
        assert!(
            source.contains(needle),
            "README cites `{file}` {needle}; it is not there"
        );
    }
}

/// The README records one gap: `pr_body` still calls `assemble_pr_body`
/// without `--tasks`. While that is true the row must stay; once the call
/// site passes the task list this test flips and the row must go.
#[test]
fn readme_documented_gap_assemble_pr_body_runs_without_tasks() {
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
        !passes_tasks && readme_owns_gap,
        "README gap row and gates.py disagree: call passes tasks = {passes_tasks}, \
         README documents the gap = {readme_owns_gap}\n{call}"
    );
}
