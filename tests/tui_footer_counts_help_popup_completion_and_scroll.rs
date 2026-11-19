//! Real-process checks written from the TUI's user-facing contract, not
//! its code: the footer's `agents` count follows sub-agents the engaged
//! agent spawns (and the ones they spawn), and its `jobs` count follows a
//! background job; the key help is a bordered pop-up over the viewport
//! that leaves the composer and footer on screen, has a blank row under
//! its title, names the "Transcript view" and "Vim mode" (never "Pager" or
//! "vi normal mode") and gives Esc and Ctrl+C rows of their own; Ctrl+U
//! cuts only the cursor's line back to its start; `.ag` and `/ag` with
//! Tab complete to the agent command in the TUI and in the plain REPL, and
//! an agent's argument rows carry a kind column; a wheel notch and
//! Shift+Up scroll the transcript one row, and mouse reporting never asks
//! for any-motion tracking. Each case runs the binary on a
//! pseudo-terminal, so the file is unix-only, and the cases run one at a
//! time.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use common::fake_llm::{Chunk, FakeLlm, Reply};
use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
use serial_test::serial;

const HELP_MARK: &str = "Suspend to the shell";
const PAGE_DOWN: &[u8] = b"\x1b[6~";
const CTRL_J: &[u8] = b"\x0a";
const CTRL_U: &[u8] = b"\x15";
const ALT_B: &[u8] = b"\x1bb";
const CTRL_L: &[u8] = b"\x0c";
const SHIFT_UP: &[u8] = b"\x1b[1;2A";
const WHEEL_UP: &[u8] = b"\x1b[<64;20;5M";

fn boot(scenario: &str, extra: &str) -> (TempDirGuard, Pty) {
    let dir = fresh_config_dir(scenario);
    let guard = TempDirGuard(dir.clone());
    write_config(&dir, &format!("save_session: false\n{extra}"));
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(400));
    (guard, pty)
}

fn leave(mut pty: Pty) {
    pty.read_for(Duration::from_millis(300));
    let _ = pty.send(b"\x03");
    pty.read_for(Duration::from_millis(300));
    let _ = pty.send(b"\x03");
    pty.read_for(Duration::from_millis(300));
    let _ = pty.send(b"\x04");
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert!(!transcript.contains("panicked"), "{transcript}");
    assert_tty_restored(&transcript);
}

/// Repaints until `pred` holds on a frame or `within` passes.
fn frame_until(pty: &mut Pty, within: Duration, pred: impl Fn(&str) -> bool) -> (bool, String) {
    let deadline = Instant::now() + within;
    loop {
        let frame = flipped_frame(pty);
        if pred(&frame) {
            return (true, frame);
        }
        if Instant::now() >= deadline {
            return (false, frame);
        }
    }
}

// ---------------------------------------------------------------------
// The footer's agents and jobs counts.
// ---------------------------------------------------------------------

fn fake_client(name: &str, fake: &FakeLlm) -> String {
    format!(
        "\x20 - type: openai-compatible\n\
         \x20   name: {name}\n\
         \x20   api_base: '{}'\n\
         \x20   api_key: 'unused'\n\
         \x20   models:\n\
         \x20     - name: fake-model\n\
         \x20       max_input_tokens: 100000\n\
         \x20       supports_function_calling: true\n",
        fake.api_base()
    )
}

/// `boss` spawns `kid` on its own fake model; `kid` spawns `grandkid` on
/// a third one, so no two agents share a script.
fn write_spawning_family(dir: &Path, boss: &FakeLlm, kid: &FakeLlm, grandkid: &FakeLlm) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: fake:fake-model\n\
             function_calling_support: true\n\
             stream: true\n\
             save_session: false\n\
             vault_password_file: '{}'\n\
             clients:\n{}{}{}\
             workspace_instructions: false\n\
             memory: false\n",
            vault_pass.display(),
            fake_client("fake", boss),
            fake_client("kidfake", kid),
            fake_client("grandfake", grandkid),
        ),
    )
    .unwrap();
    for (name, model, spawns) in [
        ("boss", "fake:fake-model", true),
        ("kid", "kidfake:fake-model", true),
        ("grandkid", "grandfake:fake-model", false),
    ] {
        let agent = dir.join("agents").join(name);
        fs::create_dir_all(&agent).unwrap();
        fs::write(
            agent.join("config.yaml"),
            format!(
                "name: {name}\n\
                 description: The {name}.\n\
                 model: {model}\n\
                 instructions: You are the {name}.\n\
                 can_spawn_agents: {spawns}\n"
            ),
        )
        .unwrap();
    }
}

