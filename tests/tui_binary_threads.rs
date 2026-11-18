//! Real-process checks of the full-screen UI over a spawned sub-agent: the
//! Alt+T roster lists the child while it runs after `.agent` engaged a new
//! supervisor, `x` on its row cancels it, the pager's thread picker lists
//! the child with its status, a chosen child's transcript is read from its
//! session file and read again when the child flushes it, a child file that
//! does not parse degrades without a panic, a child's question reaches the
//! roster and its answer reaches the child, and the transcript is printed on
//! the normal screen when the UI leaves with the pager open. A question
//! leaves the roster when its asker is cancelled, along with a grandchild's
//! question when its parent is cancelled, and a question asked after that
//! cancel is never listed; answering a question whose asker stopped
//! waiting says the reply was not delivered. The root agent and the child
//! talk to separate fake models, so the order of their requests is fixed.
//! The pager's own keys over one tool's whole output (scrolling a long
//! one, CRLF line endings, runs of CRs before an LF, an output over the
//! per-call cap) live here too. Unix-only, as the pty it drives is; the
//! cases run one at a time.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::fake_llm::{Chunk, FakeLlm, Reply};
use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
use serial_test::serial;

/// A config with two openai-compatible clients: `fake` for the root agent
/// and `kidfake` for the child, each on its own fake model.
fn write_two_model_config(dir: &Path, boss: &FakeLlm, kid: &FakeLlm, extra: &str) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: fake:fake-model\n\
             function_calling_support: true\n\
             stream: true\n\
             save_session: false\n\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai-compatible\n\
             \x20   name: fake\n\
             \x20   api_base: '{boss}'\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: fake-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             \x20 - type: openai-compatible\n\
             \x20   name: kidfake\n\
             \x20   api_base: '{kid}'\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: fake-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             workspace_instructions: false\n\
             memory: false\n\
             {extra}",
            vault_pass = vault_pass.display(),
            boss = boss.api_base(),
            kid = kid.api_base(),
        ),
    )
    .unwrap();
}

/// `boss` may spawn agents and has the root gate tool; `kid` runs on the
/// second fake model with the kid gate tool.
fn write_agents(dir: &Path) {
    let boss = dir.join("agents").join("boss");
    fs::create_dir_all(&boss).unwrap();
    fs::write(
        boss.join("config.yaml"),
        "name: boss\n\
         description: Spawns the kid.\n\
         model: fake:fake-model\n\
         instructions: You are the boss.\n\
         can_spawn_agents: true\n\
         global_tools:\n\
         \x20 - root_mark.sh\n",
    )
    .unwrap();
    let kid = dir.join("agents").join("kid");
    fs::create_dir_all(&kid).unwrap();
    fs::write(
        kid.join("config.yaml"),
        "name: kid\n\
         description: Does the kid task.\n\
         model: kidfake:fake-model\n\
         instructions: You are the kid.\n\
         global_tools:\n\
         \x20 - kid_mark.sh\n\
         \x20 - kid_gate.sh\n",
    )
    .unwrap();
}

