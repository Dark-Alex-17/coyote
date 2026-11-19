//! Real-process checks written from the TUI's user-facing contract, not
//! its code, for what the footer, completion and scrolling cases next door
//! leave open: the command popup paints a command's name and its
//! description in two different colours, the description in the yellow
//! the plain REPL's menu uses, and paints neither under `NO_COLOR`; a
//! wheel notch down scrolls back one row; a view scrolled up holds still
//! while the reply below it keeps streaming; and the footer's `jobs` count
//! follows a background job a spawned sub-agent starts. Each case runs the
//! binary on a pseudo-terminal, so the file is unix-only, and the cases
//! run one at a time.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use common::fake_llm::{Chunk, FakeLlm, Reply};
use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
use serial_test::serial;

const CTRL_L: &[u8] = b"\x0c";
const WHEEL_UP: &[u8] = b"\x1b[<64;20;5M";
const WHEEL_DOWN: &[u8] = b"\x1b[<65;20;5M";

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

// ---------------------------------------------------------------------
// The command popup's colours.
// ---------------------------------------------------------------------

/// The base colour (0-15) an SGR list sets the foreground to, if any:
/// `33` and `38;5;3` are both yellow.
fn foreground(params: &[u16]) -> Option<u16> {
    let mut fg = None;
    let mut i = 0;
    while i < params.len() {
        match params[i] {
            p @ 30..=37 => fg = Some(p - 30),
            p @ 90..=97 => fg = Some(p - 90 + 8),
            38 if params.get(i + 1) == Some(&5) => {
                fg = params.get(i + 2).copied().filter(|&c| c < 16);
                i += 3;
                continue;
            }
            38 | 48 | 58 => {
                i += match params.get(i + 1) {
                    Some(5) => 3,
                    Some(2) => 5,
                    _ => 1,
                };
                continue;
            }
            39 | 0 => fg = None,
            _ => {}
        }
        i += 1;
    }
    fg
}

/// The foreground the last painting of `needle` after `from` was drawn
/// in, read from the SGR sequences in the 48 bytes before it, and whether
/// any colour SGR at all sat there.
fn painted_colour(raw: &[u8], from: usize, needle: &[u8]) -> Option<(Option<u16>, bool)> {
    let mut at = from;
    let mut last = None;
    while let Some(found) = find(raw, needle, at) {
        let window = sgr_params(&raw[found.saturating_sub(48)..found]);
        let fg = window.iter().fold(None, |fg, params| {
            foreground(params).or(if params.contains(&0) || params.contains(&39) {
                None
            } else {
                fg
            })
        });
        let coloured = window.iter().any(|p| is_colour_sgr(p));
        last = Some((fg, coloured));
        at = found + needle.len();
    }
    last
}

fn type_dot_ag(extra_env: &[(&str, &str)]) -> (TempDirGuard, Pty, usize) {
    let dir = fresh_config_dir("popup-colours");
    let guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, extra_env);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(400));
    let from = pty.raw().len();
    pty.send(b".ag").unwrap();
    pty.wait_for_after("Use an agent", from, PTY_DEADLINE)
        .unwrap_or_else(|_| panic!("the popup never offered the agent command"));
    pty.read_for(Duration::from_millis(300));
    (guard, pty, from)
}

#[test]
#[serial]
fn the_command_popup_paints_names_and_descriptions_in_distinct_repl_colours() {
    let (_guard, pty, from) = type_dot_ag(&[]);
    let raw = pty.raw().to_vec();
    let (name, _) = painted_colour(&raw, from, b".agent").expect("no .agent row painted");
    let (description, _) =
        painted_colour(&raw, from, b"Use an agent").expect("no description painted");
    let shown = String::from_utf8_lossy(&raw[from..]).into_owned();
    assert!(name.is_some(), "the command name has no colour: {shown:?}");
    assert_eq!(
        description,
        Some(3),
        "the description is not in the REPL menu's yellow: {shown:?}"
    );
    assert_ne!(
        name, description,
        "the command name and its description share a colour: {shown:?}"
    );
    leave(pty);
}

#[test]
#[serial]
fn no_color_leaves_the_command_popup_uncoloured() {
    let (_guard, pty, from) = type_dot_ag(&[("NO_COLOR", "1")]);
    let raw = pty.raw().to_vec();
    let shown = String::from_utf8_lossy(&raw[from..]).into_owned();
    for needle in [&b".agent"[..], b"Use an agent"] {
        let (fg, coloured) = painted_colour(&raw, from, needle).unwrap_or_else(|| {
            panic!(
                "{:?} never painted: {shown:?}",
                String::from_utf8_lossy(needle)
            )
        });
        assert!(
            fg.is_none() && !coloured,
            "NO_COLOR still colours {:?}: {shown:?}",
            String::from_utf8_lossy(needle)
        );
    }
    leave(pty);
}

// ---------------------------------------------------------------------
// Scrolling: a notch down, and a scrolled view while a reply streams.
// ---------------------------------------------------------------------

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

/// The topmost transcript row of a full repaint that leaves the scroll
/// position alone.
fn top_row(pty: &mut Pty) -> u32 {
    pty.read_for(Duration::from_millis(400));
    let from = pty.raw().len();
    pty.send(CTRL_L).unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]));
    *visible_rows(&frame)
        .iter()
        .min()
        .unwrap_or_else(|| panic!("Ctrl+L repainted no transcript rows:\n{frame}"))
}