fn held_stream(gate: &Path, text: &str) -> Reply {
    let mut chunks: Vec<Chunk> = (0..4).map(|_| Chunk::WaitFor(gate.to_path_buf())).collect();
    chunks.push(Chunk::Text(text.to_string()));
    Reply::Streamed(chunks)
}

fn footer(frame: &str) -> String {
    let at = frame.rfind("agents").unwrap_or(0);
    squeezed(&frame[at..frame[at..].find('\n').map_or(frame.len(), |n| at + n)])
}

/// The engaged agent spawns a child, and that child spawns its own: while
/// both run the footer counts two agents, not zero.
#[test]
#[serial]
fn the_footer_counts_a_spawned_agent_and_the_agent_it_spawns() {
    let dir = fresh_config_dir("footer-agents");
    let _guard = TempDirGuard(dir.clone());
    let gate = dir.join("all.gate");
    struct OpenGate<'a>(&'a Path);
    impl Drop for OpenGate<'_> {
        fn drop(&mut self) {
            let _ = fs::write(self.0, "");
        }
    }
    let _open = OpenGate(&gate);
    let mut boss_replies = vec![Reply::ToolCall {
        name: "agent__spawn".to_string(),
        arguments: r#"{"agent":"kid","prompt":"kid task k1"}"#.to_string(),
    }];
    boss_replies.extend((0..4).map(|_| held_stream(&gate, "boss waits b1")));
    let boss = FakeLlm::start(boss_replies);
    let kid = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "agent__spawn".to_string(),
            arguments: r#"{"agent":"grandkid","prompt":"grandkid task g1"}"#.to_string(),
        },
        Reply::After {
            gate: gate.clone(),
            reply: Box::new(Reply::Text("kid done k2".to_string())),
        },
    ]);
    let grandkid = FakeLlm::start(vec![Reply::After {
        gate: gate.clone(),
        reply: Box::new(Reply::Text("grandkid done g2".to_string())),
    }]);
    write_spawning_family(&dir, &boss, &kid, &grandkid);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let idle = footer(&flipped_frame(&mut pty));
    assert!(idle.contains("agents0"), "idle footer: {idle:?}");

    pty.send(b".agent boss\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    pty.send(b"go boss\r").unwrap();
    let (one, frame) = frame_until(&mut pty, Duration::from_secs(20), |f| {
        footer(f).contains("agents1") || footer(f).contains("agents2")
    });
    assert!(
        one,
        "the footer never counted the spawned kid (boss served {}, kid served {}): {:?}\n{frame}",
        boss.served(),
        kid.served(),
        footer(&frame)
    );
    let (two, frame) = frame_until(&mut pty, Duration::from_secs(20), |f| {
        footer(f).contains("agents2")
    });
    assert!(
        two,
        "the footer never counted the grandkid the kid spawned (grandkid served {}): {:?}\n{frame}",
        grandkid.served(),
        footer(&frame)
    );
    fs::write(&gate, "").unwrap();
    let (back, frame) = frame_until(&mut pty, Duration::from_secs(30), |f| {
        footer(f).contains("agents0")
    });
    assert!(
        back,
        "the footer still counts agents after they finished: {:?}\n{frame}",
        footer(&frame)
    );
    leave(pty);
}

