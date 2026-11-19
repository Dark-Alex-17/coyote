//! Black-box pins for the bottom pane's process and creation edges: a
//! backtick `.file` command's no-`wait` orphan dies on Esc AND on Ctrl+C
//! while its exited shell is still unreaped (the zombie hold), and a
//! sleeper left behind by a shell that finished on its own is observed;
//! Ctrl+O and `.edit` report an editor that wrote the file and then
//! exited 3 with one identical line; a `.macro <new>` created in the
//! editor (no request made) is not a rewind barrier yet its file exists;
//! `.role <new>` writes the role file with the editor's content and `n`
//! creates nothing. The built binary on a pseudo-terminal with the
//! dry-run model. Unix-only and serial like its sibling pty suites.
#![cfg(unix)]

mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::pty::Pty;
use common::tui_harness::*;
use serial_test::serial;

/// Quit from a composer that may hold a draft: Ctrl+C clears a draft (or
/// quits an idle empty composer); Ctrl+D follows if still up.
fn quit_clearing_any_draft_and_check(pty: &mut Pty) -> String {
    pty.send(CTRL_C).unwrap();
    let after = pty.read_for(HOTKEY_GAP);
    if !after.contains("tty-probe-done") {
        pty.send(CTRL_D).unwrap();
    }
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        !transcript.contains("panicked"),
        "the UI panicked somewhere:\n{transcript}"
    );
    transcript
}

/// Every report above the composer rule that names the editor, cut by
/// its shape: from the word `editor` (any case) through the first run of
/// digits that FOLLOWS it, whitespace folded. A repainted frame has no
/// row breaks, so the word before `editor` may be glued to the previous
/// cell's text and is left out of the cut.
fn editor_report_lines(frame: &str) -> Vec<String> {
    let flat = above_rule(frame)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let lower = flat.to_lowercase();
    let mut reports = Vec::new();
    let mut start = 0;
    while let Some(rel) = lower[start..].find("editor") {
        let at = start + rel;
        let tail = &flat[at..];
        let end = tail
            .find(|c: char| c.is_ascii_digit())
            .map(|digits| {
                digits
                    + tail[digits..]
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(tail.len() - digits)
            })
            .unwrap_or(tail.len());
        let report = tail[..end].to_string();
        start = (at + end).max(at + "editor".len());
        if report.to_lowercase().contains("exit") || report.to_lowercase().contains("status") {
            reports.push(report);
        }
        if start >= flat.len() {
            break;
        }
    }
    reports
}

// ---------------------------------------------------------------------
// The no-`wait` orphan dies on Esc and on Ctrl+C under the zombie hold.
// ---------------------------------------------------------------------

