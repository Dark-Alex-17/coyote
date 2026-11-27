//! Real-process checks of the `editor` setting as the README and
//! Getting-Started describe it: with `editor` null, `.edit config` in the
//! REPL opens `$VISUAL`, then `$EDITOR`; a configured `editor` wins over
//! both. Each "editor" is a script that records its own name and exits, so
//! the check is which one ran. The REPL runs on a pseudo-terminal through
//! the shared harness with the config and cache directories under a
//! scratch directory. Unix-only; the cases run one at a time for the same
//! fork/openpty reason as the other pty suites.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use common::pty::strip_ansi;
use common::tui_harness::{
    PTY_DEADLINE, TempDirGuard, WAIT_WRAPPER, exit_code, fresh_config_dir, spawn_repl_under,
    wait_for_repl_prompt, write_config,
};
use serial_test::serial;

const EDITORS: [&str; 3] = ["visual", "editor", "configured"];

/// Each editor prints this to the terminal as it exits, so the test can
/// look for the prompt the REPL redraws after it.
const EDITOR_RAN: &str = "editor-ran-e5d";

fn write_editor(dir: &Path, name: &str) -> String {
    let path = dir.join(format!("{name}.sh"));
    fs::write(
        &path,
        format!(
            "#!/bin/sh\necho {name} >> '{}'\necho {EDITOR_RAN}\n",
            dir.join("editor-ran").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

/// Starts the REPL with `config_editor` (when given) as the `editor`
/// setting and `VISUAL` / `EDITOR` set as named, runs `.edit config`, and
/// returns the names of the editors that ran.
fn editors_run(config_editor: bool, visual: bool, editor: bool) -> Vec<String> {
    let dir = fresh_config_dir("editor-setting");
    let _guard = TempDirGuard(dir.clone());
    let paths: Vec<String> = EDITORS
        .iter()
        .map(|name| write_editor(&dir, name))
        .collect();
    let extra = if config_editor {
        format!("save_session: false\neditor: '{}'\n", paths[2])
    } else {
        "save_session: false\n".to_string()
    };
    write_config(&dir, &extra);
    let mut env = Vec::new();
    if visual {
        env.push(("VISUAL", paths[0].as_str()));
    }
    if editor {
        env.push(("EDITOR", paths[1].as_str()));
    }
    let mut repl = spawn_repl_under(&dir, &env, WAIT_WRAPPER, "/dev/tty");
    wait_for_repl_prompt(&mut repl, "dry-model)>", 0);
    repl.send(b".edit config\r").unwrap();
    let marker = dir.join("editor-ran");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.exists() && Instant::now() < deadline {
        repl.read_for(Duration::from_millis(100));
    }
    assert!(
        marker.exists(),
        "`.edit config` ran no editor:\n{}",
        strip_ansi(&String::from_utf8_lossy(repl.raw()))
    );
    let edited = repl
        .wait_for_raw(EDITOR_RAN.as_bytes(), 0, PTY_DEADLINE)
        .expect("the editor exited without printing to the terminal");
    wait_for_repl_prompt(&mut repl, "dry-model)>", edited);
    repl.send(b"\x04").unwrap();
    let text = repl
        .wait_for("tty-probe-done", Duration::from_secs(30))
        .unwrap();
    assert_eq!(exit_code(&text), 0, "the REPL exits 0:\n{text}");
    fs::read_to_string(&marker)
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

#[test]
#[serial]
fn a_null_editor_opens_visual_before_editor() {
    assert_eq!(editors_run(false, true, true), ["visual"]);
}

#[test]
#[serial]
fn a_null_editor_falls_back_to_editor_without_visual() {
    assert_eq!(editors_run(false, false, true), ["editor"]);
}

#[test]
#[serial]
fn a_configured_editor_wins_over_visual_and_editor() {
    assert_eq!(editors_run(true, true, true), ["configured"]);
}
