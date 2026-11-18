//! Real-process checks of the transcript the full-screen UI prints on the
//! normal screen when it leaves: on Ctrl+C, `.exit` and SIGTERM, nothing
//! before any turn or under `tui_dump_on_exit: false`, the printed reply
//! without colour under `NO_COLOR`, and row for row what the plain REPL
//! prints for the same reply, and a bounded, clean exit while a roster
//! cancel still waits out a job's kill grace (the Ctrl+D case lives in
//! `tui_binary.rs`, the cold start in `tui_binary_lifecycle.rs`). Each
//! case runs the binary on a pseudo-terminal under a bash wrapper that
//! reports the exit code and `stty -a` afterwards, so the file is
//! unix-only. The cases run one at a time: each spawn forks a session
//! leader and takes a controlling tty, and a fork landing between another
//! test's `openpty` and its `exec` has failed the spawn with a garbled
//! errno.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use common::fake_llm::{FakeLlm, Reply};
use common::pty::strip_ansi;
use common::tui_harness::*;
use serial_test::serial;

/// Ctrl+C on an idle empty composer leaves the UI like Ctrl+D does, and
/// the transcript is printed on the normal screen on that path too.
#[test]
#[serial]
fn ctrl_c_on_an_idle_composer_also_prints_the_transcript() {
    leaving_prints_the_transcript(
        "ctrl-c-dump",
        |pty, _| pty.send(b"\x03").unwrap(),
        "prompt to keep z1c",
        "the reply to keep h3w",
    );
}

/// `.exit` leaves the UI too, and prints the transcript the same way.
#[test]
#[serial]
fn dot_exit_also_prints_the_transcript() {
    leaving_prints_the_transcript(
        "dot-exit-dump",
        |pty, _| pty.send(b".exit\r").unwrap(),
        "prompt to keep e8y",
        "the reply to keep m4s",
    );
}

/// SIGTERM after a finished turn leaves the UI and prints the transcript
/// like a key does.
#[test]
#[serial]
fn sigterm_after_a_turn_also_prints_the_transcript() {
    leaving_prints_the_transcript(
        "sigterm-dump",
        |_, pid| {
            // SAFETY: kill takes a pid and a signal number and touches no memory.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        },
        "prompt to keep s7g",
        "the reply to keep t4m",
    );
}

/// With `tui_dump_on_exit: false` the transcript is not printed when the
/// UI leaves: the wrapper's report follows the leave directly.
#[test]
#[serial]
fn tui_dump_on_exit_false_prints_nothing_between_the_leave_and_the_report() {
    let dir = fresh_config_dir("dump-off");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text("the reply to drop k4w".to_string())]);
    write_fake_llm_config_with(&dir, &fake, "tui_dump_on_exit: false\n");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"prompt to drop j3q\r").unwrap();
    wait_for_squeezed(&mut pty, "the reply to drop k4w", "the reply");
    pty.read_for(Duration::from_millis(800));
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    let between = strip_ansi(&String::from_utf8_lossy(&raw_between_leave_and_report(
        &pty,
    )));
    assert!(
        between.trim().is_empty(),
        "the transcript was printed with tui_dump_on_exit: false: {:?}",
        between.trim()
    );
}

/// A first start where nothing is said: Ctrl+D prints no transcript, no
/// banner and no log line before the report.
#[test]
#[serial]
fn ctrl_d_before_any_turn_prints_nothing_on_the_normal_screen() {
    let dir = fresh_config_dir("dump-empty");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let between = strip_ansi(&String::from_utf8_lossy(&raw_between_leave_and_report(
        &pty,
    )));
    assert!(
        between.trim().is_empty(),
        "bytes were printed after the leave with no turn: {:?}",
        between.trim()
    );
}

/// A markdown reply with a heading, bold, inline code, a list and a
/// fenced block.
const MARKDOWN_REPLY: &str = "# Heading r6t\n\nSome **bold b4k** and `code c2m` here.\n\n- item one i1q\n- item two i2q\n\n```rust\nfn fenced_f7p() {}\n```\n";

fn is_color_sgr(params: &[u16]) -> bool {
    params
        .iter()
        .any(|p| matches!(p, 30..=38 | 40..=48 | 90..=97 | 100..=107))
}

/// With `NO_COLOR` set the transcript printed on exit carries no colour
/// escape, while the reply's words are all there.
#[test]
#[serial]
fn the_transcript_printed_on_exit_honours_no_color() {
    let dir = fresh_config_dir("dump-no-color");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(MARKDOWN_REPLY.to_string())]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[("NO_COLOR", "1")]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"markdown please m9d\r").unwrap();
    wait_for_squeezed(&mut pty, "fenced_f7p", "the reply");
    pty.read_for(Duration::from_millis(800));
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    let between = raw_between_leave_and_report(&pty);
    let text = squeezed(&strip_ansi(&String::from_utf8_lossy(&between)));
    for word in [
        "Headingr6t",
        "boldb4k",
        "codec2m",
        "itemtwoi2q",
        "fenced_f7p",
    ] {
        assert!(
            text.contains(word),
            "{word:?} missing from the dump: {text:?}"
        );
    }
    let colored: Vec<Vec<u16>> = sgr_params(&between)
        .into_iter()
        .filter(|params| is_color_sgr(params))
        .collect();
    assert!(
        colored.is_empty(),
        "the dump carries colour under NO_COLOR: {colored:?}\n{:?}",
        String::from_utf8_lossy(&between)
    );
}

