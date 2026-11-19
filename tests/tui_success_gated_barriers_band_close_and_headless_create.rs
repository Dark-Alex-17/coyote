//! Black-box pins for the bottom pane's barrier edges: a rewind barrier is
//! only committed when the command succeeded (`.session nosuch` inside a
//! session fails and cuts nothing; `.edit session` with no session fails
//! and cuts nothing; `.edit session` that ran the editor is a barrier);
//! Ctrl+C and Ctrl+D with a history-search or rewind band open close the
//! band and leave the process alive, quitting only on the next press;
//! `--headless --macro` refuses `.role <new>` and `.macro <new>` without
//! creating a role or macro file or launching an editor. The built binary
//! on a pseudo-terminal, a scripted fake model where a session has to
//! record exchanges. Unix-only and serial like its sibling pty suites.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::fake_llm::{FakeLlm, Reply};
use common::pty::Pty;
use common::tui_harness::*;
use serial_test::serial;

/// The stop notice names what cut the list ("the last macro turn", "the
/// last session command"); the prefix is the contract.
const STOP_NOTICE_PREFIX: &str = "Rewind stops at";

/// `squeezed` minus the box-drawing rule, whose length follows the width
/// a frame was repainted at.
fn flat_frame(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && *c != '\u{2500}' && *c != '\u{2502}')
        .collect()
}

