//! Cases for the full-screen UI at the awkward corners: a Confirm modal
//! that has to survive Ctrl-Z, SIGTERM and a window shrunk to a single
//! row or column, and an `$EDITOR` shell-out whose editor fails. Each
//! case runs the binary cold on a pseudo-terminal under the same bash
//! wrappers `tests/common/tui_harness.rs` holds, so the exit code and the
//! tty flags are reported after every exit path. Unix-only for the same
//! reasons.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{env, fs, iter};

use common::fake_llm::{Chunk, FakeLlm, Reply};
use common::pty::Pty;
use common::tui_harness::*;
use serial_test::serial;

/// Asks one yes/no question through the prompt protocol, records its
/// own pid at `<out>.pid` first and the answer at `<out>` afterwards.
const RECORDING_CONFIRM_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Ask the user before continuing and record the answer.\n\
    # @option --out! Where to write the answer.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    # shellcheck disable=SC1090\n\
    source \"$LLM_PROMPT_UTILS_FILE\"\n\
    \n\
    main() {\n\
    \x20   local out v\n\
    \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   echo $$ > \"${out}.pid\"\n\
    \x20   v=\"$(confirm \"Really do it? m6q\")\"\n\
    \x20   printf '%s' \"${v:-cancelled}\" > \"$out\"\n\
    \x20   echo \"fixture confirm: ${v:-cancelled} m6r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// Like [`spawn_tui_under_wrapper`], with `path` as the child's whole
/// PATH.
fn spawn_tui_with_path(
    dir: &Path,
    extra_env: &[(&str, &str)],
    wrapper: &str,
    path: &std::ffi::OsStr,
) -> Pty {
    let path = path.to_str().unwrap();
    let env: Vec<(&str, &str)> = extra_env
        .iter()
        .copied()
        .chain(iter::once(("PATH", path)))
        .collect();
    spawn_tui_under(dir, &env, wrapper, "/dev/tty")
}

/// Every entry into the alternate screen has exactly one matching exit:
/// the enter/leave pair is a strict mirror however many times the UI
/// handed the terminal away and took it back.
fn assert_alt_screen_mirrored(pty: &Pty, transcript: &str) {
    assert_eq!(
        count(pty.raw(), ENTER_ALT),
        count(pty.raw(), LEAVE_ALT),
        "alternate-screen enters and leaves do not pair up:\n{transcript}"
    );
}

fn pid_in(path: &Path) -> i32 {
    fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

/// The screen the UI comes back to shows its chrome again: the model and
/// the session tokens plus whatever the composer is expected to show.
/// Tokens are compared with whitespace squeezed out: a cell-by-cell
/// repaint moves the cursor between words instead of writing spaces.
fn assert_repainted_after(
    pty: &mut Pty,
    re_entered: usize,
    model: &str,
    composer: &str,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let repainted = squeezed(&since(pty, re_entered));
        let missing: Vec<&str> = [composer, model, "no session"]
            .into_iter()
            .filter(|token| !repainted.contains(&squeezed(token)))
            .collect();
        if missing.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "after {what} the screen was not repainted; still missing {missing:?}. \
             Printable bytes since re-entry: {repainted:?}"
        );
        pty.read_for(Duration::from_millis(200));
    }
}

/// Starts a tool-calling turn whose tool asks a Confirm question and
/// waits until the question is on the screen. Returns the answer file.
fn open_confirm_modal(pty: &mut Pty, dir: &Path) -> PathBuf {
    let out = dir.join("confirm-answer");
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"call the confirm tool\r").unwrap();
    pty.wait_for("m6q", PTY_DEADLINE)
        .expect("the tool's confirm question never reached the screen");
    wait_for_file(pty, &out.with_extension("pid"), "the tool's pid file");
    out
}

fn confirm_fixture(dir: &Path) -> FakeLlm {
    let out = dir.join("confirm-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture done m6d".to_string()),
    ]);
    write_fake_llm_config(dir, &fake);
    write_tool(dir, "fixture_confirm_rec.sh", RECORDING_CONFIRM_TOOL);
    fake
}

// ---------------------------------------------------------------------
// Ctrl-Z while a tool's Confirm modal is up.
// ---------------------------------------------------------------------

