//! Behavioural coverage for the Scheduled Task half of `scripts/mesh-relay.ps1`, run
//! on any OS that has `pwsh`: the harness in `tests/fixtures/mesh-relay-task-harness.ps1`
//! loads only the relay's function definitions, shadows the ScheduledTask cmdlets with
//! a JSON-backed store, seeds that store per scenario and calls `Install-Service`
//! once. The tests judge the calls the shadows recorded, the output and what the
//! store holds afterwards. Without `pwsh` on PATH the tests print `skipping:`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
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
    /// The signal that ended pwsh, where it died of one.
    signal: Option<i32>,
    stdout: String,
    stderr: String,
}

#[cfg(unix)]
fn exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}

impl Scenario {
    fn run(pwsh: &Path, name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("coyote-mesh-relay-task-{name}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        // pwsh keeps per-user caches (the module analysis cache, and the startup JIT
        // profile under the XDG cache dir) that each process rewrites, unlocked, as it
        // exits. The tests run their pwsh processes in parallel, and one that starts while
        // another rewrites a cache can die at startup, so each run gets caches of its own.
        let cache = dir.join("cache");
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
            .env("XDG_CACHE_HOME", &cache)
            .env(
                "PSModuleAnalysisCachePath",
                cache.join("ModuleAnalysisCache"),
            )
            .stdin(Stdio::null())
            .output()
            .expect("spawn pwsh");
        Scenario {
            dir,
            code: status.code().unwrap_or(-1),
            signal: exit_signal(&status),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }

    fn transcript(&self) -> String {
        format!(
            "exit {}{}\nstdout:\n{}\nstderr:\n{}\ncalls:\n{}",
            self.code,
            self.signal
                .map(|signal| format!(" (killed by signal {signal})"))
                .unwrap_or_default(),
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
        let rns_dir = self.dir.join(".reticulum");
        let log = self.dir.join("logs").join("rnsd.log");
        let arguments = format!(
            "/c set \"PYTHONUNBUFFERED=1\" && \"{}\" --config \"{}\" >> \"{}\" 2>&1",
            rnsd.display(),
            rns_dir.display(),
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

/// The restart the script prints: the task stopped, every process carrying its
/// `--config "<dir>"` ended with its tree, the task started.
fn restart_command(run: &Scenario) -> String {
    format!(
        "Stop-ScheduledTask -TaskName 'Coyote rnsd'; Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -and $_.CommandLine.Contains('--config \"{}\"') }} | ForEach-Object {{ taskkill.exe /PID $_.ProcessId /T /F }}; Start-ScheduledTask -TaskName 'Coyote rnsd'",
        run.dir.join(".reticulum").display()
    )
}

fn changed_note(run: &Scenario) -> String {
    format!(
        "Note: the service definition changed; apply it with: {}",
        restart_command(run)
    )
}

/// The restart after `-Relay` added a stanza ends the rnsd the running task started, its
/// cmd.exe, launcher and python child, between stopping and starting the task, so the
/// task started next is the one rnsd and reads the new config. An rnsd of another config
/// and the script's own process are left alone.
#[test]
fn the_relay_restart_ends_the_old_rnsd_process_tree_before_starting_the_task() {
    let pwsh = pwsh_or_skip!();
    let run = Scenario::run(&pwsh, "unchanged-running-relay-added");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    let order: Vec<String> = run
        .calls()
        .lines()
        .filter(|line| {
            line.starts_with("Stop-ScheduledTask")
                || line.starts_with("Invoke-TreeKill")
                || line.starts_with("Start-ScheduledTask")
        })
        .map(str::to_owned)
        .collect();
    assert_eq!(
        order,
        [
            "Stop-ScheduledTask 'Coyote rnsd'",
            "Invoke-TreeKill 100",
            "Invoke-TreeKill 101",
            "Invoke-TreeKill 102",
            "Start-ScheduledTask 'Coyote rnsd'",
        ],
        "{out}"
    );
    let added = format!(
        "Added [[Team Relay]] -> relay.example:4242 to {} and restarted 'Coyote rnsd'.",
        run.dir.join(".reticulum").join("config").display()
    );
    assert!(run.stdout.contains(&added), "{out}");
}

/// A plain re-run on the config the script wrote says so with `(already there)` and
/// prints no stanzas to merge; a config written by hand still gets them.
#[test]
fn a_rerun_on_the_scripts_own_config_says_already_there_and_prints_no_stanzas() {
    let pwsh = pwsh_or_skip!();
    let run = Scenario::run(&pwsh, "own-config-rerun");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    let config = run.dir.join(".reticulum").join("config");
    assert!(
        run.stdout.contains(&format!(
            "[coyote-mesh] {} is the one this script wrote (already there); not touched",
            config.display()
        )),
        "{out}"
    );
    assert!(
        !run.stdout.contains("already exists, not touched")
            && !run.stdout.contains("[[Coyote Sessions]]"),
        "{out}"
    );

    let run = Scenario::run(&pwsh, "foreign-config-rerun");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    assert!(
        run.stdout.contains(
            "already exists, not touched. Make sure it carries these settings and stanzas:"
        ) && run.stdout.contains("[[Coyote Sessions]]"),
        "{out}"
    );
}

/// A native command's stderr reaches the user as its plain line on stdout, never as a
/// NativeCommandError record, and its exit code is returned.
#[test]
fn a_native_commands_stderr_is_printed_as_plain_text() {
    let pwsh = pwsh_or_skip!();
    let run = Scenario::run(&pwsh, "native-stderr");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    assert!(run.stdout.contains("Collecting rns 42%\n"), "{out}");
    assert!(run.stdout.contains("exit 3"), "{out}");
    assert!(
        !run.stdout.contains("NativeCommandError") && !run.stderr.contains("NativeCommandError"),
        "{out}"
    );
}

/// rnstatus.cmd goes beside rnsd.cmd and runs the rnstatus next to the rnsd it calls with
/// `PYTHONUTF8=1`, inside setlocal; an rnsd.cmd an earlier run wrote names its rnsd.
#[test]
fn the_rnstatus_shim_runs_rnstatus_with_utf8_output() {
    let pwsh = pwsh_or_skip!();
    let run = Scenario::run(&pwsh, "rnstatus-shim");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    let scripts = run.dir.join("venv Scripts");
    let shim = fs::read_to_string(run.dir.join("bin").join("rnstatus.cmd")).expect("rnstatus.cmd");
    assert_eq!(
        shim,
        format!(
            "@rem Written by scripts/mesh-relay.ps1 for Coyote mesh; runs rnstatus with UTF-8 output.\r\n@setlocal\r\n@set \"PYTHONUTF8=1\"\r\n@\"{}\" %*\r\n",
            scripts.join("rnstatus.exe").display()
        ),
        "{out}"
    );
    assert!(
        run.stdout
            .contains(&format!("target {}", scripts.join("rnsd.exe").display())),
        "{out}"
    );
}

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

/// A running task whose definition is unchanged is stopped and started once after
/// `-Relay` added the `[[Team Relay]]` stanza to the config, so rnsd dials the relay.
#[test]
fn a_running_task_is_restarted_once_after_the_relay_stanza_was_added() {
    let pwsh = pwsh_or_skip!();
    let run = Scenario::run(&pwsh, "unchanged-running-relay-added");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    assert_eq!(run.call_count("Register-ScheduledTask"), 0, "{out}");
    assert_eq!(run.call_count("Set-ScheduledTask"), 0, "{out}");
    assert_eq!(
        run.calls()
            .lines()
            .filter(|line| line.contains("ScheduledTask 'Coyote rnsd'") && !line.starts_with("Get-"))
            .collect::<Vec<_>>(),
        [
            "Stop-ScheduledTask 'Coyote rnsd'",
            "Start-ScheduledTask 'Coyote rnsd'"
        ],
        "{out}"
    );
    assert_eq!(run.call_count("Wait-Ready"), 1, "{out}");
    assert!(
        run.stdout
            .contains("Restarting 'Coyote rnsd' so it dials the new [[Team Relay]]..."),
        "{out}"
    );
    assert!(!run.stdout.contains("not restarted"), "{out}");
    assert_eq!(run.state(), "Running", "{out}");
}

/// A changed definition and an added `[[Team Relay]]` stanza: the definition is stored and
/// the running task is restarted once, which applies both, and the closing line says so.
#[test]
fn a_running_task_whose_definition_changed_is_restarted_when_the_relay_stanza_was_added() {
    let pwsh = pwsh_or_skip!();
    let run = Scenario::run(&pwsh, "changed-arguments-running-relay-added");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    assert_eq!(
        run.call_count("Set-ScheduledTask -Action -Trigger -Principal -Settings"),
        1,
        "{out}"
    );
    assert_eq!(run.call_count("Stop-ScheduledTask"), 1, "{out}");
    assert_eq!(run.call_count("Start-ScheduledTask"), 1, "{out}");
    assert_eq!(run.call_count("Wait-Ready"), 1, "{out}");
    assert!(!run.stdout.contains(&changed_note(&run)), "{out}");
    let added = format!(
        "Added [[Team Relay]] -> relay.example:4242 to {} and restarted 'Coyote rnsd'.",
        run.dir.join(".reticulum").join("config").display()
    );
    assert!(run.stdout.contains(&added), "{out}");
    assert_eq!(run.state(), "Running", "{out}");
    run.assert_store_holds_the_desired_shape();
}

/// The rnsd a stopped task started can outlive cmd.exe and keep 127.0.0.1:4242: the task
/// is not started on top of it, the user is told how to stop it, and nothing claims a
/// restart.
#[test]
fn a_relay_restart_whose_old_rnsd_keeps_the_port_says_so_and_claims_no_restart() {
    let pwsh = pwsh_or_skip!();
    let run = Scenario::run(&pwsh, "unchanged-running-relay-added-port-stays-open");
    let out = run.transcript();
    assert_eq!(run.code, 0, "{out}");
    assert_eq!(run.call_count("Stop-ScheduledTask"), 1, "{out}");
    assert_eq!(run.call_count("Start-ScheduledTask"), 0, "{out}");
    assert_eq!(run.call_count("Wait-Ready"), 0, "{out}");
    assert!(run.call_count("Test-LoopbackPort") >= 1, "{out}");
    assert!(
        run.stdout.contains("rnsd is still running from before")
            && run.stdout.contains("Stop-Process -Id (Get-NetTCPConnection -LocalPort 4242 -State Listen).OwningProcess")
            && run.stdout.contains("then run: Start-ScheduledTask -TaskName 'Coyote rnsd'"),
        "{out}"
    );
    assert!(!run.stdout.contains("restarted"), "{out}");
    assert!(
        run.stdout.contains("Restart rnsd so it dials the relay: "),
        "{out}"
    );
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
                && run.stdout.contains(&changed_note(&run)),
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
