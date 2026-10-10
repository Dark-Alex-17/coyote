//! Behavioural coverage for the Scheduled Task half of `scripts/mesh-relay.ps1`, run
//! on any OS that has `pwsh`: the harness in `tests/fixtures/mesh-relay-task-harness.ps1`
//! loads only the relay's function definitions, shadows the ScheduledTask cmdlets with
//! a JSON-backed store, seeds that store per scenario and calls `Install-Service`
//! once. The tests judge the calls the shadows recorded, the output and what the
//! store holds afterwards. Without `pwsh` on PATH the tests print `skipping:`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `pwsh` resolved on PATH by hand: on Windows `Command::new("pwsh")` looks in
/// System32 before PATH, so the absolute path is what gets spawned.
fn pwsh() -> Option<PathBuf> {
    let name = if cfg!(windows) { "pwsh.exe" } else { "pwsh" };
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

macro_rules! pwsh_or_skip {
    () => {
        match pwsh() {
            Some(pwsh) => pwsh,
            None => {
                eprintln!("skipping: no pwsh on PATH");
                return;
            }
        }
    };
}

/// One harness run under a fresh temp dir, removed when the value drops.
struct Scenario {
    dir: PathBuf,
    code: i32,
    stdout: String,
    stderr: String,
}

impl Scenario {
    fn run(pwsh: &Path, name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("coyote-mesh-relay-task-{name}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        let Output {
            status,
            stdout,
            stderr,
        } = Command::new(pwsh)
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-File")
            .arg(
                repo_root()
                    .join("tests")
                    .join("fixtures")
                    .join("mesh-relay-task-harness.ps1"),
            )
            .arg("-Script")
            .arg(repo_root().join("scripts").join("mesh-relay.ps1"))
            .arg("-Store")
            .arg(dir.join("task.json"))
            .arg("-Calls")
            .arg(dir.join("calls.log"))
            .arg("-Scenario")
            .arg(name)
            .current_dir(repo_root())
            .stdin(Stdio::null())
            .output()
            .expect("spawn pwsh");
        Scenario {
            dir,
            code: status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }

    fn transcript(&self) -> String {
        format!(
            "exit {}\nstdout:\n{}\nstderr:\n{}\ncalls:\n{}",
            self.code,
            self.stdout,
            self.stderr,
            self.calls()
        )
    }

    /// What the shadows recorded, one invocation per line.
    fn calls(&self) -> String {
        fs::read_to_string(self.dir.join("calls.log")).unwrap_or_default()
    }

    fn call_count(&self, prefix: &str) -> usize {
        self.calls()
            .lines()
            .filter(|line| line.starts_with(prefix))
            .count()
    }

    fn store(&self) -> serde_json::Value {
        let raw = fs::read_to_string(self.dir.join("task.json")).expect("the task store");
        serde_json::from_str(&raw).expect("the task store is JSON")
    }

    /// The whole shape the relay compares, read back from the store.
    fn stored_shape(&self) -> (String, String, String, String, String, bool, String) {
        let task = self.store();
        let text = |value: &serde_json::Value| value.as_str().unwrap_or_default().to_owned();
        (
            text(&task["Actions"][0]["Execute"]),
            text(&task["Actions"][0]["Arguments"]),
            text(&task["Triggers"][0]["CimClass"]["CimClassName"]),
            text(&task["Triggers"][0]["UserId"]),
            text(&task["Principal"]["LogonType"]),
            task["Settings"]["Hidden"].as_bool().unwrap_or(false),
            text(&task["Settings"]["ExecutionTimeLimit"]),
        )
    }

    fn state(&self) -> String {
        self.store()["State"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// The shape `Install-Service` builds for this scenario's temp dir.
    fn assert_store_holds_the_desired_shape(&self) {
        let rnsd = self.dir.join("bin").join("rnsd.cmd");
        let log = self.dir.join("logs").join("rnsd.log");
        let arguments = format!(
            "/c set \"PYTHONUNBUFFERED=1\" && \"{}\" >> \"{}\" 2>&1",
            rnsd.display(),
            log.display()
        );
        assert_eq!(
            self.stored_shape(),
            (
                "cmd.exe".to_owned(),
                arguments,
                "MSFT_TaskLogonTrigger".to_owned(),
                "TESTHOST\\tester".to_owned(),
                "Interactive".to_owned(),
                true,
                "PT0S".to_owned(),
            ),
            "{}",
            self.transcript()
        );
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

const CHANGED_NOTE: &str = "Note: the service definition changed; apply it with: Stop-ScheduledTask -TaskName 'Coyote rnsd'; Start-ScheduledTask -TaskName 'Coyote rnsd'";

#[test]
fn a_fresh_host_registers_the_task_once_starts_it_and_waits_for_the_listener() {
    let pwsh = pwsh_or_skip!();
    let run = Scenario::run(&pwsh, "fresh");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    assert_eq!(run.call_count("Register-ScheduledTask"), 1, "{out}");
    assert_eq!(run.call_count("Set-ScheduledTask"), 0, "{out}");
    assert_eq!(run.call_count("Start-ScheduledTask"), 1, "{out}");
    assert_eq!(run.call_count("Wait-Ready"), 1, "{out}");
    assert!(
        run.stdout
            .contains("Registered Scheduled Task 'Coyote rnsd'")
            && run.stdout.contains("Started 'Coyote rnsd'"),
        "{out}"
    );
    assert!(!run.stdout.contains("WARNING"), "{out}");
    assert_eq!(run.state(), "Running", "{out}");
    run.assert_store_holds_the_desired_shape();
}

#[test]
fn a_task_already_registered_with_this_shape_and_running_is_left_alone() {
    let pwsh = pwsh_or_skip!();
    let run = Scenario::run(&pwsh, "unchanged-running");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    for cmdlet in [
        "Register-ScheduledTask",
        "Set-ScheduledTask",
        "Start-ScheduledTask",
    ] {
        assert_eq!(run.call_count(cmdlet), 0, "{cmdlet} was run:\n{out}");
    }
    assert!(
        run.stdout.contains("already registered")
            && run.stdout.contains("is already running; not restarted"),
        "{out}"
    );
    assert!(!run.stdout.contains("Note:"), "{out}");
    assert_eq!(run.state(), "Running", "{out}");
    run.assert_store_holds_the_desired_shape();
}

/// A changed action and a changed trigger are both stored with one full
/// `Set-ScheduledTask`, the running instance is left alone and the restart pair is
/// printed. The trigger case is what an action-only compare could not see.
#[test]
fn a_running_task_whose_action_or_trigger_differs_is_rewritten_in_full_and_only_told_how_to_restart()
 {
    let pwsh = pwsh_or_skip!();
    for scenario in ["changed-arguments-running", "changed-trigger-running"] {
        let run = Scenario::run(&pwsh, scenario);
        let out = format!("{scenario}: {}", run.transcript());
        assert_eq!(run.code, 0, "{out}");
        assert_eq!(run.call_count("Register-ScheduledTask"), 0, "{out}");
        assert_eq!(run.call_count("Start-ScheduledTask"), 0, "{out}");
        assert_eq!(
            run.call_count("Set-ScheduledTask -Action -Trigger -Principal -Settings"),
            1,
            "the changed definition is stored with all four parts, once:\n{out}"
        );
        assert!(
            run.stdout.contains("Updated Scheduled Task 'Coyote rnsd'")
                && run.stdout.contains(CHANGED_NOTE),
            "{out}"
        );
        assert_eq!(run.state(), "Running", "{out}");
        run.assert_store_holds_the_desired_shape();
    }
}

#[test]
fn a_stale_task_that_is_not_running_is_rewritten_and_started_without_a_note() {
    let pwsh = pwsh_or_skip!();
    let run = Scenario::run(&pwsh, "stale-ready");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    assert_eq!(run.call_count("Register-ScheduledTask"), 0, "{out}");
    assert_eq!(
        run.call_count("Set-ScheduledTask -Action -Trigger -Principal -Settings"),
        1,
        "{out}"
    );
    assert_eq!(run.call_count("Start-ScheduledTask"), 1, "{out}");
    assert!(
        run.stdout.contains("Updated Scheduled Task 'Coyote rnsd'")
            && run.stdout.contains("Started 'Coyote rnsd'"),
        "{out}"
    );
    assert!(
        !run.stdout.contains("Note:") && !run.stdout.contains("WARNING"),
        "{out}"
    );
    assert_eq!(run.state(), "Running", "{out}");
    run.assert_store_holds_the_desired_shape();
}