/// A tool that waits (a minute at most) until `after` exists, writes
/// `started`, then waits until `gate` exists and says `said`. Waiting for
/// `after` first keeps the root's model request from being in flight
/// while this tool blocks a worker.
fn gate_tool(after: &Path, started: &Path, gate: &Path, said: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         \n\
         # @describe Wait for the gate.\n\
         \n\
         # @env LLM_OUTPUT=/dev/stdout The output path\n\
         \n\
         main() {{\n\
         \x20   for _ in $(seq 1 300); do [ -e \"{after}\" ] && break; sleep 0.2; done\n\
         \x20   sleep 1\n\
         \x20   echo $$ > \"{started}\"\n\
         \x20   for _ in $(seq 1 300); do [ -e \"{gate}\" ] && break; sleep 0.2; done\n\
         \x20   echo \"{said}\" >> \"$LLM_OUTPUT\"\n\
         }}\n",
        after = after.display(),
        started = started.display(),
        gate = gate.display(),
    )
}

/// A tool that writes `started` and says `said`; it returns at once.
fn mark_tool(started: &Path, said: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         \n\
         # @describe Mark that the agent runs.\n\
         \n\
         # @env LLM_OUTPUT=/dev/stdout The output path\n\
         \n\
         main() {{\n\
         \x20   echo $$ > \"{started}\"\n\
         \x20   echo \"{said}\" >> \"$LLM_OUTPUT\"\n\
         }}\n",
        started = started.display(),
    )
}

struct Scene {
    dir: PathBuf,
    root_started: PathBuf,
    root_gate: PathBuf,
    kid_started: PathBuf,
    kid_gate: PathBuf,
    log: PathBuf,
    boss: FakeLlm,
    kid: FakeLlm,
    /// Last because fields drop in declaration order, after `Drop` opens
    /// the gates, so the dir outlives every file and handle in it.
    _dir_guard: TempDirGuard,
}

impl Scene {
    /// Keeps the binary's log outside the temp dir the guard removes, so a
    /// failed case can be read afterwards.
    fn log_env(&self) -> [(&'static str, &str); 1] {
        [("COYOTE_LOG_PATH", self.log.to_str().unwrap())]
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        // Open both gates so no fixture tool outlives the case.
        let _ = fs::write(&self.root_gate, "");
        let _ = fs::write(&self.kid_gate, "");
    }
}

/// A streamed reply held open on the fake's side until `gate` exists (up
/// to four holds of the fake's limit each), then `text`. The hold sits in
/// the fake model, not in a tool, so no tokio worker of the binary blocks
/// while the case looks at the screen.
fn held(gate: &Path, text: &str) -> Reply {
    let mut chunks: Vec<Chunk> = (0..4).map(|_| Chunk::WaitFor(gate.to_path_buf())).collect();
    chunks.push(Chunk::Text(text.to_string()));
    Reply::Streamed(chunks)
}

/// `kid_replies` gets the kid gate path, so a script can hold on it.
fn scene(name: &str, kid_replies: impl FnOnce(&Path) -> Vec<Reply>, extra: &str) -> Scene {
    let dir = fresh_config_dir(name);
    let root_started = dir.join("root.started");
    let root_gate = dir.join("root.gate");
    let kid_started = dir.join("kid.started");
    let kid_gate = dir.join("kid.gate");
    // The spawn, then the root's turn held on the root gate; the texts that
    // follow answer the turn-end reminders about the kid.
    let mut boss_replies = vec![
        Reply::ToolCall {
            name: "agent__spawn".to_string(),
            arguments: r#"{"agent":"kid","prompt":"kid task q7k"}"#.to_string(),
        },
        Reply::ToolCall {
            name: "root_mark".to_string(),
            arguments: "{}".to_string(),
        },
    ];
    boss_replies.extend((0..4).map(|_| held(&root_gate, "root waits r2g")));
    let boss = FakeLlm::start(boss_replies);
    let kid = FakeLlm::start(kid_replies(&kid_gate));
    write_two_model_config(&dir, &boss, &kid, extra);
    write_agents(&dir);
    write_tool(
        &dir,
        "root_mark.sh",
        &mark_tool(&root_started, "root marked r1m"),
    );
    write_tool(
        &dir,
        "kid_mark.sh",
        &mark_tool(&kid_started, "kid marked k1m"),
    );
    write_tool(
        &dir,
        "kid_gate.sh",
        &gate_tool(
            &root_started,
            &dir.join("kid.gated"),
            &kid_gate,
            "kid gate open k2g",
        ),
    );
    let log = std::env::temp_dir().join(format!("coyote-probe-threads-{name}.log"));
    let _ = fs::remove_file(&log);
    Scene {
        log,
        _dir_guard: TempDirGuard(dir.clone()),
        dir,
        root_started,
        root_gate,
        kid_started,
        kid_gate,
        boss,
        kid,
    }
}

/// The kid marks that it runs, then its model holds the final text until
/// the kid gate exists. A child's model requests do not stream, so the
/// hold is an [`Reply::After`] on the kid's fake, never a tool that would
/// block a worker of the binary.
fn kid_mark_then_held(gate: &Path) -> Vec<Reply> {
    vec![
        Reply::ToolCall {
            name: "kid_mark".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::After {
            gate: gate.to_path_buf(),
            reply: Box::new(Reply::Text("kid done z4p".to_string())),
        },
    ]
}

/// The kid sits in a tool that blocks until the kid gate exists (after the
/// root is in its own turn), then gives its final text.
fn kid_in_a_blocking_tool(_gate: &Path) -> Vec<Reply> {
    vec![
        Reply::ToolCall {
            name: "kid_mark".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::ToolCall {
            name: "kid_gate".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text("kid done z4p".to_string()),
    ]
}

/// Engages `boss` with `.agent` after the UI is up, so the roster has to
/// follow the supervisor the engage installs, then starts the turn that
/// spawns the kid and waits for both gate tools to run.
fn engage_and_spawn(pty: &mut Pty, scene: &Scene) {
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".agent boss\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    pty.send(b"go boss\r").unwrap();
    wait_for_file(pty, &scene.kid_started, "the kid's mark tool");
    wait_for_file(pty, &scene.root_started, "the root's mark tool");
    pty.read_for(Duration::from_millis(800));
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

/// The text of the row that mentions `needle`, from that point to the
/// end of the line in the repaint (rows in order, escapes gone).
fn row_after<'a>(frame: &'a str, needle: &str) -> Option<&'a str> {
    let at = frame.rfind(needle)?;
    let rest = &frame[at..];
    Some(&rest[..rest.find('\n').unwrap_or(rest.len()).min(90)])
}

/// The thread picker's band of a repainted frame: from its header down to
/// the key legend under the list, so a word elsewhere on screen does not
/// count as a picker row's.
fn picker_band(frame: &str) -> &str {
    let header = frame
        .rfind("Esc = back")
        .unwrap_or_else(|| panic!("the picker's header is not on screen:\n{frame}"));
    let band = &frame[header..];
    &band[..band.find("j/k move").unwrap_or(band.len())]
}

fn leave(pty: &mut Pty, scene: &Scene) -> String {
    let _ = fs::write(&scene.kid_gate, "");
    let _ = fs::write(&scene.root_gate, "");
    // Let the turn wind down (the turn-end reminders run out against the
    // fake's fallback reply), then interrupt anything left and leave.
    // A Ctrl+C on an idle empty composer leaves too, so a send after the
    // UI already left is not an error here.
    pty.read_for(Duration::from_millis(3000));
    let _ = pty.send(b"\x03");
    pty.read_for(Duration::from_millis(800));
    let _ = pty.send(b"\x03");
    pty.read_for(Duration::from_millis(500));
    let _ = pty.send(b"\x04");
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert!(!transcript.contains("panicked"), "{transcript}");
    assert_tty_restored(&transcript);
    transcript
}

fn children_files(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.parent().and_then(Path::file_name) == Some("children".as_ref()) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

// ---------------------------------------------------------------------------
// The roster after `.agent` engage, and `x` on an agent row.
// ---------------------------------------------------------------------------

/// While a spawned child sits in a tool the UI keeps drawing: typed keys
/// reach the composer and the screen answers a resize, so the roster and
/// the pager can be used to watch the child at all.
#[test]
#[serial]
fn the_ui_keeps_drawing_while_a_child_agent_runs_a_tool() {
    let scene = scene("thr-live", kid_in_a_blocking_tool, "");
    let mut pty = spawn_tui(&scene.dir, &scene.log_env());
    engage_and_spawn(&mut pty, &scene);
    wait_for_file(
        &mut pty,
        &scene.dir.join("kid.gated"),
        "the kid's gate tool",
    );

    let before = pty.raw().len();
    pty.send(b"typed while the kid works t8w").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut echoed = false;
    while Instant::now() < deadline {
        pty.read_for(Duration::from_millis(250));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        if squeezed(&since).contains("typedwhilethekidworkst8w") {
            echoed = true;
            break;
        }
    }
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    let _ = fs::write(&scene.kid_gate, "");
    let _ = fs::write(&scene.root_gate, "");
    assert!(
        echoed,
        "the UI drew nothing for 10 s while the kid sat in its tool \
         ({} bytes since the keys): {since:?}\nlog: {}",
        pty.raw().len() - before,
        scene.log.display()
    );
    let transcript = leave(&mut pty, &scene);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// After `.agent boss` the roster follows the supervisor the engage
/// installed: Alt+T mid-turn lists the spawned kid as running.
#[test]
#[serial]
fn alt_t_after_dot_agent_engage_lists_the_spawned_child_as_running() {
    let scene = scene("thr-roster", kid_mark_then_held, "");
    let mut pty = spawn_tui(&scene.dir, &scene.log_env());
    engage_and_spawn(&mut pty, &scene);

    pty.send(ALT_T).unwrap();
    let roster = repainted_frame_showing(&mut pty, "Esc = close", "Alt+T did not open the roster");
    let band = roster_band(&roster);
    assert!(
        band.contains("kid") && band.contains("running"),
        "the roster does not list the spawned kid as running after .agent engage: {band:?}\n{roster}"
    );
    pty.send(b"\x1b").unwrap();
    let transcript = leave(&mut pty, &scene);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// `x` on the kid's roster row cancels it: the kid never gets to its
/// final reply and the roster stops listing it as running.
#[test]
#[serial]
fn x_on_a_running_child_agent_row_cancels_it() {
    let scene = scene("thr-cancel", kid_mark_then_held, "");
    let mut pty = spawn_tui(&scene.dir, &scene.log_env());
    engage_and_spawn(&mut pty, &scene);

    pty.send(ALT_T).unwrap();
    let roster = repainted_frame_showing(&mut pty, "Esc = close", "Alt+T did not open the roster");
    assert!(
        roster_band(&roster).contains("kid"),
        "no kid row to cancel:\n{roster}"
    );
    pty.send(b"x").unwrap();
    let (cancelled, after) = frame_until(&mut pty, Duration::from_secs(15), |frame| {
        frame.rfind("Esc = close").is_some_and(|_| {
            let band = roster_band(frame);
            !(band.contains("kid") && band.contains("running"))
        })
    });
    assert!(
        cancelled,
        "x did not take the kid off the roster as running: {:?}\n{after}",
        roster_band(&after)
    );
    pty.send(b"\x1b").unwrap();
    // The kid's gate opens only now; a kid that was really cancelled does
    // not go on to finish, so once the turn is over its thread says
    // cancelled, not completed.
    let _ = fs::write(&scene.kid_gate, "");
    let _ = fs::write(&scene.root_gate, "");
    wait_idle_after(&mut pty, &scene.boss, 6);
    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );
    pty.send(b"\t").unwrap();
    let (said, picker) = frame_until(&mut pty, Duration::from_secs(6), |frame| {
        row_after(frame, "kid").is_some_and(|row| row.contains("cancelled"))
    });
    assert!(
        said,
        "the cancelled kid's thread does not say cancelled:\n{picker}"
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"q").unwrap();
    let transcript = leave(&mut pty, &scene);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

// ---------------------------------------------------------------------------
// The thread picker and a child's transcript.
// ---------------------------------------------------------------------------

/// On the first turn that spawns a child (a sessionless root, so the
/// child-session root is made by that spawn), Tab in the pager lists the
/// running kid with its status next to the root thread; Enter on it shows
/// the kid's transcript, and when the kid goes on and flushes its session
/// the open view shows the new reply with no key pressed.
#[test]
#[serial]
fn on_the_first_spawning_turn_the_picker_lists_the_running_child_and_rereads_it() {
    let scene = scene("thr-picker", kid_mark_then_held, "");
    let mut pty = spawn_tui(&scene.dir, &scene.log_env());
    engage_and_spawn(&mut pty, &scene);

    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );
    pty.send(b"\t").unwrap();
    let picker = repainted_frame_showing(
        &mut pty,
        "Threads  Enter = open  Esc = back",
        "Tab did not open the picker",
    );
    let kid_row = row_after(picker_band(&picker), "kid").unwrap_or_else(|| {
        panic!(
            "the picker does not list the running kid (child files on disk: {:?}):\n{picker}",
            children_files(&scene.dir.join("cache"))
        )
    });
    assert!(
        kid_row.contains("running"),
        "the kid's picker row does not say running: {kid_row:?}\n{picker}"
    );
    pty.send(b"\x1b[B").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    // Rounds inside the kid's tool loop show up only at its next
    // checkpoint, so the open view may still be empty here.
    let child = repainted_frame_showing(
        &mut pty,
        "(read-only)",
        "Enter on the kid did not open its read-only view",
    );
    assert!(
        !squeezed(&child).contains("kiddonez4p"),
        "the kid's final reply is on screen before the kid sent it:\n{child}"
    );

    fs::write(&scene.kid_gate, "").unwrap();
    let (reread, after) = frame_until(&mut pty, Duration::from_secs(15), |frame| {
        squeezed(frame).contains("kiddonez4p") && squeezed(frame).contains("kidtaskq7k")
    });
    assert!(
        reread,
        "the open child view did not pick up the kid's flushed reply:\n{after}"
    );

    pty.send(b"\t").unwrap();
    let (done, picker) = frame_until(&mut pty, Duration::from_secs(6), |frame| {
        row_after(frame, "kid").is_some_and(|row| row.contains("completed"))
    });
    assert!(
        done,
        "the picker does not show the finished kid as completed:\n{picker}"
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"q").unwrap();
    let transcript = leave(&mut pty, &scene);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// The boss's first turn: spawn the kid, then text answers to the
/// turn-end reminders until the turn ends with the UI idle again.
fn first_turn_spawning(prompt: &str) -> Vec<Reply> {
    let mut replies = vec![Reply::ToolCall {
        name: "agent__spawn".to_string(),
        arguments: format!(r#"{{"agent":"kid","prompt":"{prompt}"}}"#),
    }];
    replies.extend((0..4).map(|_| Reply::Text("boss turn over b0t".to_string())));
    replies
}

/// A scene whose boss and kid scripts are given whole.
fn scene_with(
    name: &str,
    boss: Vec<Reply>,
    kid: Vec<Reply>,
) -> (PathBuf, FakeLlm, FakeLlm, PathBuf) {
    let dir = fresh_config_dir(name);
    let boss = FakeLlm::start(boss);
    let kid = FakeLlm::start(kid);
    write_two_model_config(&dir, &boss, &kid, "");
    write_agents(&dir);
    write_tool(
        &dir,
        "root_mark.sh",
        &mark_tool(&dir.join("root.started"), "root marked r1m"),
    );
    write_tool(
        &dir,
        "kid_mark.sh",
        &mark_tool(&dir.join("kid.started"), "kid marked k1m"),
    );
    write_tool(
        &dir,
        "kid_gate.sh",
        &mark_tool(&dir.join("kid.gated"), "kid gate k2g"),
    );
    let log = std::env::temp_dir().join(format!("coyote-probe-threads-{name}.log"));
    let _ = fs::remove_file(&log);
    (dir, boss, kid, log)
}

/// Waits until the boss's turn is over and the composer is idle.
fn wait_idle_after(pty: &mut Pty, boss: &FakeLlm, served: usize) {
    let deadline = Instant::now() + PTY_DEADLINE;
    while boss.served() < served && Instant::now() < deadline {
        pty.read_for(Duration::from_millis(200));
    }
    pty.read_for(Duration::from_millis(2500));
}

fn leave_idle(pty: &mut Pty) -> String {
    let _ = pty.send(b"\x04");
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert!(!transcript.contains("panicked"), "{transcript}");
    assert_tty_restored(&transcript);
    transcript
}

/// After a turn that spawned the kid, back at the idle composer, the
/// picker lists the finished kid; a child session file that does not
/// parse, and then a sidecar that does not parse, degrade without a
/// panic and the UI keeps taking keys.
#[test]
#[serial]
fn a_malformed_child_session_file_degrades_without_a_panic() {
    let (dir, boss, _kid, log) = scene_with(
        "thr-malformed",
        first_turn_spawning("kid task m1f"),
        vec![Reply::Text("kid done m9f".to_string())],
    );
    let _guard = TempDirGuard(dir.clone());
    let mut pty = spawn_tui(&dir, &[("COYOTE_LOG_PATH", log.to_str().unwrap())]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".agent boss\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    pty.send(b"go boss\r").unwrap();
    wait_idle_after(&mut pty, &boss, 5);

    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );
    pty.send(b"\t").unwrap();
    let picker = repainted_frame_showing(
        &mut pty,
        "Threads  Enter = open  Esc = back",
        "Tab did not open the picker",
    );
    assert!(
        row_after(&picker, "kid").is_some(),
        "the idle picker does not list the kid the turn spawned (files: {:?}):\n{picker}",
        children_files(&dir.join("cache"))
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"q").unwrap();
    pty.read_for(Duration::from_millis(300));

    let files = children_files(&dir.join("cache"));
    let transcript_file = files
        .iter()
        .find(|f| !f.to_string_lossy().ends_with(".meta.yaml"))
        .unwrap_or_else(|| panic!("no child session file: {files:?}"));
    fs::write(transcript_file, ":\n  - [unterminated {{ not yaml\n").unwrap();
    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );
    pty.send(b"\t").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Threads  Enter = open  Esc = back",
        "Tab did not open the picker over a malformed child",
    );
    pty.send(b"\x1b[B").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    let opened = repainted_frame_showing(
        &mut pty,
        "Cannot read this transcript",
        "Enter on the malformed child did not show the error line",
    );
    assert!(
        !squeezed(&opened).contains("kiddonem9f"),
        "the malformed child file still showed its old transcript:\n{opened}"
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"q").unwrap();
    pty.read_for(Duration::from_millis(300));

    for file in &files {
        fs::write(file, "\u{0}\u{1}: [ {{\n").unwrap();
    }
    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );
    pty.send(b"\t").unwrap();
    pty.read_for(Duration::from_millis(600));
    pty.send(b"\x1b[B").unwrap();
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    for _ in 0..2 {
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(400));
    }
    pty.send(b"q").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"alive after malformed u3x").unwrap();
    wait_for_squeezed(
        &mut pty,
        "alive after malformed u3x",
        &format!("keys did not reach the composer; opened was:\n{opened}"),
    );
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(300));
    let transcript = leave_idle(&mut pty);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// A second turn that spawns a kid while an earlier turn already did:
/// the picker lists the new kid as running mid-turn, and its open view
/// picks up its final reply when it flushes.
#[test]
#[serial]
fn on_a_later_turn_the_picker_lists_a_running_child_and_rereads_it() {
    let gate_dir = fresh_config_dir("thr-second-gate");
    let _gate_guard = TempDirGuard(gate_dir.clone());
    let kid_gate = gate_dir.join("kid.gate");
    let root_gate = gate_dir.join("root.gate");
    let mut boss_replies = first_turn_spawning("kid task one o1n");
    boss_replies.push(Reply::ToolCall {
        name: "agent__spawn".to_string(),
        arguments: r#"{"agent":"kid","prompt":"kid task two p2t"}"#.to_string(),
    });
    boss_replies.push(Reply::ToolCall {
        name: "root_mark".to_string(),
        arguments: "{}".to_string(),
    });
    boss_replies.extend((0..4).map(|_| held(&root_gate, "root waits r2g")));
    let (dir, boss, _kid, log) = scene_with(
        "thr-second",
        boss_replies,
        vec![
            Reply::Text("kid one done".to_string()),
            Reply::ToolCall {
                name: "kid_mark".to_string(),
                arguments: "{}".to_string(),
            },
            Reply::After {
                gate: kid_gate.clone(),
                reply: Box::new(Reply::Text("kid two done z5q".to_string())),
            },
        ],
    );
    let _guard = TempDirGuard(dir.clone());
    let mut pty = spawn_tui(&dir, &[("COYOTE_LOG_PATH", log.to_str().unwrap())]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".agent boss\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    pty.send(b"go boss\r").unwrap();
    wait_idle_after(&mut pty, &boss, 5);
    pty.send(b"again boss\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("kid.started"),
        "the second kid's mark tool",
    );
    wait_for_file(&mut pty, &dir.join("root.started"), "the root's mark tool");
    pty.read_for(Duration::from_millis(800));

    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );
    pty.send(b"\t").unwrap();
    let picker = repainted_frame_showing(
        &mut pty,
        "Threads  Enter = open  Esc = back",
        "Tab did not open the picker",
    );
    assert!(
        squeezed(picker_band(&picker)).contains("running"),
        "the picker does not list the second kid as running on a later turn:\n{picker}"
    );
    // Open the second kid's thread: try each row under the root thread.
    let mut child = None;
    for downs in 1..=3 {
        for _ in 0..downs {
            pty.send(b"\x1b[B").unwrap();
            pty.read_for(Duration::from_millis(200));
        }
        pty.send(b"\r").unwrap();
        let view = flipped_frame(&mut pty);
        // The running kid's view: read-only, and not the first kid's
        // transcript (its prompt is `o1n`). Rounds inside its tool loop
        // show up only at its next checkpoint, so it may still be empty.
        if squeezed(&view).contains("(read-only)") && !squeezed(&view).contains("o1n") {
            child = Some(view);
            break;
        }
        pty.send(b"\t").unwrap();
        pty.read_for(Duration::from_millis(600));
    }
    let child = child.unwrap_or_else(|| panic!("no picker row opened the second kid:\n{picker}"));
    assert!(
        !squeezed(&child).contains("kidtwodonez5q"),
        "the second kid's final reply is on screen before it sent it:\n{child}"
    );
    fs::write(&kid_gate, "").unwrap();
    let (reread, after) = frame_until(&mut pty, Duration::from_secs(15), |frame| {
        squeezed(frame).contains("kidtwodonez5q") && squeezed(frame).contains("kidtasktwop2t")
    });
    let _ = fs::write(&root_gate, "");
    assert!(
        reread,
        "the open child view did not pick up the kid's flushed reply:\n{after}"
    );
    pty.send(b"q").unwrap();
    pty.read_for(Duration::from_millis(3000));
    let _ = pty.send(b"\x03");
    pty.read_for(Duration::from_millis(800));
    let transcript = leave_idle(&mut pty);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

// ---------------------------------------------------------------------------
// A child's question on the roster.
// ---------------------------------------------------------------------------

/// A kid that asks the user a yes/no question: the question shows on the
/// roster as an `ask` row, Enter answers it, and the answer reaches the
/// kid, which goes on to its final reply.
#[test]
#[serial]
fn a_childs_question_shows_on_the_roster_and_the_answer_reaches_it() {
    let scene = scene(
        "thr-escalate",
        |_| {
            vec![
                Reply::ToolCall {
                    name: "user__confirm".to_string(),
                    arguments: r#"{"question":"may the kid proceed e5q?"}"#.to_string(),
                },
                Reply::Text("kid heard the answer a8n".to_string()),
            ]
        },
        "",
    );
    let mut pty = spawn_tui(&scene.dir, &scene.log_env());
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".agent boss\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    pty.send(b"go boss\r").unwrap();
    wait_for_file(&mut pty, &scene.root_started, "the root's mark tool");
    let deadline = Instant::now() + PTY_DEADLINE;
    while scene.kid.served() < 1 && Instant::now() < deadline {
        pty.read_for(Duration::from_millis(200));
    }
    pty.read_for(Duration::from_millis(1500));

    pty.send(ALT_T).unwrap();
    let (asked, roster) = frame_until(&mut pty, Duration::from_secs(10), |frame| {
        frame.rfind("Esc = close").is_some() && squeezed(roster_band(frame)).contains("e5q")
    });
    assert!(
        asked,
        "the kid's question is not on the roster: {:?}\n{roster}",
        roster.rfind("Esc = close").map(|_| roster_band(&roster))
    );
    // Move to the ask row (agent row first) and answer it.
    let mut answered = false;
    let mut last = String::new();
    for _ in 0..3 {
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        last = flipped_frame(&mut pty);
        if squeezed(&last).contains("e5q") && !last.contains("Esc = close") {
            // A modal for the question: confirm with y then Enter.
            pty.read_for(HOTKEY_GAP);
            pty.send(b"y").unwrap();
            pty.read_for(HOTKEY_GAP);
            pty.send(b"\r").unwrap();
            answered = true;
            break;
        }
        pty.send(b"j").unwrap();
        pty.read_for(Duration::from_millis(300));
    }
    assert!(
        answered,
        "Enter did not open the question to answer:\n{last}"
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while scene.kid.served() < 2 && Instant::now() < deadline {
        pty.read_for(Duration::from_millis(200));
    }
    assert!(
        scene.kid.served() >= 2,
        "the kid never got an answer back to ask its model again"
    );
    let bodies = scene.kid.bodies();
    let last_body = bodies.last().unwrap();
    let request: serde_json::Value = serde_json::from_str(last_body).unwrap();
    let tool_content = request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().rfind(|m| m["role"] == "tool"))
        .and_then(|message| message["content"].as_str())
        .unwrap_or_else(|| panic!("the kid's model got no tool result: {last_body}"));
    match serde_json::from_str::<serde_json::Value>(tool_content) {
        Ok(answer) => assert_eq!(
            answer,
            serde_json::json!({"answer": "yes"}),
            "the answer the kid got back is not a yes: {last_body}"
        ),
        Err(_) => assert!(
            tool_content.contains(r#"{"answer":"yes"}"#),
            "the answer the kid got back is not a yes: {last_body}"
        ),
    }
    pty.send(b"\x1b").unwrap();
    let transcript = leave(&mut pty, &scene);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

// ---------------------------------------------------------------------------
// Exit paths with an overlay open.
// ---------------------------------------------------------------------------

/// SIGTERM while the pager is open still leaves through the same path:
/// exit 0, the terminal restored and the committed transcript printed
/// once on the normal screen, prompt before reply.
#[test]
#[serial]
fn sigterm_with_the_pager_open_prints_the_transcript() {
    leaving_prints_the_transcript(
        "thr-sigterm-pager",
        |pty, pid| {
            pty.send(b"\x14").unwrap();
            repainted_frame_showing(pty, "Thread: root thread", "Ctrl+T did not open the pager");
            // SAFETY: kill takes a pid and a signal number and touches no memory.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        },
        "pager exit prompt v3k",
        "pager exit reply w6d",
    );
}

/// Shrinking the terminal to a few cells (and one cell) with the pager
/// open, and pressing pager keys at each size, never panics; the pager is
/// still up at a normal size again and Ctrl+D leaves cleanly.
#[test]
#[serial]
fn shrinking_the_terminal_with_the_pager_open_keeps_the_ui_up() {
    let dir = fresh_config_dir("thr-tiny-pager");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(
        "a reply long enough to wrap at a tiny width t0y ".repeat(8),
    )]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"tiny prompt\r").unwrap();
    wait_for_squeezed(&mut pty, "t0y", "the reply");
    pty.read_for(Duration::from_millis(800));
    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );
    for (rows, cols) in [(3, 10), (1, 1), (5, 20), (2, 80), (40, 4)] {
        pty.resize(rows, cols).unwrap();
        pty.read_for(Duration::from_millis(300));
        for keys in [&b"G"[..], b"k", b"/t0y", b"\r", b"\t", b"\x1b"] {
            pty.send(keys).unwrap();
            pty.read_for(Duration::from_millis(100));
        }
    }
    let frame =
        repainted_frame_showing(&mut pty, "Thread: root thread", "the pager did not survive");
    pty.send(b"q").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert!(!transcript.contains("panicked"), "{transcript}\n{frame}");
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------------
// The pager's scroll keys over a long output, CRLF line endings, and an
// output over the per-call cap.
// ---------------------------------------------------------------------------

/// A tool that writes a hundred and twenty numbered lines to stderr.
const LONG_OUTPUT_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Print a hundred and twenty numbered lines.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   for i in $(seq -w 1 120); do echo \"pq-$i\" >&2; done\n\
    \x20   echo \"fixture printed many s3k\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// Runs `tool` (saved as `file`, called as `name`) in one turn that ends
/// with `reply`, and hands back the pty once the reply is on screen.
fn one_tool_turn(dir: &Path, name: &str, file: &str, tool: &str, reply: &str) -> (Pty, FakeLlm) {
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: name.to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text(reply.to_string()),
    ]);
    write_fake_llm_config(dir, &fake);
    write_tool(dir, file, tool);
    let mut pty = spawn_tui(dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"run the tool\r").unwrap();
    wait_for_squeezed(&mut pty, reply, "the reply after the tool");
    pty.read_for(Duration::from_millis(500));
    (pty, fake)
}

fn quit_from_composer(mut pty: Pty) {
    // One key on an empty composer: a second one typed after the UI left
    // would be echoed into the report the wrapper prints.
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert!(!transcript.contains("panicked"), "{transcript}");
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// The pager over a long output: `G` goes to the end, PgUp leaves it and
/// PgDn comes back, `g` goes to the top, `j` scrolls down a line at a time
/// and `k` back up, and `q` closes it with the composer taking keys.
#[test]
#[serial]
fn the_pager_scroll_keys_move_through_a_long_output() {
    let dir = fresh_config_dir("pager-scroll");
    let _guard = TempDirGuard(dir.clone());
    let (mut pty, _fake) = one_tool_turn(
        &dir,
        "fixture_long",
        "fixture_long.sh",
        LONG_OUTPUT_TOOL,
        "fixture done s3k",
    );

    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );

    pty.send(b"G").unwrap();
    let end = repainted_frame_showing(&mut pty, "pq-120", "G did not show the last output line");
    assert!(
        !end.contains("pq-001"),
        "G left the first line on screen:\n{end}"
    );

    pty.send(b"\x1b[5~").unwrap();
    pty.read_for(Duration::from_millis(300));
    let up = repainted_frame_showing(&mut pty, "j/k scroll", "the pager went away on PgUp");
    assert!(
        !up.contains("pq-120"),
        "PgUp did not move off the end:\n{up}"
    );
    assert!(
        up.contains("pq-0") || up.contains("pq-1"),
        "PgUp left no output on screen:\n{up}"
    );

    pty.send(b"\x1b[6~").unwrap();
    pty.read_for(Duration::from_millis(300));
    repainted_frame_showing(&mut pty, "pq-120", "PgDn did not come back to the end");

    pty.send(b"g").unwrap();
    let top = repainted_frame_showing(&mut pty, "pq-001", "g did not go to the top");
    assert!(
        !top.contains("pq-120"),
        "g left the last line on screen:\n{top}"
    );

    for _ in 0..12 {
        pty.send(b"j").unwrap();
        pty.read_for(Duration::from_millis(60));
    }
    let down = repainted_frame_showing(&mut pty, "pq-030", "j scrolled away the whole output");
    assert!(
        !down.contains("pq-001"),
        "twelve j presses did not scroll the first output line off:\n{down}"
    );
    for _ in 0..12 {
        pty.send(b"k").unwrap();
        pty.read_for(Duration::from_millis(60));
    }
    repainted_frame_showing(
        &mut pty,
        "pq-001",
        "k did not scroll back to the first line",
    );

    pty.send(b"q").unwrap();
    pty.read_for(Duration::from_millis(400));
    let back = repainted_frame_showing(&mut pty, "no session", "q did not close the pager");
    assert!(!squeezed(&back).contains("Thread:rootthread"), "{back}");
    pty.send(b"after scroll m4t").unwrap();
    wait_for_squeezed(
        &mut pty,
        "after scroll m4t",
        "keys did not reach the composer",
    );
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(300));
    quit_from_composer(pty);
}

/// A tool whose output lines end in CRLF, some with the `\r` and the
/// `\n` written apart so they reach the UI in two reads, and one progress
/// line that rewrites itself with a bare `\r`.
const CRLF_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Print lines ending in CRLF.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   for i in $(seq -w 1 30); do printf 'cr-%s tail-w7\\r\\n' \"$i\" >&2; done\n\
    \x20   for i in 1 2 3; do printf 'split-%s end-w7\\r' \"$i\" >&2; sleep 0.3; printf '\\n' >&2; done\n\
    \x20   printf 'progress 50%%\\r' >&2; sleep 0.3; printf 'progress 99%% p2q\\n' >&2\n\
    \x20   echo \"fixture printed crlf c6r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// CRLF line endings in a tool's whole output show in the pager as plain
/// lines: every line whole and no carriage return reaching the terminal
/// from the pager's text.
#[test]
#[serial]
fn crlf_line_endings_show_in_the_pager_as_whole_lines() {
    let dir = fresh_config_dir("pager-crlf");
    let _guard = TempDirGuard(dir.clone());
    let (mut pty, _fake) = one_tool_turn(
        &dir,
        "fixture_crlf",
        "fixture_crlf.sh",
        CRLF_TOOL,
        "fixture done c6r",
    );
    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );
    pty.send(b"G").unwrap();
    pty.read_for(Duration::from_millis(800));
    let wanted: Vec<String> = (28..=30)
        .map(|i| format!("cr-{i:02} tail-w7"))
        .chain((1..=3).map(|i| format!("split-{i} end-w7")))
        .chain(["progress 99% p2q".to_string()])
        .collect();
    // Each pass is a real resize (the height flips), so the UI repaints
    // every cell and the pass holds the whole page.
    let mut frame = String::new();
    for rows in [31, 33, 31, 33, 31] {
        pty.read_for(Duration::from_millis(300));
        let from = pty.raw().len();
        pty.resize(rows, 100).unwrap();
        pty.read_for(Duration::from_millis(900));
        let raw = pty.raw()[from..].to_vec();
        assert!(
            !raw.contains(&b'\r'),
            "the pager's repaint carries a carriage return: {:?}",
            String::from_utf8_lossy(&raw)
        );
        frame = strip_ansi(&String::from_utf8_lossy(&raw));
        if wanted.iter().all(|line| frame.contains(line.as_str())) {
            break;
        }
    }
    for line in &wanted {
        assert!(
            frame.contains(line.as_str()),
            "the pager lost {line:?}:\n{frame}"
        );
    }
    assert!(!frame.contains("^M"), "the pager shows ^M:\n{frame}");
    pty.send(b"q").unwrap();
    pty.read_for(Duration::from_millis(400));
    quit_from_composer(pty);
}

// ---------------------------------------------------------------------------
// A withdrawn question, and a grandchild's question.
// ---------------------------------------------------------------------------

/// Opens the roster, waits for a question row naming `tag`, and opens it
/// in its modal. Returns the modal frame.
fn open_question_row(pty: &mut Pty, tag: &str) -> String {
    pty.send(ALT_T).unwrap();
    let (asked, roster) = frame_until(pty, Duration::from_secs(15), |frame| {
        frame.rfind("Esc = close").is_some() && squeezed(roster_band(frame)).contains(tag)
    });
    assert!(asked, "the question {tag} is not on the roster:\n{roster}");
    let mut last = String::new();
    for _ in 0..4 {
        pty.send(b"j").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        last = flipped_frame(pty);
        if squeezed(&last).contains(tag) && !last.contains("Esc = close") {
            return last;
        }
    }
    panic!("Enter did not open the question {tag}:\n{last}");
}

/// The kid asks with a short escalation timeout; the user opens the
/// question, the kid stops waiting (but keeps running), and only then the
/// user answers. The answer is not delivered and the UI says so.
#[test]
#[serial]
fn answering_a_question_whose_asker_timed_out_says_not_delivered() {
    let scene = scene(
        "thr-withdrawn",
        |gate| {
            vec![
                Reply::ToolCall {
                    name: "user__confirm".to_string(),
                    arguments: r#"{"question":"may the kid go w4q?"}"#.to_string(),
                },
                // The kid stays alive after its wait runs out.
                Reply::After {
                    gate: gate.to_path_buf(),
                    reply: Box::new(Reply::Text("kid moved on w4a".to_string())),
                },
            ]
        },
        "",
    );
    fs::write(
        scene.dir.join("agents").join("kid").join("config.yaml"),
        "name: kid\n\
         description: Does the kid task.\n\
         model: kidfake:fake-model\n\
         instructions: You are the kid.\n\
         escalation_timeout: 10\n\
         global_tools:\n\
         \x20 - kid_mark.sh\n",
    )
    .unwrap();
    let mut pty = spawn_tui(&scene.dir, &scene.log_env());
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".agent boss\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    pty.send(b"go boss\r").unwrap();
    wait_for_file(&mut pty, &scene.root_started, "the root's mark tool");
    let deadline = Instant::now() + PTY_DEADLINE;
    while scene.kid.served() < 1 && Instant::now() < deadline {
        pty.read_for(Duration::from_millis(200));
    }
    let modal = open_question_row(&mut pty, "w4q");
    // Outlast the kid's timeout with the modal up.
    pty.read_for(Duration::from_secs(12));
    let (_, mid) = frame_until(&mut pty, Duration::from_secs(1), |_| false);
    pty.send(b"y").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\r").unwrap();
    let (said, after) = frame_until(&mut pty, Duration::from_secs(8), |frame| {
        squeezed(frame).contains("notdelivered")
    });
    assert!(
        said,
        "answering a withdrawn question did not say the reply was not delivered\n\
         modal:\n{modal}\nafter timeout:\n{mid}\nafter answer:\n{after}"
    );
    let roster_open = |frame: &str| frame.rfind("Esc = close").is_some();
    let (mut shown, mut roster) = frame_until(&mut pty, Duration::from_secs(2), roster_open);
    if !shown {
        pty.send(ALT_T).unwrap();
        (shown, roster) = frame_until(&mut pty, Duration::from_secs(10), roster_open);
    }
    assert!(shown, "the roster did not open after the answer:\n{roster}");
    assert!(
        !squeezed(roster_band(&roster)).contains("w4q"),
        "the withdrawn question is still on the roster:\n{roster}"
    );
    let _ = pty.send(b"\x1b");
    let transcript = leave(&mut pty, &scene);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// Three clients: `fake` (boss), `kidfake` (kid), `gkfake` (grandkid).
fn write_three_model_config(dir: &Path, boss: &FakeLlm, kid: &FakeLlm, gk: &FakeLlm) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    let client = |name: &str, base: String| {
        format!(
            "\x20 - type: openai-compatible\n\
             \x20   name: {name}\n\
             \x20   api_base: '{base}'\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: fake-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n"
        )
    };
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
            client("fake", boss.api_base()),
            client("kidfake", kid.api_base()),
            client("gkfake", gk.api_base()),
        ),
    )
    .unwrap();
    fs::write(
        dir.join("agents").join("kid").join("config.yaml"),
        "name: kid\n\
         description: Does the kid task.\n\
         model: kidfake:fake-model\n\
         instructions: You are the kid.\n\
         can_spawn_agents: true\n\
         global_tools:\n\
         \x20 - kid_mark.sh\n",
    )
    .unwrap();
    let gk = dir.join("agents").join("gkid");
    fs::create_dir_all(&gk).unwrap();
    fs::write(
        gk.join("config.yaml"),
        "name: gkid\n\
         description: Does the grandkid task.\n\
         model: gkfake:fake-model\n\
         instructions: You are the grandkid.\n",
    )
    .unwrap();
}

fn grandkid_scene(name: &str, gk_replies: Vec<Reply>) -> (Scene, FakeLlm) {
    let scene = scene(
        name,
        |gate| {
            vec![
                Reply::ToolCall {
                    name: "agent__spawn".to_string(),
                    arguments: r#"{"agent":"gkid","prompt":"grandkid task g0k"}"#.to_string(),
                },
                Reply::After {
                    gate: gate.to_path_buf(),
                    reply: Box::new(Reply::Text("kid done z4p".to_string())),
                },
            ]
        },
        "",
    );
    let gk = FakeLlm::start(gk_replies);
    write_three_model_config(&scene.dir, &scene.boss, &scene.kid, &gk);
    (scene, gk)
}

/// Starts the boss and waits until the grandkid has asked its model at
/// least once.
fn start_boss_until_the_grandkid_asks(pty: &mut Pty, scene: &Scene, gk: &FakeLlm) {
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".agent boss\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    pty.send(b"go boss\r").unwrap();
    wait_for_file(pty, &scene.root_started, "the root's mark tool");
    let deadline = Instant::now() + PTY_DEADLINE;
    while gk.served() < 1 && Instant::now() < deadline {
        pty.read_for(Duration::from_millis(200));
    }
    assert!(gk.served() >= 1, "the grandkid never asked its model");
    pty.read_for(Duration::from_millis(1500));
}

/// Cancels the kid from an open roster: the kid's row takes the focus on
/// open, so `x` acts on it.
fn cancel_the_kid_from_the_roster(pty: &mut Pty) -> String {
    pty.send(b"x").unwrap();
    let (cancelled, last) = frame_until(pty, Duration::from_secs(15), |frame| {
        frame.rfind("Esc = close").is_some_and(|_| {
            let band = roster_band(frame);
            !band
                .lines()
                .any(|l| l.contains("kid") && !l.contains("gkid") && l.contains("running"))
        })
    });
    assert!(
        cancelled,
        "could not cancel the kid from the roster:\n{last}"
    );
    last
}

/// A grandchild asks while its parent runs; cancelling the parent from
/// the roster takes the grandchild's question off the roster too, since
/// the subtree that would receive the answer is gone.
#[test]
#[serial]
fn cancelling_the_parent_drops_the_grandchilds_question() {
    let (scene, gk) = grandkid_scene(
        "thr-gk-ask",
        vec![
            Reply::ToolCall {
                name: "user__confirm".to_string(),
                arguments: r#"{"question":"may the grandkid go g7q?"}"#.to_string(),
            },
            Reply::Text("grandkid heard g7a".to_string()),
        ],
    );
    let mut pty = spawn_tui(&scene.dir, &scene.log_env());
    start_boss_until_the_grandkid_asks(&mut pty, &scene, &gk);
    pty.send(ALT_T).unwrap();
    let (asked, roster) = frame_until(&mut pty, Duration::from_secs(15), |frame| {
        frame.rfind("Esc = close").is_some() && squeezed(roster_band(frame)).contains("g7q")
    });
    assert!(
        asked,
        "the grandkid's question is not on the roster:\n{roster}"
    );
    cancel_the_kid_from_the_roster(&mut pty);
    let (cleared, after) = frame_until(&mut pty, Duration::from_secs(15), |frame| {
        frame.rfind("Esc = close").is_some() && !squeezed(roster_band(frame)).contains("g7q")
    });
    assert!(
        cleared,
        "the grandkid's question outlived its cancelled parent: {:?}\n{after}",
        roster_band(&after)
    );
    pty.send(b"\x1b").unwrap();
    let transcript = leave(&mut pty, &scene);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// The grandchild's question would come only after its parent was
/// cancelled; the question never lands on the roster as one to answer.
#[test]
#[serial]
fn a_grandchild_question_after_its_parent_is_cancelled_is_not_listed() {
    let gk_gate_holder = std::env::temp_dir().join(format!("thr-gk-gate-{}", std::process::id()));
    let _ = fs::remove_file(&gk_gate_holder);
    let (scene, gk) = grandkid_scene(
        "thr-gk-late",
        vec![
            Reply::After {
                gate: gk_gate_holder.clone(),
                reply: Box::new(Reply::ToolCall {
                    name: "user__confirm".to_string(),
                    arguments: r#"{"question":"late grandkid asks l8q?"}"#.to_string(),
                }),
            },
            Reply::Text("grandkid heard l8a".to_string()),
        ],
    );
    let mut pty = spawn_tui(&scene.dir, &scene.log_env());
    start_boss_until_the_grandkid_asks(&mut pty, &scene, &gk);
    pty.send(ALT_T).unwrap();
    repainted_frame_showing(&mut pty, "Esc = close", "Alt+T did not open the roster");
    cancel_the_kid_from_the_roster(&mut pty);
    fs::write(&gk_gate_holder, "").unwrap();
    let (listed, frame) = frame_until(&mut pty, Duration::from_secs(10), |frame| {
        frame.rfind("Esc = close").is_some() && squeezed(roster_band(frame)).contains("l8q")
    });
    let _ = fs::remove_file(&gk_gate_holder);
    assert!(
        !listed,
        "a question from the cancelled kid's grandchild landed on the roster: {:?}",
        roster_band(&frame)
    );
    pty.send(b"\x1b").unwrap();
    let transcript = leave(&mut pty, &scene);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// A tool that prints well over four MiB to stderr, then a last marker.
const HUGE_OUTPUT_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Print a lot.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   echo \"huge-first-line-b1g\" >&2\n\
    \x20   yes 'huge-filler-0123456789-abcdefghijklmnopqrstuvwxyz-0123456789' | head -n 90000 >&2\n\
    \x20   echo \"huge-last-line-b1g\" >&2\n\
    \x20   echo \"fixture printed huge h9u\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// An output over the per-call cap keeps the UI responsive: the turn
/// finishes, the pager opens over it, its end is reachable, and at the top
/// a note says how much of the start was not kept (the cap keeps the
/// tail, so the first line is gone), and the find still works over the
/// kept text.
#[test]
#[serial]
fn an_output_over_the_cap_pages_with_a_visible_note() {
    let dir = fresh_config_dir("pager-huge");
    let _guard = TempDirGuard(dir.clone());
    let (mut pty, _fake) = one_tool_turn(
        &dir,
        "fixture_huge",
        "fixture_huge.sh",
        HUGE_OUTPUT_TOOL,
        "fixture done h9u",
    );
    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );
    pty.send(b"G").unwrap();
    pty.read_for(Duration::from_millis(800));
    repainted_frame_showing(
        &mut pty,
        "huge-last-line-b1g",
        "G did not reach the output's end",
    );
    pty.send(b"g").unwrap();
    pty.read_for(Duration::from_millis(800));
    let top = repainted_frame_showing(&mut pty, "fixture_huge({})", "g did not reach the top");
    assert!(
        top.contains("bytes not kept"),
        "no visible note at the top that the output was cut:\n{top}"
    );
    assert!(
        !top.contains("huge-first-line-b1g"),
        "the output was not cut, the first line is still kept:\n{top}"
    );
    pty.send(b"/huge-last-line").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let found = repainted_frame_showing(
        &mut pty,
        "huge-last-line-b1g",
        "find did not reach the last line of a capped output",
    );
    assert!(!found.contains("No match"), "{found}");
    pty.send(b"q").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"alive after huge").unwrap();
    wait_for_squeezed(
        &mut pty,
        "alive after huge",
        "keys did not reach the composer",
    );
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(300));
    quit_from_composer(pty);
}