/// Ctrl-Z with a Confirm modal open hands the terminal back cooked and
/// out of the alternate screen like any other suspend; `fg` re-enters
/// and the modal is drawn again from state (the question is back on the
/// screen); `y` then answers it, the tool reads a yes, the turn finishes
/// and the exit is clean with exactly one re-entry.
#[test]
#[serial]
fn ctrl_z_under_a_tools_confirm_modal_hands_the_terminal_back_and_fg_redraws_the_question() {
    let dir = fresh_config_dir("ctrl-z-confirm");
    let _guard = TempDirGuard(dir.clone());
    let fake = confirm_fixture(&dir);
    let mut pty = spawn_tui_under_wrapper(&dir, &[], JOB_CONTROL_WRAPPER);

    let out = open_confirm_modal(&mut pty, &dir);
    pty.read_for(Duration::from_millis(300));
    let before_suspend = pty.raw().len();
    let stopped_marker = suspend_and_check_handback(&mut pty, before_suspend);
    assert!(
        !out.exists(),
        "the suspend itself answered the modal:\n{}",
        since(&pty, before_suspend)
    );

    let re_entered = wait_for_re_entry(&mut pty, stopped_marker);
    wait_for_squeezed_after(&mut pty, "m6q", re_entered, "the confirm question after fg");
    assert!(
        !out.exists(),
        "the resume itself answered the modal:\n{}",
        since(&pty, stopped_marker)
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed_after(
        &mut pty,
        "fixture done m6d",
        re_entered,
        "the reply after the tool",
    );
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "`y` on the resumed confirm modal did not reach the tool as a yes:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(
        count(&pty.raw()[before_suspend..], ENTER_ALT),
        1,
        "exactly one alt-screen re-entry is expected after the suspend:\n{transcript}"
    );
    assert_alt_screen_mirrored(&pty, &transcript);
}

// ---------------------------------------------------------------------
// SIGTERM, twice, while a tool's Confirm modal is up.
// ---------------------------------------------------------------------

/// Two SIGTERMs in quick succession while a tool waits on its Confirm
/// modal: the process leaves the alternate screen exactly once, the tty
/// comes back cooked, the exit is orderly and bounded, and the tool that
/// was blocked on the prompt channel is gone rather than orphaned.
#[test]
#[serial]
fn two_sigterms_under_a_tools_confirm_modal_leave_once_and_take_the_tool_along() {
    let dir = fresh_config_dir("sigterm-confirm");
    let _guard = TempDirGuard(dir.clone());
    let _fake = confirm_fixture(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    let out = open_confirm_modal(&mut pty, &dir);
    let tool_pid = pid_in(&out.with_extension("pid"));
    assert!(alive(tool_pid), "the tool was gone before the signal");
    let pid = child_pid(&since(&pty, 0));
    pty.read_for(Duration::from_millis(300));
    let signalled = Instant::now();
    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    std::thread::sleep(Duration::from_millis(30));
    // SAFETY: as above; ESRCH on an already-gone process is fine.
    let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    let took = signalled.elapsed();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        took < Duration::from_secs(10),
        "the exit under a blocked prompt took {took:?}:\n{transcript}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        1,
        "two SIGTERMs left the alternate screen {} times:\n{transcript}",
        count(pty.raw(), LEAVE_ALT)
    );
    assert_alt_screen_mirrored(&pty, &transcript);
    assert!(
        !transcript.contains("panicked"),
        "the teardown under a blocked prompt panicked:\n{transcript}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(tool_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !alive(tool_pid),
        "the tool blocked on its confirm question outlived the UI (pid {tool_pid}):\n{transcript}"
    );
}

// ---------------------------------------------------------------------
// Windows shrunk to one row, one column and one cell while a modal or a
// live stream is on the screen; every widget clamps to the buffer.
// ---------------------------------------------------------------------

