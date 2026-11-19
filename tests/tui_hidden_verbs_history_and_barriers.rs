//! Black-box pins for the bottom pane's formerly hidden verbs, history
//! walk and rewind barriers: the verbs the palette once left out while
//! their prompts refused under the UI are listed cold and after the
//! state warms up (a command ran, an agent was switched) under both
//! prefixes, and typing one in full still runs it; the history walk
//! (Up/Down, Ctrl+R) never reopens the palette over
//! the recalled line; `.macro enable` is not a rewind stop and a picker
//! with nothing above the macro cannot crash; a backtick document
//! command's grandchild dies on Esc while its shell still leads the
//! group; Ctrl+D during a turn drops the queued lines. The built binary
//! on a pseudo-terminal, a scripted fake model where a turn has to be
//! held open. Unix-only and serial like its sibling pty suites.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use common::fake_llm::{FakeLlm, Reply};
use common::pty::Pty;
use common::tui_harness::*;
use serial_test::serial;

/// The verbs the palette left out while their prompts still refused
/// under the UI, each paired with a typed head that narrows the palette
/// to it (and to nothing else spelling it).
const FORMERLY_HIDDEN_HEADS: [(&str, &str); 5] = [
    ("ra", "rag"),
    ("va", "vault"),
    ("ins", "install"),
    ("unin", "uninstall"),
    ("upd", "update"),
];

/// An agent with nothing to ask for, so `.agent <name>` switches at once.
fn write_plain_agent(dir: &Path, name: &str) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        format!(
            "name: {name}\n\
             description: a plain agent\n\
             version: 0.1.0\n\
             instructions: |\n\
             \x20 You are the plain agent. Reply briefly.\n"
        ),
    )
    .unwrap();
}

/// Alternates the width (141/140) so every call is a real resize, and
/// returns the frame.
fn next_wide_frame(pty: &mut Pty, toggle: &mut bool) -> String {
    *toggle = !*toggle;
    repainted_frame_after_resize(pty, 60, if *toggle { 141 } else { 140 })
}

/// Types `<sigil><head>` into an idle composer, repaints, then closes the
/// band and clears the draft. Returns every printable byte written from
/// the typing on.
fn palette_frame_for(pty: &mut Pty, sigil: char, head: &str, toggle: &mut bool) -> String {
    let typed_at = pty.raw().len();
    pty.send(format!("{sigil}{head}").as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(400));
    next_wide_frame(pty, toggle);
    let frame = since(pty, typed_at);
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(200));
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(300));
    frame
}

/// Every formerly hidden verb that does NOT show up as `<sigil><verb>`
/// in the frame painted for its head (the composer holds only the head,
/// so a full spelling is a palette row), plus the positive control:
/// `ses` must offer `<sigil>session` or the band was not read at all.
/// While an agent is active `.rag` is not a command at all (the REPL
/// table gates it on no agent), so in that state its row must be absent
/// and its absence is not a miss.
fn formerly_hidden_verbs_missing(
    pty: &mut Pty,
    sigil: char,
    toggle: &mut bool,
    state: &str,
    agent_active: bool,
) -> Vec<String> {
    let control = palette_frame_for(pty, sigil, "ses", toggle);
    assert!(
        control.contains(&format!("{sigil}session")),
        "{state}: under `{sigil}` typing `{sigil}ses` offers no `{sigil}session` row; the band \
         was not read:\n{control}"
    );
    let mut missing = Vec::new();
    for (head, verb) in FORMERLY_HIDDEN_HEADS {
        let frame = palette_frame_for(pty, sigil, head, toggle);
        let spelled = format!("{sigil}{verb}");
        let expected = !(agent_active && verb == "rag");
        if frame.contains(&spelled) != expected {
            missing.push(format!(
                "{state}: `{sigil}{head}` {} {spelled}:\n{frame}",
                if expected { "does not offer" } else { "offers" }
            ));
        }
    }
    missing
}