// ---------------------------------------------------------------------------
// A question whose asker is cancelled, and runs of CRs before an LF.
// ---------------------------------------------------------------------------

/// A kid asks the user a question and is then cancelled from the roster
/// before anyone answers: its question row goes with it, since nobody is
/// left to receive the answer.
#[test]
#[serial]
fn a_cancelled_askers_question_leaves_the_roster() {
    let scene = scene(
        "thr-dead-ask",
        |_| {
            vec![
                Reply::ToolCall {
                    name: "user__confirm".to_string(),
                    arguments: r#"{"question":"may the kid go d9q?"}"#.to_string(),
                },
                Reply::Text("kid heard d9a".to_string()),
            ]
        },
        "",
    );
    let mut pty = spawn_tui(&scene.dir, &scene.log_env());
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".agent boss\r").unwrap();
    pty.read_for(Duration::from_millis(2500));
    pty.send(b"go boss\r").unwrap();
    wait_for_file(&mut pty, &scene.root_started, "the root's mark tool");
    let deadline = Instant::now() + PTY_DEADLINE;
    while scene.kid.served() < 1 && Instant::now() < deadline {
        pty.read_for(Duration::from_millis(200));
    }
    pty.read_for(Duration::from_millis(1500));

    pty.send(ALT_T).unwrap();
    let (asked, roster) = frame_until(&mut pty, Duration::from_secs(10), |frame| {
        frame.rfind("Esc = close").is_some() && squeezed(roster_band(frame)).contains("d9q")
    });
    assert!(asked, "the kid's question is not on the roster:\n{roster}");
    // The kid's agent row is listed first and takes the focus on open.
    pty.send(b"x").unwrap();
    let (cancelled, last) = frame_until(&mut pty, Duration::from_secs(15), |frame| {
        frame.rfind("Esc = close").is_some_and(|_| {
            let band = roster_band(frame);
            !(band.contains("kid") && band.contains("running"))
        })
    });
    assert!(
        cancelled,
        "could not cancel the kid from the roster:\n{last}"
    );
    let kid_requests = || {
        scene
            .kid
            .bodies()
            .iter()
            .filter(|body| body.contains("kid task q7k"))
            .count()
    };
    assert_eq!(kid_requests(), 1, "the kid asked its model more than once");
    pty.read_for(Duration::from_secs(2));
    assert_eq!(
        kid_requests(),
        1,
        "the kid asked its model again after it was cancelled"
    );
    let (cleared, after) = frame_until(&mut pty, Duration::from_secs(10), |frame| {
        frame.rfind("Esc = close").is_some() && !squeezed(roster_band(frame)).contains("d9q")
    });
    assert!(
        cleared,
        "the cancelled kid's question is still on the roster: {:?}\n{after}",
        roster_band(&after)
    );
    pty.send(b"\x1b").unwrap();
    let transcript = leave(&mut pty, &scene);
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// A tool whose lines end in runs of CRs before the LF, one run spread
/// over three writes.
const CR_RUN_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Print lines ending in runs of CRs.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   printf 'rr-1 one-v5\\r\\r\\n' >&2\n\
    \x20   printf 'rr-2 two-v5\\r\\r\\r\\n' >&2\n\
    \x20   printf 'rr-3 three-v5\\r' >&2; sleep 0.3; printf '\\r' >&2; sleep 0.3; printf '\\r\\n' >&2\n\
    \x20   printf 'rr-4 four-v5\\n' >&2\n\
    \x20   echo \"fixture printed runs v5d\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// Runs of CRs before an LF show in the pager as plain whole lines, one
