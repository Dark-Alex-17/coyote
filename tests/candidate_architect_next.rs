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
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

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
#[test]
#[ignore]
fn mode_matrix_walks_every_combination() {
    let Some(python) = python3() else { return };
    let scratch = scratch_dir("matrix");
    let output = battery(&python, &scratch)
        .arg("scripts/mode_matrix.py")
        .output()
        .unwrap();
    let report = transcript(&output);
    assert!(output.status.success(), "{report}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.lines().any(|l| l.starts_with("FAIL ")), "{report}");
    let (passed, total) = summary_counts(&stdout, &report);
    assert_eq!(passed, total, "{report}");
    assert!(
        total >= MATRIX_FLOOR,
        "the matrix shrank below {MATRIX_FLOOR} checks: {total}\n{report}"
    );
    assert_no_bytecode_under_candidate();
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