// ---------------------------------------------------------------------
// Formerly hidden verbs are listed in every palette state.
// ---------------------------------------------------------------------

/// Under `sigil` the five formerly hidden verbs are offered by the
/// palette on a cold start, after a command ran, after a dry-run
/// exchange, and after an `.agent` switch rebuilt the palette's state;
/// the `session` control row is offered in every state, and `.rag`,
/// which the REPL table withholds while an agent is active, is absent
/// from the agent state alone.
fn assert_formerly_hidden_verbs_are_listed(prefix_yaml: &str, sigil: char, scenario: &str) {
    let dir = fresh_config_dir(scenario);
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, prefix_yaml);
    write_plain_agent(&dir, "plain-agent");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(500));
    let mut toggle = false;
    let mut missing = Vec::new();

    missing.extend(formerly_hidden_verbs_missing(
        &mut pty,
        sigil,
        &mut toggle,
        "cold",
        false,
    ));

    let info_at = pty.raw().len();
    pty.send(format!("{sigil}info tools\r").as_bytes()).unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "Tools enabled for the next request",
        info_at,
        "`info tools` running cold",
    );
    pty.read_for(Duration::from_millis(500));
    missing.extend(formerly_hidden_verbs_missing(
        &mut pty,
        sigil,
        &mut toggle,
        "after a command ran",
        false,
    ));

    let turn_at = pty.raw().len();
    pty.send(b"warm-up-q8w\r").unwrap();
    wait_for_squeezed_after(&mut pty, "> warm-up-q8w", turn_at, "the dry-run exchange");
    pty.read_for(Duration::from_millis(2000));
    missing.extend(formerly_hidden_verbs_missing(
        &mut pty,
        sigil,
        &mut toggle,
        "after an exchange",
        false,
    ));

    let agent_at = pty.raw().len();
    pty.send(format!("{sigil}agent plain-agent\r").as_bytes())
        .unwrap();
    wait_for_squeezed_after(&mut pty, "plain-agent", agent_at, "the agent switch");
    pty.read_for(Duration::from_millis(1500));
    missing.extend(formerly_hidden_verbs_missing(
        &mut pty,
        sigil,
        &mut toggle,
        "after an agent switch",
        true,
    ));

    pty.read_for(HOTKEY_GAP);
    quit_clearing_any_draft(&mut pty);
    assert!(
        missing.is_empty(),
        "under `{sigil}` the palette leaves out formerly hidden verbs in {} state(s):\n\n{}",
        missing.len(),
        missing.join("\n\n")
    );
}

#[test]
#[serial]
fn formerly_hidden_verbs_are_listed_in_the_palette_cold_and_warm_under_dot() {
    assert_formerly_hidden_verbs_are_listed("", '.', "listed-warm-dot");
}

#[test]
#[serial]
fn formerly_hidden_verbs_are_listed_in_the_palette_cold_and_warm_under_slash() {
    assert_formerly_hidden_verbs_are_listed("command_prefix: slash\n", '/', "listed-warm-slash");
}

/// A formerly hidden verb typed in full still runs: `.rag`, `.vault` and
/// `.uninstall` under the TUI end in the command's own error or usage
/// (`.rag` with no embedding model configured fails before it has
/// anything to ask), never a hang or a crash, and the composer takes
/// keys afterwards. (`.install`/`.update` reach the network.)
#[test]
#[serial]
fn a_formerly_hidden_verb_typed_in_full_still_runs_and_the_ui_survives() {
    let dir = fresh_config_dir("listed-typed");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(500));

    for (verb, outcome) in [
        ("rag", "No available embedding model"),
        ("vault", "Usage: .vault"),
        ("uninstall", "Usage: .uninstall"),
    ] {
        let at = pty.raw().len();
        pty.send(format!(".{verb}\r").as_bytes()).unwrap();
        let started = Instant::now();
        // The outcome is a command cell; wait for the command header to
        // be committed, then for the composer to take keys again.
        pty.read_for(Duration::from_millis(1500));
        assert_composer_takes_keys(&mut pty, at, &format!("`.{verb}` typed in full"));
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "`.{verb}` held the turn for {:?}",
            started.elapsed()
        );
        // The command cell's header and its error or usage: the typed
        // line alone would also be echoed by the composer.
        let after = squeezed(&since(&pty, at));
        assert!(
            after.contains(&format!(">.{verb}")) && after.contains(&squeezed(outcome)),
            "`.{verb}` did not run to `{outcome}` in the transcript:\n{}",
            since(&pty, at)
        );
    }
    quit_and_check(&mut pty);
}

