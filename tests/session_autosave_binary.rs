//! Binary-level contract for what an auto-saving named session says and
//! does when the REPL leaves it, driven from a cold config dir through
//! `--macro` (the only non-interactive path that runs REPL lines through
//! the real binary). The dry-run model never dirties a session, so every
//! exit here is the "clean" case: a session whose file is already current.
//! Such an exit must confirm the file the way a dirty exit does, exactly
//! once and without rewriting it, when the session auto-saves (its own
//! `save_session: true` or the one-shot `--save-session` flag), and must say
//! nothing and ask nothing otherwise. A fresh name with no file behind it
//! gets no file from a clean exit. Every run uses a fresh fake HOME and a
//! scrubbed sandbox/provider env so host state cannot leak in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SESSION: &str = "notes";
const SAVED_PREFIX: &str = "Saved the session to '";

fn fresh_config_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-session-autosave-{label}-{}-{unique}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("macros")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

/// Lays out a dry-run config dir; `config_extra` is spliced verbatim into
/// config.yaml. The `leave` macro ends the session the way `.exit session`
/// does at the prompt; `fresh` starts a never-saved name first.
fn write_fixture(dir: &Path, config_extra: &str) {
    // Without IS_SANDBOX (scrubbed below) a run insists on a vault password
    // file, so the fixture provides one.
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    // Paths land in single-quoted YAML scalars: double-quoted ones would read
    // the backslashes in a Windows temp path (`C:\Users\...`) as escapes.
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             {config_extra}\
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
             memory: false\n\
             stream: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
    fs::write(
        dir.join("macros/leave.yaml"),
        "isolated: false\nsteps:\n  - \".exit session\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("macros/fresh.yaml"),
        "isolated: false\nsteps:\n  - \".session fresh-name\"\n  - \".exit session\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("macros/undo-leave.yaml"),
        "isolated: false\nsteps:\n  - \".undo\"\n  - \".exit session\"\n",
    )
    .unwrap();
}

/// A named session file in the global scope; `save_line` is the session's
/// own `save_session` key (or nothing, for the "ask" default).
fn write_session_file(dir: &Path, save_line: &str) -> PathBuf {
    let path = dir.join("sessions").join(format!("{SESSION}.yaml"));
    fs::write(
        &path,
        format!(
            "model: dryrun:dry-model\n\
             {save_line}\
             messages:\n\
             \x20 - role: user\n\
             \x20   content: hello\n\
             \x20 - role: assistant\n\
             \x20   content: hi there\n"
        ),
    )
    .unwrap();
    path
}

fn run_coyote(dir: &Path, args: &[&str]) -> Output {
    let home = dir.join("home");
    Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("COYOTE_SAVE_SESSION")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn combined(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_ok(label: &str, output: &Output) {
    let text = combined(output);
    assert!(
        output.status.success(),
        "{label}: exit {:?}\n{text}",
        output.status
    );
    assert!(
        !text.contains("panicked"),
        "{label}: the binary panicked:\n{text}"
    );
}

/// The `Saved the session to '<path>'.` lines in stdout.
fn saved_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(SAVED_PREFIX))
        .map(str::to_string)
        .collect()
}

fn session_dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir.join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Leaves the loaded session `notes` through the real binary and returns
/// the run plus the file's bytes before it.
fn leave_notes(label: &str, save_line: &str, extra_args: &[&str]) -> (Output, PathBuf, String) {
    let dir = fresh_config_dir(label);
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    let path = write_session_file(&dir, save_line);
    let before = fs::read_to_string(&path).unwrap();
    let modified_before = path.metadata().unwrap().modified().unwrap();

    let mut args = vec!["--session", SESSION];
    args.extend_from_slice(extra_args);
    args.extend_from_slice(&["--macro", "leave"]);
    let output = run_coyote(&dir, &args);

    assert_ok(label, &output);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        before,
        "{label}: a clean exit rewrites nothing"
    );
    assert_eq!(
        path.metadata().unwrap().modified().unwrap(),
        modified_before,
        "{label}: a clean exit does not touch the file"
    );
    assert_eq!(
        session_dir_entries(&dir),
        vec![format!("{SESSION}.yaml")],
        "{label}: no temp file, no second session"
    );
    (output, path, before)
}