/// after the other with no blank line between them and no CR repainted.
#[test]
#[serial]
fn cr_runs_before_lf_show_as_consecutive_lines_in_the_pager() {
    let dir = fresh_config_dir("pager-crrun");
    let _guard = TempDirGuard(dir.clone());
    let (mut pty, _fake) = one_tool_turn(
        &dir,
        "fixture_crrun",
        "fixture_crrun.sh",
        CR_RUN_TOOL,
        "fixture done v5d",
    );
    pty.send(b"\x14").unwrap();
    repainted_frame_showing(
        &mut pty,
        "Thread: root thread",
        "Ctrl+T did not open the pager",
    );
    pty.send(b"G").unwrap();
    pty.read_for(Duration::from_millis(800));
    let wanted = [
        "rr-1 one-v5",
        "rr-2 two-v5",
        "rr-3 three-v5",
        "rr-4 four-v5",
    ];
    let mut frame = String::new();
    let mut raw = Vec::new();
    for rows in [31, 33, 31, 33, 31] {
        pty.read_for(Duration::from_millis(300));
        let from = pty.raw().len();
        pty.resize(rows, 100).unwrap();
        pty.read_for(Duration::from_millis(900));
        raw = pty.raw()[from..].to_vec();
        assert!(
            !raw.contains(&b'\r'),
            "the pager's repaint carries a carriage return: {:?}",
            String::from_utf8_lossy(&raw)
        );
        frame = strip_ansi(&String::from_utf8_lossy(&raw));
        if wanted.iter().all(|line| frame.contains(line)) {
            break;
        }
    }
    let rows: Vec<u16> = wanted
        .iter()
        .map(|w| {
            painted_at(&raw, w.as_bytes(), 0)
                .last()
                .map(|(row, _)| *row)
                .unwrap_or_else(|| panic!("the pager lost {w:?}:\n{frame}"))
        })
        .collect();
    for pair in rows.windows(2) {
        assert_eq!(
            pair[1],
            pair[0] + 1,
            "a run of CRs left a gap between lines (rows {rows:?}):\n{frame}"
        );
    }
    assert!(!frame.contains("^M"), "the pager shows ^M:\n{frame}");
    pty.send(b"q").unwrap();
    pty.read_for(Duration::from_millis(400));
    quit_from_composer(pty);
}