/// A background job the model starts in the plain session shows as
/// `jobs 1` while it runs, and it stays counted after the turn that
/// started it is interrupted.
#[test]
#[serial]
fn the_footer_counts_a_background_job_of_the_plain_session() {
    let dir = fresh_config_dir("footer-jobs");
    let _guard = TempDirGuard(dir.clone());
    let job_pid = dir.join("job.pid");
    let blocker_pid = dir.join("blocker.pid");
    let _sleepers = Sleepers(vec![job_pid.clone(), blocker_pid.clone()]);
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "job__start".to_string(),
            arguments: r#"{"tool":"fixture_sleep","arguments":{}}"#.to_string(),
        },
        Reply::ToolCall {
            name: "fixture_block".to_string(),
            arguments: "{}".to_string(),
        },
    ]);
    write_fake_llm_config_with(
        &dir,
        &fake,
        "enabled_tools:\n  - fixture_sleep\n  - fixture_block\n",
    );
    write_tool(&dir, "fixture_sleep.sh", &sleeping_tool(&job_pid));
    write_tool(&dir, "fixture_block.sh", &sleeping_tool(&blocker_pid));
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"start the sleeper\r").unwrap();
    wait_for_file(&mut pty, &job_pid, "the job's pid file");
    wait_for_file(&mut pty, &blocker_pid, "the blocking tool's pid file");
    let (counted, frame) = frame_until(&mut pty, Duration::from_secs(10), |f| {
        footer(f).contains("jobs1")
    });
    assert!(
        counted,
        "the footer does not count the running job: {:?}\n{frame}",
        footer(&frame)
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let (still, frame) = frame_until(&mut pty, Duration::from_secs(6), |f| {
        footer(f).contains("jobs1")
    });
    assert!(
        still,
        "the footer dropped the job the interrupted turn left running: {:?}\n{frame}",
        footer(&frame)
    );
    leave(pty);
}

// ---------------------------------------------------------------------
// The key help pop-up.
// ---------------------------------------------------------------------

/// Every page of the key help at 30 x 100, paging with PageDown until the
/// text stops moving.
fn help_pages(pty: &mut Pty) -> Vec<String> {
    let mut pages = vec![repainted_frame(pty, 30, 100)];
    for _ in 0..30 {
        pty.send(PAGE_DOWN).unwrap();
        pty.read_for(Duration::from_millis(200));
        repainted_frame(pty, 28, 100);
        let page = repainted_frame(pty, 30, 100);
        if Some(&page) == pages.last() {
            break;
        }
        pages.push(page);
    }
    pages
}

#[test]
#[serial]
fn the_key_help_is_a_pop_up_over_the_viewport_with_the_new_names() {
    let (_guard, mut pty) = boot("help-popup", "");
    pty.send(b".keys\r").unwrap();
    let frame = repainted_frame_showing(&mut pty, HELP_MARK, ".keys did not open the key help");
    let flat = squeezed(&frame);
    assert!(
        flat.contains("nosession") && flat.contains("agents0"),
        "the key help hides the footer, a full-screen takeover:\n{frame}"
    );
    assert!(
        frame.contains('\u{250c}') && frame.contains('\u{2518}'),
        "the key help is not drawn in a bordered box:\n{frame}"
    );

    // The repaint comes row by row with the escapes gone, so with blanks
    // squeezed out a blank bordered row is two side borders in a row.
    let title = flat
        .find("\u{250c}Keys")
        .unwrap_or_else(|| panic!("no bordered title row naming the keys:\n{frame}"));
    let after_title = &flat[title..];
    let close = after_title.find('\u{2510}').unwrap() + '\u{2510}'.len_utf8();
    let under: String = after_title[close..].chars().take(3).collect();
    assert!(
        under.starts_with("\u{2502}\u{2502}"),
        "the row under the key help's title is not blank: {under:?}\n{frame}"
    );

    let all = help_pages(&mut pty).join("\n");
    for wanted in ["Transcript view", "Vim mode", "Vim insert mode"] {
        assert!(
            squeezed(&all).contains(&squeezed(wanted)),
            "the key help never says {wanted:?}:\n{all}"
        );
    }
    for gone in [
        "Pager",
        "pager",
        "vi normal",
        "Vi normal",
        "vi insert",
        "Vi insert",
    ] {
        assert!(
            !all.contains(gone),
            "the key help still says {gone:?}:\n{all}"
        );
    }
    let flat_all = squeezed(&all);
    let question = &flat_all[flat_all
        .find("QuestionQuestion")
        .or_else(|| flat_all.find("Question\u{2502}\u{2502}y/n"))
        .unwrap_or_else(|| panic!("no Question group in the key help:\n{all}"))..];
    let question = &question[..question.find("Palette").unwrap_or(question.len())];
    assert!(
        !question.contains("Esc/Ctrl+C"),
        "Esc and Ctrl+C still share a row under Question: {question}"
    );
    assert!(
        question.contains("EscCancelthequestion") && question.contains("Ctrl+CCancelthequestion"),
        "Esc and Ctrl+C do not each have a row cancelling the question: {question}"
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(400));
    leave(pty);
}

