//! Usage probes of `scripts/mesh-relay.ps1`'s Windows paths, run on any OS that has
//! `pwsh`: `tests/fixtures/mesh-relay-windows-probe.ps1` loads only the relay's function
//! definitions and drives one scenario. Without `pwsh` on PATH the tests print `skipping:`.
//!
//! - The restart line the script prints is pasted and run as-is: it stops the task, ends
//!   every process carrying the task's `--config "<dir>"` with its tree, then starts the
//!   task, for any config dir a Windows profile can have.
//! - A re-run on a host whose `rnsd.cmd` an earlier run wrote adds `rnstatus.cmd` beside
//!   it, and the Diagnostics line names that shim.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

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

struct Probe {
    root: PathBuf,
    stdout: String,
    stderr: String,
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn probe(pwsh: &Path, scenario: &str, rns_dir: Option<&str>) -> Probe {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = env::temp_dir().join(format!("coyote-mesh-relay-probe-{scenario}-{unique}"));
    fs::create_dir_all(&root).unwrap();
    let cache = root.join("cache");
    let mut cmd = Command::new(pwsh);
    cmd.arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-File")
        .arg(repo_root().join("tests/fixtures/mesh-relay-windows-probe.ps1"))
        .arg("-Script")
        .arg(repo_root().join("scripts/mesh-relay.ps1"))
        .arg("-Root")
        .arg(&root)
        .arg("-Scenario")
        .arg(scenario)
        .env("XDG_CACHE_HOME", &cache)
        .env(
            "PSModuleAnalysisCachePath",
            cache.join("ModuleAnalysisCache"),
        )
        .stdin(Stdio::null());
    if let Some(dir) = rns_dir {
        cmd.arg("-RnsDir").arg(dir);
    }
    let out = cmd.output().expect("spawn pwsh");
    Probe {
        root,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn assert_printed_restart_runs(rns_dir: &str) {
    let pwsh = pwsh_or_skip!();
    let run = probe(&pwsh, "printed-restart", Some(rns_dir));
    let transcript = format!("stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    let calls: Vec<&str> = run
        .stdout
        .lines()
        .filter(|line| line.starts_with("CALL ") || line.starts_with("ERROR "))
        .collect();
    assert_eq!(
        calls,
        [
            "CALL Stop-ScheduledTask Coyote rnsd",
            "CALL taskkill /PID 101 /T /F",
            "CALL taskkill /PID 102 /T /F",
            "CALL Start-ScheduledTask Coyote rnsd",
        ],
        "the printed restart, pasted as-is for config dir {rns_dir:?}, must stop the task, \
         end that rnsd's tree (never the shell running it) and start the task:\n{transcript}"
    );
}

#[test]
fn the_printed_restart_runs_for_a_config_dir_with_a_space() {
    assert_printed_restart_runs(r"C:\Users\Jane Doe\.reticulum");
}

/// `'` is legal in a Windows user name, so in `%USERPROFILE%` and the config dir.
#[test]
fn the_printed_restart_runs_for_a_config_dir_with_an_apostrophe() {
    assert_printed_restart_runs(r"C:\Users\O'Brien\.reticulum");
}

#[test]
fn a_rerun_over_an_earlier_rnsd_cmd_adds_rnstatus_cmd_and_names_it() {
    let pwsh = pwsh_or_skip!();
    let run = probe(&pwsh, "present-rerun", None);
    let transcript = format!("stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    let shim = run.root.join("bin").join("rnstatus.cmd");
    let text =
        fs::read_to_string(&shim).unwrap_or_else(|_| panic!("no rnstatus.cmd:\n{transcript}"));
    assert!(
        text.contains("@set \"PYTHONUTF8=1\"\r\n")
            && text.contains(&format!(
                "@\"{}\" %*\r\n",
                run.root.join("venv Scripts").join("rnstatus.exe").display()
            )),
        "{text}\n{transcript}"
    );
    assert!(
        run.stdout.contains(&format!(
            "Diagnostics: '{}' attaches to the shared instance and lists its interfaces; run it in Windows Terminal",
            shim.display()
        )),
        "{transcript}"
    );
}

#[test]
fn without_an_rnstatus_exe_no_shim_is_written_and_diagnostics_names_plain_rnstatus() {
    let pwsh = pwsh_or_skip!();
    let run = probe(&pwsh, "present-no-rnstatus", None);
    let transcript = format!("stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert!(
        !run.root.join("bin").join("rnstatus.cmd").exists(),
        "{transcript}"
    );
    assert!(
        run.stdout.contains(
            "Diagnostics: 'rnstatus' attaches to the shared instance and lists its interfaces; run it in Windows Terminal"
        ),
        "{transcript}"
    );
}