/// Two `.file` turns whose backtick shell backgrounds a sleeper WITHOUT
/// `wait` and exits (the sleeper keeps the shell's stdout, so the shell
/// is exited-but-unreaped while it lives). The first turn is ended by
/// Esc, the second by Ctrl+C. Each sleeper is gone within three seconds
/// of the key, the composer takes keys again, and the session quits
/// cleanly. A third turn pairs a sleeper whose shell released the pipe
/// at once (that command finished on its own, so nothing is held for
/// it) with a live blocker; the blocker dies, the other sleeper's fate
/// is reported, not asserted.
#[test]
#[serial]
fn a_no_wait_orphan_dies_on_esc_and_on_ctrl_c_while_its_shell_is_unreaped() {
    let dir = fresh_config_dir("zombie-hold");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let esc_pid = dir.join("esc.pid");
    let ctrlc_pid = dir.join("ctrlc.pid");
    let finished_pid = dir.join("finished.pid");
    let blocker_pid = dir.join("blocker.pid");
    let orphan_esc = write_script(
        &dir,
        "orphan-esc.sh",
        &format!(
            "#!/bin/sh\nsleep 6181 & echo $! > '{}'\necho backgrounded\n",
            esc_pid.display()
        ),
    );
    let orphan_ctrlc = write_script(
        &dir,
        "orphan-ctrlc.sh",
        &format!(
            "#!/bin/sh\nsleep 6182 & echo $! > '{}'\necho backgrounded\n",
            ctrlc_pid.display()
        ),
    );
    let finished = write_script(
        &dir,
        "finished.sh",
        &format!(
            "#!/bin/sh\nsleep 6183 </dev/null >/dev/null 2>&1 & echo $! > '{}'\necho released\n",
            finished_pid.display()
        ),
    );
    let blocker = write_script(
        &dir,
        "blocker.sh",
        &format!(
            "#!/bin/sh\nsleep 6184 & echo $! > '{}'\nwait\n",
            blocker_pid.display()
        ),
    );
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let mut misses: Vec<String> = Vec::new();
    let mut sleepers = SleeperPids(Vec::new());
    for (script, pid_file, interrupt, what) in [
        (&orphan_esc, &esc_pid, ESC, "Esc on a no-wait orphan"),
        (
            &orphan_ctrlc,
            &ctrlc_pid,
            CTRL_C,
            "Ctrl+C on a no-wait orphan",
        ),
    ] {
        let turn_at = pty.raw().len();
        pty.send(format!(".file `sh {script}` -- summarize\r").as_bytes())
            .unwrap();
        let pid = wait_for_pid_in(&mut pty, pid_file, &format!("{what}: the sleeper's pid"));
        sleepers.0.push(pid);
        assert!(
            alive(pid),
            "{what}: the sleeper (pid {pid}) is gone before the key:\n{}",
            since(&pty, turn_at)
        );
        // Let the shell exit so the key lands on an exited-but-unreaped
        // leader, which is the case under test. The turn cannot have
        // ended on its own: the orphan still holds the shell's stdout, so
        // the capture never reaches EOF while it lives.
        pty.read_for(Duration::from_millis(1200));
        pty.send(interrupt).unwrap();
        let key_at = Instant::now();
        if wait_for_process_gone(pid, Duration::from_secs(3)) {
            sleepers.forget(pid);
        } else {
            misses.push(format!(
                "{what}: the sleeper (pid {pid}) outlived the key by three seconds:\n{}",
                since(&pty, turn_at)
            ));
        }
        assert_composer_takes_keys(&mut pty, turn_at, what);
        if key_at.elapsed() > Duration::from_secs(10) {
            misses.push(format!(
                "{what}: took {:?} to end the backtick turn",
                key_at.elapsed()
            ));
        }
        let after = since(&pty, turn_at);
        if after.contains("panicked") {
            misses.push(format!("{what}: the UI panicked:\n{after}"));
        }
    }

    let turn_at = pty.raw().len();
    pty.send(format!(".file `sh {finished}` `sh {blocker}` -- summarize\r").as_bytes())
        .unwrap();
    let finished_sleeper =
        wait_for_pid_in(&mut pty, &finished_pid, "the finished shell's sleeper pid");
    sleepers.0.push(finished_sleeper);
    let blocker_sleeper = wait_for_pid_in(&mut pty, &blocker_pid, "the blocker sleeper's pid");
    sleepers.0.push(blocker_sleeper);
    pty.read_for(Duration::from_millis(1200));
    pty.send(ESC).unwrap();
    let blocker_gone = wait_for_process_gone(blocker_sleeper, Duration::from_secs(3));
    let finished_gone = wait_for_process_gone(finished_sleeper, Duration::from_secs(1));
    if blocker_gone {
        sleepers.forget(blocker_sleeper);
    } else {
        misses.push(format!(
            "the live blocker's grandchild (pid {blocker_sleeper}) outlived Esc:\n{}",
            since(&pty, turn_at)
        ));
    }
    if finished_gone {
        sleepers.forget(finished_sleeper);
    } else {
        // Alive a moment ago and this test's own sleeper, which no held
        // group will ever reach: end it here rather than leave it behind.
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        let _ = unsafe { libc::kill(finished_sleeper, libc::SIGKILL) };
        sleepers.forget(finished_sleeper);
    }
    eprintln!(
        "observation: the sleeper left by the shell that finished on its own (pid {finished_sleeper}) {} Esc",
        if finished_gone {
            "DIED on"
        } else {
            "SURVIVED (nothing is held for a finished command)"
        }
    );
    assert_composer_takes_keys(&mut pty, turn_at, "Esc on the finished + blocker pair");

    quit_clearing_any_draft_and_check(&mut pty);
    assert!(
        misses.is_empty(),
        "{} zombie-hold expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// Ctrl+O and `.edit config` report an editor that wrote the file and
// then exited 3 with one identical line.
// ---------------------------------------------------------------------

/// Opens an `$EDITOR` script via `open`; returns the new editor report
/// lines above the composer and the frame they were read from.
fn editor_report_after(
    open: &[u8],
    what: &str,
    editor_body: &str,
    scenario: &str,
) -> (Vec<String>, String) {
    let dir = fresh_config_dir(scenario);
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let editor = write_script(&dir, "editor.sh", editor_body);
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 100).unwrap();
    pty.read_for(Duration::from_millis(400));
    dry_run_exchange(&mut pty, &format!("warm-up-{scenario}"));
    let mut toggle = false;
    let before = next_frame(&mut pty, &mut toggle);
    let reports_before = editor_report_lines(&before);

    let edit_at = pty.raw().len();
    if open == CTRL_O {
        pty.send(b"draft-text").unwrap();
        pty.read_for(Duration::from_millis(300));
    }
    pty.send(open).unwrap();
    wait_for_lend_and_return(&mut pty, edit_at, what);
    pty.read_for(Duration::from_millis(1200));
    let frame = next_frame(&mut pty, &mut toggle);
    let reports: Vec<String> = editor_report_lines(&frame)
        .into_iter()
        .filter(|line| !reports_before.contains(line))
        .collect();
    let settled_at = pty.raw().len();
    clear_any_draft(&mut pty);
    assert_composer_takes_keys(&mut pty, settled_at, what);
    quit_clearing_any_draft_and_check(&mut pty);
    (reports, frame)
}

/// An editor that appends ` edited-by-script` to the file it was handed
/// and exits 3: Ctrl+O (with a draft) and `.edit config` each leave
/// exactly one report naming the exit status 3, and the two reports are
/// the same text.
#[test]
#[serial]
fn ctrl_o_and_dot_edit_report_a_modifying_editor_that_exits_3_identically() {
    const EDITOR: &str = "#!/bin/sh\nprintf ' edited-by-script\\n' >> \"$1\"\nexit 3\n";
    let (from_ctrl_o, ctrl_o_frame) = editor_report_after(CTRL_O, "Ctrl+O", EDITOR, "ctrl-o-mod-3");
    let (from_dot_edit, dot_edit_frame) = editor_report_after(
        b".edit config\r",
        "`.edit config`",
        EDITOR,
        "dot-edit-mod-3",
    );
    let mut misses: Vec<String> = Vec::new();
    if from_ctrl_o.len() != 1 {
        misses.push(format!(
            "Ctrl+O into an editor exiting 3 left {} report(s) {from_ctrl_o:?}:\n{ctrl_o_frame}",
            from_ctrl_o.len()
        ));
    }
    if from_dot_edit.len() != 1 {
        misses.push(format!(
            "`.edit config` into an editor exiting 3 left {} report(s) {from_dot_edit:?}:\n{dot_edit_frame}",
            from_dot_edit.len()
        ));
    }
    if !from_ctrl_o.iter().all(|r| r.contains('3'))
        || !from_dot_edit.iter().all(|r| r.contains('3'))
    {
        misses.push(format!(
            "a report does not carry the editor's status 3: Ctrl+O {from_ctrl_o:?}, `.edit` {from_dot_edit:?}"
        ));
    }
    if from_ctrl_o != from_dot_edit {
        misses.push(format!(
            "Ctrl+O and `.edit config` report the editor differently:\n  Ctrl+O: {from_ctrl_o:?}\n  .edit:  {from_dot_edit:?}\n{ctrl_o_frame}\n----\n{dot_edit_frame}"
        ));
    }
    assert!(
        misses.is_empty(),
        "{} editor-parity expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// `.macro <new>` created in the editor is not a rewind barrier.
// ---------------------------------------------------------------------

/// Answers a `Create ...` confirm with `key` after the idle gap and, on
/// `y`, waits for the editor's lend to return. Returns the offset the
/// command was sent at.
fn create_via_modal(pty: &mut Pty, command: &str, key: &[u8], what: &str) -> usize {
    let at = pty.raw().len();
    pty.send(format!("{command}\r").as_bytes()).unwrap();
    pty.wait_for_after("Create", at, PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("`{command}` did not ask to create in a modal: {e}"));
    pty.read_for(HOTKEY_GAP);
    let answered = pty.raw().len();
    pty.send(key).unwrap();
    if key == b"y" {
        wait_for_lend_and_return(pty, answered, what);
        pty.read_for(Duration::from_millis(1200));
    } else {
        pty.read_for(Duration::from_millis(1200));
    }
    at
}

/// `before-create`, `.macro fresh-macro` (confirm `y`, editor writes a
/// one-step body, exit 0), `after-create`. The macro file exists. Esc
/// Esc: the picker offers BOTH prompts with no "Rewind stops at the last
/// macro turn" notice; choosing `before-create` pops both and prefills
/// it.
#[test]
#[serial]
fn creating_a_macro_in_the_editor_is_not_a_rewind_barrier_and_writes_its_file() {
    let dir = fresh_config_dir("macro-create-barrier");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let editor = write_script(
        &dir,
        "macro-editor.sh",
        "#!/bin/sh\nprintf 'description: a macro made in the editor\\nisolated: false\\nsteps:\\n  - fresh-macro-step\\n' > \"$1\"\nexit 0\n",
    );
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    dry_run_exchange(&mut pty, "before-create");
    let create_at = create_via_modal(&mut pty, ".macro fresh-macro", b"y", "the macro editor");
    let mut misses: Vec<String> = Vec::new();
    let created = files_named(&dir, "fresh-macro");
    if !created.iter().any(|p| p.is_file()) {
        misses.push(format!(
            "`.macro fresh-macro` + `y` + editor wrote no macro file (found {created:?}):\n{}",
            since(&pty, create_at)
        ));
    } else {
        let body = fs::read_to_string(&created[0]).unwrap_or_default();
        if !body.contains("fresh-macro-step") {
            misses.push(format!(
                "the macro file does not hold the editor's body: {body:?}"
            ));
        }
    }
    let after_create = since(&pty, create_at);
    if squeezed(&after_create).contains("fresh-macro-step") {
        misses.push(format!(
            "creating the macro ran its step (the creation requested the model):\n{after_create}"
        ));
    }
    clear_any_draft(&mut pty);
    dry_run_exchange(&mut pty, "after-create");

    let picker_at = open_rewind_picker(&mut pty);
    let picker = repainted_frame_after_resize(&mut pty, 60, 101);
    let band_flat = squeezed(&picker_band(&picker));
    if !band_flat.contains("after-create") || !band_flat.contains("before-create") {
        misses.push(format!(
            "the picker does not offer both prompts across `.macro <new>` creation:\n{picker}"
        ));
    }
    if squeezed(&since(&pty, picker_at)).contains(&squeezed(STOP_NOTICE)) {
        misses.push(format!(
            "`.macro <new>` creation was taken for a barrier (a stop notice is shown):\n{}",
            since(&pty, picker_at)
        ));
    }
    pty.send(DOWN).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = repainted_frame_after_resize(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    if flat.contains("Rewindto") {
        misses.push(format!("the picker is still up after Enter:\n{frame}"));
    }
    if flat.contains(">after-create") || flat.contains(">before-create") {
        misses.push(format!(
            "choosing the older row did not pop both exchanges:\n{frame}"
        ));
    }
    let row = composer_row_with_footer(&frame, "dryrun:dry-model");
    if row != "before-create" {
        misses.push(format!(
            "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    clear_any_draft(&mut pty);
    quit_clearing_any_draft_and_check(&mut pty);
    assert!(
        misses.is_empty(),
        "{} macro-creation expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// `.role <new>` writes the role file with the editor's content; `n`
// creates nothing.
// ---------------------------------------------------------------------

/// `.role declined-role` + `n`: no file. `.role fresh-role` + `y`: the
/// editor (appends a marker line, exits 0) is lent the terminal and
/// afterwards a file named for the role exists under the config dir and
/// carries the marker; no editor report is shown; the UI takes keys and
/// quits cleanly.
#[test]
#[serial]
fn dot_role_new_writes_the_role_file_with_the_editors_content_and_n_creates_nothing() {
    let dir = fresh_config_dir("role-create");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let editor = write_script(
        &dir,
        "role-editor.sh",
        "#!/bin/sh\nprintf 'role-marker-line\\n' >> \"$1\"\nexit 0\n",
    );
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 100).unwrap();
    pty.read_for(Duration::from_millis(400));
    let mut misses: Vec<String> = Vec::new();

    let declined_at = create_via_modal(&mut pty, ".role declined-role", b"n", "the declined role");
    let declined = files_named(&dir, "declined-role");
    if !declined.is_empty() {
        misses.push(format!(
            "`n` at the create-role confirm still created {declined:?}:\n{}",
            since(&pty, declined_at)
        ));
    }
    if since(&pty, declined_at).contains("\u{1b}[?1049l") {
        misses.push("`n` lent the terminal to an editor".to_string());
    }
    clear_any_draft(&mut pty);

    let create_at = create_via_modal(&mut pty, ".role fresh-role", b"y", "the role editor");
    let created = files_named(&dir, "fresh-role");
    match created.iter().find(|p| p.is_file()) {
        None => misses.push(format!(
            "`.role fresh-role` + `y` + editor wrote no role file (found {created:?}):\n{}",
            since(&pty, create_at)
        )),
        Some(file) => {
            let body = fs::read_to_string(file).unwrap_or_default();
            if !body.contains("role-marker-line") {
                misses.push(format!(
                    "the role file {} does not hold the editor's line: {body:?}",
                    file.display()
                ));
            }
        }
    }
    let mut toggle = false;
    let frame = next_frame(&mut pty, &mut toggle);
    let reports = editor_report_lines(&frame);
    if !reports.is_empty() {
        misses.push(format!(
            "a clean (exit 0) role editor was reported: {reports:?}\n{frame}"
        ));
    }
    let settled_at = pty.raw().len();
    clear_any_draft(&mut pty);
    assert_composer_takes_keys(&mut pty, settled_at, "the role editor");
    quit_clearing_any_draft_and_check(&mut pty);
    assert!(
        misses.is_empty(),
        "{} role-creation expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}
