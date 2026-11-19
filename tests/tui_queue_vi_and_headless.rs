//! Black-box pins for the bottom pane's queue across an agent switch, the
//! vi layer, the command highlight, Ctrl+R, backtick document commands and
//! the headless prompter's refusal: the built binary on a pseudo-terminal
//! and a scripted fake model where a turn has to be held open. Unix-only
//! and serial like its sibling pty suites.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::fake_llm::{Chunk, FakeLlm, Reply};
use common::pty::Pty;
use common::tui_harness::*;
use serial_test::serial;

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
             \x20 You are the plain agent kq7. Reply briefly.\n"
        ),
    )
    .unwrap();
}

/// The last `CSI r;c H` before `at`: its offset and the position it set.
fn cursor_position_before(haystack: &[u8], at: usize) -> Option<(usize, u16, u16)> {
    let mut end = at;
    while let Some(start) = rfind(&haystack[..end], b"\x1b[") {
        let params: Vec<u8> = haystack[start + 2..at]
            .iter()
            .copied()
            .take_while(|b| b.is_ascii_digit() || *b == b';')
            .collect();
        if haystack.get(start + 2 + params.len()) == Some(&b'H')
            && let Some((row, col)) = std::str::from_utf8(&params)
                .ok()
                .and_then(|p| p.split_once(';'))
            && let (Ok(row), Ok(col)) = (row.parse(), col.parse())
        {
            return Some((start, row, col));
        }
        end = start;
    }
    None
}

/// The SGR lists between the cursor move that started the run of cells
/// `at` belongs to and `at` itself (the style of the text at `at` after a
/// full repaint).
fn sgr_before(raw: &[u8], at: usize) -> Vec<Vec<u16>> {
    let (start, _, _) = cursor_position_before(raw, at)
        .unwrap_or_else(|| panic!("no cursor position precedes offset {at}"));
    sgr_params(&raw[start..at])
}

/// The SGR lists between `from` and `to`.
fn sgr_between(raw: &[u8], from: usize, to: usize) -> Vec<Vec<u16>> {
    sgr_params(&raw[from..to])
}

fn has_indexed_foreground(params: &[Vec<u16>], index: u16) -> bool {
    params
        .iter()
        .any(|p| p.windows(3).any(|w| w == [38, 5, index]))
}

/// Whether any SGR list sets a foreground colour at all: an indexed or
/// truecolor `38`, or one of the basic `30-37` / `90-97` codes. `39`
/// (default) and `0` (reset) are not colours.
fn sets_a_foreground(params: &[Vec<u16>]) -> bool {
    params.iter().any(|p| {
        p.iter()
            .any(|&code| code == 38 || (30..=37).contains(&code) || (90..=97).contains(&code))
    })
}

const GREEN: u16 = 2;
const RED: u16 = 1;

/// Files under `root` (recursively) whose contents contain `needle`.
fn files_containing(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if fs::read(&path)
                .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
            {
                hits.push(path);
            }
        }
    }
    hits
}

// ---------------------------------------------------------------------
// Queued lines survive a `.agent` switch.
// ---------------------------------------------------------------------

