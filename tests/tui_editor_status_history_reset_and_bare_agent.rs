//! Black-box pins for more of the bottom pane's edges: a `.edit`
//! `$EDITOR` that exits non-zero is reported on screen while
//! a clean editor is not, and Ctrl+O reports it with the same words;
//! clearing the composer, accepting a Ctrl+R
//! match, prefilling from the rewind picker or taking a queued line back
//! each end the history walk so the next Up starts from the newest
//! entry; a bare `.agent` (usage only) is not a rewind barrier; the
//! headless creation gate for `.role <new>` / `.macro <new>` speaks one
//! sentence skeleton shared with the headless prompter's refusal. The
//! built binary on a pseudo-terminal, a scripted fake model where a tool
//! round is needed. Unix-only and serial like its sibling pty suites.
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

const UP: &[u8] = b"\x1b[A";

/// The composer after `times` Up presses, read from a fresh frame.
fn up_times(pty: &mut Pty, times: usize, toggle: &mut bool, footer: &str) -> (String, String) {
    for _ in 0..times {
        key(pty, UP);
    }
    let frame = next_frame(pty, toggle);
    (composer_row_with_footer(&frame, footer), frame)
}

/// Lines above the composer's rule that mention the editor and an exit,
/// status, failure or error: what a report of a failed editor looks like
/// whatever its exact wording.
/// Every editor report painted above the composer rule. A repainted
/// frame carries no row breaks (its rows are cursor-positioned), so the
/// report is cut out by its own shape: from `The editor` to the status
/// number that ends it.
fn editor_report_lines(frame: &str) -> Vec<String> {
    let flat = above_rule(frame)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut reports = Vec::new();
    let mut rest = flat.as_str();
    while let Some(at) = rest.find("The editor") {
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| c.is_ascii_digit())
            .map(|digits| {
                digits
                    + tail[digits..]
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(tail.len() - digits)
            })
            .unwrap_or(tail.len());
        reports.push(tail[..end].to_string());
        rest = &tail[end..];
    }
    reports
}

// ---------------------------------------------------------------------
// An editor that exits non-zero is reported; a clean one is not.
// ---------------------------------------------------------------------

/// Opens an `$EDITOR` script with the keys `open` (`.edit config` or
/// Ctrl+O); returns the report lines left above the composer (none for
/// a clean editor) after the terminal came back, and asserts the
/// composer takes keys afterwards.
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
    // A first exchange so the transcript holds cells (the welcome
    // block's key hints must not be mistaken for a report).
    dry_run_exchange(&mut pty, &format!("warm-up-{scenario}"));
    let mut toggle = false;
    let before = next_frame(&mut pty, &mut toggle);
    let reports_before = editor_report_lines(&before);

    let edit_at = pty.raw().len();
    pty.send(open).unwrap();
    wait_for_lend_and_return(&mut pty, edit_at, what);
    pty.read_for(Duration::from_millis(1200));
    let frame = next_frame(&mut pty, &mut toggle);
    let reports: Vec<String> = editor_report_lines(&frame)
        .into_iter()
        .filter(|line| !reports_before.contains(line))
        .collect();
    let settled_at = pty.raw().len();
    assert_composer_takes_keys(&mut pty, settled_at, what);
    let transcript = quit_clearing_any_draft(&mut pty);
    assert!(
        !transcript.contains("panicked"),
        "the UI panicked somewhere:\n{transcript}"
    );
    (reports, frame)
}

fn dot_edit_config_with(editor_body: &str, scenario: &str) -> (Vec<String>, String) {
    editor_report_after(b".edit config\r", "`.edit config`", editor_body, scenario)
}