/// No status hint in Vim mode calls it "vi normal".
#[test]
#[serial]
fn vim_mode_hints_use_the_new_name() {
    let (_guard, mut pty) = boot("vim-hint", "keybindings: vi\n");
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = flipped_frame(&mut pty);
    assert!(
        !squeezed(&frame).to_lowercase().contains("vinormal"),
        "the normal-mode hint still uses the old name:\n{frame}"
    );
    leave(pty);
}

// ---------------------------------------------------------------------
// Ctrl+U on a multi-line composer.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn ctrl_u_cuts_only_the_cursor_line_back_to_its_start() {
    let (_guard, mut pty) = boot("ctrl-u-lines", "");
    pty.send(b"one").unwrap();
    pty.send(CTRL_J).unwrap();
    pty.send(b"two three").unwrap();
    pty.read_for(Duration::from_millis(200));
    pty.send(ALT_B).unwrap();
    pty.read_for(Duration::from_millis(200));
    pty.send(CTRL_U).unwrap();
    let frame = repainted_frame_showing(&mut pty, "three", "the composer lost its text");
    assert_eq!(
        composer_row(&frame),
        "one three",
        "Ctrl+U did not cut just 'two ' on the cursor's line:\n{frame}"
    );
    leave(pty);
}

// ---------------------------------------------------------------------
// Completing the agent command, and its argument rows.
// ---------------------------------------------------------------------

fn write_agent_with_variable(dir: &Path) {
    let agent = dir.join("agents").join("helper");
    fs::create_dir_all(&agent).unwrap();
    fs::write(
        agent.join("config.yaml"),
        "name: helper\n\
         description: Helps.\n\
         model: dryrun:dry-model\n\
         instructions: You help.\n\
         variables:\n\
         \x20 - name: tone\n\
         \x20   description: How to sound\n\
         \x20 - name: pace\n\
         \x20   description: How fast to go\n",
    )
    .unwrap();
}

/// In the TUI, Tab moves the palette's highlight and Enter takes it, so
/// `.ag` then Tab then Enter leaves the agent command in the composer;
/// `/ag` does the same under `command_prefix: slash`.
#[test]
#[serial]
fn tab_completes_the_agent_command_under_either_prefix_in_the_tui() {
    for (prefix, extra) in [(".", ""), ("/", "command_prefix: slash\n")] {
        let (_guard, mut pty) = boot("tab-agent", extra);
        pty.send(format!("{prefix}ag").as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(500));
        pty.send(b"\t").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(600));
        let frame = flipped_frame(&mut pty);
        let row = composer_row(&frame);
        assert!(
            row.starts_with(&format!("{prefix}agent")),
            "{prefix}ag + Tab + Enter did not complete to {prefix}agent: {row:?}\n{frame}"
        );
        leave(pty);
    }
}

#[test]
#[serial]
fn the_agent_argument_rows_carry_a_kind_column_in_the_tui() {
    let dir = fresh_config_dir("agent-kind-tui");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\n");
    write_agent_with_variable(&dir);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b".agent helper ").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\t").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = flipped_frame(&mut pty);
    let row = frame
        .lines()
        .find(|line| line.contains("tone="))
        .unwrap_or_else(|| panic!("no completion row for the tone variable:\n{frame}"));
    let value = row.find("tone=").unwrap();
    let kind = row
        .find("variable")
        .unwrap_or_else(|| panic!("the tone row has no kind column: {row:?}\n{frame}"));
    let description = row
        .find("How to sound")
        .unwrap_or_else(|| panic!("the tone row lost its description: {row:?}\n{frame}"));
    assert!(
        value < kind && kind < description,
        "the kind column is not between the value and the description: {row:?}"
    );
    leave(pty);
}

