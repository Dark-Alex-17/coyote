//! Real-process checks of the command menus the prompter routes: the
//! questions `.vault`, `.rag`, `.agent`, `.install`, `.update` and
//! `.session` raise become modals on a real tty and inquire prompts in
//! the plain REPL, keys typed ahead of a confirm settle or do not settle
//! it, and a run with no human to ask (no terminal at all, a terminal
//! with stdin from a pipe or /dev/null, a headless `--agent`) refuses in
//! the shared shape, skips the optional knowledge base or lists the
//! unset variables instead of hanging on a prompt. Each case runs the
//! binary on a pseudo-terminal under a bash wrapper that reports the
//! exit code and `stty -a` afterwards, so the file is unix-only. The
//! cases run one at a time: each spawn forks a session leader and takes
//! a controlling tty, and a fork landing between another test's
//! `openpty` and its `exec` has failed the spawn with a garbled errno.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::fake_llm::{Chunk, FakeLlm, Reply};
use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
use serial_test::serial;

// ---------------------------------------------------------------------
// The modals a command's questions become under the UI (a secret's
// value, a new rag's embedding model, a package-managed update), the
// Input modal an agent variable raises, a confirm on a screen too short
// for its question, the interrupted notice's plain text, and concealed
// text in a reply.
// ---------------------------------------------------------------------

/// A dot-command flow whose question is a modal under the UI.
struct ModalFlow {
    scenario: &'static str,
    command: &'static str,
    /// Path fragments no new file outside the cache may carry afterwards.
    footprints: &'static [&'static str],
    /// Fixture written into the config dir before the UI starts.
    setup: Option<fn(&Path)>,
}

/// Drives one prompt-backed flow whose question is a modal under the UI:
/// the modal showing the squeezed `modal_markers` comes up, Esc cancels
/// it, nothing of the flow is written to the config dir, `$EDITOR` is
/// never launched (a flow that fell through to its editor on Esc would
/// record itself), the terminal is never released, and the UI keeps
/// taking keys and leaves clean on Ctrl+D.
fn assert_asks_in_the_modal_and_esc_cancels(flow: &ModalFlow, modal_markers: &[&str]) {
    let dir = fresh_config_dir(flow.scenario);
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    if let Some(setup) = flow.setup {
        setup(&dir);
    }
    let launched = dir.join("editor-launched");
    let editor = recording_editor(&dir, &launched);
    let config_before = fs::read_to_string(dir.join("config.yaml")).unwrap();
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(format!("{}\r", flow.command).as_bytes()).unwrap();
    wait_for_the_modal(&mut pty, before, flow.command, modal_markers);
    esc_cancels_the_modal(&mut pty, flow.command);

    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    for footprint in flow.footprints {
        let written = new_files_named(&dir, &files_before, footprint);
        assert!(
            written.is_empty(),
            "{}: the cancelled `{}` still wrote {written:?}:\n{since}",
            flow.scenario,
            flow.command
        );
    }
    assert_eq!(
        fs::read_to_string(dir.join("config.yaml")).unwrap(),
        config_before,
        "{}: the cancelled `{}` rewrote config.yaml:\n{since}",
        flow.scenario,
        flow.command
    );
    assert!(
        !launched.exists(),
        "{}: the cancelled `{}` launched the editor:\n{since}",
        flow.scenario,
        flow.command
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "{}: the cancelled `{}` released the terminal:\n{since}",
        flow.scenario,
        flow.command
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .unwrap_or_else(|e| {
            panic!(
                "{}: `.info` after the cancelled `{}` never rendered: {e:?}",
                flow.scenario, flow.command
            )
        });

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{}: {transcript}", flow.scenario);
    assert_tty_restored(&transcript);
    for footprint in flow.footprints {
        let written = new_files_named(&dir, &files_before, footprint);
        assert!(
            written.is_empty(),
            "{}: {written:?} appeared by the time the UI left:\n{transcript}",
            flow.scenario
        );
    }
    assert!(
        !launched.exists(),
        "{}: the editor was launched by the time the UI left:\n{transcript}",
        flow.scenario
    );
}

/// `.save role` on a temp role asks `Role name:` in a modal and writes the
/// role file under that name.
#[test]
#[serial]
fn dot_save_role_on_a_temp_role_asks_for_the_name_in_the_modal() {
    let dir = fresh_config_dir("save-role-modal");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".temp-role you are the temp role g4r\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".save role\r").unwrap();
    pty.wait_for_after("Role name:", before, PTY_DEADLINE)
        .expect("`.save role` on a temp role did not ask `Role name:` in a modal");
    answer_field_modal(&mut pty, "saved-role-xk3");
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let saved = loop {
        let saved = new_files_named(&dir, &files_before, "saved-role-xk3");
        if !saved.is_empty() {
            break saved;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no saved-role-xk3 role file was written under {dir:?}:\n{}",
            strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]))
        );
        pty.read_for(Duration::from_millis(200));
    };
    assert!(
        saved
            .iter()
            .any(|path| path.to_string_lossy().contains("/roles/")),
        "the role was written outside the roles dir: {saved:?}"
    );

    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", before, PTY_DEADLINE)
        .expect("keys were not handled after the role was saved");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `.vault add <name>` asks for the secret's value; under the UI the
/// question is the Password modal, and Esc leaves the vault untouched.
#[test]
#[serial]
fn dot_vault_add_asks_for_the_value_in_the_password_modal() {
    assert_asks_in_the_modal_and_esc_cancels(
        &ModalFlow {
            scenario: "modal-vault-add",
            command: ".vault add vault-secret-g4r",
            footprints: &["g4r", "vault"],
            setup: None,
        },
        &["Enterthesecretvalue:"],
    );
}

/// `.rag <new-name>` asks which embedding model and documents to use;
/// under the UI the first question is the Select modal listing the
/// configured embedding models, and Esc leaves no rag behind.
#[test]
#[serial]
fn dot_rag_for_a_new_rag_asks_for_the_embedding_model_in_the_select_modal() {
    assert_asks_in_the_modal_and_esc_cancels(
        &ModalFlow {
            scenario: "modal-rag-create",
            command: ".rag new-rag-g4r",
            footprints: &["g4r", "/rags/"],
            setup: Some(write_dry_run_config_with_embeddings),
        },
        &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
    );
}

/// `.update` on a Cargo-installed binary warns and asks `Update anyway?`
/// before it checks for or downloads anything; under the UI the question
/// is the Confirm modal (no raw tty prompt), `n` cancels the update with
/// the binary untouched, no network round trip made and no write probe
/// left behind, and the UI keeps taking keys and leaves clean.
#[test]
#[serial]
fn dot_update_on_a_cargo_install_asks_in_the_confirm_modal_and_n_cancels() {
    let dir = fresh_config_dir("update-confirm-modal");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let binary = cargo_installed_binary(&dir);
    let binary_before = fs::metadata(&binary).unwrap();
    let mut pty = spawn_binary_under(&binary, &dir, &[], WAIT_WRAPPER, "/dev/tty", "");

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".update\r").unwrap();
    let modal = wait_for_the_modal(&mut pty, before, ".update", &["Updateanyway?", "y=yes"]);
    assert!(
        !squeezed(&modal).contains("(y/N)"),
        "`Update anyway?` was asked on the raw tty, not in the modal:\n{modal}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"n").unwrap();
    wait_for_squeezed(
        &mut pty,
        "Update cancelled.",
        "the cancellation after `n` on the modal",
    );
    pty.read_for(Duration::from_millis(500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    // The command's output is a Command cell, painted once the command
    // returns: the warning that preceded the question lands with the
    // cancellation.
    let flat = squeezed(&since);
    assert!(
        flat.contains("WARNING:") && flat.contains(&squeezed("appears to be installed via")),
        "the Cargo-install warning was not printed with the cancellation:\n{since}"
    );
    // The updater probes the install dir for write access only after the
    // confirm, so no probe file means `n` stopped it before that step.
    let probes = new_files_named(&dir, &files_before, "coyote-update-write-test");
    assert!(
        probes.is_empty(),
        "the cancelled update went on to probe for write access: {probes:?}:\n{since}"
    );
    let binary_after = fs::metadata(&binary).unwrap();
    assert!(
        binary_after.len() == binary_before.len()
            && binary_after.modified().unwrap() == binary_before.modified().unwrap(),
        "the cancelled update touched the binary at {}",
        binary.display()
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the question released the terminal:\n{since}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("`.info` after the cancelled update never rendered");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// An agent whose `username` variable has no default, so starting it asks
/// for the value.
fn write_agent_needing_a_variable(dir: &Path, name: &str) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        format!(
            "name: {name}\n\
             description: an agent with a variable\n\
             version: 0.1.0\n\
             instructions: |\n\
             \x20 You are an assistant for {{{{username}}}}. Reply briefly.\n\
             variables:\n\
             \x20 - name: username\n\
             \x20   description: Your user name\n"
        ),
    )
    .unwrap();
}

/// `.agent <name>` for an agent whose variable has no default asks for
/// the value through the prompter, not on the raw tty: under the UI the
/// question is the Input modal (name, description, `Enter = answer`),
/// typing a value and Enter answers it, the agent comes up with that
/// value, and no raw prompt and no released terminal are seen on the
/// way.
#[test]
#[serial]
fn dot_agent_needing_a_variable_asks_in_the_input_modal_and_enter_answers_it() {
    let dir = fresh_config_dir("agent-variable-modal");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    write_agent_needing_a_variable(&dir, "vars-agent-g4r");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b".agent vars-agent-g4r\r").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let modal = loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        let flat = squeezed(&since);
        if flat.contains("username(Yourusername)") && flat.contains("Enter=answer") {
            break since;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "`.agent` with an unset variable did not raise the Input modal within 10s:\n{since}"
        );
    };
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the variable question released the terminal:\n{modal}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"typed-user-g4r").unwrap();
    // Squeezed: the field is painted across frames while the status row's
    // clock ticks, so the literal can be split by the painter.
    wait_for_squeezed(
        &mut pty,
        "typed-user-g4r",
        "the typed value in the Input modal",
    );
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));

    // The agent is up with the value: `.info agent` exports its
    // variables and the interpolated instructions, and the question is
    // no longer on screen. The export runs to well over a hundred rows
    // (the built-in tool guidance is appended to the instructions), so
    // the window is made tall enough to show it whole.
    pty.resize(300, 120).unwrap();
    pty.read_for(Duration::from_millis(800));
    let info_at = pty.raw().len();
    pty.send(b".info agent\r").unwrap();
    pty.wait_for_after("config_file", info_at, PTY_DEADLINE)
        .expect("`.info agent` never rendered after the modal was answered");
    pty.read_for(Duration::from_millis(800));
    let info = strip_ansi(&String::from_utf8_lossy(&pty.raw()[info_at..]));
    let flat = squeezed(&info);
    assert!(
        flat.contains("username:typed-user-g4r"),
        "`.info agent` does not carry the value typed into the Input modal:\n{info}"
    );
    assert!(
        flat.contains("assistantfortyped-user-g4r"),
        "the agent's instructions were not interpolated with the typed value:\n{info}"
    );
    assert!(
        info.contains("vars-agent-g4r"),
        "`.info agent` did not name the agent:\n{info}"
    );
    assert!(
        !flat.contains("Enter=answer"),
        "the Input modal is still up after Enter:\n{info}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// The recording confirm tool that also marks the moment it asks, so a
/// test can tell "the modal is up" apart from "the question is painted"
/// on a screen too short to paint it.
const ASKING_CONFIRM_TOOL: &str = "#!/usr/bin/env bash\n\
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
    \x20   : > \"$out.asked\"\n\
    \x20   v=\"$(confirm \"Really do it? q9d-q\")\"\n\
    \x20   printf '%s' \"${v:-cancelled}\" > \"$out\"\n\
    \x20   echo \"fixture confirm: ${v:-cancelled} q9d-r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// The asking confirm tool with a three-line question: too tall for the
/// rows a 40x5 frame can give it, short enough to fit once the window
/// grows.
fn three_line_question_confirm_tool() -> String {
    ASKING_CONFIRM_TOOL.replace(
        "\"Really do it? q9d-q\"",
        "\"line one q3l-a\"$'\\n'\"line two q3l-b\"$'\\n'\"line three q3l-c\"",
    )
}

/// Starts the confirm flow on a `rows`x40 screen: resizes once the UI is
/// up, submits the prompt, waits for the tool to ask and hands back the
/// printable bytes since the prompt was sent after a moment for the
/// modal to paint.
fn open_confirm_on_short_screen(pty: &mut Pty, asked: &Path, rows: u16) -> String {
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(rows, 40).unwrap();
    pty.read_for(Duration::from_millis(500));
    let before = pty.raw().len();
    pty.send(b"call the confirm tool\r").unwrap();
    wait_for_file(pty, asked, "the tool's confirm request");
    pty.read_for(Duration::from_millis(1000));
    strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]))
}