/// `.edit config` with an `$EDITOR` that exits 3 without touching the
/// file: the terminal is lent and comes back, and a line above the
/// composer names the editor and its exit status 3. The same command
/// with an editor that exits 0 leaves NO such line.
#[test]
#[serial]
fn dot_edit_with_a_failing_editor_is_reported_and_a_clean_one_is_not() {
    let mut misses: Vec<String> = Vec::new();

    let (reports, frame) = dot_edit_config_with("#!/bin/sh\nexit 3\n", "edit-fails-w4");
    if reports.is_empty() {
        misses.push(format!(
            "`.edit config` whose editor exited 3 shows no line naming the editor and an \
             exit/status/failure:\n{frame}"
        ));
    } else if !reports.iter().any(|line| line.contains('3')) {
        misses.push(format!(
            "the editor report does not name the exit status 3: {reports:?}\n{frame}"
        ));
    }

    let (reports, frame) = dot_edit_config_with("#!/bin/sh\nexit 0\n", "edit-clean-w4");
    if !reports.is_empty() {
        misses.push(format!(
            "`.edit config` with a clean editor (exit 0) was reported as a failure: \
             {reports:?}\n{frame}"
        ));
    }

    assert!(
        misses.is_empty(),
        "{} editor-report expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

/// Ctrl+O into an `$EDITOR` that exits 3 leaves the line `.edit config`
/// leaves, word for word, and a clean editor leaves none.
#[test]
#[serial]
fn ctrl_o_with_a_failing_editor_is_reported_the_way_dot_edit_is() {
    const FAILING_EDITOR: &str = "#!/bin/sh\nexit 3\n";
    let (from_ctrl_o, ctrl_o_frame) =
        editor_report_after(CTRL_O, "Ctrl+O", FAILING_EDITOR, "ctrl-o-fails-w4");
    assert_eq!(
        from_ctrl_o,
        ["The editor exited with exit status: 3"],
        "Ctrl+O into an editor exiting 3:\n{ctrl_o_frame}"
    );
    let (from_dot_edit, dot_edit_frame) =
        dot_edit_config_with(FAILING_EDITOR, "edit-fails-again-w4");
    assert_eq!(
        from_dot_edit, from_ctrl_o,
        "`.edit config` reports the editor differently from Ctrl+O:\n{dot_edit_frame}"
    );

    let (reports, frame) =
        editor_report_after(CTRL_O, "Ctrl+O", "#!/bin/sh\nexit 0\n", "ctrl-o-clean-w4");
    assert!(
        reports.is_empty(),
        "Ctrl+O into a clean editor (exit 0) was reported as a failure: {reports:?}\n{frame}"
    );
}

// ---------------------------------------------------------------------
// Clear, Ctrl+R accept, picker prefill and unqueue end the history walk.
// ---------------------------------------------------------------------

/// Three dry-run prompts `h1`, `h2`, `h3`. Up x3 reaches `h1` (a fourth
/// Up stays there). Ctrl+C clears the composer; the next Up is `h3`,
/// not `h1`. Back at `h1` again, Ctrl+R `h2` Enter recalls `h2` with no
/// palette; the next Up is `h3`.
#[test]
#[serial]
fn clearing_the_composer_or_accepting_a_ctrl_r_match_restarts_the_history_walk() {
    let dir = fresh_config_dir("history-reset");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 100).unwrap();
    pty.read_for(Duration::from_millis(400));
    for prompt in ["h1-w4h", "h2-w4h", "h3-w4h"] {
        dry_run_exchange(&mut pty, prompt);
    }
    let footer = "dryrun:dry-model";
    let mut toggle = false;
    let mut misses: Vec<String> = Vec::new();

    // Walk to the oldest; one more Up stays there.
    let (row, frame) = up_times(&mut pty, 3, &mut toggle, footer);
    assert_eq!(
        row, "h1-w4h",
        "three Ups did not reach the oldest entry:\n{frame}"
    );
    let (row, frame) = up_times(&mut pty, 1, &mut toggle, footer);
    if row != "h1-w4h" {
        misses.push(format!(
            "a fourth Up at the oldest entry moved the composer (`{row}`):\n{frame}"
        ));
    }

    // Ctrl+C clears; the walk restarts at the newest.
    key(&mut pty, CTRL_C);
    pty.read_for(Duration::from_millis(400));
    let frame = next_frame(&mut pty, &mut toggle);
    if !shows_placeholder(&squeezed(&frame)) {
        misses.push(format!(
            "Ctrl+C on a recalled entry did not clear the composer (`{}`):\n{frame}",
            composer_row_with_footer(&frame, footer)
        ));
    }
    let (row, frame) = up_times(&mut pty, 1, &mut toggle, footer);
    if row != "h3-w4h" {
        misses.push(format!(
            "after Ctrl+C cleared the composer the next Up did not start from the newest entry \
             (composer: `{row}`, expected `h3-w4h`):\n{frame}"
        ));
    }

    // Back to the oldest, then Ctrl+R `h2` Enter.
    key(&mut pty, CTRL_C);
    pty.read_for(Duration::from_millis(400));
    let (row, frame) = up_times(&mut pty, 3, &mut toggle, footer);
    if row != "h1-w4h" {
        misses.push(format!(
            "three Ups after a clear did not reach the oldest entry (`{row}`):\n{frame}"
        ));
    }
    pty.send(CTRL_R).unwrap();
    pty.read_for(Duration::from_millis(600));
    pty.send(b"h2").unwrap();
    pty.read_for(Duration::from_millis(600));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = next_frame(&mut pty, &mut toggle);
    let row = composer_row_with_footer(&frame, footer);
    if row != "h2-w4h" {
        misses.push(format!(
            "Ctrl+R `h2` Enter did not recall `h2-w4h` (composer: `{row}`):\n{frame}"
        ));
    }
    let (row, frame) = up_times(&mut pty, 1, &mut toggle, footer);
    if row != "h3-w4h" {
        misses.push(format!(
            "after a Ctrl+R accept the next Up did not start from the newest entry (composer: \
             `{row}`, expected `h3-w4h`):\n{frame}"
        ));
    }
    if frame.contains("Show system info") {
        misses.push(format!("the history walk reopened the palette:\n{frame}"));
    }

    clear_any_draft(&mut pty);
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} history-reset expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

/// Three dry-run prompts; Up x3 reaches `h1`; Ctrl+C clears; Esc Esc,
/// Enter on the top row pops `h3` and prefills it. The walk restarted:
/// Up Up reaches `h2` (a stale cursor would sit at `h1`).
#[test]
#[serial]
fn prefilling_from_the_rewind_picker_restarts_the_history_walk() {
    let dir = fresh_config_dir("history-prefill");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 100).unwrap();
    pty.read_for(Duration::from_millis(400));
    for prompt in ["h1-w4p", "h2-w4p", "h3-w4p"] {
        dry_run_exchange(&mut pty, prompt);
    }
    let footer = "dryrun:dry-model";
    let mut toggle = false;
    let mut misses: Vec<String> = Vec::new();

    let (row, frame) = up_times(&mut pty, 3, &mut toggle, footer);
    assert_eq!(
        row, "h1-w4p",
        "three Ups did not reach the oldest entry:\n{frame}"
    );
    key(&mut pty, CTRL_C);
    pty.read_for(Duration::from_millis(400));

    let picker_at = open_rewind_picker(&mut pty);
    let picker = picker_frame(&mut pty, picker_at, 101);
    assert!(
        squeezed(&picker_band(&picker)).contains("h3-w4p"),
        "the newest prompt is not the top rewind row:\n{picker}"
    );
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = next_frame(&mut pty, &mut toggle);
    let row = composer_row_with_footer(&frame, footer);
    if row != "h3-w4p" {
        misses.push(format!(
            "the chosen row was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    if squeezed(&frame).contains(">h3-w4p") {
        misses.push(format!("the chosen exchange was not popped:\n{frame}"));
    }

    let (row, frame) = up_times(&mut pty, 2, &mut toggle, footer);
    if row != "h2-w4p" {
        misses.push(format!(
            "after the picker prefilled `h3-w4p`, Up Up did not reach `h2-w4p` (composer: \
             `{row}`; a stale walk cursor sits at `h1-w4p`):\n{frame}"
        ));
    }

    clear_any_draft(&mut pty);
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} prefill history expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

/// Three fake-model prompts, then a fourth turn on a sleeping tool.
/// `q` is queued (`pending: 1`); Up x4 walks the history to `h2` (the
/// entries newest-first: `q`, the tool prompt, `h3`, `h2`); Alt+Up
/// trades the walked `h2` for the queued `q` (the composer shows `q`,
/// the strip now previews `h2`). The walk ended with that swap: the
/// next Up starts from the newest entry (the just-held `h2`, or `q`),
/// NOT from the stale cursor's neighbour `h1`. The tool is then
/// released: the held `h2` rides the next request, the taken-back `q`
/// does not.
#[test]
#[serial]
fn taking_a_queued_line_back_restarts_the_history_walk() {
    let dir = fresh_config_dir("history-unqueue");
    let _guard = TempDirGuard(dir.clone());
    let tool = dir.join("tool-a");
    let fake = FakeLlm::start(vec![
        Reply::Text("reply-one-w4q".to_string()),
        Reply::Text("reply-two-w4q".to_string()),
        Reply::Text("reply-three-w4q".to_string()),
        sleep_call(&tool),
        Reply::Text("done-w4q".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 100).unwrap();
    pty.read_for(Duration::from_millis(400));
    fake_exchange(&mut pty, "h1-w4q", "reply-one-w4q");
    fake_exchange(&mut pty, "h2-w4q", "reply-two-w4q");
    fake_exchange(&mut pty, "h3-w4q", "reply-three-w4q");
    let footer = "fake:fake-model";
    let mut toggle = false;
    let mut misses: Vec<String> = Vec::new();

    let turn_at = pty.raw().len();
    pty.send(b"call the sleep tool w4q\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-a-started"),
        "the sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"q-w4q\r").unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    assert!(
        squeezed(&frame).contains("pending:1"),
        "a line queued while the tool slept did not show `pending: 1`:\n{frame}"
    );

    let (row, frame) = up_times(&mut pty, 4, &mut toggle, footer);
    if row != "h2-w4q" {
        misses.push(format!(
            "four Ups while the tool slept did not reach `h2-w4q` (composer: `{row}`):\n{frame}"
        ));
    }
    // Alt+Up trades the walked entry for the queued line.
    pty.send(ALT_UP).unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    let row = composer_row_with_footer(&frame, footer);
    if row != "q-w4q" {
        misses.push(format!(
            "Alt+Up did not bring the queued line back (composer: `{row}`):\n{frame}"
        ));
    }
    let strip = squeezed(&above_rule(&frame));
    if !strip.contains("pending:1") || !strip.contains("h2-w4q") {
        misses.push(format!(
            "Alt+Up over a draft did not hold the draft in its place (strip should preview \
             `h2-w4q` as `pending: 1`):\n{frame}"
        ));
    }
    let (row, frame) = up_times(&mut pty, 1, &mut toggle, footer);
    if row == "h1-w4q" {
        misses.push(format!(
            "after Alt+Up took the queued line back the next Up walked on from the stale \
             cursor to `h1-w4q` (the walk was not ended):\n{frame}"
        ));
    } else if row != "h2-w4q" && row != "q-w4q" {
        misses.push(format!(
            "after Alt+Up took the queued line back the next Up did not start from the newest \
             entry (composer: `{row}`):\n{frame}"
        ));
    }

    // Release the tool: the held draft rides the next request, the
    // taken-back line does not (nothing re-queued it).
    fs::write(dir.join("tool-a-release"), b"").unwrap();
    wait_for_squeezed_after(&mut pty, "done-w4q", turn_at, "the turn's final reply");
    pty.read_for(Duration::from_millis(1200));
    let bodies = fake.bodies();
    if bodies.len() != 5 {
        misses.push(format!(
            "expected five requests, got {}:\n{}",
            bodies.len(),
            bodies.join("\n---\n")
        ));
    } else {
        // The walked entry sits in every request's history already; only
        // one more mention proves it rode as a new message.
        if bodies[4].matches("h2-w4q").count() != bodies[3].matches("h2-w4q").count() + 1 {
            misses.push(format!(
                "the draft Alt+Up held in the queue did not ride the next request as a new \
                 user message:\n{}\n--- previous request ---\n{}",
                bodies[4], bodies[3]
            ));
        }
        if bodies[4].contains("q-w4q") {
            misses.push(format!(
                "the line Alt+Up took back still reached the model:\n{}",
                bodies[4]
            ));
        }
    }
    clear_any_draft(&mut pty);
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} unqueue history expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// A bare `.agent` is not a rewind barrier.
// ---------------------------------------------------------------------

/// `before`, a bare `.agent` (prints its usage, engages nothing),
/// `after`. The picker offers BOTH prompts with no stop notice; choosing
/// `before` pops both exchanges and prefills `before`; the composer then
/// runs another exchange.
#[test]
#[serial]
fn a_bare_dot_agent_usage_print_is_not_a_rewind_barrier() {
    let dir = fresh_config_dir("bare-agent");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    dry_run_exchange(&mut pty, "before-w4a");
    let agent_at = pty.raw().len();
    pty.send(b".agent\r").unwrap();
    pty.read_for(Duration::from_millis(2000));
    let printed = since(&pty, agent_at);
    let flat = squeezed(&printed);
    assert!(
        flat.contains(".agent"),
        "the bare `.agent` command left no cell:\n{printed}"
    );
    assert!(
        !flat.contains("Rewindto"),
        "a bare `.agent` opened the rewind picker?\n{printed}"
    );
    dry_run_exchange(&mut pty, "after-w4a");

    let mut misses: Vec<String> = Vec::new();
    let picker_at = open_rewind_picker(&mut pty);
    let picker = picker_frame(&mut pty, picker_at, 101);
    let band_flat = squeezed(&picker_band(&picker));
    if !band_flat.contains("after-w4a") {
        misses.push(format!("the newest prompt is not a rewind row:\n{picker}"));
    }
    if !band_flat.contains("before-w4a") {
        misses.push(format!(
            "a bare `.agent` (usage only) cut the rewind rows: `before-w4a` is not offered:\n\
             {picker}"
        ));
    }
    if squeezed(&since(&pty, picker_at)).contains(&squeezed(STOP_NOTICE)) {
        misses.push(format!(
            "the picker says it stops at a session command though `.agent` only printed its \
             usage:\n{}",
            since(&pty, picker_at)
        ));
    }
    // Down to `before`, Enter: both exchanges go, `before` is the draft.
    pty.send(DOWN).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = repainted_frame_after_resize(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    if flat.contains("Rewindto") {
        misses.push(format!("the picker is still up after Enter:\n{frame}"));
    }
    if flat.contains(">after-w4a") || flat.contains(">before-w4a") {
        misses.push(format!(
            "choosing the older row across a bare `.agent` did not pop both exchanges:\n{frame}"
        ));
    }
    let row = composer_row_with_footer(&frame, "dryrun:dry-model");
    if row != "before-w4a" {
        misses.push(format!(
            "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    clear_any_draft(&mut pty);
    dry_run_exchange(&mut pty, "still-alive-w4a");
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} bare `.agent` rewind expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// The headless creation gate speaks the prompter's sentence.
// ---------------------------------------------------------------------

/// Runs `--headless --macro <name> go` with stdin closed; returns (exit
/// status text, stdout+stderr). The positional `go` matters: without it
/// `--headless` bails before the macro runs.
fn run_headless_macro(dir: &Path, name: &str) -> (String, String) {
    let mut cmd = ttyless_command(built_binary(), dir);
    cmd.args(["--headless", "--macro", name, "go"])
        .env("EDITOR", "/usr/bin/false")
        .env("VISUAL", "/usr/bin/false")
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

/// The first line of `everything` that names a human, blanks folded.
fn human_line(everything: &str) -> Option<String> {
    everything
        .lines()
        .find(|line| line.to_lowercase().contains("human"))
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// The longest run of consecutive words two sentences share.
fn longest_common_word_run(a: &str, b: &str) -> usize {
    let a: Vec<&str> = a.split_whitespace().collect();
    let b: Vec<&str> = b.split_whitespace().collect();
    let mut best = 0;
    for i in 0..a.len() {
        for j in 0..b.len() {
            let mut k = 0;
            while i + k < a.len() && j + k < b.len() && a[i + k] == b[j + k] {
                k += 1;
            }
            best = best.max(k);
        }
    }
    best
}

/// Macro `mkrole` = `.role w4-gate-role`, `mkmacro` = `.macro
/// w4-gate-macro`, `tmpsave` = [`.temp-role ...`, `.save role`] under
/// `--headless`: each step is echoed, nothing is created, each refusal
/// names the missing human and is not the old `Error: No role` bail; the
/// `.role` and `.macro` refusals are one sentence skeleton (swap the kind
/// and the name), and that skeleton shares a run of at least six words
/// with the headless prompter's refusal for `.save role`.
#[test]
#[serial]
fn headless_creation_refusals_share_one_sentence_with_the_prompters() {
    let dir = fresh_config_dir("headless-sentence");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    write_macro(&dir, "mkrole", &[".role w4-gate-role"]);
    write_macro(&dir, "mkmacro", &[".macro w4-gate-macro"]);
    write_macro(&dir, "tmpsave", &[".temp-role be terse w4", ".save role"]);
    fs::create_dir_all(dir.join("roles")).unwrap();

    let mut misses: Vec<String> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for (macro_name, needle, step) in [
        ("mkrole", "w4-gate-role", ".role w4-gate-role"),
        ("mkmacro", "w4-gate-macro", ".macro w4-gate-macro"),
        ("tmpsave", "roles/", ".save role"),
    ] {
        let (status, everything) = run_headless_macro(&dir, macro_name);
        if !everything.contains(&format!(">> {step}")) {
            misses.push(format!(
                "`--headless --macro {macro_name}` never echoed its step `{step}` (exit \
                 {status}):\n{everything}"
            ));
        }
        let created = files_named(&dir, needle);
        if !created.is_empty() {
            misses.push(format!(
                "headless `{step}` created {created:?} (exit {status}):\n{everything}"
            ));
        }
        if everything.contains("Error: No role") || everything.contains("Error: No macro") {
            misses.push(format!(
                "headless `{step}` still bails with the pre-gate `Error: No ...` text (exit \
                 {status}):\n{everything}"
            ));
        }
        match human_line(&everything) {
            Some(line) => lines.push(line),
            None => {
                misses.push(format!(
                    "headless `{step}` did not name the missing human (exit {status}):\n\
                     {everything}"
                ));
                lines.push(String::new());
            }
        }
    }
    if lines.iter().all(|line| !line.is_empty()) {
        let role_as_macro = lines[0]
            .replace("w4-gate-role", "w4-gate-macro")
            .replace("role", "macro");
        if role_as_macro != lines[1] {
            misses.push(format!(
                "the `.role` and `.macro` refusals are not one sentence skeleton:\n  role:  \
                 {}\n  macro: {}",
                lines[0], lines[1]
            ));
        }
        let shared = longest_common_word_run(&lines[0], &lines[2]);
        if shared < 6 {
            misses.push(format!(
                "the creation gate's sentence shares only {shared} consecutive words with the \
                 prompter's refusal:\n  gate:     {}\n  prompter: {}",
                lines[0], lines[2]
            ));
        }
    }
    assert!(
        misses.is_empty(),
        "{} headless sentence expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}