#[test]
#[serial]
fn the_repl_completes_dot_ag_to_the_agent_command_and_shows_kinds() {
    let dir = fresh_config_dir("repl-agent");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    write_agent_with_variable(&dir);
    let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");
    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    let at = pty.raw().len();
    pty.send(b".ag").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\t").unwrap();
    // The line editor asks for the cursor position, which a pty never
    // answers, so the completion lands only after its timeout.
    let tabbed = pty
        .wait_for_after(".agent", at, PTY_DEADLINE)
        .unwrap_or_default();
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    let out = strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..]));
    assert!(
        !out.contains("Unknown command"),
        ".ag + Tab + Enter ran an unknown command, so Tab did not complete .agent:\n{out}\nraw: {:?}",
        String::from_utf8_lossy(&pty.raw()[at..])
    );
    assert!(
        tabbed.contains(".agent"),
        "the line never showed .agent after Tab:\n{tabbed}\nafter Enter:\n{out}\nraw: {:?}",
        String::from_utf8_lossy(&pty.raw()[at..])
    );

    let at = pty.raw().len();
    pty.send(b".agent helper ").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\t").unwrap();
    let out = pty
        .wait_for_after("How to sound", at, PTY_DEADLINE)
        .unwrap_or_else(|_| strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..])));
    pty.read_for(Duration::from_millis(500));
    let row = out
        .lines()
        .find(|line| line.contains("tone="))
        .unwrap_or_else(|| panic!("the REPL menu has no tone row:\n{out}"));
    let value = row.find("tone=").unwrap();
    let kind = row.find("variable");
    let description = row.find("How to sound");
    assert!(
        matches!((kind, description), (Some(k), Some(d)) if value < k && k < d),
        "the REPL menu row has no kind column between value and description: {row:?}\n{out}"
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(600));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

// ---------------------------------------------------------------------
// Scrolling the transcript a row at a time.
// ---------------------------------------------------------------------

/// The row numbers of `row-NNN` lines visible in `frame`.
fn visible_rows(frame: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut rest = frame;
    while let Some(at) = rest.find("row-") {
        let digits: String = rest[at + 4..].chars().take(3).collect();
        if let Ok(n) = digits.parse() {
            out.push(n);
        }
        rest = &rest[at + 4..];
    }
    out
}

/// A full repaint without a resize, so the scroll position is untouched.
fn redrawn(pty: &mut Pty) -> Vec<u32> {
    pty.read_for(Duration::from_millis(400));
    let from = pty.raw().len();
    pty.send(CTRL_L).unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]));
    let rows = visible_rows(&frame);
    assert!(
        !rows.is_empty(),
        "Ctrl+L repainted no transcript rows:\n{frame}"
    );
    rows
}

#[test]
#[serial]
fn a_wheel_notch_and_shift_up_scroll_the_transcript_one_row() {
    let dir = fresh_config_dir("wheel-step");
    let _guard = TempDirGuard(dir.clone());
    let body: String = (1..=150).map(|n| format!("row-{n:03}\n")).collect();
    let fake = FakeLlm::start(vec![Reply::Text(format!("```\n{body}```\n"))]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    assert!(
        find(pty.raw(), b"\x1b[?1003h", 0).is_none(),
        "the UI turned on any-motion mouse tracking"
    );
    pty.resize(30, 100).unwrap();
    pty.send(b"print the rows\r").unwrap();
    pty.wait_for("row-150", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(1500));

    let start = redrawn(&mut pty);
    let top = *start.iter().min().unwrap();
    pty.send(WHEEL_UP).unwrap();
    let one = redrawn(&mut pty);
    assert_eq!(
        *one.iter().min().unwrap(),
        top - 1,
        "one wheel notch did not scroll exactly one row: {start:?} -> {one:?}"
    );
    for _ in 0..3 {
        pty.send(WHEEL_UP).unwrap();
        pty.read_for(Duration::from_millis(80));
    }
    let four = redrawn(&mut pty);
    assert_eq!(
        *four.iter().min().unwrap(),
        top - 4,
        "four wheel notches did not scroll four rows: {start:?} -> {four:?}"
    );
    pty.send(SHIFT_UP).unwrap();
    let five = redrawn(&mut pty);
    assert_eq!(
        *five.iter().min().unwrap(),
        top - 5,
        "Shift+Up did not scroll exactly one row: {four:?} -> {five:?}"
    );
    leave(pty);
}