/// Pumps the pty until the fake has served `n` requests.
fn wait_for_served(pty: &mut Pty, fake: &FakeLlm, n: usize, what: &str) {
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    while fake.served() < n {
        let text = pty.read_for(Duration::from_millis(100));
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: the fake served {} of {n} requests:\n{text}",
            fake.served()
        );
    }
}

/// A three-line question on a 40x5 frame cannot be shown whole: the modal
/// says so, offers only Esc, and `y` does nothing (the tool gets no
/// answer). Once the window is tall enough the whole question and the
/// approving keys are painted and `y` answers it.
#[test]
#[serial]
fn a_three_line_confirm_on_a_five_row_screen_refuses_y_until_the_terminal_is_enlarged() {
    let dir = fresh_config_dir("confirm-squeezed-5-rows");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("confirm-answer");
    let asked = dir.join("confirm-answer.asked");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture squeezed done s5r".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_confirm_rec.sh",
        &three_line_question_confirm_tool(),
    );
    let mut pty = spawn_tui(&dir, &[]);

    let shown = open_confirm_on_short_screen(&mut pty, &asked, 5);
    let flat = squeezed(&shown);
    assert!(
        !flat.contains("linethreeq3l-c"),
        "a 40x5 frame painted the whole three-line question (then `y` must answer, and this \
         case is moot):\n{shown}"
    );
    assert!(
        flat.contains("enlargetheterminal"),
        "the squeezed question carries no `enlarge the terminal` marker or legend:\n{shown}"
    );
    assert!(
        !flat.contains("y=yes"),
        "the squeezed modal still offers an approving key:\n{shown}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    pty.read_for(Duration::from_millis(1500));
    assert!(
        !out.exists(),
        "`y` answered a question the 40x5 frame could not show whole: the tool recorded {:?}",
        fs::read_to_string(&out).unwrap_or_default()
    );

    let grown_at = pty.raw().len();
    pty.resize(24, 40).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let grown = loop {
        pty.read_for(Duration::from_millis(200));
        let grown = strip_ansi(&String::from_utf8_lossy(&pty.raw()[grown_at..]));
        let flat = squeezed(&grown);
        if flat.contains("linethreeq3l-c") && flat.contains("y=yes") {
            break grown;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "after growing to 40x24 the whole question and the approving keys were not painted:\n{grown}"
        );
    };
    assert!(
        !squeezed(&grown).contains("enlargetheterminal"),
        "the enlarge legend is still shown once the question fits:\n{grown}"
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer after enlarging");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "`y` after enlarging did not reach the tool as a yes"
    );
    wait_for_squeezed(
        &mut pty,
        "fixture squeezed done s5r",
        "the reply after the answered confirm",
    );
    assert_eq!(fake.served(), 2);

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A one-line question fits a 40x5 frame: the question and the approving
/// keys are painted, `y` reaches the tool as a yes, the turn finishes
/// with the model's follow-up, and the UI is still taking keys. (A tiny
/// frame has no transcript rows; the window is grown to read the reply.)
#[test]
#[serial]
fn a_one_line_confirm_fits_a_five_row_screen_and_y_answers_it() {
    let dir = fresh_config_dir("confirm-fits-5-rows");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("confirm-answer");
    let asked = dir.join("confirm-answer.asked");
    let reply = "fixture short screen done f5r";
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text(reply.to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_confirm_rec.sh", ASKING_CONFIRM_TOOL);
    let mut pty = spawn_tui(&dir, &[]);

    let shown = open_confirm_on_short_screen(&mut pty, &asked, 5);
    let flat = squeezed(&shown);
    assert!(
        flat.contains("Reallydoit?q9d-q"),
        "the one-line question was not painted whole on the 5-row screen:\n{shown}"
    );
    assert!(
        !flat.contains("enlargetheterminal"),
        "the question is painted whole yet the modal says to enlarge the terminal:\n{shown}"
    );
    assert!(
        flat.contains("y=yes"),
        "the question fits but the modal does not offer the approving keys:\n{shown}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_file(
        &mut pty,
        &out,
        "the tool's recorded answer on the 5-row screen",
    );
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "`y` on the 5-row modal did not reach the tool as a yes"
    );
    wait_for_served(&mut pty, &fake, 2, "the turn after the answered confirm");

    // Grow the window to read the reply; the turn must have finished and
    // the UI must still be taking keys.
    let grown_at = pty.raw().len();
    pty.resize(24, 80).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let painted = loop {
        pty.read_for(Duration::from_millis(200));
        let grown = strip_ansi(&String::from_utf8_lossy(&pty.raw()[grown_at..]));
        if squeezed(&grown).contains(&squeezed(reply)) {
            break grown;
        }
        if std::time::Instant::now() >= deadline {
            // Say whether the UI is alive at all before failing.
            let alive_at = pty.raw().len();
            pty.send(b".info\r").unwrap();
            let alive = pty
                .wait_for_after("log_path", alive_at, Duration::from_secs(5))
                .is_ok();
            pty.send(b"\x04").unwrap();
            let exited = pty
                .wait_for("tty-probe-done", Duration::from_secs(8))
                .ok()
                .map(|t| exit_code(&t));
            panic!(
                "the reply after the answered confirm never appeared on the grown screen \
                 (fake served {}; UI answered `.info` afterwards: {alive}; exit after Ctrl+D: \
                 {exited:?}):\n{grown}",
                fake.served()
            );
        }
    };
    assert!(
        !squeezed(&painted).contains("y=yes"),
        "the answered modal is still up after the turn finished:\n{painted}"
    );

    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", grown_at, PTY_DEADLINE)
        .expect("keys were not handled after the confirm on the 5-row screen");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// Esc on a held stream leaves the interrupted notice as plain words: the
/// REPL paints it red, the UI must scrub that before it reaches a cell.
/// Under `NO_COLOR` nothing may paint red, so a `[31m` anywhere after the
/// Esc - as painted text or as a live escape - is the REPL's colouring
/// leaking through.
#[test]
#[serial]
fn esc_on_a_held_stream_shows_the_interrupted_notice_as_plain_words() {
    let dir = fresh_config_dir("esc-notice-plain");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("never-released");
    let fake = FakeLlm::start(vec![Reply::Streamed(vec![
        Chunk::Text("partial before esc n3p".to_string()),
        Chunk::WaitFor(release),
        Chunk::Text(" tail never shown n3q".to_string()),
    ])]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[("NO_COLOR", "1")]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".session\r").unwrap();
    pty.wait_for("temp", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"stream and hold\r").unwrap();
    wait_for_squeezed(&mut pty, "partial before esc n3p", "the first delta");
    pty.wait_for("Esc to interrupt", PTY_DEADLINE)
        .expect("the status row never offered Esc to interrupt");
    let pressed_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
        if squeezed(&since).contains("Responseinterrupted") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no cell said `Response interrupted` after Esc:\n{since}"
        );
    }
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
    assert!(
        !since.contains("[31m") && !since.contains("[0m") && !since.contains("[1m"),
        "the interrupted notice carried its colouring as text:\n{since}"
    );
    let raw_since = &pty.raw()[pressed_at..];
    let coloured: Vec<_> = sgr_params(raw_since)
        .into_iter()
        .filter(|p| is_colour_sgr(p))
        .collect();
    assert!(
        coloured.is_empty(),
        "NO_COLOR=1 but colour SGRs were emitted after the interrupt: {coloured:?}\n{since}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A reply that conceals a word with SGR 8 shows it anyway: the UI never
/// hides text it was given, never forwards the conceal attribute to the
/// terminal, and never paints the sequence as text.
#[test]
#[serial]
fn a_reply_with_concealed_text_shows_it_and_sends_no_conceal_to_the_tty() {
    let dir = fresh_config_dir("conceal-visible");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(
        "before c8a \x1b[8mhidden-word-c8b\x1b[28m after c8c".to_string(),
    )]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let submitted_at = pty.raw().len();
    pty.send(b"conceal please\r").unwrap();
    let transcript = wait_for_squeezed(&mut pty, "after c8c", "the reply's tail");
    assert_eq!(fake.served(), 1, "{transcript}");
    pty.read_for(Duration::from_millis(600));

    let raw = pty.raw();
    let visible = strip_ansi(&String::from_utf8_lossy(&raw[submitted_at..]));
    assert!(
        squeezed(&visible).contains("hidden-word-c8b"),
        "the concealed word was not painted:\n{visible}"
    );
    assert!(
        !visible.contains("[8m") && !visible.contains("[28m"),
        "the conceal sequence was painted as text:\n{visible}"
    );
    let concealing: Vec<_> = sgr_params(&raw[submitted_at..])
        .into_iter()
        .filter(|p| is_conceal_sgr(p))
        .collect();
    assert!(
        concealing.is_empty(),
        "a conceal SGR reached the tty: {concealing:?}\n{visible}"
    );

    // A full repaint keeps the word visible too.
    let repaint_at = raw.len();
    pty.resize(30, 90).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let repainted = loop {
        pty.read_for(Duration::from_millis(200));
        let repainted = strip_ansi(&String::from_utf8_lossy(&pty.raw()[repaint_at..]));
        if squeezed(&repainted).contains("afterc8c") {
            break repainted;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the reply was not repainted after the resize:\n{repainted}"
        );
    };
    assert!(
        squeezed(&repainted).contains("hidden-word-c8b"),
        "the concealed word vanished on repaint:\n{repainted}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let visible_after = strip_ansi(&String::from_utf8_lossy(pty.raw()));
    assert!(
        !visible_after.contains("[8m"),
        "the conceal sequence reached the normal screen as text:\n{visible_after}"
    );
}

/// `n` to the incorporate question seats the named session, empty.
#[test]
#[serial]
fn dot_session_after_a_turn_asks_in_the_modal_and_n_seats_the_session() {
    let dir = fresh_config_dir("session-after-turn-n");
    let _guard = TempDirGuard(dir.clone());
    let (pty, _fake) = open_session_after_a_turn(&dir, "after-turn-xk3");
    answer_and_expect_session(pty, b"n", "after-turn-xk3", true);
}

/// `y` to the incorporate question seats the named session holding the exchange.
#[test]
#[serial]
fn dot_session_after_a_turn_asks_in_the_modal_and_y_seats_the_session() {
    let dir = fresh_config_dir("session-after-turn-y");
    let _guard = TempDirGuard(dir.clone());
    let (pty, _fake) = open_session_after_a_turn(&dir, "after-turn-yk3");
    answer_and_expect_session(pty, b"y", "after-turn-yk3", false);
}

// ---------------------------------------------------------------------------
// Type-ahead at the two kinds of confirm in the plain REPL, and what a run
// with no human reports when it skips or refuses a knowledge base.
// ---------------------------------------------------------------------------

/// A tool's `confirm` question in the plain REPL keeps taking a `y` and
/// Enter typed before the question renders: the prompt and the answer go
/// down the tty in one write, the question still shows, the typed-ahead
/// answer settles it as a yes and the tool records it. (Only the command
/// menus drain what was typed ahead of them; a tool's confirm does not.)
#[test]
#[serial]
fn a_tools_confirm_question_in_the_plain_repl_takes_a_y_typed_ahead_of_it() {
    let dir = fresh_config_dir("tool-confirm-typeahead-repl");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("confirm-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture done t4a".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_confirm_rec.sh", RECORDING_CONFIRM_TOOL);
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let asked_at = pty.raw().len();
    // The prompt and the answer to the question it will raise, in one write.
    pty.send(b"call the confirm tool\ry\r").unwrap();
    pty.wait_for_after("q9d-q", asked_at, PTY_DEADLINE)
        .expect("the tool's confirm question never reached the terminal");
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done t4a", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "`y` and Enter typed ahead of the tool's confirm did not reach the tool as a yes:\n{transcript}"
    );
    assert!(
        squeezed(&transcript).contains("q9d-qYes"),
        "the confirm did not echo the typed-ahead `Yes`:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{transcript}"
    );

    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `.install macros` in the plain REPL with `y` and Enter typed ahead of
/// its `Continue?` question, in the same write as the command: the stale
/// keystrokes are drained before the question opens, so it still waits
/// with its `(y/N)` legend and nothing is reinstalled; the next bare Enter
/// then takes the `No` default.
#[test]
#[serial]
fn dot_install_macros_in_the_plain_repl_drains_a_y_typed_ahead_of_its_confirm() {
    let dir = fresh_config_dir("install-builtins-typeahead-repl");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let macros_dir = dir.join("macros");
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let files_before = walk(&macros_dir);
    let before = pty.raw().len();
    pty.send(b".install macros\ry\r").unwrap();
    pty.wait_for_after("(y/N)", before, PTY_DEADLINE)
        .expect("`.install macros` did not ask `Continue?` with its `(y/N)` legend");
    let after = pty.read_for(Duration::from_millis(1500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    let flat = squeezed(&since);
    assert!(
        !flat.contains("Continue?Yes")
            && !flat.contains("Continue?No")
            && !flat.contains("Aborted")
            && !flat.contains("Installed")
            && !flat.contains("Reinstalled"),
        "the `y` and Enter typed ahead of `Continue?` were taken as its answer:\n{after}"
    );
    assert_eq!(
        walk(&macros_dir),
        files_before,
        "the keystrokes typed ahead of `Continue?` changed the macros directory"
    );

    pty.send(b"\r").unwrap();
    let settled = wait_for_squeezed(
        &mut pty,
        "No files were changed",
        "the plain REPL's answer to Enter on `Continue?`",
    );
    assert!(
        squeezed(&settled).contains("Continue?No"),
        "Enter on the confirm did not take the `No` default:\n{settled}"
    );
    assert_eq!(
        walk(&macros_dir),
        files_before,
        "the declined reinstall changed the macros directory"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{settled}"
    );

    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A graph agent whose `rag` node names its documents but none of
/// `embedding_model`, `chunk_size` and `chunk_overlap`, so building its
/// knowledge base would have to ask.
fn write_graph_agent_with_an_unbuilt_rag_node(dir: &Path, name: &str) -> PathBuf {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(agent_dir.join("knowledge")).unwrap();
    fs::write(
        agent_dir.join("knowledge").join("fact.md"),
        "# Facts\n\nThe probe fact for the graph agent's knowledge base.\n",
    )
    .unwrap();
    fs::write(
        agent_dir.join("graph.yaml"),
        format!(
            "name: {name}\n\
             description: a graph agent with a rag node\n\
             version: \"1.0\"\n\
             start: retrieve\n\
             nodes:\n\
             \x20 retrieve:\n\
             \x20   id: retrieve\n\
             \x20   type: rag\n\
             \x20   documents:\n\
             \x20     - ./knowledge/\n\
             \x20   query: \"{{{{initial_prompt}}}}\"\n\
             \x20   state_updates:\n\
             \x20     context: \"{{{{output.context}}}}\"\n\
             \x20   next: done\n\
             \x20 done:\n\
             \x20   id: done\n\
             \x20   type: end\n\
             \x20   output: \"ctx={{{{context}}}}\"\n"
        ),
    )
    .unwrap();
    agent_dir
}

/// A graph agent with a `rag` node that is not fully specified, run with
/// no terminal at all: the build is refused in the shared shape, naming
/// the node and the agent, the remedy names the three node fields, the
/// run exits non-zero before any document is loaded, and nothing is
/// written under the agent directory or `rags/`.
#[test]
#[serial]
fn a_tty_less_graph_agent_with_an_unbuilt_rag_node_is_refused_naming_the_node() {
    let dir = fresh_config_dir("ttyless-graph-rag-node");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let name = "graph-rag-t4c";
    let agent_dir = write_graph_agent_with_an_unbuilt_rag_node(&dir, name);
    let agent_files_before = walk(&agent_dir);
    let files_before = walk(&dir);

    let (ok, out) = run_ttyless(
        &dir,
        &["--agent", name, "what is the fact"],
        "tty-less graph agent with an unbuilt rag node",
    );
    assert!(!ok, "the refused tty-less graph run exited 0:\n{out}");
    let (action, remedy) = refusal_shape(&out).unwrap_or_else(|| {
        panic!("the refusal is not `Refusing to <action> non-interactively. Re-run in a terminal...`:\n{out}")
    });
    assert!(
        action.contains("'retrieve'") && action.contains(name),
        "the refusal does not name the rag node and its agent: {action:?}\n{out}"
    );
    for field in ["embedding_model", "chunk_size", "chunk_overlap"] {
        assert!(
            remedy.contains(field),
            "the remedy does not name `{field}` as the setting that avoids the question: {remedy:?}\n{out}"
        );
    }
    assert!(
        !out.contains("Initializing RAG") && !out.contains("fact.md"),
        "the refused run started building the knowledge base first:\n{out}"
    );
    assert_eq!(
        walk(&agent_dir),
        agent_files_before,
        "the refused run wrote into the agent directory:\n{out}"
    );
    let written = new_files_named(&dir, &files_before, "/rags/");
    assert!(
        written.is_empty(),
        "the refused run still wrote {written:?}:\n{out}"
    );
}

/// Runs the binary with no terminal at all, like [`run_ttyless`], with
/// its log at `debug` level into `log`, so a test can read what a silent
/// run recorded.
fn run_ttyless_logging(dir: &Path, args: &[&str], log: &Path, what: &str) -> (bool, String) {
    let env = [
        ("COYOTE_LOG_LEVEL", "debug"),
        ("COYOTE_LOG_PATH", log.to_str().unwrap()),
    ];
    run_ttyless_with_env(built_binary(), dir, args, &env, what)
}

/// The `[DEBUG]` lines of `log` that record `name` skipping its optional
/// knowledge base.
fn rag_skip_lines(log: &Path, name: &str) -> Vec<String> {
    let text = fs::read_to_string(log).unwrap_or_default();
    text.lines()
        .filter(|line| line.contains("[DEBUG]"))
        .filter(|line| line.contains(&format!("Agent '{name}' has documents but no")))
        .map(str::to_string)
        .collect()
}

/// An agent with documents started where nobody can answer `init RAG?`
/// skips the optional knowledge base silently on the terminal and says
/// so once in the debug log, on both silent paths: with no terminal at
/// all (nothing to ask on), and headless on a terminal (a terminal, but
/// no human seated to ask). Each run reaches the model and exits 0,
/// prints neither the question nor a notice, builds no rag, and the log
/// carries exactly one `[DEBUG]` line naming the agent and the skip.
#[test]
#[serial]
fn a_run_with_nobody_to_ask_about_rag_logs_the_skip_at_debug_and_prints_nothing() {
    // No terminal at all.
    let dir = fresh_config_dir("rag-skip-debug-ttyless");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    write_agent_with_documents(&dir, "docs-agent-t4d");
    let files_before = walk(&dir);
    let log = dir.join("probe.log");
    let (ok, out) = run_ttyless_logging(
        &dir,
        &["--agent", "docs-agent-t4d", "say hello-sentinel-t4d"],
        &log,
        "tty-less agent with documents",
    );
    assert!(ok, "the tty-less agent run did not exit 0:\n{out}");
    assert!(
        out.contains("hello-sentinel-t4d"),
        "the tty-less run never reached the dry-run model:\n{out}"
    );
    assert!(
        !out.contains(INIT_RAG_QUESTION) && !out.contains("starting without"),
        "the tty-less run printed the question or a notice instead of logging the skip:\n{out}"
    );
    let skips = rag_skip_lines(&log, "docs-agent-t4d");
    assert_eq!(
        skips.len(),
        1,
        "expected one debug line recording the skipped rag of the tty-less run, got {skips:?}:\n{out}"
    );
    assert!(
        skips[0].contains("starting without it"),
        "the debug line does not say the agent started without its rag: {skips:?}"
    );
    let written = new_files_named(&dir, &files_before, "/rags/");
    assert!(
        written.is_empty(),
        "the tty-less run built a rag without asking: {written:?}:\n{out}"
    );

    // A terminal, but headless: no human seated to ask.
    let dir = fresh_config_dir("rag-skip-debug-headless");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    write_agent_with_documents(&dir, "docs-agent-t4e");
    let files_before = walk(&dir);
    let log = dir.join("probe.log");
    let log_path = log.to_str().unwrap();
    let mut pty = spawn_tui_under(
        &dir,
        &[
            ("COYOTE_UI", "repl"),
            ("COYOTE_LOG_LEVEL", "debug"),
            ("COYOTE_LOG_PATH", log_path),
        ],
        "'{coyote}' --agent docs-agent-t4e --headless 'say hello-sentinel-t4e' <{stdin} & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}",
        "/dev/tty",
    );
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(
        exit_code(&transcript),
        0,
        "the headless agent run on a terminal did not exit 0:\n{transcript}"
    );
    assert!(
        transcript.contains("hello-sentinel-t4e"),
        "the headless run never reached the dry-run model:\n{transcript}"
    );
    assert!(
        !transcript.contains(INIT_RAG_QUESTION) && !transcript.contains("starting without"),
        "the headless run printed the question or a notice instead of logging the skip:\n{transcript}"
    );
    let skips = rag_skip_lines(&log, "docs-agent-t4e");
    assert_eq!(
        skips.len(),
        1,
        "expected one debug line recording the skipped rag of the headless run, got {skips:?}:\n{transcript}"
    );
    assert!(
        skips[0].contains("starting without it"),
        "the debug line does not say the agent started without its rag: {skips:?}"
    );
    let written = new_files_named(&dir, &files_before, "/rags/");
    assert!(
        written.is_empty(),
        "the headless run built a rag without asking: {written:?}:\n{transcript}"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "a headless run entered the alternate screen:\n{transcript}"
    );
    assert_tty_restored(&transcript);
}

/// An agent with two variables that have no default, started with no
/// terminal at all and nothing setting them: the run is not asked (so it
/// does not hang), reports every missing variable by name with its
/// description in one message, and exits non-zero.
#[test]
#[serial]
fn a_tty_less_agent_with_missing_required_variables_lists_them_instead_of_asking() {
    let dir = fresh_config_dir("ttyless-agent-variables");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "vars-agent-t4f";
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        format!(
            "name: {name}\n\
             description: an agent with two variables\n\
             version: 0.1.0\n\
             instructions: |\n\
             \x20 You are an assistant for {{{{username}}}} on {{{{project}}}}. Reply briefly.\n\
             variables:\n\
             \x20 - name: username\n\
             \x20   description: Your user name t4f-u\n\
             \x20 - name: project\n\
             \x20   description: The project t4f-p\n"
        ),
    )
    .unwrap();

    let (ok, out) = run_ttyless(
        &dir,
        &["--agent", name, "hello"],
        "tty-less agent with unset variables",
    );
    assert!(
        !ok,
        "the tty-less agent with unset variables exited 0:\n{out}"
    );
    assert!(
        out.contains("The following agent variables are required:"),
        "the run did not report the missing variables in one message:\n{out}"
    );
    for (variable, description) in [("username", "t4f-u"), ("project", "t4f-p")] {
        assert!(
            out.contains(&format!("- {variable}: ")) && out.contains(description),
            "the report does not list `{variable}` with its description:\n{out}"
        );
    }
    assert!(
        !out.contains("This field is required.") && !out.contains("Refusing to"),
        "the run tried to ask for a variable, or refused instead of listing them:\n{out}"
    );
    assert!(
        !out.contains("Reply briefly"),
        "the agent started without its variables:\n{out}"
    );
}

// ---------------------------------------------------------------------
// Pins that fit this half's budget: the piped-stdout update refusal, the
// re-asked agent variable in the plain REPL and the tty-less bare `.rag`.
// ---------------------------------------------------------------------

/// `--update` on a Cargo install with stdin on the terminal but stdout
/// redirected to a file (a user capturing the output): the confirm has
/// nowhere to paint, so the update is refused in the shared shape naming
/// `--force` (accepted plain-REPL delta), exits non-zero, never asks
/// `Update anyway?`, and the binary is untouched.
#[test]
#[serial]
fn update_with_a_piped_stdout_and_a_tty_on_stdin_is_refused_in_the_shared_shape() {
    let dir = fresh_config_dir("update-piped-stdout");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let cargo_copy = cargo_installed_binary(&dir);
    let binary_before = fs::metadata(&cargo_copy).unwrap();
    let captured = dir.join("update-stdout.txt");

    let mut pty = spawn_binary_under(
        &cargo_copy,
        &dir,
        &[("COYOTE_UI", "repl")],
        WAIT_WRAPPER,
        "/dev/tty",
        &format!(" --update >'{}'", captured.display()),
    );
    let on_tty = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    let on_stdout = fs::read_to_string(&captured).unwrap_or_default();
    let everything = format!("--- tty (stderr) ---\n{on_tty}\n--- stdout file ---\n{on_stdout}");

    assert_ne!(exit_code(&on_tty), 0, "{everything}");
    let (action, remedy) = refusal_shape(&everything).unwrap_or_else(|| {
        panic!("the piped-stdout update is not refused in the shared shape:\n{everything}")
    });
    assert_eq!(action, "update a Cargo install", "{everything}");
    assert!(
        remedy.contains("--force"),
        "the refusal's remedy does not name `--force`:\n{everything}"
    );
    assert!(
        !everything.contains("Update anyway?"),
        "the confirm was asked with nowhere to paint it:\n{everything}"
    );
    assert!(
        !everything.contains("Re-run in the plain REPL"),
        "the refusal blames the UI, but no UI stood in the way:\n{everything}"
    );
    let binary_after = fs::metadata(&cargo_copy).unwrap();
    assert!(
        binary_after.len() == binary_before.len()
            && binary_after.modified().unwrap() == binary_before.modified().unwrap(),
        "the refused update touched the binary at {}",
        cargo_copy.display()
    );
    assert_tty_restored(&on_tty);
}

/// An agent whose `username` variable has no default, so starting it asks
/// for the value.
fn write_agent_with_a_required_variable(dir: &Path, name: &str) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        format!(
            "name: {name}\n\
             description: an agent with a variable\n\
             version: 0.1.0\n\
             instructions: |\n\
             \x20 You are an assistant for {{{{username}}}}. Reply briefly.\n\
             variables:\n\
             \x20 - name: username\n\
             \x20   description: Your user name\n"
        ),
    )
    .unwrap();
}

/// `.agent <name>` in the plain REPL for an agent whose variable has no
/// default: a blank Enter is re-asked with the reason as a sentence
/// (`This field is required.`) instead of being taken, the next answer
/// is taken, and `.info agent` carries it.
#[test]
#[serial]
fn dot_agent_needing_a_variable_in_the_plain_repl_reasks_a_blank_answer_with_the_reason() {
    let dir = fresh_config_dir("agent-variable-repl");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    write_agent_with_a_required_variable(&dir, "vars-agent-p3r");
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    pty.send(b".agent vars-agent-p3r\r").unwrap();
    pty.wait_for_after("username", before, PTY_DEADLINE)
        .expect("`.agent` with an unset variable did not ask for it in the plain REPL");
    pty.read_for(Duration::from_millis(500));

    pty.send(b"\r").unwrap();
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let reasked = loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        if since.contains("This field is required.") && since.matches("username").count() >= 2 {
            break since;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a blank required variable was not re-asked with `This field is required.`:\n{since}"
        );
    };
    assert!(
        !squeezed(&reasked).contains("config_file"),
        "the blank answer was taken and the agent started:\n{reasked}"
    );
    pty.read_for(Duration::from_millis(300));

    pty.send(b"typed-user-p3r\r").unwrap();
    // The prompt comes back carrying the agent's name.
    pty.wait_for_after("vars-agent-p3r>>", before, PTY_DEADLINE)
        .expect("the prompt did not come back after the variable was answered");
    pty.read_for(Duration::from_millis(300));
    let info_at = pty.raw().len();
    pty.send(b".info agent\r").unwrap();
    pty.wait_for_after("config_file", info_at, PTY_DEADLINE)
        .expect("`.info agent` never rendered after the variable was answered");
    pty.read_for(Duration::from_millis(800));
    let info = squeezed(&strip_ansi(&String::from_utf8_lossy(&pty.raw()[info_at..])));
    assert!(
        info.contains("username:typed-user-p3r"),
        "`.info agent` does not carry the value typed after the re-ask:\n{info}"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{info}"
    );

    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A bare `.rag` with no terminal, while a previous temp rag is on disk:
/// the step is refused in the shared shape naming `temp` BEFORE the temp
/// rag is removed, so the file and its contents are still there
/// afterwards, nothing new is written and the macro does not go on.
#[test]
#[serial]
fn a_tty_less_bare_dot_rag_is_refused_before_the_temp_rag_is_removed() {
    let dir = fresh_config_dir("ttyless-rag-temp");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    fs::create_dir_all(dir.join("macros")).unwrap();
    fs::write(
        dir.join("macros").join("bare-rag.yaml"),
        "isolated: false\nsteps:\n  - \".rag\"\n  - \".info\"\n",
    )
    .unwrap();
    let rags = dir.join("rags");
    fs::create_dir_all(&rags).unwrap();
    let temp_rag = rags.join("temp.yaml");
    let seeded = "embedding_model: dryrun:dry-embed-a\nchunk_size: 1000\nchunk_overlap: 50\ndocument_paths: []\nfiles: []\nmarker: temp-rag-seeded-p3t\n";
    fs::write(&temp_rag, seeded).unwrap();
    let files_before = walk(&dir);

    let (ok, out) = run_ttyless(&dir, &["--macro", "bare-rag"], "tty-less bare .rag");
    assert!(!ok, "a refused tty-less bare `.rag` exited 0:\n{out}");
    let (action, _remedy) = refusal_shape(&out).unwrap_or_else(|| {
        panic!("the refusal is not `Refusing to <action> non-interactively. Re-run in a terminal...`:\n{out}")
    });
    assert!(
        action.contains("'temp'"),
        "the refusal does not name the temp rag it would have initialized: {action:?}\n{out}"
    );
    assert_eq!(
        fs::read_to_string(&temp_rag).ok().as_deref(),
        Some(seeded),
        "the refused tty-less `.rag` removed or rewrote the temp rag first:\n{out}"
    );
    // The first run seeds the built-in assets; only the rag directory is
    // what the refused step could have written to.
    let written = new_files_named(&dir, &files_before, "/rags/");
    assert!(
        written.is_empty(),
        "the refused tty-less `.rag` still wrote {written:?}:\n{out}"
    );
    assert!(
        !out.contains("log_path"),
        "the macro went on to `.info` after the refused `.rag`:\n{out}"
    );
}

/// Agents started with a terminal on stdout but stdin redirected from
/// `/dev/null` (a pipeline feeding the command): a graph agent whose
/// `rag` node is not fully specified is refused in the shared shape
/// naming the node, the agent and the three fields, exactly as with no
/// terminal at all, with nothing written; and an agent with documents
/// skips its optional knowledge base silently (no question painted, one
/// `[DEBUG]` skip line, exit 0, the model reached). Neither run touches
/// the controlling terminal: the piped stdin is what decides.
#[test]
#[serial]
fn agents_with_a_tty_on_stdout_but_a_piped_stdin_refuse_the_unbuilt_rag_node_and_skip_the_optional_rag()
 {
    let dir = fresh_config_dir("agents-piped-stdin");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let graph = "graph-p9r";
    let agent_dir = write_graph_agent_with_an_unbuilt_rag_node(&dir, graph);
    write_agent_with_documents(&dir, "docs-p9r");
    let files_before = walk(&dir);

    let mut pty = spawn_binary_under(
        built_binary(),
        &dir,
        &[("COYOTE_UI", "repl")],
        WAIT_WRAPPER,
        "/dev/null",
        &format!(" --agent {graph} hi"),
    );
    let plain = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_ne!(exit_code(&plain), 0, "{plain}");
    let (action, remedy) = refusal_shape(&plain).unwrap_or_else(|| {
        panic!("the piped-stdin graph agent is not refused in the shared shape:\n{plain}")
    });
    assert!(
        action.contains("'retrieve'") && action.contains(graph),
        "the refusal does not name the rag node and its agent: {action:?}\n{plain}"
    );
    for field in ["embedding_model", "chunk_size", "chunk_overlap"] {
        assert!(
            remedy.contains(field),
            "the remedy does not name `{field}`: {remedy:?}\n{plain}"
        );
    }
    assert!(
        !plain.contains("Select") && !plain.contains("? "),
        "a widget was painted with nothing to read an answer from:\n{plain}"
    );
    assert!(
        !plain.contains("Load ") && !plain.contains("Initializing RAG"),
        "the refused run went on to load documents:\n{plain}"
    );
    assert_tty_restored(&plain);
    let written: Vec<PathBuf> = walk(&dir)
        .into_iter()
        .filter(|path| !files_before.contains(path))
        .filter(|path| path.starts_with(&agent_dir) || path.to_string_lossy().contains("/rags/"))
        .collect();
    assert!(
        written.is_empty(),
        "the refused run wrote under the agent or rags: {written:?}\n{plain}"
    );

    let log = dir.join("probe.log");
    let log_path = log.to_str().unwrap();
    let mut pty = spawn_binary_under(
        built_binary(),
        &dir,
        &[
            ("COYOTE_UI", "repl"),
            ("COYOTE_LOG_LEVEL", "debug"),
            ("COYOTE_LOG_PATH", log_path),
        ],
        WAIT_WRAPPER,
        "/dev/null",
        " --agent docs-p9r 'say hello-sentinel-p9r'",
    );
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(
        exit_code(&transcript),
        0,
        "the piped-stdin agent with documents did not exit 0:\n{transcript}"
    );
    assert!(
        transcript.contains("hello-sentinel-p9r"),
        "the run never reached the dry-run model:\n{transcript}"
    );
    assert!(
        !transcript.contains(INIT_RAG_QUESTION) && !transcript.contains("starting without"),
        "the run painted the question or a notice instead of logging the skip:\n{transcript}"
    );
    let skips = rag_skip_lines(&log, "docs-p9r");
    assert_eq!(
        skips.len(),
        1,
        "expected one debug line recording the skipped rag, got {skips:?}:\n{transcript}"
    );
    assert!(
        skips[0].contains("no terminal") && skips[0].contains("starting without it"),
        "the debug line does not record a terminal-less skip: {skips:?}"
    );
    let written = new_files_named(&dir, &files_before, "/rags/");
    assert!(
        written.is_empty(),
        "the run built a rag without asking: {written:?}\n{transcript}"
    );
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------
// A bundled-category reinstall with no terminal to ask on.
// ---------------------------------------------------------------------

/// `--install-builtins macros` with no terminal at all is refused in the
/// shared shape, naming `--yes` as the remedy, and overwrites nothing:
/// every bundled macro is seeded with a local edit first (so the
/// startup install, which only adds missing files, leaves the directory
/// alone) and each edit survives the refused run.
#[test]
#[serial]
fn a_tty_less_install_builtins_is_refused_in_the_shared_shape_and_touches_no_file() {
    let dir = fresh_config_dir("ttyless-install-builtins");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let macros_dir = dir.join("macros");
    fs::create_dir_all(&macros_dir).unwrap();
    let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/macros");
    for entry in fs::read_dir(&bundled).unwrap() {
        let name = entry.unwrap().file_name();
        fs::write(macros_dir.join(name), "# local edit\n").unwrap();
    }
    let files_before = walk(&macros_dir);
    assert!(!files_before.is_empty(), "no bundled macros to seed");

    let (ok, out) = run_ttyless(
        &dir,
        &["--install-builtins", "macros"],
        "tty-less --install-builtins macros",
    );
    assert!(!ok, "the tty-less builtin reinstall exited 0:\n{out}");
    let (action, remedy) = refusal_shape(&out).unwrap_or_else(|| {
        panic!("the refusal is not `Refusing to <action> non-interactively. Re-run in a terminal...`:\n{out}")
    });
    assert!(
        action.starts_with("reinstall bundled macros"),
        "the refusal does not name the reinstall: {action:?}\n{out}"
    );
    assert!(
        remedy.contains("--yes"),
        "the remedy does not name `--yes`: {remedy:?}\n{out}"
    );
    assert!(
        !out.contains("Reinstalled") && !out.contains("Continue?"),
        "the refused run asked or reinstalled anyway:\n{out}"
    );
    assert_eq!(
        walk(&macros_dir),
        files_before,
        "the refused run changed the macros directory:\n{out}"
    );
    for path in &files_before {
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "# local edit\n",
            "{} was overwritten by the refused run:\n{out}",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Prompter-routed command menus: the answer a modal (or, in the plain REPL,
// the inquire prompt) collects reaches the command that asked, under both
// shells, from a clean config dir.
// ---------------------------------------------------------------------------

/// `.vault add <name>` under the UI, answered: the Password modal takes
/// the value, Enter sends it, the vault reports the secret added, the
/// typed value is never painted on the screen (it is a password), and
/// `.vault get <name>` afterwards prints exactly the value that was typed
/// into the modal. Then the UI keeps taking keys and leaves clean.
#[test]
#[serial]
fn dot_vault_add_answered_in_the_password_modal_stores_the_value_without_echoing_it() {
    let dir = fresh_config_dir("vault-add-modal-answered");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    secure_vault_password_file(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b".vault add vault-secret-p7k\r").unwrap();
    wait_for_the_modal(&mut pty, before, ".vault add", &["Enterthesecretvalue:"]);
    answer_field_modal(&mut pty, "hush-value-p7k");
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let added = loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        if squeezed(&since).contains("Secret'vault-secret-p7k'addedtothevault") {
            break since;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the answered Password modal did not add the secret:\n{since}"
        );
    };
    assert!(
        !added.contains("hush-value-p7k"),
        "the secret value typed into the Password modal was painted on the screen:\n{added}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the Password modal released the terminal:\n{added}"
    );

    let get_at = pty.raw().len();
    pty.send(b".vault get vault-secret-p7k\r").unwrap();
    pty.wait_for_after("hush-value-p7k", get_at, PTY_DEADLINE)
        .expect("`.vault get` did not print the value the modal collected");

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after the vault add");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// The same `.vault add` in the plain REPL: the question is the inquire
/// password prompt, asked once (no second "type it again" entry); an
/// empty answer re-asks with `A response is required.` instead of storing
/// an empty secret; the typed value is masked; and `.vault get` prints
/// the value back. `.exit` then leaves the tty cooked.
#[test]
#[serial]
fn dot_vault_add_in_the_plain_repl_asks_once_reasks_on_empty_and_stores_the_value() {
    let dir = fresh_config_dir("vault-add-repl");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    secure_vault_password_file(&dir);
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    pty.send(b".vault add vault-secret-r2p\r").unwrap();
    pty.wait_for_after("Enter the secret value:", before, PTY_DEADLINE)
        .expect("`.vault add` in the plain REPL did not ask for the value");
    pty.read_for(Duration::from_millis(300));

    // An empty answer is re-asked, not stored.
    pty.send(b"\r").unwrap();
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let reasked = loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        if since.contains("is required") && since.matches("Enter the secret value:").count() >= 2 {
            break since;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "an empty secret value was not re-asked as required:\n{since}"
        );
    };
    assert!(
        !squeezed(&reasked).contains("addedtothevault"),
        "an empty secret value was stored:\n{reasked}"
    );
    pty.read_for(Duration::from_millis(300));

    pty.send(b"hush-value-r2p\r").unwrap();
    wait_for_squeezed(
        &mut pty,
        "Secret 'vault-secret-r2p' added to the vault",
        "the plain REPL's `.vault add` confirmation",
    );
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    assert!(
        !since.contains("hush-value-r2p"),
        "the secret value typed at the password prompt was echoed:\n{since}"
    );
    let lower = since.to_lowercase();
    assert!(
        !["again", "confirm", "re-enter", "repeat", "retype"]
            .iter()
            .any(|w| lower.contains(w)),
        "the password was asked for a second confirmation entry; a single entry is the \
         contract:\n{since}"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{since}"
    );

    pty.wait_for_after(
        "dry-model)>",
        pty.raw().len().saturating_sub(64),
        PTY_DEADLINE,
    )
    .expect("the prompt did not come back after the secret was added");
    pty.read_for(Duration::from_millis(300));
    let get_at = pty.raw().len();
    pty.send(b".vault get vault-secret-r2p\r").unwrap();
    pty.wait_for_after("hush-value-r2p", get_at, PTY_DEADLINE)
        .expect("`.vault get` did not print the value the prompt collected");

    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------------
// `Agent::init`'s "init RAG?" question. An agent with
// `documents:` attached asks whether to build its knowledge base when it
// starts; under the UI the question is the Confirm modal, in the plain
// REPL it is the inquire confirm it always was, and without a terminal
// the agent starts without RAG, silently.

/// The notice the UI used to print instead of asking, gone now that the
/// question is a modal.
const RETIRED_RAG_NOTICE: &str = "starting without RAG";

/// Starts the UI on an agent-with-documents config and sends `.agent
/// <name>`; hands back the pty and the byte offset the command was sent
/// at once the Confirm modal asking `init RAG?` is on screen.
fn open_the_init_rag_modal(dir: &Path, name: &str) -> (Pty, usize) {
    write_dry_run_config_with_embeddings(dir);
    write_agent_with_documents(dir, name);
    let mut pty = spawn_tui(dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 120).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    let command = format!(".agent {name}");
    pty.send(format!("{command}\r").as_bytes()).unwrap();
    let modal = wait_for_the_modal(
        &mut pty,
        before,
        &command,
        &[&squeezed(INIT_RAG_QUESTION), "y=yes"],
    );
    assert!(
        !squeezed(&modal).contains("(Y/n)") && !squeezed(&modal).contains("(y/N)"),
        "`init RAG?` was asked on the raw tty, not in the modal:\n{modal}"
    );
    assert!(
        !modal.contains(RETIRED_RAG_NOTICE),
        "the retired `{RETIRED_RAG_NOTICE}` notice was printed under the UI:\n{modal}"
    );
    (pty, before)
}

/// `.agent <name>` for an agent with documents asks `init RAG?` in the
/// Confirm modal (no raw `(Y/n)` prompt, no retired notice, terminal
/// never released); `n` starts the agent without RAG: `.info agent`
/// renders for it, no rag is written, and the UI leaves clean on Ctrl+D.
#[test]
#[serial]
fn dot_agent_with_documents_asks_init_rag_in_the_confirm_modal_and_n_starts_it_without_rag() {
    let dir = fresh_config_dir("agent-init-rag-n");
    let _guard = TempDirGuard(dir.clone());
    let (mut pty, before) = open_the_init_rag_modal(&dir, "docs-agent-n7q");
    let files_before = walk(&dir);

    pty.read_for(HOTKEY_GAP);
    pty.send(b"n").unwrap();
    pty.read_for(Duration::from_millis(1500));

    // The export runs to well over a hundred rows (the built-in tool
    // guidance is appended to the instructions), so the window is made
    // tall enough to show it whole.
    pty.resize(300, 120).unwrap();
    pty.read_for(Duration::from_millis(800));
    let info_at = pty.raw().len();
    pty.send(b".info agent\r").unwrap();
    pty.wait_for_after("config_file", info_at, PTY_DEADLINE)
        .expect("`.info agent` never rendered after `n` on the init RAG modal");
    pty.read_for(Duration::from_millis(800));
    let info = strip_ansi(&String::from_utf8_lossy(&pty.raw()[info_at..]));
    let flat = squeezed(&info);
    assert!(
        flat.contains("name:docs-agent-n7q") && flat.contains("documents:-notes.md"),
        "`.info agent` does not show the agent with its documents after `n`:\n{info}"
    );
    assert!(
        !flat.contains("y=yes"),
        "the Confirm modal is still up after `n`:\n{info}"
    );
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    let written = new_files_named(&dir, &files_before, "/rags/");
    assert!(
        written.is_empty(),
        "`n` on `init RAG?` still wrote a rag: {written:?}:\n{since}"
    );
    assert!(
        !since.contains(RETIRED_RAG_NOTICE),
        "the retired `{RETIRED_RAG_NOTICE}` notice was printed after `n`:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the `init RAG?` question released the terminal:\n{since}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `y` on the `init RAG?` modal flows back as the answer: the init goes
/// on to the RAG wizard, whose first question (which embedding model) is
/// the Select modal listing both configured models; Esc there cancels,
/// the UI keeps taking keys and leaves clean.
#[test]
#[serial]
fn dot_agent_with_documents_y_on_the_init_rag_modal_moves_on_to_the_embedding_select_modal() {
    let dir = fresh_config_dir("agent-init-rag-y");
    let _guard = TempDirGuard(dir.clone());
    let (mut pty, before) = open_the_init_rag_modal(&dir, "docs-agent-y7q");

    pty.read_for(HOTKEY_GAP);
    let answered_at = pty.raw().len();
    pty.send(b"y").unwrap();
    let select = wait_for_the_modal(
        &mut pty,
        answered_at,
        ".agent docs-agent-y7q (after y)",
        &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
    );
    assert!(
        !squeezed(&select).contains("(Y/n)"),
        "the embedding question was asked on the raw tty:\n{select}"
    );
    esc_cancels_the_modal(&mut pty, ".agent docs-agent-y7q");

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("`.info` after the cancelled RAG wizard never rendered");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the RAG wizard released the terminal:\n{since}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `.agent <name>` in the plain REPL asks the same question with inquire
/// (`(Y/n)` on the normal screen, no modal legend, no alternate screen):
/// the prompter routing left the REPL's behaviour in place. `n` starts
/// the agent without RAG and nothing is written.
#[test]
#[serial]
fn dot_agent_with_documents_in_the_plain_repl_asks_init_rag_with_inquire() {
    let dir = fresh_config_dir("agent-init-rag-repl");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    write_agent_with_documents(&dir, "docs-agent-r7q");
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".agent docs-agent-r7q\r").unwrap();
    let asked = pty
        .wait_for_after(INIT_RAG_QUESTION, before, PTY_DEADLINE)
        .expect("`.agent <name>` in the plain REPL did not ask `init RAG?`");
    pty.read_for(Duration::from_millis(500));
    let asked = format!(
        "{asked}{}",
        strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]))
    );
    assert!(
        asked.contains("(Y/n)"),
        "the inquire confirm does not show its `(Y/n)` default:\n{asked}"
    );
    assert!(
        !squeezed(&asked).contains("Esc=cancel") && !squeezed(&asked).contains("y=yes"),
        "the plain REPL drew the modal legend instead of the inquire prompt:\n{asked}"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{asked}"
    );

    let answered_at = pty.raw().len();
    pty.send(b"n\r").unwrap();
    pty.wait_for_after("docs-agent-r7q", answered_at, PTY_DEADLINE)
        .expect("the agent prompt never came back after `n`");
    pty.read_for(Duration::from_millis(500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    let written = new_files_named(&dir, &files_before, "/rags/");
    assert!(
        written.is_empty(),
        "`n` on `init RAG?` in the plain REPL still wrote a rag: {written:?}:\n{since}"
    );

    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// Without a terminal (`coyote --agent <name> --headless <prompt>` with
/// stdin at /dev/null and stdout a file), the agent starts without RAG
/// and says nothing about it: no question, no retired notice, the dry-run
/// reply lands in the file, exit 0, no rag written and no alternate
/// screen.
#[test]
#[serial]
fn headless_agent_with_documents_starts_without_rag_silently() {
    let dir = fresh_config_dir("agent-init-rag-headless");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    write_agent_with_documents(&dir, "docs-agent-h7q");
    let files_before = walk(&dir);
    let captured = dir.join("stdout.log");
    let wrapper = format!(
        "'{{coyote}}' --agent docs-agent-h7q --headless 'say hello-sentinel-h7q' <{{stdin}} >'{}' & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {{report}}",
        captured.display()
    );
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], &wrapper, "/dev/null");

    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    let file = String::from_utf8_lossy(&fs::read(&captured).unwrap_or_default()).to_string();
    let everything = format!("{transcript}\n{file}");
    assert_eq!(
        exit_code(&transcript),
        0,
        "the headless agent run did not exit 0:\n{everything}"
    );
    assert!(
        file.contains("hello-sentinel-h7q"),
        "the headless run never reached the dry-run model:\n{everything}"
    );
    assert!(
        !everything.contains(INIT_RAG_QUESTION),
        "the headless run asked `init RAG?` without a terminal:\n{everything}"
    );
    assert!(
        !everything.contains(RETIRED_RAG_NOTICE),
        "the headless run printed the retired `{RETIRED_RAG_NOTICE}` notice:\n{everything}"
    );
    let written = new_files_named(&dir, &files_before, "/rags/");
    assert!(
        written.is_empty(),
        "the headless run built a rag without asking: {written:?}:\n{everything}"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "a headless run entered the alternate screen:\n{transcript}"
    );
    assert_tty_restored(&transcript);
}

/// A macro run with no terminal at all (`coyote --macro <name>` detached
/// from any controlling tty, stdin at /dev/null, stdout and stderr
/// pipes) reaching a rerouted question (`.vault add`, `.rag <new>`) is
/// refused the way the plain REPL always refused it: the run ends on its
/// own instead of hanging on a prompt, exits non-zero, says what is
/// missing in the shared refusal shape (naming the secret or the
/// knowledge base), writes nothing for the secret or the rag, and the
/// macro does not go on to its next step.
#[test]
#[serial]
fn a_tty_less_macro_step_that_would_ask_is_refused_and_writes_nothing() {
    for (scenario, step, footprints, action_prefix) in [
        (
            "vault-add",
            ".vault add probe-secret-m7q",
            &["m7q", "vault.yml"][..],
            "add secret 'probe-secret-m7q'",
        ),
        (
            "rag-create",
            ".rag probe-rag-m7q",
            &["m7q", "/rags/"][..],
            "initialize RAG knowledge base 'probe-rag-m7q'",
        ),
    ] {
        let dir = fresh_config_dir(&format!("ttyless-macro-{scenario}"));
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_embeddings(&dir);
        fs::create_dir_all(dir.join("macros")).unwrap();
        fs::write(
            dir.join("macros").join("asking.yaml"),
            format!("isolated: false\nsteps:\n  - \"{step}\"\n  - \".info\"\n"),
        )
        .unwrap();
        // Seed the built-in assets first so the file walk below sees only
        // what the refused step leaves behind.
        ttyless_command(built_binary(), &dir)
            .arg("--info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        let files_before = walk(&dir);

        let (ok, everything) = run_ttyless(
            &dir,
            &["--macro", "asking"],
            &format!("{scenario}: `--macro` with `{step}`"),
        );
        assert!(
            !ok,
            "{scenario}: a refused tty-less `{step}` exited 0:\n{everything}"
        );
        let (action, _remedy) = refusal_shape(&everything).unwrap_or_else(|| {
            panic!("{scenario}: the refusal of `{step}` is not `Refusing to <action> non-interactively. Re-run in a terminal...`:\n{everything}")
        });
        assert!(
            action.starts_with(action_prefix),
            "{scenario}: the refusal names the action {action:?}, not `{action_prefix}...`:\n{everything}"
        );
        for footprint in footprints {
            let written = new_files_named(&dir, &files_before, footprint);
            assert!(
                written.is_empty(),
                "{scenario}: the refused `{step}` still wrote {written:?}:\n{everything}"
            );
        }
        assert!(
            !everything.contains("log_path"),
            "{scenario}: the macro went on to `.info` after the refused `{step}`:\n{everything}"
        );
    }
}

/// Any question the wizard can ask after the embedding model; one of
/// them on screen means the chosen model reached the wizard and it
/// moved on.
const RAG_WIZARD_FOLLOW_UPS: [&str; 6] = [
    "RAGstoragedriver:",
    "Extractormodel",
    "Setchunksize:",
    "Setchunkoverlay:",
    "Setgraphexpansionhops:",
    "Adddocuments:",
];

/// `.rag <new>` under the UI, answered: Enter on the Select modal picks
/// the highlighted embedding model and the wizard moves on to its next
/// question in a fresh modal (the answer flowed back); the modals are
/// walked with Enter until `Set chunk size:` comes up, whose Input modal
/// shows the default `1000` pre-filled; Esc there leaves no rag behind
/// and the terminal was never released. The UI then keeps taking keys
/// and leaves clean.
#[test]
#[serial]
fn dot_rag_wizard_moves_on_after_the_select_modal_and_the_chunk_size_input_shows_its_default() {
    let dir = fresh_config_dir("rag-wizard-answered");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".rag new-rag-p7k\r").unwrap();
    wait_for_the_modal(
        &mut pty,
        before,
        ".rag new-rag-p7k",
        &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"\r").unwrap();
    let mut frame = repainted_frame(&mut pty, 50, 120);
    assert!(
        !squeezed(&frame).contains("Selectembeddingmodel:"),
        "Enter on the Select modal did not take the highlighted embedding model:\n{frame}"
    );
    assert!(
        RAG_WIZARD_FOLLOW_UPS
            .iter()
            .any(|q| squeezed(&frame).contains(q))
            && squeezed(&frame).contains("Esc=cancel"),
        "the wizard did not move on to its next question in a modal after the model was chosen:\n{frame}"
    );

    // Walk the wizard with Enter (the highlighted choice, or an empty /
    // default answer) until the chunk-size question comes up.
    let mut steps = 0;
    let mut rows = 50;
    while !squeezed(&frame).contains("Setchunksize:") {
        assert!(
            !squeezed(&frame).contains("Adddocuments:"),
            "the wizard reached `Add documents:` without asking the chunk size:\n{frame}"
        );
        assert!(
            steps < 4,
            "the wizard did not reach `Set chunk size:` within 4 answers:\n{frame}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        rows = if rows == 50 { 48 } else { 50 };
        frame = repainted_frame(&mut pty, rows, 120);
        steps += 1;
    }
    assert!(
        squeezed(&frame).contains("Setchunksize:1000") && squeezed(&frame).contains("Esc=cancel"),
        "the `Set chunk size:` Input modal does not show its default 1000:\n{frame}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "a wizard question released the terminal:\n{frame}"
    );

    esc_cancels_the_modal(&mut pty, ".rag new-rag-p7k");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    for footprint in ["p7k", "/rags/"] {
        let written = new_files_named(&dir, &files_before, footprint);
        assert!(
            written.is_empty(),
            "the cancelled wizard still wrote {written:?}:\n{since}"
        );
    }

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after the cancelled wizard");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    for footprint in ["p7k", "/rags/"] {
        let written = new_files_named(&dir, &files_before, footprint);
        assert!(
            written.is_empty(),
            "{written:?} appeared by the time the UI left:\n{transcript}"
        );
    }
}

/// `.rag <new>` in the plain REPL from the same clean config: the
/// embedding-model question is the inquire select on the normal screen
/// (no alternate screen, no modal legend), Esc cancels it, and no rag is
/// written. Pins that the routing through the prompter left the REPL's
/// inquire behaviour in place.
#[test]
#[serial]
fn dot_rag_for_a_new_rag_in_the_plain_repl_asks_with_inquire_and_esc_writes_nothing() {
    let dir = fresh_config_dir("rag-create-repl");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".rag new-rag-r2p\r").unwrap();
    let asked = pty
        .wait_for_after("Select embedding model:", before, PTY_DEADLINE)
        .expect("`.rag <new>` in the plain REPL did not ask for the embedding model");
    pty.read_for(Duration::from_millis(500));
    let asked = format!(
        "{asked}{}",
        strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]))
    );
    assert!(
        asked.contains("dry-embed-a") && asked.contains("dry-embed-b"),
        "the inquire select does not list both embedding models:\n{asked}"
    );
    assert!(
        !squeezed(&asked).contains("Esc=cancel"),
        "the plain REPL drew the modal legend instead of the inquire prompt:\n{asked}"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{asked}"
    );

    let esc_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    pty.wait_for_after("dry-model)>", esc_at, PTY_DEADLINE)
        .expect("Esc on the inquire select did not bring the prompt back");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    for footprint in ["r2p", "/rags/"] {
        let written = new_files_named(&dir, &files_before, footprint);
        assert!(
            written.is_empty(),
            "the cancelled `.rag` still wrote {written:?}:\n{since}"
        );
    }

    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------------
// A local file the bundle also ships, with different content: the
// `Conflict at <path>` menu under the UI, in the plain REPL, and with no
// terminal at all, next to the other no-tty refusals.
// ---------------------------------------------------------------------------

/// The content a bundle role is given locally before the install, so the
/// install finds a conflict at every role the bundle ships.
const LOCAL_ROLE_TEXT: &str =
    "---\nmodel: dryrun:dry-model\n---\nYou are the LOCAL role, edited by hand.\n";

/// Writes `LOCAL_ROLE_TEXT` under both role names `write_bundle_repo`
/// ships for `name`, and returns the two paths (sorted, the order the
/// install walks them in).
fn write_conflicting_local_roles(dir: &Path, name: &str) -> Vec<PathBuf> {
    let roles = dir.join("roles");
    fs::create_dir_all(&roles).unwrap();
    let mut paths: Vec<PathBuf> = [format!("{name}-role.md"), format!("{name}-obsolete.md")]
        .iter()
        .map(|file| roles.join(file))
        .collect();
    paths.sort();
    for path in &paths {
        fs::write(path, LOCAL_ROLE_TEXT).unwrap();
    }
    paths
}

/// The bundle's own text for the role at `path`, read from the bundle
/// repository so the test does not restate what `write_bundle_repo` wrote.
fn bundle_text_for(repo: &Path, path: &Path) -> String {
    fs::read_to_string(repo.join("roles").join(path.file_name().unwrap())).unwrap()
}

/// The `Conflict at <path>` Select menu rows, as the spec names them.
const CONFLICT_ROWS: [&str; 5] = ["keep", "replace", "keep-all", "replace-all", "abort"];

/// Which of `paths` the conflict modal painted in `modal` is about: the
/// only one whose file name appears in the squeezed frame (the path may
/// wrap); `modal` must be a frame painted before any conflict was
/// answered, or the echoed answer names a second file.
fn conflicting_file_named(modal: &str, paths: &[PathBuf]) -> PathBuf {
    let flat = squeezed(modal);
    let mut named = paths
        .iter()
        .filter(|path| flat.contains(path.file_name().unwrap().to_str().unwrap()));
    let first = named
        .next()
        .unwrap_or_else(|| panic!("the conflict modal names none of {paths:?}:\n{modal}"));
    assert!(
        named.next().is_none(),
        "the conflict modal names more than one file at once:\n{modal}"
    );
    first.clone()
}

/// The file of `paths` that is not `answered`: the one the next conflict
/// question must be about.
fn the_other_file(paths: &[PathBuf], answered: &Path) -> PathBuf {
    paths
        .iter()
        .find(|path| path.as_path() != answered)
        .unwrap()
        .clone()
}

/// `.install <local bundle>` whose two roles already exist locally with
/// other content, under the UI: each conflict is a closed Select modal
/// (`Conflict at <path> (category: roles)`, rows keep / replace /
/// keep-all / replace-all / abort, no custom row) and the digit answers
/// flow back one by one: `1` keeps the first file (its local text
/// stays), `2` replaces the second (the bundle's text lands), the report
/// says `1 replaced, 1 kept`, the terminal is never released, and the UI
/// keeps taking keys and leaves clean.
#[test]
#[serial]
fn dot_install_with_conflicting_local_files_asks_each_in_a_closed_select_modal_and_the_digits_decide()
 {
    let dir = fresh_config_dir("install-file-conflict-tui");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-c3f";
    let repo = write_bundle_repo(&dir, name, &[]);
    let locals = write_conflicting_local_roles(&dir, name);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(300));
    let install_at = pty.raw().len();
    pty.send(format!(".install {}\r", repo.display()).as_bytes())
        .unwrap();

    // First conflict: `1` keeps.
    let first_modal = wait_for_the_modal(
        &mut pty,
        install_at,
        ".install (first conflict)",
        &[
            "Conflictat",
            "(category:roles)",
            "keep-all",
            "replace-all",
            "abort",
            "Enter=choose",
        ],
    );
    let first_flat = squeezed(&first_modal);
    for row in CONFLICT_ROWS {
        assert!(
            first_flat.contains(row),
            "the conflict modal lacks the `{row}` row:\n{first_modal}"
        );
    }
    assert!(
        !first_flat.contains("Other(custom)"),
        "the conflict select is closed but the modal offers a custom row:\n{first_modal}"
    );
    let kept = conflicting_file_named(&first_modal, &locals);
    pty.read_for(HOTKEY_GAP);
    let answered_first = pty.raw().len();
    pty.send(b"1").unwrap();
    // The next question differs from this one by a few cells; a resize
    // forces the full repaint that puts it on the pty whole.
    pty.read_for(Duration::from_millis(500));
    pty.resize(61, 140).unwrap();

    // Second conflict, about the other file: `2` replaces. The modal is
    // looked for by that file's name, so a late repaint of the first
    // question cannot stand in for it.
    let replaced = the_other_file(&locals, &kept);
    let replaced_name = replaced.file_name().unwrap().to_str().unwrap();
    wait_for_the_modal(
        &mut pty,
        answered_first,
        ".install (second conflict)",
        &[
            "Conflictat",
            replaced_name,
            "(category:roles)",
            "keep-all",
            "replace-all",
            "abort",
        ],
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(b"2").unwrap();

    wait_for_squeezed(
        &mut pty,
        "Installed:0new,1replaced,1kept,0identical.",
        "the install report after keep then replace",
    );
    pty.read_for(Duration::from_millis(500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
    assert_eq!(
        fs::read_to_string(&kept).ok().as_deref(),
        Some(LOCAL_ROLE_TEXT),
        "`1` (keep) on the conflict modal did not keep the local text of {}:\n{since}",
        kept.display()
    );
    assert_eq!(
        fs::read_to_string(&replaced).ok(),
        Some(bundle_text_for(&repo, &replaced)),
        "`2` (replace) on the conflict modal did not land the bundle's text in {}:\n{since}",
        replaced.display()
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "a conflict question released the terminal:\n{since}"
    );

    assert_ui_alive_then_quit(&mut pty, "the conflicting install");
}

/// The same install in the plain REPL: the question is the inquire
/// Select (its own `to move, enter to select` legend, no modal legend,
/// no custom row), Enter on the highlighted `keep` keeps the first file,
/// and the Down arrow then Enter picks `replace` for the second; the
/// report and both files agree. `.exit` leaves the tty cooked.
#[test]
#[serial]
fn dot_install_with_conflicting_local_files_in_the_plain_repl_asks_with_inquire() {
    let dir = fresh_config_dir("install-file-conflict-repl");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-c3r";
    let repo = write_bundle_repo(&dir, name, &[]);
    let locals = write_conflicting_local_roles(&dir, name);
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.resize(60, 200).unwrap();
    pty.read_for(Duration::from_millis(300));
    let install_at = pty.raw().len();
    pty.send(format!(".install {}\r", repo.display()).as_bytes())
        .unwrap();
    let first = pty
        .wait_for_after("to move, enter to select", install_at, PTY_DEADLINE)
        .expect("the first conflict was not asked with the inquire select");
    pty.read_for(Duration::from_millis(300));
    let first = format!(
        "{first}{}",
        strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]))
    );
    assert!(
        first.contains("Conflict at") && first.contains("(category: roles)"),
        "the inquire select does not carry the conflict question:\n{first}"
    );
    for row in CONFLICT_ROWS {
        assert!(
            first.contains(row),
            "the inquire select lacks the `{row}` row:\n{first}"
        );
    }
    assert!(
        !first.contains("Other (custom)"),
        "the closed conflict select offers a custom row in the plain REPL:\n{first}"
    );
    assert!(
        !squeezed(&first).contains("Esc=cancel"),
        "the plain REPL drew the UI's modal legend:\n{first}"
    );
    let kept = conflicting_file_named(&first, &locals);

    // Enter keeps the highlighted first row; the second question is
    // looked for by the other file's name.
    let replaced = the_other_file(&locals, &kept);
    let replaced_name = replaced.file_name().unwrap().to_str().unwrap();
    let answered_first = pty.raw().len();
    pty.send(b"\r").unwrap();
    pty.wait_for_after(replaced_name, answered_first, PTY_DEADLINE)
        .expect("the second conflict was not asked after the first was kept");
    pty.read_for(Duration::from_millis(300));
    let second = strip_ansi(&String::from_utf8_lossy(&pty.raw()[answered_first..]));
    assert!(
        second.contains("to move, enter to select"),
        "the second conflict is not an inquire select:\n{second}"
    );
    // Down, then Enter: `replace`.
    pty.send(b"\x1b[B").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.wait_for_after(
        "Installed: 0 new, 1 replaced, 1 kept, 0 identical.",
        install_at,
        PTY_DEADLINE,
    )
    .expect("the install report after keep then replace never printed");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
    assert_eq!(
        fs::read_to_string(&kept).ok().as_deref(),
        Some(LOCAL_ROLE_TEXT),
        "Enter on `keep` did not keep the local text of {}:\n{since}",
        kept.display()
    );
    assert_eq!(
        fs::read_to_string(&replaced).ok(),
        Some(bundle_text_for(&repo, &replaced)),
        "Down + Enter (`replace`) did not land the bundle's text in {}:\n{since}",
        replaced.display()
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the plain REPL entered the alternate screen:\n{since}"
    );

    // The install goes on to print its MCP merge summary; `.exit` waits
    // for the prompt to come back so it is typed at the prompt, not
    // into the drain of stale input.
    pty.wait_for_after("dry-model)>", answered_first, PTY_DEADLINE)
        .expect("the prompt did not come back after the install");
    pty.read_for(Duration::from_millis(300));
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// With no terminal, the three menus that still refuse do so in one
/// shape, each naming the flag that settles it: `--install` of a second
/// bundle at a conflicting local file (`--install-force`, the file keeps
/// its local text), `--update` on a Cargo install (`--force`), and
/// `--uninstall` of a bundle installed for real by a tty-less `--install`
/// with nothing to ask, without `--yes` (`--yes`); all exit non-zero.
/// `--add-secret` with no terminal is refused in the same shape, with
/// nothing but a terminal as the remedy, and writes no vault file.
#[test]
#[serial]
fn every_tty_less_refusal_has_one_shape_and_names_its_flag() {
    let dir = fresh_config_dir("ttyless-refusal-shape");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let name = "bundle-c3h";
    let repo = write_bundle_repo(&dir, name, &[]);
    let repo_arg = repo.to_str().unwrap();
    let (ok, out) = run_ttyless(&dir, &["--install", repo_arg], "tty-less install");
    let role = dir.join("roles").join(format!("{name}-role.md"));
    assert!(
        ok && role.exists(),
        "a tty-less `--install` of a bundle with nothing to ask did not proceed silently:\n{out}"
    );

    let conflicting = "bundle-c3h-conflict";
    let conflicting_repo = dir.join("bundle-conflict-src");
    write_bundle_repo_at(&conflicting_repo, conflicting, &[]);
    let locals = write_conflicting_local_roles(&dir, conflicting);
    let cargo_copy = cargo_installed_binary(&dir);

    let mut shapes = Vec::new();
    for (what, binary, args, flag, action_prefix) in [
        (
            "install at a conflicting file",
            built_binary().to_path_buf(),
            vec!["--install", conflicting_repo.to_str().unwrap()],
            "--install-force",
            "overwrite local file ",
        ),
        (
            "self-update of a Cargo install",
            cargo_copy.clone(),
            vec!["--update"],
            "--force",
            "update a Cargo install",
        ),
        (
            "uninstall without --yes",
            built_binary().to_path_buf(),
            vec!["--uninstall", name],
            "--yes",
            "uninstall bundle 'bundle-c3h'",
        ),
    ] {
        let (ok, out) = run_ttyless_binary(&binary, &dir, &args, what);
        assert!(!ok, "{what}: a refused tty-less run exited 0:\n{out}");
        let (action, remedy) = refusal_shape(&out).unwrap_or_else(|| {
            panic!("{what}: the refusal is not `Refusing to <action> non-interactively. Re-run in a terminal...`:\n{out}")
        });
        assert!(
            action.starts_with(action_prefix),
            "{what}: the refusal names the action {action:?}, not `{action_prefix}...`:\n{out}"
        );
        assert!(
            remedy.contains(flag),
            "{what}: the refusal's remedy does not name `{flag}`:\n{out}"
        );
        assert!(
            !out.contains("Re-run in the plain REPL"),
            "{what}: the refusal blames the UI, but no UI stood in the way:\n{out}"
        );
        shapes.push((what, action, remedy));
    }
    for path in &locals {
        assert_eq!(
            fs::read_to_string(path).ok().as_deref(),
            Some(LOCAL_ROLE_TEXT),
            "the refused tty-less install changed {}:\n{shapes:?}",
            path.display()
        );
    }
    assert!(
        role.exists(),
        "the refused tty-less uninstall still removed a bundle file:\n{shapes:?}"
    );

    let files_before = walk(&dir);
    let (ok, out) = run_ttyless(&dir, &["--add-secret", "ttyless-secret-c3h"], "add-secret");
    assert!(!ok, "a tty-less `--add-secret` exited 0:\n{out}");
    assert_eq!(
        refusal_shape(&out),
        Some(("add secret 'ttyless-secret-c3h'".into(), ".".into())),
        "the tty-less `--add-secret` refusal is not in the shared shape:\n{out}"
    );
    let written = new_files_named(&dir, &files_before, "vault");
    assert!(
        written.is_empty(),
        "the refused tty-less `--add-secret` still wrote {written:?}:\n{out}"
    );
}

/// `--update` on a Cargo install with a terminal on stdout but stdin
/// redirected from `/dev/null` (a pipeline feeding the command): the
/// inquire confirm could paint but never read, so the update is refused
/// in the shared shape naming `--force`, exits non-zero, and no widget
/// error reaches the terminal.
#[test]
#[serial]
fn update_with_a_tty_on_stdout_but_a_piped_stdin_is_refused_cleanly() {
    let dir = fresh_config_dir("update-piped-stdin");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let cargo_copy = cargo_installed_binary(&dir);

    let mut pty = spawn_binary_under(
        &cargo_copy,
        &dir,
        &[("COYOTE_UI", "repl")],
        WAIT_WRAPPER,
        "/dev/null",
        " --update",
    );
    let plain = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_ne!(exit_code(&plain), 0, "{plain}");
    let (action, remedy) = refusal_shape(&plain).unwrap_or_else(|| {
        panic!("the piped-stdin update is not refused in the shared shape:\n{plain}")
    });
    assert_eq!(action, "update a Cargo install", "{plain}");
    assert_eq!(remedy, " or pass --force.", "{plain}");
    assert!(
        !plain.contains("Update anyway?"),
        "the confirm was asked with nothing to read it from:\n{plain}"
    );
    assert!(
        !plain.to_lowercase().contains("raw mode") && !plain.contains("IO error"),
        "the widget failed on the piped stdin instead of the refusal running first:\n{plain}"
    );
    assert_tty_restored(&plain);
}

/// `--add-secret <name>` with a terminal on stdout but stdin redirected
/// from `/dev/null`: the Password prompt could paint but never read, so
/// the vault refuses in the shared shape with nothing but a terminal as
/// the remedy, exits non-zero, paints no prompt and writes no vault file.
#[test]
#[serial]
fn add_secret_with_a_tty_on_stdout_but_a_piped_stdin_is_refused_in_the_shared_shape() {
    let dir = fresh_config_dir("add-secret-piped-stdin");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    secure_vault_password_file(&dir);
    let files_before = walk(&dir);

    let mut pty = spawn_binary_under(
        built_binary(),
        &dir,
        &[("COYOTE_UI", "repl")],
        WAIT_WRAPPER,
        "/dev/null",
        " --add-secret piped-secret-k2v",
    );
    let plain = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_ne!(exit_code(&plain), 0, "{plain}");
    assert_eq!(
        refusal_shape(&plain),
        Some(("add secret 'piped-secret-k2v'".into(), ".".into())),
        "the piped-stdin `--add-secret` is not refused in the shared shape:\n{plain}"
    );
    assert!(
        !plain.contains("Enter the secret value") && !plain.contains("Secret?"),
        "the secret prompt was painted with nothing to read it from:\n{plain}"
    );
    for name in ["vault", "piped-secret-k2v"] {
        let written = new_files_named(&dir, &files_before, name);
        assert!(
            written.is_empty(),
            "the refused `--add-secret` still wrote {written:?}:\n{plain}"
        );
    }
    assert_tty_restored(&plain);
}

// ---------------------------------------------------------------------
// A terminal on stdout with stdin redirected from a pipe or /dev/null:
// the command-line menus and the plain REPL's wizards cannot read an
// answer, so they refuse in the shared shape before painting a widget,
// consume nothing from the pipe and change nothing on disk.
// ---------------------------------------------------------------------

/// A bash `{stdin}` slot carrying a would-be answer on a real pipe, so a
/// prompt that read it anyway would be answered. The leading space keeps
/// the wrapper's `<` from gluing onto `<(` as a heredoc.
fn piped_answer(text: &str) -> String {
    format!(" <(printf '{text}\\n')")
}

/// `--update-secret <name>` fed a value on a pipe, and `--install` of a
/// bundle whose roles conflict with local files, once from `/dev/null`
/// and once fed `replace-all` on a pipe, all with a terminal on stdout:
/// each refuses in the shared shape (the secret with nothing but a
/// terminal as the remedy, the install naming `--install-force`), exits
/// non-zero, paints neither the secret prompt nor a conflict row, leaves
/// the piped answer unconsumed (the local roles keep their text) and
/// writes no vault file.
#[test]
#[serial]
fn cli_menus_with_a_tty_on_stdout_but_a_piped_stdin_refuse_without_reading_the_pipe() {
    let dir = fresh_config_dir("cli-menus-piped-stdin");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    secure_vault_password_file(&dir);
    let name = "bundle-p7s";
    let repo = write_bundle_repo(&dir, name, &[]);
    let locals = write_conflicting_local_roles(&dir, name);
    let files_before = walk(&dir);

    let mut pty = spawn_binary_under(
        built_binary(),
        &dir,
        &[("COYOTE_UI", "repl")],
        WAIT_WRAPPER,
        &piped_answer("piped-value-p7s"),
        " --update-secret piped-secret-p7s",
    );
    let plain = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_ne!(exit_code(&plain), 0, "{plain}");
    assert_eq!(
        refusal_shape(&plain),
        Some(("update secret 'piped-secret-p7s'".into(), ".".into())),
        "the piped-stdin `--update-secret` is not refused in the shared shape:\n{plain}"
    );
    assert!(
        !plain.contains("Enter the secret value") && !plain.contains("Secret?"),
        "the secret prompt was painted with only a pipe to read it from:\n{plain}"
    );
    assert_tty_restored(&plain);

    for (what, stdin) in [
        ("install from /dev/null", "/dev/null".to_string()),
        ("install fed `replace-all`", piped_answer("replace-all")),
    ] {
        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[("COYOTE_UI", "repl")],
            WAIT_WRAPPER,
            &stdin,
            &format!(" --install {}", repo.display()),
        );
        let plain = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_ne!(exit_code(&plain), 0, "{what}:\n{plain}");
        let (action, remedy) = refusal_shape(&plain).unwrap_or_else(|| {
            panic!("{what}: the conflict is not refused in the shared shape:\n{plain}")
        });
        assert!(
            action.starts_with("overwrite local file "),
            "{what}: the refusal names the action {action:?}:\n{plain}"
        );
        assert!(
            remedy.contains("--install-force"),
            "{what}: the remedy does not name `--install-force`:\n{plain}"
        );
        for row in ["keep-all", "replace-all", "abort"] {
            assert!(
                !plain.contains(row),
                "{what}: the conflict menu row `{row}` was painted with nothing to read an answer from:\n{plain}"
            );
        }
        for path in &locals {
            assert_eq!(
                fs::read_to_string(path).ok().as_deref(),
                Some(LOCAL_ROLE_TEXT),
                "{what}: the refused install changed {}:\n{plain}",
                path.display()
            );
        }
        assert_tty_restored(&plain);
    }

    for needle in ["vault", "piped-secret-p7s"] {
        let written = new_files_named(&dir, &files_before, needle);
        assert!(
            written.is_empty(),
            "the refused secret entry still wrote {written:?}"
        );
    }
}

/// The plain REPL started on a terminal with stdin redirected from
/// `/dev/null` (its line editor still reads the terminal) and its
/// wizards typed in: bare `.rag`, `.vault add`, `.install` of a bundle
/// at conflicting local files and `.uninstall` of an installed bundle
/// each refuse in the shared shape, nothing is painted to answer, the
/// REPL keeps taking commands, and `.exit` leaves with the tty restored;
/// no rag, vault file or role changes, and the installed bundle stays.
#[test]
#[serial]
fn a_plain_repl_on_a_tty_with_stdin_from_dev_null_refuses_its_wizards_in_the_shared_shape() {
    let dir = fresh_config_dir("repl-wizards-stdin-null");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    secure_vault_password_file(&dir);
    let installed = "bundle-r7k";
    let installed_repo = write_bundle_repo(&dir, installed, &[]);
    let (ok, out) = run_ttyless(
        &dir,
        &["--install", installed_repo.to_str().unwrap()],
        "tty-less install of the bundle to uninstall",
    );
    let installed_role = dir.join("roles").join(format!("{installed}-role.md"));
    assert!(ok && installed_role.exists(), "{out}");
    let conflicting = "bundle-r7k-conflict";
    let conflicting_repo = dir.join("bundle-conflict-src");
    write_bundle_repo_at(&conflicting_repo, conflicting, &[]);
    let locals = write_conflicting_local_roles(&dir, conflicting);
    let files_before = walk(&dir);

    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/null");
    pty.wait_for(")> ", PTY_DEADLINE).unwrap();
    let mut shapes = Vec::new();
    for (command, action_prefix, remedy_needle) in [
        (".rag".to_string(), "initialize RAG knowledge base", "."),
        (
            ".vault add repl-secret-r7k".to_string(),
            "add secret 'repl-secret-r7k'",
            ".",
        ),
        (
            format!(".install {}", conflicting_repo.display()),
            "overwrite local file ",
            "--install-force",
        ),
        (
            format!(".uninstall {installed}"),
            "uninstall bundle 'bundle-r7k'",
            "--yes",
        ),
    ] {
        let from = pty.raw().len();
        pty.send(format!("{command}\r").as_bytes()).unwrap();
        let since = pty
            .wait_for_after("Re-run in a terminal", from, PTY_DEADLINE)
            .unwrap_or_else(|err| panic!("`{command}` did not refuse: {err}"));
        let (action, remedy) = refusal_shape(&since)
            .unwrap_or_else(|| panic!("`{command}` is not refused in the shared shape:\n{since}"));
        assert!(
            action.starts_with(action_prefix),
            "`{command}` names the action {action:?}, not `{action_prefix}...`:\n{since}"
        );
        assert!(
            remedy.contains(remedy_needle),
            "`{command}` remedy {remedy:?} does not name `{remedy_needle}`:\n{since}"
        );
        for widget in ["Enter the secret value", "keep-all", "Continue?", "? "] {
            assert!(
                !since.contains(widget),
                "`{command}` painted `{widget}` with nothing to read an answer from:\n{since}"
            );
        }
        shapes.push((command, action, remedy));
        pty.read_for(Duration::from_millis(300));
    }
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    for needle in ["/rags/", "vault", "repl-secret-r7k"] {
        let written = new_files_named(&dir, &files_before, needle);
        assert!(
            written.is_empty(),
            "a refused REPL wizard still wrote {written:?}:\n{shapes:?}"
        );
    }
    for path in &locals {
        assert_eq!(
            fs::read_to_string(path).ok().as_deref(),
            Some(LOCAL_ROLE_TEXT),
            "the refused `.install` changed {}:\n{shapes:?}",
            path.display()
        );
    }
    assert!(
        installed_role.exists(),
        "the refused `.uninstall` still removed a bundle file:\n{shapes:?}"
    );
}