#[test]
fn a_clean_exit_from_an_autosaving_session_confirms_the_file_once_without_writing() {
    let (output, path, _) = leave_notes("autosave", "save_session: true\n", &[]);

    let lines = saved_lines(&output);
    assert_eq!(
        lines.len(),
        1,
        "exactly one confirmation line:\n{}",
        combined(&output)
    );
    assert!(
        lines[0].ends_with(&format!("{SAVED_PREFIX}{}'.", path.display())),
        "the line names the session's own file: {:?}",
        lines[0]
    );
}

#[test]
fn a_clean_exit_says_nothing_unless_the_session_autosaves() {
    for (label, save_line) in [("ask", ""), ("never", "save_session: false\n")] {
        let (output, _, _) = leave_notes(label, save_line, &[]);
        assert!(
            saved_lines(&output).is_empty(),
            "{label}: a clean non-autosaving exit prints no save line:\n{}",
            combined(&output)
        );
        assert!(
            !combined(&output).contains("Save session?"),
            "{label}: a clean exit never asks:\n{}",
            combined(&output)
        );
    }
}

#[test]
fn the_one_shot_save_session_flag_makes_a_clean_exit_confirm_the_file() {
    let (output, path, _) = leave_notes("this-time", "", &["--save-session"]);

    let lines = saved_lines(&output);
    assert_eq!(lines.len(), 1, "{}", combined(&output));
    assert!(lines[0].ends_with(&format!("{SAVED_PREFIX}{}'.", path.display())));
}

/// `.session <new-name>` under `save_session: true` has no file until it
/// is saved or exited dirty; a clean exit mints nothing and says nothing.
#[test]
fn a_fresh_named_session_left_clean_gets_no_file_and_no_confirmation() {
    let dir = fresh_config_dir("fresh");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "save_session: true\n");

    let output = run_coyote(&dir, &["--macro", "fresh"]);

    assert_ok("fresh", &output);
    assert!(
        saved_lines(&output).is_empty(),
        "nothing was written, so nothing is confirmed:\n{}",
        combined(&output)
    );
    assert!(
        session_dir_entries(&dir).is_empty(),
        "{:?}",
        session_dir_entries(&dir)
    );
    let workspace = dir.join(".coyote").join("sessions");
    assert!(
        !workspace.exists() || fs::read_dir(&workspace).unwrap().next().is_none(),
        "no workspace session file either"
    );
}

/// The clean exit's confirmation is the dirty exit's line, not a second
/// wording: `.undo` dirties the loaded session (the dry-run model cannot),
/// so leaving it rewrites the file and prints the line once; a clean exit
/// of the same file prints the identical line.
#[test]
fn usage_probe_a_dirty_exit_and_a_clean_exit_print_the_same_confirmation_line() {
    let dir = fresh_config_dir("undo");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    let path = write_session_file(&dir, "save_session: true\n");
    let before = fs::read_to_string(&path).unwrap();

    let dirty = run_coyote(&dir, &["--session", SESSION, "--macro", "undo-leave"]);
    assert_ok("dirty", &dirty);
    let after_dirty = fs::read_to_string(&path).unwrap();
    assert_ne!(after_dirty, before, "the dirty exit rewrote the file");
    assert!(
        !after_dirty.contains("hi there"),
        "the undone exchange is gone from the file:\n{after_dirty}"
    );
    assert_eq!(
        session_dir_entries(&dir),
        vec![format!("{SESSION}.yaml")],
        "no temp file survives the rewrite"
    );
    let dirty_lines = saved_lines(&dirty);
    assert_eq!(
        dirty_lines.len(),
        1,
        "a dirty exit confirms once:\n{}",
        combined(&dirty)
    );

    let clean = run_coyote(&dir, &["--session", SESSION, "--macro", "leave"]);
    assert_ok("clean", &clean);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        after_dirty,
        "the clean exit rewrote nothing"
    );
    let clean_lines = saved_lines(&clean);
    assert_eq!(clean_lines.len(), 1, "{}", combined(&clean));
    assert_eq!(
        clean_lines[0], dirty_lines[0],
        "both exits print the one shared line, byte for byte"
    );
    assert!(
        clean_lines[0].ends_with(&format!("{SAVED_PREFIX}{}'.", path.display())),
        "{:?}",
        clean_lines[0]
    );
}