fn rows(range: std::ops::RangeInclusive<u32>) -> String {
    range.map(|n| format!("row-{n:03}\n")).collect()
}

#[test]
#[serial]
fn a_wheel_notch_down_scrolls_back_one_row() {
    let dir = fresh_config_dir("wheel-down");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(format!("```\n{}```\n", rows(1..=150)))]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(30, 100).unwrap();
    pty.send(b"print the rows\r").unwrap();
    pty.wait_for("row-150", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(1500));
    let bottom = top_row(&mut pty);
    for _ in 0..4 {
        pty.send(WHEEL_UP).unwrap();
        pty.read_for(Duration::from_millis(80));
    }
    let up = top_row(&mut pty);
    assert_eq!(up, bottom - 4, "four notches up did not scroll four rows");
    pty.send(WHEEL_DOWN).unwrap();
    let down = top_row(&mut pty);
    assert_eq!(down, up + 1, "one notch down did not scroll back one row");
    for _ in 0..10 {
        pty.send(WHEEL_DOWN).unwrap();
        pty.read_for(Duration::from_millis(80));
    }
    assert_eq!(
        top_row(&mut pty),
        bottom,
        "scrolling down past the end did not stop at the bottom"
    );
    leave(pty);
}

#[test]
#[serial]
fn a_view_scrolled_up_holds_still_while_the_reply_keeps_streaming() {
    let dir = fresh_config_dir("scroll-hold");
    let _guard = TempDirGuard(dir.clone());
    let gate = dir.join("more.gate");
    let fake = FakeLlm::start(vec![Reply::Streamed(vec![
        Chunk::Text(format!("```\n{}", rows(1..=120))),
        Chunk::WaitFor(gate.clone()),
        Chunk::Text(rows(121..=160)),
        Chunk::Text("```\nall rows sent\n".to_string()),
    ])]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(30, 100).unwrap();
    pty.send(b"print the rows\r").unwrap();
    pty.wait_for("row-120", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(800));
    let live = top_row(&mut pty);
    for _ in 0..6 {
        pty.send(WHEEL_UP).unwrap();
        pty.read_for(Duration::from_millis(80));
    }
    let held = top_row(&mut pty);
    assert_eq!(
        held,
        live - 6,
        "six notches up did not scroll six rows mid-stream"
    );
    fs::write(&gate, "").unwrap();
    let deadline = Instant::now() + PTY_DEADLINE;
    while fake.served() < 1 && Instant::now() < deadline {
        pty.read_for(Duration::from_millis(200));
    }
    pty.read_for(Duration::from_millis(2000));
    let after = top_row(&mut pty);
    assert_eq!(
        after, held,
        "the scrolled view moved while the reply streamed on: row-{held:03} -> row-{after:03}"
    );
    leave(pty);
}

// ---------------------------------------------------------------------
// The footer's jobs count, for a job a spawned sub-agent starts.
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

fn footer(frame: &str) -> String {
    let at = frame.rfind("agents").unwrap_or(0);
    squeezed(&frame[at..frame[at..].find('\n').map_or(frame.len(), |n| at + n)])
}

#[test]
#[serial]
fn the_footer_counts_a_background_job_a_spawned_agent_starts() {
    let dir = fresh_config_dir("child-job");
    let _guard = TempDirGuard(dir.clone());
    let gate = dir.join("all.gate");
    let job_pid = dir.join("job.pid");
    let _sleepers = Sleepers(vec![job_pid.clone()]);
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
    boss_replies.extend((0..4).map(|_| {
        Reply::Streamed(vec![
            Chunk::WaitFor(gate.clone()),
            Chunk::Text("boss waits b1".to_string()),
        ])
    }));
    let boss = FakeLlm::start(boss_replies);
    let kid = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "job__start".to_string(),
            arguments: r#"{"tool":"fixture_sleep","arguments":{}}"#.to_string(),
        },
        Reply::After {
            gate: gate.clone(),
            reply: Box::new(Reply::Text("kid done k2".to_string())),
        },
    ]);
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: fake:fake-model\n\
             function_calling_support: true\n\
             stream: true\n\
             save_session: false\n\
             enabled_tools:\n  - fixture_sleep\n\
             vault_password_file: '{}'\n\
             clients:\n{}{}\
             workspace_instructions: false\n\
             memory: false\n",
            vault_pass.display(),
            fake_client("fake", &boss),
            fake_client("kidfake", &kid),
        ),
    )
    .unwrap();
    write_tool(&dir, "fixture_sleep.sh", &sleeping_tool(&job_pid));
    for (name, model, extra) in [
        ("boss", "fake:fake-model", "can_spawn_agents: true\n"),
        (
            "kid",
            "kidfake:fake-model",
            "global_tools:\n  - fixture_sleep.sh\n",
        ),
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
                 {extra}"
            ),
        )
        .unwrap();
    }
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".agent boss\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    pty.send(b"go boss\r").unwrap();
    wait_for_file(&mut pty, &job_pid, "the kid's job pid file");
    let deadline = Instant::now() + Duration::from_secs(10);
    let frame = loop {
        let frame = flipped_frame(&mut pty);
        if footer(&frame).contains("jobs1") || Instant::now() >= deadline {
            break frame;
        }
    };
    assert!(
        footer(&frame).contains("jobs1"),
        "the footer does not count the job the spawned kid started (kid served {}): {:?}\n{frame}",
        kid.served(),
        footer(&frame)
    );
    leave(pty);
}