/// A `.agent` command and a text line queued while the model streams
/// (no tool round, so neither is handed over mid-turn): once the reply
/// ends the command runs as its own turn and the text still reaches the
/// model as the next prompt, on the switched agent's context. Regression
/// target: the carried-over lines went onto the old context's queue and
/// were silently dropped after the `.agent` turn.
#[test]
#[serial]
fn a_text_line_queued_behind_a_dot_agent_command_still_reaches_the_model() {
    let dir = fresh_config_dir("queue-across-agent");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("release");
    let fake = FakeLlm::start(vec![
        Reply::Streamed(vec![
            Chunk::Text("part one kq7 ".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("reply one kq7".to_string()),
        ]),
        Reply::Text("reply two kq7".to_string()),
        Reply::Text("reply three kq7".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_plain_agent(&dir, "plain-agent");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let turn_at = pty.raw().len();
    pty.send(b"first prompt kq7\r").unwrap();
    wait_for_squeezed_after(&mut pty, "part one kq7", turn_at, "the held stream");
    pty.read_for(Duration::from_millis(300));

    let queued_at = pty.raw().len();
    pty.send(b".agent plain-agent\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"hello-after-agent-kq7\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame_after_resize(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("pending:2"),
        "the command and the text queued while the model streamed did not show `pending: 2`:\n{frame}"
    );

    fs::write(&release, b"").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "reply one kq7",
        queued_at,
        "the end of the held reply",
    );
    // The queued text's own reply.
    wait_for_squeezed_after(
        &mut pty,
        "reply two kq7",
        queued_at,
        "the reply to the text queued behind `.agent`",
    );
    wait_until_no_pending(&mut pty);

    let bodies = fake.bodies();
    let carried = bodies
        .iter()
        .position(|b| b.contains("hello-after-agent-kq7"))
        .unwrap_or_else(|| {
            panic!(
                "the text queued behind `.agent` never reached the model; {} requests:\n{}",
                bodies.len(),
                bodies.join("\n---\n")
            )
        });
    assert!(
        bodies[carried].contains("plain agent kq7"),
        "the queued text was sent before the `.agent` switch (request {} carries no agent \
         instructions):\n{}",
        carried + 1,
        bodies[carried]
    );

    // The switch is in effect for what the user types next, too.
    let after_at = pty.raw().len();
    pty.send(b"after-switch-kq7\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "reply three kq7",
        after_at,
        "the reply after the switch",
    );
    let bodies = fake.bodies();
    let last = bodies.last().unwrap();
    assert!(
        last.contains("after-switch-kq7") && last.contains("plain agent kq7"),
        "the prompt typed after the switch did not go out on the agent's context:\n{last}"
    );
    pty.read_for(Duration::from_millis(500));
    quit_and_check(&mut pty);
}

// ---------------------------------------------------------------------
// A headless macro run cannot save a role by answering itself.
// ---------------------------------------------------------------------

/// `--headless --macro` with a temp role and a bare `.save role`: the
/// prompter has no human, so the name question is refused and NO role
/// file is written (before the fix the JSON refusal was taken as the
/// role's name). The positional `go` matters: with no prompt argument
/// and no stdin text `--headless` bails before the macro runs, so the
/// step's echo is asserted too.
#[test]
#[serial]
fn a_headless_macro_save_role_on_a_temp_role_writes_no_role_file() {
    let dir = fresh_config_dir("headless-save-role");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let macros = dir.join("macros");
    fs::create_dir_all(&macros).unwrap();
    fs::write(
        macros.join("temp-save.yaml"),
        "description: a temp role then a bare save\n\
         steps:\n\
         \x20 - .temp-role be terse kq7\n\
         \x20 - .save role\n",
    )
    .unwrap();
    let roles = dir.join("roles");
    fs::create_dir_all(&roles).unwrap();

    let mut cmd = ttyless_command(built_binary(), &dir);
    cmd.args(["--headless", "--macro", "temp-save", "go"])
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
            panic!("`--headless --macro temp-save` hung on the role-name question");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let written: Vec<PathBuf> = fs::read_dir(&roles)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert!(
        written.is_empty(),
        "a headless `.save role` on a temp role wrote {written:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    let everything = format!("{stdout}\n{stderr}");
    assert!(
        everything.contains(">> .save role"),
        "the macro never echoed its `.save role` step (exit {status}):\n{everything}"
    );
    assert!(
        !everything.contains("Saved role") && !everything.contains("saved role"),
        "the headless run claims a saved role (exit {status}):\n{everything}"
    );
    assert!(
        everything.contains(
            "The input prompt needs a human and none is attached to this run; run it without \
             --headless or --acp-server"
        ),
        "the headless run did not give the prompter's refusal (exit {status}):\n{everything}"
    );
}

// ---------------------------------------------------------------------
// The command highlight stops at a word boundary.
// ---------------------------------------------------------------------

/// Reads the style the field paints `typed` with after a repaint; closes
/// the palette band first so the field's copy is the one found. The
/// composer is painted below the transcript (whose welcome block spells
/// `.help`), so the LAST copy after the repaint is the field's.
fn command_style(pty: &mut Pty, typed: &str, cols: u16) -> (Vec<Vec<u16>>, String) {
    pty.send(typed.as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    let repainted = repaint(pty, 40, cols);
    let frame = since(pty, repainted);
    let at = rfind_after(pty.raw(), typed.as_bytes(), repainted)
        .unwrap_or_else(|| panic!("`{typed}` was not repainted:\n{frame}"));
    (sgr_before(pty.raw(), at), frame)
}

/// The last occurrence of `needle` at or after `from`.
fn rfind_after(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let from = from.min(haystack.len());
    rfind(&haystack[from..], needle).map(|at| from + at)
}

/// Clears the field (one Ctrl+C on a draft).
fn clear_draft(pty: &mut Pty) {
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(400));
}

/// `.helpx` is not a command even though `.help` is its prefix: red, not
/// green. `.help x` keeps `.help` green with the argument plain. Same
/// under the slash prefix.
#[test]
#[serial]
fn a_command_prefix_followed_by_more_letters_is_painted_red_not_green() {
    for (prefix_yaml, sigil) in [("", '.'), ("command_prefix: slash\n", '/')] {
        let dir = fresh_config_dir(&format!("word-boundary-{}", sigil as u8));
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, prefix_yaml);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        let known = format!("{sigil}help");
        let (style, frame) = command_style(&mut pty, &known, 101);
        assert!(
            has_indexed_foreground(&style, GREEN),
            "`{known}` was not painted green: {style:?}\n{frame}"
        );
        clear_draft(&mut pty);

        let extended = format!("{sigil}helpx");
        let (style, frame) = command_style(&mut pty, &extended, 100);
        assert!(
            has_indexed_foreground(&style, RED),
            "`{extended}` (a known command plus letters) was not painted red: {style:?}\n{frame}"
        );
        assert!(
            !has_indexed_foreground(&style, GREEN),
            "`{extended}` was painted green as if `{known}` matched by prefix: {style:?}\n{frame}"
        );
        clear_draft(&mut pty);

        let with_arg = format!("{sigil}help x");
        pty.send(with_arg.as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(300));
        let repainted = repaint(&mut pty, 40, 101);
        let frame = since(&pty, repainted);
        let name_at = rfind_after(pty.raw(), known.as_bytes(), repainted)
            .unwrap_or_else(|| panic!("`{with_arg}` was not repainted:\n{frame}"));
        let name_style = sgr_before(pty.raw(), name_at);
        assert!(
            has_indexed_foreground(&name_style, GREEN),
            "`{known}` before an argument was not painted green: {name_style:?}\n{frame}"
        );
        // Between the command word and its argument the colour is reset:
        // the argument is plain, neither red nor green. The terminal only
        // emits SGR on a change, so an argument painted in the command's
        // colour would leave no sequence at all; the reset has to be seen.
        let arg_at = find(pty.raw(), b" x", name_at + known.len())
            .unwrap_or_else(|| panic!("the argument of `{with_arg}` was not repainted:\n{frame}"));
        let arg_style = sgr_between(pty.raw(), name_at + known.len(), arg_at + 2);
        assert!(
            arg_style.iter().any(|p| p.contains(&39) || p == &[0]),
            "no reset between `{known}` and its argument: {arg_style:?}\n{frame}"
        );
        assert!(
            !sets_a_foreground(&arg_style),
            "the argument after `{known}` is painted, not plain: {arg_style:?}\n{frame}"
        );
        clear_draft(&mut pty);
        pty.read_for(HOTKEY_GAP);
        quit_and_check(&mut pty);
    }
}

// ---------------------------------------------------------------------
// Ctrl+R history search; the history file is private.
// ---------------------------------------------------------------------

/// After one exchange, Ctrl+R opens a history search; typing part of the
/// earlier prompt and Enter puts that prompt in the composer without
/// submitting it; Ctrl+R then Esc leaves the composer as it was. The
/// history file the pane writes is mode 0600.
#[test]
#[serial]
fn ctrl_r_recalls_an_earlier_prompt_into_the_composer_and_esc_backs_out() {
    let dir = fresh_config_dir("ctrl-r");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let first_at = pty.raw().len();
    pty.send(b"alpha-history-kq7\r").unwrap();
    wait_for_squeezed_after(&mut pty, "> alpha-history-kq7", first_at, "the exchange");
    pty.read_for(Duration::from_millis(2500));
    let second_at = pty.raw().len();
    pty.send(b"bravo-history-kq7\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "> bravo-history-kq7",
        second_at,
        "the second exchange",
    );
    pty.read_for(Duration::from_millis(2500));

    // Ctrl+R, Esc: nothing inserted, UI live.
    let search_at = pty.raw().len();
    pty.send(b"\x12").unwrap();
    pty.read_for(Duration::from_millis(600));
    let opened = since(&pty, search_at);
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = repainted_frame_after_resize(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("Typeapromptora.command"),
        "after Ctrl+R then Esc the composer is not back to its empty placeholder \
         (Ctrl+R painted:\n{opened}\n):\n{frame}"
    );

    // Ctrl+R, type `alpha`, Enter: the older prompt lands in the field.
    let search_at = pty.raw().len();
    pty.send(b"\x12").unwrap();
    pty.read_for(Duration::from_millis(600));
    pty.send(b"alpha").unwrap();
    pty.read_for(Duration::from_millis(600));
    let narrowed = since(&pty, search_at);
    assert!(
        squeezed(&narrowed).contains("alpha-history-kq7"),
        "the history search does not list the matching prompt:\n{narrowed}"
    );
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame_after_resize(&mut pty, 40, 102);
    let flat = squeezed(&frame);
    assert!(
        !flat.contains("Typeapromptora.command"),
        "Enter on the history match left the composer empty:\n{frame}"
    );
    assert_eq!(
        flat.matches(">alpha-history-kq7").count(),
        1,
        "Enter on the history match submitted the prompt instead of recalling it:\n{frame}"
    );
    assert_eq!(
        composer_row(&frame),
        "alpha-history-kq7",
        "the recalled prompt is not the composer's line:\n{frame}"
    );

    quit_clearing_any_draft(&mut pty);

    let history: Vec<PathBuf> = files_containing(&dir.join("cache"), "alpha-history-kq7");
    assert!(
        !history.is_empty(),
        "no history file under the cache dir records the prompts (looked for alpha-history-kq7)"
    );
    for file in &history {
        let mode = fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "history file {} is mode {mode:o}, not 0600",
            file.display()
        );
    }
}

// ---------------------------------------------------------------------
// The vi layer's minimum set.
// ---------------------------------------------------------------------

/// `keybindings: vi`: Esc leaves insert mode; `0 x` deletes the first
/// character, `$ x` the last, `A` appends, `dd` clears the line, `i`
/// returns to insert. Every step is observed before the verdict so one
/// motion's miss does not hide the others.
#[test]
#[serial]
fn vi_keybindings_edit_the_composer_in_normal_mode() {
    let dir = fresh_config_dir("vi-layer");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "keybindings: vi\n");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let mut misses: Vec<String> = Vec::new();
    let mut toggle = false;

    pty.send(b"hello world").unwrap();
    pty.read_for(Duration::from_millis(400));
    key(&mut pty, b"\x1b");
    let frame = next_frame(&mut pty, &mut toggle);
    if !frame.contains("NORMAL") {
        misses.push(format!("Esc did not show the NORMAL mode title:\n{frame}"));
    }
    key(&mut pty, b"0");
    key(&mut pty, b"x");
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    if !flat.contains("elloworld") || flat.contains("helloworld") {
        misses.push(format!(
            "Esc 0 x did not delete the first character:\n{frame}"
        ));
    }

    // From here the checks read the composer row itself: squeezing the
    // whole frame glues the row to the footer that follows it, so
    // `ello worl` + `dryrun` would read as `elloworld`.
    // `$` in vi lands ON the last character, so `x` deletes it.
    key(&mut pty, b"$");
    key(&mut pty, b"x");
    let frame = next_frame(&mut pty, &mut toggle);
    if composer_row(&frame) != "ello worl" {
        misses.push(format!("$ x did not delete the last character:\n{frame}"));
    }

    // `h` then `x`: one left of the cursor, deletes that character.
    key(&mut pty, b"h");
    key(&mut pty, b"x");
    let frame = next_frame(&mut pty, &mut toggle);
    if composer_row(&frame) != "ello wol" {
        misses.push(format!(
            "h x did not delete the character left of the cursor (expected `ello wol`):\n{frame}"
        ));
    }

    key(&mut pty, b"A");
    key(&mut pty, b"!");
    let frame = next_frame(&mut pty, &mut toggle);
    if composer_row(&frame) != "ello wol!" {
        misses.push(format!("A did not append at the end of the line:\n{frame}"));
    }

    key(&mut pty, b"\x1b");
    key(&mut pty, b"d");
    key(&mut pty, b"d");
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    if flat.contains("ellowor") {
        misses.push(format!("dd did not clear the line:\n{frame}"));
    }

    key(&mut pty, b"i");
    pty.send(b"insert-again-kq7").unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = next_frame(&mut pty, &mut toggle);
    if !squeezed(&frame).contains("insert-again-kq7") {
        misses.push(format!("i did not return to insert mode:\n{frame}"));
    }

    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} vi-layer step(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// Backtick document commands under the TUI.
// ---------------------------------------------------------------------

/// `` .file `cat` `` runs the command with stdin closed: `cat` exits at
/// once instead of waiting on the tty, the turn ends and the composer
/// takes keys. `` .file `sleep N` `` is abortable: Esc ends the turn and
/// the sleeper, which wrote its own pid for the check, does not outlive
/// the session.
#[test]
#[serial]
fn backtick_document_commands_never_read_the_tty_and_esc_aborts_them() {
    let dir = fresh_config_dir("backtick-doc");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let cat_at = pty.raw().len();
    pty.send(b".file `cat` -- summarize\r").unwrap();
    let started = Instant::now();
    // Keys typed while cat waited on the tty would be eaten by cat.
    pty.read_for(Duration::from_millis(1500));
    assert_composer_takes_keys(&mut pty, cat_at, "`.file `cat``");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the backtick `cat` held the turn for {:?}",
        started.elapsed()
    );

    let sleep_at = pty.raw().len();
    let sleeper_pid = dir.join("sleeper.pid");
    pty.send(
        format!(
            ".file `echo $$ > '{}'; exec sleep 6173` -- summarize\r",
            sleeper_pid.display()
        )
        .as_bytes(),
    )
    .unwrap();
    wait_for_file(&mut pty, &sleeper_pid, "the sleeper's pid");
    let pid: i32 = fs::read_to_string(&sleeper_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut sleepers = SleeperPids(vec![pid]);
    pty.read_for(Duration::from_millis(1500));
    pty.send(b"\x1b").unwrap();
    let esc_at = Instant::now();
    assert_composer_takes_keys(&mut pty, sleep_at, "Esc on `.file `sleep``");
    assert!(
        esc_at.elapsed() < Duration::from_secs(10),
        "Esc took {:?} to end the backtick turn",
        esc_at.elapsed()
    );

    quit_and_check(&mut pty);
    // SAFETY: kill with signal 0 touches no memory; it only checks the pid.
    let alive = unsafe { libc::kill(pid, 0) == 0 };
    assert!(
        !alive,
        "the aborted backtick command outlived the session (pid {pid})"
    );
    sleepers.forget(pid);
}

// ---------------------------------------------------------------------
// The palette's `.macro` arguments list the user's macros cold.
// ---------------------------------------------------------------------

/// A macro in the config dir is offered as a `.macro` argument on a cold
/// start, before any command has run.
#[test]
#[serial]
fn the_palette_offers_a_config_dir_macro_as_a_dot_macro_argument_cold() {
    let dir = fresh_config_dir("macro-args");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let macros = dir.join("macros");
    fs::create_dir_all(&macros).unwrap();
    fs::write(
        macros.join("cold-macro-kq7.yaml"),
        "description: a macro of the config dir\n\
         steps:\n\
         \x20 - .info\n",
    )
    .unwrap();
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(500));

    let typed_at = pty.raw().len();
    pty.send(b".macro ").unwrap();
    pty.read_for(Duration::from_millis(600));
    repaint(&mut pty, 60, 141);
    let frame = since(&pty, typed_at);
    assert!(
        squeezed(&frame).contains("cold-macro-kq7"),
        "`.macro ` on a cold start does not offer the config dir's macro:\n{frame}"
    );

    pty.send(b"col").unwrap();
    pty.read_for(Duration::from_millis(600));
    let narrowed_at = repaint(&mut pty, 60, 140);
    let narrowed = since(&pty, narrowed_at);
    assert!(
        squeezed(&narrowed).contains("cold-macro-kq7"),
        "`.macro col` does not narrow to the macro:\n{narrowed}"
    );

    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    quit_clearing_any_draft(&mut pty);
}

// ---------------------------------------------------------------------
// The placeholder names the configured command key.
// ---------------------------------------------------------------------

/// Under `command_prefix: slash` the empty composer's placeholder points
/// at `/` for commands, not `.`.
#[test]
#[serial]
fn the_placeholder_names_the_configured_command_prefix() {
    for (prefix_yaml, sigil) in [("command_prefix: slash\n", '/'), ("", '.')] {
        let dir = fresh_config_dir(&format!("placeholder-{}", sigil as u8));
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, prefix_yaml);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        let flat = squeezed(&frame);
        let other = if sigil == '/' { '.' } else { '/' };
        assert!(
            flat.contains(&format!("Typeapromptora{sigil}command")),
            "under `{sigil}` the placeholder does not point at `{sigil}` for commands:\n{frame}"
        );
        assert!(
            !flat.contains(&format!("Typeapromptora{other}command")),
            "under `{sigil}` the placeholder still points at `{other}`:\n{frame}"
        );
        quit_and_check(&mut pty);
    }
}