/// A Confirm modal on a window dragged to 1 row, then 1 column, then
/// 1x1 and back: no panic (the alternate screen is never left on its
/// own), the question is drawn again at full size, and `y` still answers
/// it so the tool and the turn complete.
#[test]
#[serial]
fn a_confirm_modal_survives_one_row_one_column_and_one_cell_windows_and_y_still_answers() {
    let dir = fresh_config_dir("confirm-extremes");
    let _guard = TempDirGuard(dir.clone());
    let fake = confirm_fixture(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    let out = open_confirm_modal(&mut pty, &dir);
    pty.read_for(Duration::from_millis(300));
    let shrink_at = pty.raw().len();
    for (rows, cols) in [(1u16, 100u16), (40, 1), (1, 1)] {
        pty.resize(rows, cols).unwrap();
        pty.read_for(Duration::from_millis(500));
        let shown = since(&pty, shrink_at);
        assert!(
            !shown.contains("panicked") && !shown.contains("exit="),
            "a {cols}x{rows} window with the confirm modal up brought the UI down:\n{shown}"
        );
    }
    assert!(
        find(pty.raw(), LEAVE_ALT, shrink_at).is_none(),
        "the UI left the alternate screen on its own while shrunk (a panic restore?):\n{}",
        since(&pty, shrink_at)
    );
    assert!(!out.exists(), "a resize answered the modal");

    let grow_at = pty.raw().len();
    pty.resize(40, 100).unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "m6q",
        grow_at,
        "the confirm question after growing back",
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed_after(
        &mut pty,
        "fixture done m6d",
        grow_at,
        "the reply after the tool",
    );
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "`y` after the window grew back did not reach the tool as a yes:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
}

/// A held stream on a window dragged to 1 row, 1 column and 1x1: the UI
/// stays up while the deltas keep arriving into a one-cell buffer, the
/// released tail lands once the window is back, and the exit is clean.
#[test]
#[serial]
fn a_held_stream_survives_one_row_one_column_and_one_cell_windows() {
    let dir = fresh_config_dir("stream-extremes");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("release");
    let fake = FakeLlm::start(vec![Reply::Streamed(vec![
        Chunk::Text("head before the shrink m6h".to_string()),
        Chunk::WaitFor(release.clone()),
        Chunk::Text(" tail after the grow m6t".to_string()),
    ])]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"stream and hold\r").unwrap();
    wait_for_squeezed_after(&mut pty, "head before the shrink m6h", 0, "the first delta");
    pty.read_for(Duration::from_millis(300));
    let shrink_at = pty.raw().len();
    for (rows, cols) in [(1u16, 100u16), (40, 1), (1, 1)] {
        pty.resize(rows, cols).unwrap();
        pty.read_for(Duration::from_millis(500));
    }
    // The tail arrives while the window is a single cell.
    fs::write(&release, b"").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let shown = since(&pty, shrink_at);
    assert!(
        !shown.contains("panicked") && !shown.contains("exit="),
        "a one-cell window during a stream brought the UI down:\n{shown}"
    );
    assert!(
        find(pty.raw(), LEAVE_ALT, shrink_at).is_none(),
        "the UI left the alternate screen on its own while shrunk (a panic restore?):\n{shown}"
    );

    let grow_at = pty.raw().len();
    pty.resize(40, 100).unwrap();
    let transcript = wait_for_squeezed_after(
        &mut pty,
        "tail after the grow m6t",
        grow_at,
        "the rest of the reply after growing back",
    );
    assert!(
        squeezed(&transcript).contains(&squeezed("head before the shrink m6h")),
        "the head of the reply was lost across the shrink:\n{transcript}"
    );
    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after the one-cell round trip");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
    assert_eq!(fake.served(), 1, "{transcript}");
}

// ---------------------------------------------------------------------
// `$EDITOR` shell-outs whose editor fails.
// ---------------------------------------------------------------------

/// Writes a shell script at `dir/<name>` and returns its path.
fn script(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_string()
}

/// After the editor has had the terminal, the UI is back with the draft
/// still in the composer and keys handled; the draft is then cleared and
/// the usual clean exit follows.
fn assert_back_with_draft_and_quit(mut pty: Pty, re_entered: usize, draft: &str, what: &str) {
    assert_repainted_after(&mut pty, re_entered, "dryrun:dry-model", draft, what);
    // The draft must be cleared before Ctrl+D is the quit chord.
    pty.send(b"\x03").unwrap();
    pty.read_for(HOTKEY_GAP);
    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .unwrap_or_else(|err| panic!("keys were not handled after {what}: {err}"));
    pty.read_for(Duration::from_millis(300));

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        !transcript.contains("panicked"),
        "{what} panicked somewhere:\n{transcript}"
    );
    assert_alt_screen_mirrored(&pty, &transcript);
}