// ---------------------------------------------------------------------
// The history walk never reopens the palette.
// ---------------------------------------------------------------------

/// After `.info tools` and `.info` ran, Up on the empty composer recalls
/// `.info` with NO palette band over it (no `Show system info` row), the
/// next Up walks on to `.info tools` instead of moving a band highlight,
/// Down comes back to `.info`, and Enter runs it. Ctrl+R to `.info` is a
/// recall without a band too. The palette returns with the next real
/// edit.
#[test]
#[serial]
fn walking_the_history_with_up_down_and_ctrl_r_never_reopens_the_palette() {
    let dir = fresh_config_dir("history-walk");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(500));

    let at = pty.raw().len();
    pty.send(b".info tools\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "Tools enabled for the next request",
        at,
        "`.info tools`",
    );
    pty.read_for(Duration::from_millis(800));
    let at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", at, PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(1500));

    let mut toggle = false;
    let mut misses: Vec<String> = Vec::new();

    // Up: `.info` recalled, no band.
    key(&mut pty, b"\x1b[A");
    let frame = next_wide_frame(&mut pty, &mut toggle);
    let row = composer_row_with_footer(&frame, "dryrun:dry-model");
    if row != ".info" {
        misses.push(format!(
            "Up did not recall `.info` (composer: `{row}`):\n{frame}"
        ));
    }
    if frame.contains("Show system info") {
        misses.push(format!(
            "Up reopened the palette over the recalled `.info`:\n{frame}"
        ));
    }

    // Up again: the walk continues to `.info tools`.
    key(&mut pty, b"\x1b[A");
    let frame = next_wide_frame(&mut pty, &mut toggle);
    let row = composer_row_with_footer(&frame, "dryrun:dry-model");
    if row != ".info tools" {
        misses.push(format!(
            "a second Up did not walk on to `.info tools` (composer: `{row}`):\n{frame}"
        ));
    }
    if frame.contains("Show system info") {
        misses.push(format!("the second Up shows a palette band:\n{frame}"));
    }

    // Down: back to `.info`.
    key(&mut pty, b"\x1b[B");
    let frame = next_wide_frame(&mut pty, &mut toggle);
    let row = composer_row_with_footer(&frame, "dryrun:dry-model");
    if row != ".info" {
        misses.push(format!(
            "Down did not come back to `.info` (composer: `{row}`):\n{frame}"
        ));
    }

    // Enter runs the recalled command (a palette would swallow it).
    let run_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    if pty
        .wait_for_after("log_path", run_at, PTY_DEADLINE)
        .is_err()
    {
        misses.push(format!(
            "Enter on the recalled `.info` did not run it:\n{}",
            since(&pty, run_at)
        ));
    }
    pty.read_for(Duration::from_millis(1500));

    // Ctrl+R, `info`, Enter: recalled without a band.
    pty.send(b"\x12").unwrap();
    pty.read_for(Duration::from_millis(600));
    pty.send(b"info").unwrap();
    pty.read_for(Duration::from_millis(600));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = next_wide_frame(&mut pty, &mut toggle);
    let row = composer_row_with_footer(&frame, "dryrun:dry-model");
    if !row.starts_with(".info") {
        misses.push(format!(
            "Ctrl+R `info` Enter did not recall an `.info` line (composer: `{row}`):\n{frame}"
        ));
    }
    if frame.contains("Show system info") {
        misses.push(format!("the Ctrl+R recall reopened the palette:\n{frame}"));
    }
    // Enter on the recalled line must run it, as it does after Up.
    let run_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    if pty
        .wait_for_after("log_path", run_at, Duration::from_secs(6))
        .is_err()
    {
        let frame = next_wide_frame(&mut pty, &mut toggle);
        misses.push(format!(
            "Enter on the Ctrl+R-recalled `.info` did not run it (composer now: `{}`):\n{}",
            composer_row_with_footer(&frame, "dryrun:dry-model"),
            since(&pty, run_at)
        ));
    }
    pty.read_for(Duration::from_millis(800));

    // A real edit brings the palette back: any draft cleared, `.in`
    // opens.
    clear_any_draft(&mut pty);
    pty.send(b".in").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = next_wide_frame(&mut pty, &mut toggle);
    if !frame.contains("Show system info") {
        misses.push(format!(
            "after the history walk a fresh `.in` no longer opens the palette:\n{frame}"
        ));
    }
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} history-walk expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// `.macro enable` is not a rewind stop; nothing above the macro is safe.
// ---------------------------------------------------------------------