/// A `$EDITOR` that records the path it was handed and exits 0.
fn path_recording_editor(dir: &Path, launched: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let editor = dir.join("recording-editor.sh");
    fs::write(
        &editor,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" > '{}'\nexit 0\n",
            launched.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    editor.to_str().unwrap().to_string()
}

// ---------------------------------------------------------------------
// Success-gated barriers: a failed `.session` cuts nothing, a successful
// one does.
// ---------------------------------------------------------------------

/// `zero`, `.session s1` (a barrier), `before`, `.session nosuch` (fails:
/// already in a session), `after`. The picker offers `after` and `before`
/// with the notice; `zero` sits below the successful switch. Choosing
/// `before` pops both exchanges, keeps `zero`, prefills `before`, and the
/// next request carries neither popped exchange (session and transcript
/// in step).
#[test]
#[serial]
fn a_failed_dot_session_inside_a_session_is_not_a_rewind_barrier() {
    let dir = fresh_config_dir("failed-session");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::Text("reply-zero-m8s".to_string()),
        Reply::Text("reply-before-m8s".to_string()),
        Reply::Text("reply-after-m8s".to_string()),
        Reply::Text("reply-tail-m8s".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    fake_exchange(&mut pty, "zero-m8s", "reply-zero-m8s");
    start_session(&mut pty, "s1-m8s");
    fake_exchange(&mut pty, "before-m8s", "reply-before-m8s");
    let at = pty.raw().len();
    pty.send(b".session nosuch-m8s\r").unwrap();
    let refused = wait_for_squeezed_after(
        &mut pty,
        "Already in a session",
        at,
        "the `.session nosuch` refusal",
    );
    assert!(
        !squeezed(&refused).contains("nosuch-m8s.agents"),
        "`.session nosuch-m8s` switched sessions instead of failing:\n{refused}"
    );
    pty.read_for(Duration::from_millis(1200));
    fake_exchange(&mut pty, "after-m8s", "reply-after-m8s");
    assert_eq!(fake.served(), 3, "{}", fake.bodies().join("\n---\n"));

    let mut misses: Vec<String> = Vec::new();
    let picker_at = open_rewind_picker(&mut pty);
    let picker = picker_frame(&mut pty, picker_at, 101);
    let band_flat = squeezed(&picker_band(&picker));
    if !band_flat.contains("after-m8s") {
        misses.push(format!("the newest prompt is not a rewind row:\n{picker}"));
    }
    if !band_flat.contains("before-m8s") {
        misses.push(format!(
            "a FAILED `.session nosuch` cut the rewind rows: `before` is not offered:\n{picker}"
        ));
    }
    if band_flat.contains("zero-m8s") {
        misses.push(format!(
            "the successful `.session s1` is not a barrier: `zero` is offered:\n{picker}"
        ));
    }
    if !squeezed(&since(&pty, picker_at)).contains(&squeezed(STOP_NOTICE_PREFIX)) {
        misses.push(format!(
            "the stop notice is missing though `.session s1` cut `zero`:\n{}",
            since(&pty, picker_at)
        ));
    }
    // Down to `before`, Enter: two exchanges go, `zero` stays.
    pty.send(DOWN).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = repainted_frame_after_resize(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    if flat.contains("Rewindto") {
        misses.push(format!("the picker is still up after Enter:\n{frame}"));
    }
    if flat.contains("Rewindstopped") {
        misses.push(format!("the rewind stopped short:\n{frame}"));
    }
    if flat.contains(">after-m8s") || flat.contains(">before-m8s") {
        misses.push(format!(
            "choosing `before` did not pop both exchanges above the failed `.session`:\n{frame}"
        ));
    }
    if !flat.contains(">zero-m8s") {
        misses.push(format!(
            "the rewind went past the successful `.session`:\n{frame}"
        ));
    }
    let row = composer_row_with_footer(&frame, "fake:fake-model");
    if row != "before-m8s" {
        misses.push(format!(
            "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    clear_any_draft(&mut pty);
    // The session is in step: the next request carries neither popped
    // exchange.
    fake_exchange(&mut pty, "tail-m8s", "reply-tail-m8s");
    let bodies = fake.bodies();
    if bodies.len() != 4 {
        misses.push(format!(
            "expected four requests, got {}:\n{}",
            bodies.len(),
            bodies.join("\n---\n")
        ));
    } else if bodies[3].contains("before-m8s") || bodies[3].contains("after-m8s") {
        misses.push(format!(
            "a popped exchange is still in the session's next request:\n{}",
            bodies[3]
        ));
    }
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} barrier expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// `.edit session`: a barrier when it ran, not when it failed.
// ---------------------------------------------------------------------

/// `.session s2`, `before`, `.edit session` (the editor runs on the lent
/// terminal and returns), `after`. The picker offers `after` only, with
/// the notice; choosing it pops one exchange, keeps `before`, prefills
/// `after`, and the next request still carries `before` but not `after`.
#[test]
#[serial]
fn editing_the_session_is_a_rewind_barrier() {
    let dir = fresh_config_dir("edit-session-barrier");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::Text("reply-before-m8e".to_string()),
        Reply::Text("reply-after-m8e".to_string()),
        Reply::Text("reply-tail-m8e".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    let launched = dir.join("editor-launched");
    let editor = path_recording_editor(&dir, &launched);
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    start_session(&mut pty, "s2-m8e");
    fake_exchange(&mut pty, "before-m8e", "reply-before-m8e");
    let before = pty.raw().len();
    pty.send(b".edit session\r").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("`.edit session` did not release the terminal for the editor");
    let re_entered = pty
        .wait_for_raw(ENTER_ALT, left, PTY_DEADLINE)
        .expect("the alternate screen was not re-entered after the editor");
    pty.wait_for_raw(PASTE_ON, re_entered, PTY_DEADLINE)
        .expect("bracketed paste was not re-enabled on re-entry");
    pty.read_for(Duration::from_millis(500));
    let edited = fs::read_to_string(&launched)
        .unwrap_or_else(|e| panic!("`.edit session` never launched the editor ({e})"));
    assert!(
        edited.contains("s2-m8e"),
        "the editor was not handed the session file: {edited:?}"
    );
    pty.wait_for_after("Type a prompt", re_entered, PTY_DEADLINE)
        .expect("the composer was not repainted after the session editor returned");
    fake_exchange(&mut pty, "after-m8e", "reply-after-m8e");
    assert_eq!(fake.served(), 2, "{}", fake.bodies().join("\n---\n"));

    let mut misses: Vec<String> = Vec::new();
    let picker_at = open_rewind_picker(&mut pty);
    let picker = picker_frame(&mut pty, picker_at, 101);
    let band_flat = squeezed(&picker_band(&picker));
    if !band_flat.contains("after-m8e") {
        misses.push(format!(
            "the prompt after `.edit session` is not a rewind row:\n{picker}"
        ));
    }
    if band_flat.contains("before-m8e") {
        misses.push(format!(
            "`.edit session` is not a rewind barrier: `before` is offered:\n{picker}"
        ));
    }
    if !squeezed(&since(&pty, picker_at)).contains(&squeezed(STOP_NOTICE_PREFIX)) {
        misses.push(format!(
            "the stop notice is missing though `.edit session` cut `before`:\n{}",
            since(&pty, picker_at)
        ));
    }
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = repainted_frame_after_resize(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    if flat.contains("Rewindto") {
        misses.push(format!("the picker is still up after Enter:\n{frame}"));
    }
    if flat.contains("Rewindstopped") {
        misses.push(format!("the rewind stopped short:\n{frame}"));
    }
    if flat.contains(">after-m8e") || flat.contains("reply-after-m8e") {
        misses.push(format!("choosing `after` did not pop it:\n{frame}"));
    }
    if !flat.contains(">before-m8e") {
        misses.push(format!("the rewind went past `.edit session`:\n{frame}"));
    }
    let row = composer_row_with_footer(&frame, "fake:fake-model");
    if row != "after-m8e" {
        misses.push(format!(
            "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    clear_any_draft(&mut pty);
    fake_exchange(&mut pty, "tail-m8e", "reply-tail-m8e");
    let bodies = fake.bodies();
    if bodies.len() != 3 {
        misses.push(format!(
            "expected three requests, got {}:\n{}",
            bodies.len(),
            bodies.join("\n---\n")
        ));
    } else {
        if bodies[2].contains("after-m8e") {
            misses.push(format!(
                "the popped exchange is still in the session's next request:\n{}",
                bodies[2]
            ));
        }
        if !bodies[2].contains("before-m8e") {
            misses.push(format!(
                "the exchange below the `.edit session` barrier left the session:\n{}",
                bodies[2]
            ));
        }
    }
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} barrier expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

/// `before`, `.edit session` with NO session (fails, the editor never
/// runs), `after`: nothing was edited, so the picker offers both prompts
/// with no notice and choosing `before` pops both.
#[test]
#[serial]
fn a_failed_edit_session_outside_a_session_is_not_a_rewind_barrier() {
    let dir = fresh_config_dir("edit-session-failed");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let launched = dir.join("editor-launched");
    let editor = path_recording_editor(&dir, &launched);
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    dry_run_exchange(&mut pty, "before-m8f");
    let at = pty.raw().len();
    pty.send(b".edit session\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    let refused = since(&pty, at);
    assert!(
        !launched.exists(),
        "`.edit session` with no session launched the editor:\n{refused}"
    );
    assert!(
        squeezed(&refused).to_lowercase().contains("session"),
        "`.edit session` with no session said nothing about it:\n{refused}"
    );
    dry_run_exchange(&mut pty, "after-m8f");

    let mut misses: Vec<String> = Vec::new();
    let picker_at = open_rewind_picker(&mut pty);
    let picker = picker_frame(&mut pty, picker_at, 101);
    let band_flat = squeezed(&picker_band(&picker));
    if !band_flat.contains("after-m8f") || !band_flat.contains("before-m8f") {
        misses.push(format!(
            "a FAILED `.edit session` (no session) was committed as a barrier: \
             the picker does not offer both prompts:\n{picker}"
        ));
    }
    if squeezed(&since(&pty, picker_at)).contains(&squeezed(STOP_NOTICE_PREFIX)) {
        misses.push(format!(
            "the stop notice is shown though nothing was edited:\n{}",
            since(&pty, picker_at)
        ));
    }
    pty.send(DOWN).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = repainted_frame_after_resize(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    if flat.contains(">before-m8f") || flat.contains(">after-m8f") {
        misses.push(format!(
            "choosing `before` did not pop both exchanges:\n{frame}"
        ));
    }
    let row = composer_row_with_footer(&frame, "dryrun:dry-model");
    if row != "before-m8f" {
        misses.push(format!(
            "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    clear_any_draft(&mut pty);
    let settled_at = pty.raw().len();
    assert_composer_takes_keys(
        &mut pty,
        settled_at,
        "rewinding over a failed `.edit session`",
    );
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} barrier expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// Ctrl+C / Ctrl+D with a band open close the band first.
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Band {
    Search,
    Rewind,
}

/// Opens `band` over an idle empty composer after one exchange; returns
/// the raw offset it was asked at.
fn open_band(pty: &mut Pty, band: Band) -> usize {
    match band {
        Band::Rewind => open_rewind_picker(pty),
        Band::Search => {
            let at = pty.raw().len();
            pty.send(CTRL_R).unwrap();
            pty.read_for(Duration::from_millis(800));
            at
        }
    }
}

/// One exchange, the band, `key`: the band closes and the UI lives on;
/// `key` again on the empty composer quits with the terminal restored.
fn band_closes_before_the_key_quits(band: Band, key: &[u8], key_name: &str) -> Vec<String> {
    let dir = fresh_config_dir("band-close");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 100).unwrap();
    pty.read_for(Duration::from_millis(400));
    dry_run_exchange(&mut pty, "row-m8b");

    let what = format!("{band:?} band + {key_name}");
    let mut misses: Vec<String> = Vec::new();
    let baseline = flat_frame(&repainted_frame_after_resize(&mut pty, 40, 101));
    open_band(&mut pty, band);
    let opened = repainted_frame_after_resize(&mut pty, 40, 100);
    if flat_frame(&opened) == baseline {
        misses.push(format!(
            "[{what}] the band did not open (frame unchanged):\n{opened}"
        ));
    }
    if matches!(band, Band::Rewind) && !opened.contains("Rewind to") {
        misses.push(format!(
            "[{what}] the rewind picker is not on screen:\n{opened}"
        ));
    }

    // The key: the band goes, the process stays.
    let key_at = pty.raw().len();
    pty.send(key).unwrap();
    let after = pty.read_for(HOTKEY_GAP);
    if after.contains("tty-probe-done") {
        misses.push(format!(
            "[{what}] {key_name} with the band open quit the UI instead of closing the band:\n{}",
            since(&pty, key_at)
        ));
        return misses;
    }
    let closed = repainted_frame_after_resize(&mut pty, 40, 101);
    if closed.contains("Rewind to") {
        misses.push(format!(
            "[{what}] the rewind picker survived {key_name}:\n{closed}"
        ));
    }
    if flat_frame(&closed) != baseline {
        misses.push(format!(
            "[{what}] after {key_name} the screen is not back to the pre-band frame:\n--- before ---\n{}\n--- after ---\n{closed}",
            baseline
        ));
    }
    if !squeezed(&closed).contains(">row-m8b") {
        misses.push(format!(
            "[{what}] {key_name} on the band popped the exchange:\n{closed}"
        ));
    }
    if !shows_placeholder(&squeezed(&closed)) {
        misses.push(format!(
            "[{what}] the composer is not empty after the band closed:\n{closed}"
        ));
    }
    // Keys still land: the composer is live.
    let live_at = pty.raw().len();
    assert_composer_takes_keys(&mut pty, live_at, &what);

    // The same key on an idle empty composer is the quit chord.
    pty.send(key).unwrap();
    match pty.wait_for("tty-probe-done", PTY_DEADLINE) {
        Ok(transcript) => {
            if exit_code(&transcript) != 0 {
                misses.push(format!("[{what}] the quit exited non-zero:\n{transcript}"));
            }
            assert_tty_restored(&transcript);
        }
        Err(e) => {
            misses.push(format!(
                "[{what}] a second {key_name} on the empty composer did not quit: {e}"
            ));
            quit_clearing_any_draft(&mut pty);
        }
    }
    misses
}

/// Ctrl+C and Ctrl+D over the history-search band and the rewind picker:
/// four spawns, each closes the band then quits on the repeat.
#[test]
#[serial]
fn ctrl_c_and_ctrl_d_close_an_open_band_before_they_quit() {
    let mut misses: Vec<String> = Vec::new();
    for (band, key, key_name) in [
        (Band::Search, CTRL_C, "Ctrl+C"),
        (Band::Search, CTRL_D, "Ctrl+D"),
        (Band::Rewind, CTRL_C, "Ctrl+C"),
        (Band::Rewind, CTRL_D, "Ctrl+D"),
    ] {
        misses.extend(band_closes_before_the_key_quits(band, key, key_name));
    }
    assert!(
        misses.is_empty(),
        "{} band-close expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// Headless `.role <new>` / `.macro <new>` create nothing.
// ---------------------------------------------------------------------

/// Runs `--headless --macro <name> go` with stdin closed and a recording
/// editor; returns (exit status text, stdout+stderr). The positional
/// `go` matters: with no prompt argument and no stdin text `--headless`
/// bails with "requires a prompt argument" before the macro runs. A
/// macro without variables tolerates the surplus argument.
fn run_headless_macro(dir: &Path, name: &str, editor: &str) -> (String, String) {
    let mut cmd = ttyless_command(built_binary(), dir);
    cmd.args(["--headless", "--macro", name, "go"])
        .env("EDITOR", editor)
        .env("VISUAL", editor)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + PTY_DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("`--headless --macro {name}` hung (a creation prompt waited for a human?)");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    (status.to_string(), format!("{stdout}\n{stderr}"))
}

/// Macro `mkrole` = `.role m8-role`: the step is echoed, no role file,
/// no editor, the creation gate's refusal. Macro `mkmacro` = `.macro
/// m8-macro`: the same for a macro.
#[test]
#[serial]
fn a_headless_macro_cannot_create_a_role_or_a_macro() {
    let dir = fresh_config_dir("headless-create");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let macros = dir.join("macros");
    fs::create_dir_all(&macros).unwrap();
    fs::write(
        macros.join("mkrole.yaml"),
        "description: create a role headless\n\
         steps:\n\
         \x20 - .role m8-role\n",
    )
    .unwrap();
    fs::write(
        macros.join("mkmacro.yaml"),
        "description: create a macro headless\n\
         steps:\n\
         \x20 - .macro m8-macro\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("roles")).unwrap();
    let launched = dir.join("editor-launched");
    let editor = path_recording_editor(&dir, &launched);

    let mut misses: Vec<String> = Vec::new();
    for (macro_name, needle, kind) in [
        ("mkrole", "m8-role", "role"),
        ("mkmacro", "m8-macro", "macro"),
    ] {
        let (status, everything) = run_headless_macro(&dir, macro_name, &editor);
        if !everything.contains(&format!(">> .{kind} {needle}")) {
            misses.push(format!(
                "`--headless --macro {macro_name}` never echoed its step (did the macro run at \
                 all?) (exit {status}):\n{everything}"
            ));
        }
        let created = files_named(&dir, needle);
        if !created.is_empty() {
            misses.push(format!(
                "headless `.{kind} {needle}` created {created:?} (exit {status}):\n{everything}"
            ));
        }
        if launched.exists() {
            misses.push(format!(
                "headless `.{kind} {needle}` launched the editor ({}; exit {status}):\n{everything}",
                fs::read_to_string(&launched).unwrap_or_default().trim()
            ));
            let _ = fs::remove_file(&launched);
        }
        let refusal = format!(
            "Creating {kind} '{needle}' needs a human to answer the prompt and none is attached \
             to this run; run it without --headless or --acp-server"
        );
        if !everything.contains(&refusal) {
            misses.push(format!(
                "headless `.{kind} {needle}` did not give the refusal `{refusal}` (exit \
                 {status}):\n{everything}"
            ));
        }
    }
    assert!(
        misses.is_empty(),
        "{} headless expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}