/// An editor that exits non-zero without touching the file: the UI
/// re-enters and repaints, the draft typed before Ctrl-O is still in the
/// composer, keys work and the exit is clean.
#[test]
#[serial]
fn an_editor_that_exits_non_zero_brings_the_draft_back_and_the_ui_stays_live() {
    let dir = fresh_config_dir("editor-fails");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let editor = script(&dir, "failing-editor.sh", "#!/bin/sh\nexit 3\n");
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"keep me m6k").unwrap();
    pty.wait_for("keep me m6k", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b"\x0f").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("Ctrl-O did not release the terminal for the editor");
    let re_entered = wait_for_re_entry(&mut pty, left);
    assert_back_with_draft_and_quit(pty, re_entered, "keep me m6k", "a failing editor");
}

/// A PATH holding only what the wrappers and the binary need (bash, sh,
/// stty, tr, sleep and the fixture `tput`): no `nano` to fall back to.
fn path_without_an_editor(dir: &Path) -> std::ffi::OsString {
    use std::os::unix::fs::symlink;
    let bin = dir.join("bare-bin");
    fs::create_dir_all(&bin).unwrap();
    for tool in ["bash", "sh", "stty", "tr", "sleep"] {
        let real = env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(tool))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{tool} is not on the PATH"));
        symlink(real, bin.join(tool)).unwrap();
    }
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
    env::join_paths([fixture_bin, bin]).unwrap()
}

/// No editor can be resolved at all (`$EDITOR` names a command that does
/// not exist and nothing on PATH stands in for it): Ctrl-O never hands
/// the terminal away, the refusal is a notice on the screen, the draft is
/// intact, keys work, enters and leaves of the alternate screen pair up,
/// and the exit is clean.
#[test]
#[serial]
fn ctrl_o_without_any_resolvable_editor_refuses_on_screen_and_keeps_the_draft() {
    let dir = fresh_config_dir("editor-missing");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let editor = dir.join("no-such-editor-m6e").to_str().unwrap().to_string();
    let path = path_without_an_editor(&dir);
    let mut pty = spawn_tui_with_path(
        &dir,
        &[("EDITOR", &editor), ("VISUAL", &editor)],
        WAIT_WRAPPER,
        &path,
    );

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"keep me m6m").unwrap();
    pty.wait_for("keep me m6m", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b"\x0f").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let shown = since(&pty, before);
    assert!(
        find(pty.raw(), LEAVE_ALT, before).is_none(),
        "Ctrl-O handed the terminal away with no editor to give it to:\n{shown}"
    );
    assert!(
        shown.contains("Editor not found"),
        "no refusal reached the screen when Ctrl-O had no editor:\n{shown}"
    );
    // The terminal was never handed away, so nothing forces a full
    // repaint; a real size change does, and the chrome plus the draft
    // must all be there afterwards.
    let at = pty.raw().len();
    pty.resize(39, 100).unwrap();
    pty.read_for(Duration::from_millis(300));
    assert_back_with_draft_and_quit(pty, at, "keep me m6m", "a missing editor");
}

/// An editor that is killed by a signal while it holds the terminal: the
/// UI re-enters, the draft is back, keys work and the exit is clean.
#[test]
#[serial]
fn an_editor_killed_by_a_signal_brings_the_draft_back_and_the_ui_stays_live() {
    let dir = fresh_config_dir("editor-killed");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let editor = script(&dir, "self-killing-editor.sh", "#!/bin/sh\nkill -KILL $$\n");
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"keep me m6s").unwrap();
    pty.wait_for("keep me m6s", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b"\x0f").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("Ctrl-O did not release the terminal for the editor");
    let re_entered = wait_for_re_entry(&mut pty, left);
    assert_back_with_draft_and_quit(pty, re_entered, "keep me m6s", "a signal-killed editor");
}