/// `before`, `.macro enable two-step`, `after`: the toggle only enables
/// one macro and runs no exchange, so the picker offers BOTH prompts
/// without the macro-stop notice, and choosing `before` pops both
/// exchanges and prefills `before`.
#[test]
#[serial]
fn macro_enable_is_not_a_rewind_stop_and_two_rows_pop_two_exchanges() {
    let dir = fresh_config_dir("rewind-macro-enable");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    write_two_prompt_macro(&dir, "two-step");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    let at = pty.raw().len();
    pty.send(b"before-q8w\r").unwrap();
    wait_for_squeezed_after(&mut pty, "> before-q8w", at, "the first exchange");
    pty.read_for(Duration::from_millis(2500));
    let at = pty.raw().len();
    pty.send(b".macro enable two-step\r").unwrap();
    pty.read_for(Duration::from_millis(2000));
    let toggled = since(&pty, at);
    assert!(
        !squeezed(&toggled).contains("macro-prompt-one"),
        "`.macro enable two-step` ran the macro instead of enabling it:\n{toggled}"
    );
    let at = pty.raw().len();
    pty.send(b"after-q8w\r").unwrap();
    wait_for_squeezed_after(&mut pty, "> after-q8w", at, "the second exchange");
    pty.read_for(Duration::from_millis(2500));

    let picker_at = open_rewind_picker(&mut pty);
    let picker = repainted_frame_after_resize(&mut pty, 60, 101);
    let band_flat = squeezed(&picker_band(&picker));
    let mut misses: Vec<String> = Vec::new();
    if !band_flat.contains("after-q8w") || !band_flat.contains("before-q8w") {
        misses.push(format!(
            "the picker does not offer both prompts across `.macro enable`:\n{picker}"
        ));
    }
    if squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
        misses.push(format!(
            "`.macro enable` was taken for a macro turn (the stop notice is shown):\n{}",
            since(&pty, picker_at)
        ));
    }
    // Down to `before-q8w`, Enter: both exchanges go, `before-q8w` is the
    // draft.
    pty.send(b"\x1b[B").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = repainted_frame_after_resize(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    if flat.contains("Rewindto") {
        misses.push(format!("the picker is still up after Enter:\n{frame}"));
    }
    if flat.contains(">after-q8w") || flat.contains(">before-q8w") {
        misses.push(format!(
            "choosing the older row did not pop both exchanges:\n{frame}"
        ));
    }
    let row = composer_row_with_footer(&frame, "dryrun:dry-model");
    if row != "before-q8w" {
        misses.push(format!(
            "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    clear_any_draft(&mut pty);
    let alive_at = pty.raw().len();
    pty.send(b"still-alive-q8w\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "> still-alive-q8w",
        alive_at,
        "the exchange after",
    );
    pty.read_for(Duration::from_millis(800));
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} rewind expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

/// The newest turn IS the macro: Esc Esc has nothing to offer. Whatever
/// the pane shows (the notice, an empty picker, nothing), Enter and Esc
/// cannot crash it, no macro-internal prompt is offered or prefilled, the
/// macro's exchanges stay, and the composer takes keys afterwards.
#[test]
#[serial]
fn esc_esc_with_nothing_above_the_macro_turn_does_not_crash_or_pop() {
    let dir = fresh_config_dir("rewind-only-macro");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    write_two_prompt_macro(&dir, "two-step");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    let at = pty.raw().len();
    pty.send(b".two-step\r").unwrap();
    wait_for_squeezed_after(&mut pty, "macro-prompt-two", at, "the macro's second step");
    pty.read_for(Duration::from_millis(2500));

    let picker_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(150));
    pty.send(b"\x1b").unwrap();
    pty.read_for(HOTKEY_GAP);
    let opened = since(&pty, picker_at);
    let frame = repainted_frame_after_resize(&mut pty, 60, 101);
    let band_flat = squeezed(&picker_band(&frame));
    assert!(
        !band_flat.contains("macro-prompt-one") && !band_flat.contains("macro-prompt-two"),
        "the picker offers a prompt the macro ran:\n{frame}"
    );
    assert!(
        !band_flat.contains("two-step"),
        "the macro turn is a rewind row:\n{frame}"
    );
    // Enter, then Esc: neither may crash or pop the macro's exchanges.
    let keys_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let after_enter = since(&pty, keys_at);
    assert!(
        !after_enter.contains("tty-probe-done"),
        "Enter on the picker over a macro-only transcript ended the process (exit {}); \
         Esc Esc painted:\n{opened}\n--- after Enter ---\n{after_enter}",
        exit_code(&after_enter)
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(800));
    let after_esc = since(&pty, keys_at);
    assert!(
        !after_esc.contains("tty-probe-done"),
        "Esc after Enter on the picker over a macro-only transcript ended the process (exit {}); \
         Esc Esc painted:\n{opened}\n--- after Enter, Esc ---\n{after_esc}",
        exit_code(&after_esc)
    );
    let frame = repainted_frame_after_resize(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    assert!(
        flat.contains("macro-prompt-two"),
        "Enter on an empty picker popped the macro's exchanges (Esc Esc painted:\n{opened}\n):\n{frame}"
    );
    let row = composer_row_with_footer(&frame, "dryrun:dry-model");
    assert!(
        !row.contains("macro-prompt"),
        "a macro-internal prompt was prefilled (composer: `{row}`):\n{frame}"
    );
    clear_any_draft(&mut pty);
    let settled_at = pty.raw().len();
    assert_composer_takes_keys(&mut pty, settled_at, "Esc Esc over a macro-only transcript");
    quit_clearing_any_draft(&mut pty);
}

// ---------------------------------------------------------------------
// A backtick document command's grandchild dies on Esc while its shell
// still leads the group.
// ---------------------------------------------------------------------

/// `` .file `sh <script>` `` where the script forks a sleeper and waits
/// on it (a grandchild of the document command's shell), then one where
/// it backgrounds the sleeper and exits, leaving it holding the capture
/// pipe. Each script records the sleeper's pid, so the checks and the
/// cleanup touch that process and no other. Esc ends each turn within
/// ten seconds and the composer takes keys. Both sleepers die with the
/// shell's group within two seconds of Esc: the exited shell is kept a
/// zombie until then, so its pid still names the group it led.
#[test]
#[serial]
fn a_backtick_document_commands_grandchild_dies_on_esc_even_after_its_shell_exited() {
    let dir = fresh_config_dir("backtick-grandchild");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let waits_pid = dir.join("waits.pid");
    let orphans_pid = dir.join("orphans.pid");
    let waits = write_script(
        &dir,
        "gc-waits.sh",
        &format!(
            "#!/bin/sh\nsleep 6174 & echo $! > '{}'\nwait\necho waited\n",
            waits_pid.display()
        ),
    );
    let orphans = write_script(
        &dir,
        "gc-orphans.sh",
        &format!(
            "#!/bin/sh\nsleep 6175 & echo $! > '{}'\necho backgrounded\n",
            orphans_pid.display()
        ),
    );
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let mut misses: Vec<String> = Vec::new();
    let mut sleepers = SleeperPids(Vec::new());
    for (script, pid_file, what) in [
        (waits, &waits_pid, "a waited-on grandchild"),
        (orphans, &orphans_pid, "a backgrounded, orphaned grandchild"),
    ] {
        let turn_at = pty.raw().len();
        pty.send(format!(".file `sh {script}` -- summarize\r").as_bytes())
            .unwrap();
        // The sleeper must be up before Esc, or the test proves nothing.
        let pid = wait_for_pid_in(&mut pty, pid_file, &format!("{what}: the sleeper's pid"));
        sleepers.0.push(pid);
        assert!(
            alive(pid),
            "{what}: the sleeper (pid {pid}) is gone before Esc:\n{}",
            since(&pty, turn_at)
        );
        pty.read_for(Duration::from_millis(800));
        pty.send(b"\x1b").unwrap();
        let esc_at = Instant::now();
        if wait_for_process_gone(pid, Duration::from_secs(2)) {
            sleepers.forget(pid);
        } else {
            misses.push(format!(
                "{what}: the sleeper (pid {pid}) outlived Esc by two seconds:\n{}",
                since(&pty, turn_at)
            ));
        }
        assert_composer_takes_keys(&mut pty, turn_at, &format!("Esc on `.file` with {what}"));
        if esc_at.elapsed() > Duration::from_secs(10) {
            misses.push(format!(
                "{what}: Esc took {:?} to end the backtick turn",
                esc_at.elapsed()
            ));
        }
    }
    quit_and_check(&mut pty);
    assert!(
        misses.is_empty(),
        "{} grandchild expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// Ctrl+D during a turn drops the queued lines.
// ---------------------------------------------------------------------

/// A line queued while a tool sleeps, then Ctrl+D: the session ends
/// cleanly (exit 0, terminal restored) within the deadline, and the
/// queued line never opens a request of its own on the way out.
#[test]
#[serial]
fn ctrl_d_during_a_tool_round_quits_without_sending_the_queued_line() {
    let dir = fresh_config_dir("ctrl-d-queued");
    let _guard = TempDirGuard(dir.clone());
    let tool_a = dir.join("tool-a");
    let fake = FakeLlm::start(vec![
        sleep_call(&tool_a),
        Reply::Text("never-sent-q8w".to_string()),
        Reply::Text("never-sent-either-q8w".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    pty.send(b"first-turn-q8w\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-a-started"),
        "the sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"ghost-d-q8w\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame_after_resize(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("pending:1"),
        "the line queued while the tool slept did not show `pending: 1`:\n{frame}"
    );

    let quit_at = Instant::now();
    pty.send(b"\x04").unwrap();
    let transcript = pty
        .wait_for("tty-probe-done", PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("Ctrl+D during the tool round did not end the session: {e}"));
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert!(
        quit_at.elapsed() < Duration::from_secs(15),
        "Ctrl+D took {:?} to end the session",
        quit_at.elapsed()
    );
    // Time for a wrongly flushed queued line to open a request.
    std::thread::sleep(Duration::from_millis(1500));
    let bodies = fake.bodies();
    assert!(
        bodies.iter().all(|b| !b.contains("ghost-d-q8w")),
        "the line queued before Ctrl+D reached the model:\n{}",
        bodies.join("\n---\n")
    );
    assert_eq!(
        fake.served(),
        1,
        "Ctrl+D let the queued line (or the tool's result) open another request:\n{}",
        bodies.join("\n---\n")
    );
    assert!(
        !transcript.contains("never-sent"),
        "a reply scripted behind the quit streamed:\n{transcript}"
    );
}