/// The non-blank text rows of a stretch of output, trimmed.
fn text_rows(raw: &[u8]) -> Vec<String> {
    strip_ansi(&String::from_utf8_lossy(raw))
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// The reply rows the transcript printed on exit holds are the rows the
/// plain REPL prints for the same reply, in the same order.
#[test]
#[serial]
fn the_transcript_printed_on_exit_renders_the_reply_like_the_repl() {
    let repl_dir = fresh_config_dir("dump-vs-repl-repl");
    let _repl_guard = TempDirGuard(repl_dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(MARKDOWN_REPLY.to_string())]);
    write_fake_llm_config(&repl_dir, &fake);
    let mut repl = spawn_tui(&repl_dir, &[("COYOTE_UI", "repl")]);
    repl.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
    repl.read_for(Duration::from_millis(300));
    let from = repl.raw().len();
    repl.send(b"markdown please m9d\r").unwrap();
    repl.wait_for_after("fenced_f7p", from, PTY_DEADLINE)
        .unwrap();
    repl.read_for(Duration::from_millis(1200));
    let repl_rows = text_rows(&repl.raw()[from..]);
    repl.send(b"\x04").unwrap();
    let transcript = repl.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let start = repl_rows
        .iter()
        .position(|row| row.contains("Heading r6t"))
        .unwrap_or_else(|| panic!("the REPL printed no heading: {repl_rows:#?}"));
    let end = repl_rows
        .iter()
        .position(|row| row.contains("fenced_f7p"))
        .unwrap();
    let repl_reply = &repl_rows[start..=end];

    let dir = fresh_config_dir("dump-vs-repl-tui");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(MARKDOWN_REPLY.to_string())]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"markdown please m9d\r").unwrap();
    wait_for_squeezed(&mut pty, "fenced_f7p", "the reply");
    pty.read_for(Duration::from_millis(800));
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let dumped = text_rows(&raw_between_leave_and_report(&pty));
    let at = dumped
        .iter()
        .position(|row| row.contains("Heading r6t"))
        .unwrap_or_else(|| panic!("the dump holds no heading: {dumped:#?}"));
    let dump_reply: Vec<String> = dumped[at..]
        .iter()
        .take(repl_reply.len())
        .cloned()
        .collect();
    assert_eq!(dump_reply, repl_reply, "dump rows differ from the REPL's");
}

/// A job that shrugs off SIGTERM: it records its pid and loops forever.
fn term_ignoring_tool(pid_file: &Path) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         \n\
         # @describe Ignore SIGTERM and loop.\n\
         \n\
         # @env LLM_OUTPUT=/dev/stdout The output path\n\
         \n\
         main() {{\n\
         \x20   trap '' TERM\n\
         \x20   echo $$ > \"{}\"\n\
         \x20   for _ in $(seq 1 600); do sleep 0.5; done\n\
         }}\n",
        pid_file.display()
    )
}

/// `x` on a job that ignores SIGTERM starts a cancel that has to sit
/// through the kill grace; the user leaves at once. The exit is bounded,
/// exits 0, restores the terminal and prints the transcript.
#[test]
#[serial]
fn leaving_during_a_slow_roster_cancel_is_bounded_and_clean() {
    let dir = fresh_config_dir("roster-slow-cancel-exit");
    let _guard = TempDirGuard(dir.clone());
    let job_pid = dir.join("job.pid");
    let blocker_pid = dir.join("blocker.pid");
    let _sleepers = Sleepers(vec![blocker_pid.clone()]);
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "job__start".to_string(),
            arguments: r#"{"tool":"fixture_stubborn","arguments":{}}"#.to_string(),
        },
        Reply::ToolCall {
            name: "fixture_block".to_string(),
            arguments: "{}".to_string(),
        },
    ]);
    write_fake_llm_config_with(
        &dir,
        &fake,
        "enabled_tools:\n  - fixture_stubborn\n  - fixture_block\n",
    );
    write_tool(&dir, "fixture_stubborn.sh", &term_ignoring_tool(&job_pid));
    write_tool(&dir, "fixture_block.sh", &sleeping_tool(&blocker_pid));
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"start the stubborn one s5w\r").unwrap();
    wait_for_file(&mut pty, &job_pid, "the job's pid file");
    wait_for_file(&mut pty, &blocker_pid, "the blocking tool's pid file");
    pty.read_for(Duration::from_millis(1200));
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let pid: i32 = fs::read_to_string(&job_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    struct KillOnDrop(i32);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            // SAFETY: kill takes a pid and a signal number and touches no memory.
            let _ = unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }
    let _kill = KillOnDrop(pid);
    assert!(alive(pid), "the job died with the interrupted turn");

    pty.send(ALT_T).unwrap();
    let roster =
        repainted_frame_showing(&mut pty, "fixture_stubborn", "Alt+T did not list the job");
    assert!(
        roster_band(&roster).contains("running"),
        "the job is not running on the roster:\n{roster}"
    );
    pty.send(b"x").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    let from = pty.raw().len();
    let started = std::time::Instant::now();
    pty.send(b"\x04").unwrap();
    let transcript = pty
        .wait_for("tty-probe-done", Duration::from_secs(40))
        .unwrap_or_else(|err| panic!("the UI did not leave within 40s: {err:?}"));
    let took = started.elapsed();
    let tail = String::from_utf8_lossy(&pty.raw()[from..]).to_string();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(!transcript.contains("panicked"), "{transcript}");
    let printed = strip_ansi(&tail);
    let left = printed.rfind("exit=").unwrap_or(printed.len());
    assert!(
        printed[..left].contains("s5w"),
        "the transcript was not printed on exit:\n{printed}"
    );
    assert!(
        took < Duration::from_secs(30),
        "leaving during a slow cancel took {took:?}"
    );
}
