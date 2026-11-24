//! Real-process checks of the modals: the command menus, the setup
//! wizards, and a Confirm modal at the awkward corners (Ctrl-Z, SIGTERM,
//! a one-row window) next to an `$EDITOR` shell-out whose editor fails.
//! Every module drives the binary through the shared pty harness in
//! `common`, so the file is unix-only.
#![cfg(unix)]

mod common;

mod binary_menus {
    //! Real-process checks of the command menus the prompter routes: the
    //! questions `.vault`, `.rag`, `.agent`, `.install`, `.update` and
    //! `.session` raise become modals on a real tty and inquire prompts in
    //! the plain REPL, keys typed ahead of a confirm never settle it, a
    //! terminal on stdout with stdin from a pipe or /dev/null still asks
    //! (inquire reads the terminal), and a run with no human to ask (stdout
    //! not a terminal, a headless `--agent`) refuses in the shared shape,
    //! skips the optional knowledge base or lists the unset variables
    //! instead of hanging on a prompt; a cold start picks
    //! the plain REPL or the full-screen UI from the `ui:` key and `--tui` /
    //! `--repl`, never from `COYOTE_UI`. Each case runs the binary on a
    //! pseudo-terminal under a bash wrapper that reports the exit code and
    //! `stty -a` afterwards, so the module is unix-only. The cases run one at a time:
    //! each spawn forks a session leader and takes a controlling tty, and a
    //! fork landing between another test's `openpty` and its `exec` has
    //! failed the spawn with a garbled errno.
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::common::fake_llm::{Chunk, FakeLlm, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
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
        let mut pty = spawn_binary_under(&binary, &dir, &[], WAIT_WRAPPER, "/dev/tty", " --tui");

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

    /// A tool's `confirm` question in the plain REPL drops a `y` and Enter
    /// typed before it renders: the prompt and that answer go down the tty in
    /// one write, the question shows and stays open with nothing recorded,
    /// and Enter sent once it is on screen takes the `No` it starts on. With
    /// `stdin` a non-tty the keys still come from the terminal, so the stale
    /// ones are dropped the same way.
    fn assert_tool_confirm_drops_typeahead_and_enter_answers_no(scenario: &str, stdin: &str) {
        let dir = fresh_config_dir(scenario);
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
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, stdin);

        let asked_at = wait_for_repl_prompt(&mut pty, "fake-model)>", 0);
        pty.send(b"call the confirm tool\ry\r").unwrap();
        pty.wait_for_after("q9d-q", asked_at, PTY_DEADLINE)
            .expect("the tool's confirm question never reached the terminal");
        pty.read_for(Duration::from_millis(1500));
        let shown = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
        assert!(
            fs::read_to_string(&out).is_err(),
            "[{stdin}] the `y` and Enter typed ahead of the tool's confirm answered it with {:?}:\n{shown}",
            fs::read_to_string(&out)
        );
        assert_eq!(fake.served(), 1, "[{stdin}] {shown}");

        let answered = pty.raw().len();
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done t4a", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "0",
            "[{stdin}] Enter on the open tool confirm did not reach the tool as a no:\n{transcript}"
        );
        assert!(
            squeezed(&transcript).contains("q9d-qNo"),
            "[{stdin}] the confirm did not echo `No`:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");
        assert!(
            find(pty.raw(), ENTER_ALT, 0).is_none(),
            "the plain REPL entered the alternate screen:\n{transcript}"
        );

        wait_for_repl_prompt(&mut pty, "fake-model)>", answered);
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    #[test]
    #[serial]
    fn a_tools_confirm_question_in_the_plain_repl_drops_a_y_typed_ahead_of_it() {
        assert_tool_confirm_drops_typeahead_and_enter_answers_no(
            "tool-confirm-typeahead-repl",
            "/dev/tty",
        );
    }

    #[test]
    #[serial]
    fn a_tools_confirm_question_in_the_plain_repl_with_stdin_from_dev_null_drops_a_y_typed_ahead_of_it()
     {
        assert_tool_confirm_drops_typeahead_and_enter_answers_no(
            "tool-confirm-typeahead-repl-null-stdin",
            "/dev/null",
        );
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
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
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

        let answered = pty.raw().len();
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

        wait_for_repl_prompt(&mut pty, "dry-model)>", answered);
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
        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[("COYOTE_LOG_LEVEL", "debug"), ("COYOTE_LOG_PATH", log_path)],
            WAIT_WRAPPER,
            "/dev/tty",
            " --agent docs-agent-t4e --headless 'say hello-sentinel-t4e'",
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
    // The piped-stdout update refusal, the re-asked agent variable in the
    // plain REPL and the tty-less bare `.rag`.
    // ---------------------------------------------------------------------

    /// `--update` on a Cargo install with stdin on the terminal but stdout
    /// redirected to a file (a user capturing the output): the confirm has
    /// nowhere to paint, so the update is refused in the shared shape naming
    /// `--force`, exits non-zero, never asks `Update anyway?`, and the binary
    /// is untouched.
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
            &[],
            WAIT_WRAPPER,
            "/dev/tty",
            &format!(" --repl --update >'{}'", captured.display()),
        );
        let on_tty = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let on_stdout = fs::read_to_string(&captured).unwrap_or_default();
        let everything =
            format!("--- tty (stderr) ---\n{on_tty}\n--- stdout file ---\n{on_stdout}");

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
        write_agent_needing_a_variable(&dir, "vars-agent-p3r");
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        let before = wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
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

        let answered = pty.raw().len();
        pty.send(b"typed-user-p3r\r").unwrap();
        // The prompt comes back carrying the agent's name.
        let info_at = wait_for_repl_prompt(&mut pty, "vars-agent-p3r>>", answered);
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

        let shown = find(pty.raw(), b"config_file", info_at)
            .expect("`config_file` was seen in the stripped transcript but not in the raw bytes");
        wait_for_repl_prompt(&mut pty, "vars-agent-p3r>>", shown);
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
    /// `/dev/null` (a pipeline feeding the command) still ask on the
    /// terminal, which is where the answer is read from: a graph agent whose
    /// `rag` node is not fully specified asks whether to build it and `n`
    /// stops the run with nothing written; an agent with documents asks
    /// `init RAG?` and `n` starts it without one, reaching the model.
    #[test]
    #[serial]
    fn agents_with_a_tty_on_stdout_but_a_piped_stdin_ask_their_rag_questions_on_the_terminal() {
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
            &[],
            WAIT_WRAPPER,
            "/dev/null",
            &format!(" --agent {graph} hi"),
        );
        pty.wait_for(
            "Initialize RAG knowledge base for rag node 'retrieve'?",
            PTY_DEADLINE,
        )
        .expect("the piped-stdin graph agent did not ask to build its rag node");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"n\r").unwrap();
        let plain = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_ne!(exit_code(&plain), 0, "{plain}");
        assert!(
            squeezed(&plain).contains("RAGinitializationisrequiredforthisagent"),
            "`n` on the rag node question did not stop the graph agent:\n{plain}"
        );
        assert!(
            refusal_shape(&plain).is_none(),
            "the piped-stdin graph agent was refused instead of asked:\n{plain}"
        );
        assert_tty_restored(&plain);
        let written: Vec<PathBuf> = walk(&dir)
            .into_iter()
            .filter(|path| !files_before.contains(path))
            .filter(|path| {
                path.starts_with(&agent_dir) || path.to_string_lossy().contains("/rags/")
            })
            .collect();
        assert!(
            written.is_empty(),
            "the declined run wrote under the agent or rags: {written:?}\n{plain}"
        );

        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            WAIT_WRAPPER,
            "/dev/null",
            " --agent docs-p9r 'say hello-sentinel-p9r'",
        );
        pty.wait_for(INIT_RAG_QUESTION, PTY_DEADLINE)
            .expect("the piped-stdin agent with documents did not ask `init RAG?`");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"n\r").unwrap();
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
        let written = new_files_named(&dir, &files_before, "/rags/");
        assert!(
            written.is_empty(),
            "`n` on `init RAG?` still built a rag: {written:?}\n{transcript}"
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
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        let before = wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
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
            if since.contains("is required")
                && since.matches("Enter the secret value:").count() >= 2
            {
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

        let answered = pty.raw().len();
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

        let get_at = wait_for_repl_prompt(&mut pty, "dry-model)>", answered);
        pty.send(b".vault get vault-secret-r2p\r").unwrap();
        pty.wait_for_after("hush-value-r2p", get_at, PTY_DEADLINE)
            .expect("`.vault get` did not print the value the prompt collected");

        let got = find(pty.raw(), b"hush-value-r2p", get_at)
            .expect("the value was seen in the stripped transcript but not in the raw bytes");
        wait_for_repl_prompt(&mut pty, "dry-model)>", got);
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
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        pty.wait_for("dry-model)>", REPL_PROMPT_DEADLINE).unwrap();
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
        wait_for_repl_prompt(&mut pty, "dry-model)>", answered_first);
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

    /// With no terminal, `--install` of a bundle whose mcp.json references
    /// secrets the vault lacks is not a refusal: nobody can answer `Add
    /// {{ X }} to the vault now?`, so every secret is deferred the way
    /// declining each would, the summary names them under the deferred
    /// notice, the merged mcp.json and the bundle record land, the vault
    /// gains no file, and the run exits 0.
    #[test]
    #[serial]
    fn a_tty_less_install_of_a_bundle_with_secrets_defers_them_all_and_still_lands_the_bundle() {
        let dir = fresh_config_dir("ttyless-install-defers-secrets");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-c3s";
        let secrets = ["SECRET_A", "SECRET_B"];
        let repo = write_bundle_repo(&dir, name, &secrets);
        let files_before = walk(&dir);

        let (ok, out) = run_ttyless(
            &dir,
            &["--install", repo.to_str().unwrap()],
            "tty-less install of a bundle with secrets",
        );
        assert!(
            ok,
            "a tty-less `--install` of a bundle with missing secrets did not exit 0:\n{out}"
        );
        assert!(
            refusal_shape(&out).is_none() && !out.contains("to the vault now?"),
            "the tty-less install refused or asked instead of deferring the secrets:\n{out}"
        );
        let notice_at = out
            .find("still required by your MCP servers")
            .unwrap_or_else(|| {
                panic!("the install did not print the deferred-secrets notice:\n{out}")
            });
        for secret in secrets {
            assert!(
                out[notice_at..].contains(&format!("{{{{ {secret} }}}}")),
                "the deferred list does not name {secret}:\n{out}"
            );
        }

        let mcp_json = fs::read_to_string(dir.join("mcp.json"))
            .unwrap_or_else(|_| panic!("the merged mcp.json was not written:\n{out}"));
        assert!(
            mcp_json.contains("bundle-k7s-server") && secrets.iter().all(|s| mcp_json.contains(s)),
            "the merged mcp.json lacks the bundle's server or its secret references:\n{mcp_json}"
        );
        assert!(
            !new_files_named(&dir, &files_before, name).is_empty(),
            "no file records the installed bundle `{name}`:\n{out}"
        );
        assert!(
            dir.join("roles").join(format!("{name}-role.md")).exists(),
            "the bundle's role was not installed:\n{out}"
        );
        let vault_files = new_files_named(&dir, &files_before, "vault");
        assert!(
            vault_files.is_empty(),
            "deferring the secrets still wrote {vault_files:?}:\n{out}"
        );
    }

    // ---------------------------------------------------------------------
    // A terminal on stdout with stdin redirected from a pipe or /dev/null:
    // the command-line menus and the plain REPL's wizards still ask, on the
    // terminal, which is where inquire and the line editor read keys from.
    // Nothing is read from the pipe and no question is refused.
    // ---------------------------------------------------------------------

    /// A bash `{stdin}` slot carrying a would-be answer on a real pipe, so a
    /// prompt that read it would be answered. The leading space keeps the
    /// wrapper's `<` from gluing onto `<(` as a heredoc.
    fn piped_answer(text: &str) -> String {
        format!(" <(printf '{text}\\n')")
    }

    /// With a terminal on stdout and stdin from `/dev/null` or a pipe:
    /// `--update` on a Cargo install asks `Update anyway?` and `n` cancels
    /// it with exit 0; `--add-secret` asks for the value and Esc leaves the
    /// piped value unstored; `--install` fed `replace-all` on a pipe paints
    /// the conflict menu and Esc aborts with the local roles untouched.
    #[test]
    #[serial]
    fn cli_menus_with_a_tty_on_stdout_but_a_piped_stdin_ask_on_the_terminal() {
        let dir = fresh_config_dir("cli-menus-piped-stdin");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        secure_vault_password_file(&dir);
        let name = "bundle-p7s";
        let repo = write_bundle_repo(&dir, name, &[]);
        let locals = write_conflicting_local_roles(&dir, name);
        let cargo_copy = cargo_installed_binary(&dir);
        let files_before = walk(&dir);

        let mut pty = spawn_binary_under(
            &cargo_copy,
            &dir,
            &[],
            WAIT_WRAPPER,
            "/dev/null",
            " --repl --update",
        );
        pty.wait_for("Update anyway?", PTY_DEADLINE)
            .expect("the piped-stdin `--update` did not ask `Update anyway?`");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"n\r").unwrap();
        let plain = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&plain), 0, "{plain}");
        assert!(
            plain.contains("Update cancelled."),
            "`n` on `Update anyway?` did not cancel the update:\n{plain}"
        );
        assert!(
            refusal_shape(&plain).is_none(),
            "the piped-stdin `--update` was refused:\n{plain}"
        );
        assert_tty_restored(&plain);

        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            WAIT_WRAPPER,
            &piped_answer("piped-value-k2v"),
            " --add-secret piped-secret-k2v",
        );
        pty.wait_for("Enter the secret value:", PTY_DEADLINE)
            .expect("the piped-stdin `--add-secret` did not ask for the value");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"\x1b").unwrap();
        let plain = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert!(
            refusal_shape(&plain).is_none(),
            "the piped-stdin `--add-secret` was refused:\n{plain}"
        );
        assert!(
            !squeezed(&plain).contains("addedtothevault"),
            "Esc on the secret prompt still stored a value:\n{plain}"
        );
        assert_tty_restored(&plain);

        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            WAIT_WRAPPER,
            &piped_answer("replace-all"),
            &format!(" --install {}", repo.display()),
        );
        pty.wait_for("Conflict at", PTY_DEADLINE)
            .expect("the piped-stdin `--install` did not paint the conflict menu");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"\x1b").unwrap();
        let plain = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_ne!(
            exit_code(&plain),
            0,
            "Esc on the conflict menu did not abort the install:\n{plain}"
        );
        assert!(
            refusal_shape(&plain).is_none(),
            "the piped-stdin `--install` was refused:\n{plain}"
        );
        for path in &locals {
            assert_eq!(
                fs::read_to_string(path).ok().as_deref(),
                Some(LOCAL_ROLE_TEXT),
                "the piped `replace-all` or the aborted install changed {}:\n{plain}",
                path.display()
            );
        }
        assert_tty_restored(&plain);

        for needle in ["vault", "piped-secret-k2v"] {
            let written = new_files_named(&dir, &files_before, needle);
            assert!(
                written.is_empty(),
                "the cancelled secret entry still wrote {written:?}"
            );
        }
    }

    /// The plain REPL started on a terminal with stdin redirected from
    /// `/dev/null` asks its wizards on the terminal, where its line editor
    /// reads too: bare `.rag`, `.vault add`, `.install` of a bundle at
    /// conflicting local files and `.uninstall` of an installed bundle each
    /// paint their first question without a refusal, Esc closes it, the REPL
    /// keeps taking commands and `.exit` leaves with the tty restored; no
    /// rag, vault file or role changes, and the installed bundle stays.
    #[test]
    #[serial]
    fn a_plain_repl_on_a_tty_with_stdin_from_dev_null_asks_its_wizards_on_the_terminal() {
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

        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/null");
        pty.wait_for(")> ", REPL_PROMPT_DEADLINE).unwrap();
        let mut cancelled = pty.raw().len();
        pty.resize(60, 200).unwrap();
        for (command, question) in [
            (".rag".to_string(), "Select embedding model:".to_string()),
            (
                ".vault add repl-secret-r7k".to_string(),
                "Enter the secret value:".to_string(),
            ),
            (
                // The line editor repaints the prompt once per typed key,
                // about 100 ms each in a debug build, so the absolute temp
                // path alone would eat most of the deadline; the binary runs
                // in `dir`, where the relative path names the same repository.
                ".install ./bundle-conflict-src".to_string(),
                "Conflict at".to_string(),
            ),
            (
                format!(".uninstall {installed}"),
                format!("Uninstall bundle '{installed}'?"),
            ),
        ] {
            let from = wait_for_repl_prompt(&mut pty, ")> ", cancelled);
            pty.send(format!("{command}\r").as_bytes()).unwrap();
            pty.wait_for_after(&question, from, PTY_DEADLINE)
                .unwrap_or_else(|err| panic!("`{command}` did not ask `{question}`: {err}"));
            pty.read_for(Duration::from_millis(500));
            cancelled = pty.raw().len();
            pty.send(b"\x1b").unwrap();
            pty.read_for(Duration::from_millis(1000));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]));
            assert!(
                !since.contains("Re-run in a terminal"),
                "`{command}` was refused instead of asked:\n{since}"
            );
        }
        wait_for_repl_prompt(&mut pty, ")> ", cancelled);
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);

        for needle in ["/rags/", "vault", "repl-secret-r7k"] {
            let written = new_files_named(&dir, &files_before, needle);
            assert!(
                written.is_empty(),
                "a cancelled REPL wizard still wrote {written:?}:\n{transcript}"
            );
        }
        for path in &locals {
            assert_eq!(
                fs::read_to_string(path).ok().as_deref(),
                Some(LOCAL_ROLE_TEXT),
                "the cancelled `.install` changed {}:\n{transcript}",
                path.display()
            );
        }
        assert!(
            installed_role.exists(),
            "the cancelled `.uninstall` still removed a bundle file:\n{transcript}"
        );
    }

    // ---------------------------------------------------------------------
    // The no-tty runs of the reinstall with `--yes`, the MCP add that needs a
    // vault secret, and the very first run with no config file.
    // ---------------------------------------------------------------------

    /// `--install-builtins macros --yes` with no terminal at all proceeds
    /// without a question: exit 0, no refusal, no `Continue?`, and every
    /// locally edited bundled macro is back to the asset's bytes; the run
    /// says what it reinstalled.
    #[test]
    #[serial]
    fn a_tty_less_install_builtins_with_yes_reinstalls_every_bundled_file_without_asking() {
        let dir = fresh_config_dir("ttyless-install-builtins-yes");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let macros_dir = dir.join("macros");
        fs::create_dir_all(&macros_dir).unwrap();
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/macros");
        let mut expected = Vec::new();
        for entry in fs::read_dir(&bundled).unwrap() {
            let entry = entry.unwrap();
            fs::write(macros_dir.join(entry.file_name()), "# local edit\n").unwrap();
            expected.push((
                macros_dir.join(entry.file_name()),
                fs::read(entry.path()).unwrap(),
            ));
        }
        assert!(!expected.is_empty(), "no bundled macros to seed");

        let (ok, out) = run_ttyless(
            &dir,
            &["--install-builtins", "macros", "--yes"],
            "tty-less --install-builtins macros --yes",
        );
        assert!(
            ok,
            "`--install-builtins macros --yes` with no terminal failed:\n{out}"
        );
        assert!(
            refusal_shape(&out).is_none() && !out.contains("Continue?"),
            "`--yes` still refused or asked:\n{out}"
        );
        assert!(
            out.contains("Reinstalled bundled macros"),
            "the `--yes` run did not say what it reinstalled:\n{out}"
        );
        for (path, bytes) in &expected {
            assert_eq!(
                fs::read(path).ok().as_deref(),
                Some(bytes.as_slice()),
                "{} was not restored to the bundled asset by `--yes`:\n{out}",
                path.display()
            );
        }
    }

    /// `--mcp-add` of a server whose env references a vault secret the vault
    /// does not hold, with no terminal at all: refused in the shared shape
    /// before any `to the vault now?` question, the remedy naming
    /// `coyote --add-secret <NAME>`, and the server is not written. With a
    /// terminal on stdout but stdin from /dev/null the question is asked
    /// instead, and `n` aborts naming the same command. The same command
    /// without the placeholder succeeds, so the refusal is the secret's, not
    /// the vault's.
    #[test]
    #[serial]
    fn a_tty_less_mcp_add_needing_a_vault_secret_is_refused_naming_add_secret_before_any_question()
    {
        let dir = fresh_config_dir("ttyless-mcp-add-secret");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        secure_vault_password_file(&dir);

        let (ok, out) = run_ttyless(
            &dir,
            &[
                "--mcp-add",
                "probe-srv-m4c",
                "--env",
                "TOKEN={{ PROBE_MCP_TOKEN_M4C }}",
                "--",
                "echo",
                "hi",
            ],
            "tty-less --mcp-add with a vault placeholder",
        );
        assert!(
            !ok,
            "the tty-less `--mcp-add` needing a secret exited 0:\n{out}"
        );
        let (action, remedy) = refusal_shape(&out).unwrap_or_else(|| {
        panic!("the refusal is not `Refusing to <action> non-interactively. Re-run in a terminal...`:\n{out}")
    });
        assert!(
            action.contains("probe-srv-m4c") && action.contains("secret"),
            "the refusal does not name the server's secrets: {action:?}\n{out}"
        );
        assert!(
            remedy.contains("coyote --add-secret PROBE_MCP_TOKEN_M4C"),
            "the remedy does not name `coyote --add-secret PROBE_MCP_TOKEN_M4C`: {remedy:?}\n{out}"
        );
        assert!(
            !out.contains("to the vault now?") && !out.contains("Added MCP server"),
            "the refused run asked or added the server anyway:\n{out}"
        );
        let mentions = walk(&dir)
            .into_iter()
            .filter(|p| fs::read_to_string(p).is_ok_and(|s| s.contains("probe-srv-m4c")))
            .collect::<Vec<_>>();
        assert!(
            mentions.is_empty(),
            "the refused `--mcp-add` still wrote the server into {mentions:?}:\n{out}"
        );

        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            WAIT_WRAPPER,
            "/dev/null",
            " --repl --mcp-add probe-srv-m4d --env TOKEN={{PROBE_MCP_TOKEN_M4D}} -- echo hi",
        );
        pty.wait_for("to the vault now?", PTY_DEADLINE)
            .expect("the piped-stdin `--mcp-add` did not ask to add the secret");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"n\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let plain = strip_ansi(&transcript);
        assert_ne!(
            exit_code(&plain),
            0,
            "`n` on the secret question did not stop `--mcp-add`:\n{plain}"
        );
        assert!(
            refusal_shape(&plain).is_none()
                && plain.contains("coyote --add-secret PROBE_MCP_TOKEN_M4D"),
            "the piped-stdin `--mcp-add` was refused, or its abort does not name the remedy:\n{plain}"
        );
        assert!(
            !fs::read_to_string(dir.join("mcp.json")).is_ok_and(|s| s.contains("probe-srv-m4d")),
            "the aborted `--mcp-add` still wrote the server:\n{plain}"
        );
        assert_tty_restored(&plain);

        // Control: no placeholder, no question to ask, the add goes through.
        let (ok, out) = run_ttyless(
            &dir,
            &["--mcp-add", "plain-srv-m4e", "--", "echo", "hi"],
            "tty-less --mcp-add without a placeholder",
        );
        assert!(
            ok,
            "a tty-less `--mcp-add` with nothing to ask failed:\n{out}"
        );
        assert!(
            fs::read_to_string(dir.join("mcp.json")).is_ok_and(|s| s.contains("plain-srv-m4e")),
            "the secret-free `--mcp-add` did not write the server:\n{out}"
        );
    }

    /// The first run ever with no config file and nobody to ask: with a
    /// in the sandbox with no terminal at all, the run refuses to create the
    /// config file in the shared shape, paints no `create a new one?`
    /// question, and writes neither a config file nor a vault password file.
    /// With a terminal on stdout but stdin from /dev/null there is someone to
    /// ask: the question is painted and `n` leaves with nothing written.
    #[test]
    #[serial]
    fn a_first_run_with_no_human_refuses_to_create_the_config_file_and_a_piped_stdin_still_asks() {
        let dir = fresh_config_dir("first-run-no-human");
        let _guard = TempDirGuard(dir.clone());
        assert!(!dir.join("config.yaml").exists());

        let nothing_written = |what: &str, out: &str| {
            assert!(
                !dir.join("config.yaml").exists(),
                "{what}: the refused first run wrote config.yaml:\n{out}"
            );
            let secrets = walk(&dir)
                .into_iter()
                .filter(|p| {
                    let text = p.to_string_lossy().to_ascii_lowercase();
                    text.contains("vault")
                        || text.contains("pass")
                        || text.starts_with(&format!("{}/home/", dir.display()))
                })
                .collect::<Vec<_>>();
            assert!(
                secrets.is_empty(),
                "{what}: the refused first run wrote a password or vault file {secrets:?}:\n{out}"
            );
        };

        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/null");
        pty.wait_for("create a new one?", PTY_DEADLINE)
            .expect("the first run with stdin from /dev/null did not ask to create the config");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"n\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let plain = strip_ansi(&transcript);
        assert_eq!(exit_code(&plain), 0, "`n` on `create a new one?`:\n{plain}");
        assert!(
            refusal_shape(&plain).is_none(),
            "the first run with stdin from /dev/null was refused:\n{plain}"
        );
        assert_tty_restored(&plain);
        nothing_written("stdout tty, stdin /dev/null", &plain);

        let (ok, out) = run_ttyless_with_env(
            built_binary(),
            &dir,
            &[],
            &[("IS_SANDBOX", "1")],
            "sandbox first run with no terminal",
        );
        assert!(!ok, "the tty-less sandbox first run exited 0:\n{out}");
        let (action, _) = refusal_shape(&out).unwrap_or_else(|| {
            panic!("the tty-less sandbox first run is not refused in the shared shape:\n{out}")
        });
        assert!(
            action.starts_with("create the config file at '"),
            "the sandbox refusal does not name the config file: {action:?}\n{out}"
        );
        assert!(!out.contains("create a new one?"), "{out}");
        nothing_written("sandbox, no tty", &out);
    }

    // ---------------------------------------------------------------------
    // The hooks reinstall under `--yes`, a tool's Input question carrying a
    // numeric default, a headless macro reaching a select on a real
    // terminal, the MCP remove/overwrite confirms and the OAuth client pick
    // with no human to ask.
    // ---------------------------------------------------------------------

    /// The `--yes` flag's help sentence as a first-time user reads it.
    const YES_HELP: &str = "Proceed without questions: --uninstall/--update-bundle keep locally \
                        modified items, --install-builtins keeps edited hooks but reinstalls \
                        the other bundled categories";

    /// Seeds the bundled hooks into `dir` (a `--yes` reinstall with nothing
    /// local to conflict), edits one of them locally and deletes another, and
    /// hands back the edited path with its local bytes and the deleted path
    /// with the bytes the bundled asset would restore.
    fn seed_hooks_with_a_local_edit_and_a_missing_file(
        dir: &Path,
    ) -> (PathBuf, &'static str, PathBuf, Vec<u8>) {
        let (ok, out) = run_ttyless(
            dir,
            &["--install-builtins", "hooks", "--yes"],
            "seeding the hooks",
        );
        assert!(ok, "seeding the bundled hooks failed:\n{out}");
        let hooks = dir.join("hooks");
        let edited = hooks.join("notify.sh");
        let local = "# local edit kept-k7h\n";
        fs::write(&edited, local).unwrap();
        let missing = hooks.join("log-events.sh");
        let asset = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets")
                .join("hooks")
                .join("log-events.sh"),
        )
        .unwrap();
        fs::remove_file(&missing).unwrap();
        (edited, local, missing, asset)
    }

    /// `--install-builtins hooks --yes` asks nothing anywhere: with no
    /// terminal at all it exits 0 without a refusal or a `Continue?`, the
    /// locally edited hook keeps its bytes, the deleted bundled hook comes
    /// back, and the run says what it reinstalled. Without `--yes` the same
    /// tty-less run is refused in the shared shape naming the `--yes` remedy
    /// and still leaves the edit alone. The flag's help carries the sentence
    /// that says so.
    #[test]
    #[serial]
    fn install_builtins_hooks_with_yes_asks_nothing_and_keeps_a_local_edit_without_a_terminal() {
        let dir = fresh_config_dir("hooks-yes-ttyless");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let (edited, local, missing, asset) = seed_hooks_with_a_local_edit_and_a_missing_file(&dir);

        let (ok, out) = run_ttyless(
            &dir,
            &["--install-builtins", "hooks", "--yes"],
            "tty-less --install-builtins hooks --yes",
        );
        assert!(
            ok,
            "`--install-builtins hooks --yes` with no terminal failed:\n{out}"
        );
        assert!(
            refusal_shape(&out).is_none() && !out.contains("Continue?") && !out.contains("(y/N)"),
            "`--yes` still refused or asked:\n{out}"
        );
        assert!(
            out.contains("Reinstalled bundled hooks"),
            "the `--yes` run did not say what it reinstalled:\n{out}"
        );
        assert_eq!(
            fs::read_to_string(&edited).ok().as_deref(),
            Some(local),
            "`--yes` replaced the locally edited hook instead of keeping it:\n{out}"
        );
        assert_eq!(
            fs::read(&missing).ok().as_deref(),
            Some(asset.as_slice()),
            "`--yes` did not bring the deleted bundled hook back:\n{out}"
        );

        let (ok, out) = run_ttyless(
            &dir,
            &["--install-builtins", "hooks"],
            "tty-less --install-builtins hooks",
        );
        assert!(
            !ok,
            "a tty-less hooks reinstall without `--yes` exited 0:\n{out}"
        );
        let (action, remedy) = refusal_shape(&out).unwrap_or_else(|| {
            panic!("the hooks reinstall is not refused in the shared shape:\n{out}")
        });
        assert_eq!(action, "reinstall bundled hooks", "{out}");
        assert_eq!(remedy, " or pass --yes.", "{out}");
        assert_eq!(
            fs::read_to_string(&edited).ok().as_deref(),
            Some(local),
            "the refused run touched the locally edited hook:\n{out}"
        );

        let (ok, help) = run_ttyless(&dir, &["--help"], "--help");
        assert!(ok, "{help}");
        assert!(
            squeezed(&help).contains(&squeezed(YES_HELP)),
            "`--help` does not carry the `--yes` sentence `{YES_HELP}`:\n{help}"
        );
    }

    /// The same `--yes` reinstall on a real terminal (where a question could
    /// be asked) asks none either: no `Continue?`, no per-file keep/replace
    /// menu, exit 0, the local edit kept and the terminal left clean.
    #[test]
    #[serial]
    fn install_builtins_hooks_with_yes_on_a_terminal_asks_no_question_and_keeps_the_edit() {
        let dir = fresh_config_dir("hooks-yes-tty");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let (edited, local, missing, asset) = seed_hooks_with_a_local_edit_and_a_missing_file(&dir);

        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            WAIT_WRAPPER,
            "/dev/tty",
            " --repl --install-builtins hooks --yes",
        );
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let plain = strip_ansi(&transcript);
        assert_eq!(exit_code(&plain), 0, "{plain}");
        let flat = squeezed(&plain);
        assert!(
            !flat.contains("Continue?") && !flat.contains("(y/N)") && !flat.contains("Keep"),
            "`--yes` on a terminal still asked:\n{plain}"
        );
        assert!(
            plain.contains("Reinstalled bundled hooks"),
            "the `--yes` run did not say what it reinstalled:\n{plain}"
        );
        assert_eq!(
            fs::read_to_string(&edited).ok().as_deref(),
            Some(local),
            "`--yes` on a terminal replaced the locally edited hook:\n{plain}"
        );
        assert_eq!(
            fs::read(&missing).ok().as_deref(),
            Some(asset.as_slice()),
            "`--yes` on a terminal did not bring the deleted bundled hook back:\n{plain}"
        );
        assert_tty_restored(&plain);
    }

    /// A tool that asks one free-text question over the prompt channel with
    /// a NUMERIC default (`"default": 1000` on the wire, not a string) and
    /// records the answer it gets back.
    const NUMERIC_DEFAULT_INPUT_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Ask the user for a chunk size and record the answer.\n\
    # @option --out! Where to write the answer.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    # shellcheck disable=SC1090\n\
    source \"$LLM_PROMPT_UTILS_FILE\"\n\
    \n\
    main() {\n\
    \x20   local out id line reply v\n\
    \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   id=\"q1k-$$\"\n\
    \x20   line=\"$(jq -cn --arg id \"$id\" '{coyote_prompt:{id:$id,action:\"input\",question:\"Chunk size q1k-q\",options:[],default:1000}}')\"\n\
    \x20   printf '%s\\n' \"$line\" >&\"$_COYOTE_PROMPT_OUT\"\n\
    \x20   while IFS= read -r reply <\"$_COYOTE_PROMPT_REPLY_PATH\"; do\n\
    \x20       if printf '%s' \"$reply\" | jq -e --arg id \"$id\" '.id == $id' >/dev/null 2>&1; then break; fi\n\
    \x20   done\n\
    \x20   v=\"$(printf '%s' \"$reply\" | jq -r 'if (.answer | type) == \"string\" then .answer else \"cancelled\" end')\"\n\
    \x20   printf '%s' \"$v\" > \"$out\"\n\
    \x20   echo \"fixture chunk: $v q1k-r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// A tool's Input question whose default is the JSON number `1000` under
    /// the UI: the modal shows `1000` as a dim placeholder (not as typed text),
    /// Enter on the untouched field answers the string `1000`, and on the
    /// next call typing `5` answers `5` (the placeholder is not prepended).
    #[test]
    #[serial]
    fn a_tools_input_question_with_a_numeric_default_shows_it_dim_and_enter_answers_it_under_the_ui()
     {
        let dir = fresh_config_dir("tool-input-numeric-default");
        let _guard = TempDirGuard(dir.clone());
        let first = dir.join("chunk-answer-1");
        let second = dir.join("chunk-answer-2");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_chunk_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", first.display()),
            },
            Reply::ToolCall {
                name: "fixture_chunk_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", second.display()),
            },
            Reply::Text("fixture done q1k".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_chunk_rec.sh", NUMERIC_DEFAULT_INPUT_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let asked_at = pty.raw().len();
        pty.send(b"call the chunk tool\r").unwrap();
        let modal = wait_for_the_modal(
            &mut pty,
            asked_at,
            "the chunk tool",
            &["Chunksizeq1k-q", "1000", "Enter=answer"],
        );
        assert!(
            painted_dim(pty.raw(), asked_at, b"1000"),
            "the numeric default 1000 is not painted dim as a placeholder:\n{modal}\n--- raw ---\n{}",
            String::from_utf8_lossy(&pty.raw()[asked_at..]).escape_debug()
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &first, "the tool's first recorded answer");
        assert_eq!(
            fs::read_to_string(&first).unwrap().trim(),
            "1000",
            "Enter on the untouched Input modal did not answer the numeric default as `1000`"
        );

        let second_at = pty.raw().len();
        let modal = wait_for_the_modal(
            &mut pty,
            second_at,
            "the chunk tool (second call)",
            &["Chunksizeq1k-q", "1000", "Enter=answer"],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"5").unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &second, "the tool's second recorded answer");
        let typed = fs::read_to_string(&second).unwrap();
        assert_eq!(
            typed.trim(),
            "5",
            "typing `5` over the placeholder answered {typed:?} (the default was prepended or kept):\n{modal}"
        );
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done q1k", "the reply after the tool");
        assert_eq!(fake.served(), 3, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// `--headless --macro <name> <prompt>` on a real terminal (both ttys
    /// present, but no human declared) whose macro issues a bare `.rag`: the
    /// embedding-model select is refused with the typed headless sentence
    /// naming the select and the flag, the run exits non-zero, the macro does
    /// not go on to its next step, and the previous temp rag is still on disk
    /// with its bytes untouched (it is removed only after a successful init).
    #[test]
    #[serial]
    fn a_headless_macro_bare_dot_rag_on_a_terminal_is_refused_with_the_typed_select_and_keeps_the_temp_rag()
     {
        let dir = fresh_config_dir("headless-rag-temp-tty");
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
        let seeded = "embedding_model: dryrun:dry-embed-a\nchunk_size: 1000\nchunk_overlap: 50\ndocument_paths: []\nfiles: []\nmarker: temp-rag-seeded-h7r\n";
        fs::write(&temp_rag, seeded).unwrap();
        let files_before = walk(&dir);

        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            WAIT_WRAPPER,
            "/dev/tty",
            " --headless --macro bare-rag probe-h7r",
        );
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let plain = strip_ansi(&transcript);
        assert_ne!(
            exit_code(&plain),
            0,
            "the headless bare `.rag` exited 0:\n{plain}"
        );
        assert!(
            plain.contains("select prompt needs a human") && plain.contains("--headless"),
            "the headless bare `.rag` is not refused with the typed select sentence:\n{plain}"
        );
        assert_eq!(
            fs::read_to_string(&temp_rag).ok().as_deref(),
            Some(seeded),
            "the refused headless `.rag` removed or rewrote the previous temp rag:\n{plain}"
        );
        let written = new_files_named(&dir, &files_before, "/rags/");
        assert!(
            written.is_empty(),
            "the refused headless `.rag` still wrote {written:?}:\n{plain}"
        );
        assert!(
            !plain.contains("log_path"),
            "the macro went on to `.info` after the refused `.rag`:\n{plain}"
        );
        assert!(
            find(pty.raw(), ENTER_ALT, 0).is_none(),
            "a headless run entered the alternate screen:\n{plain}"
        );
        assert_tty_restored(&plain);
    }

    /// `--mcp-remove <name>` and `--mcp-add <name>` for a server that is
    /// already configured are confirms; with no terminal they are refused in
    /// the shared shape (`remove MCP server '<name>'` / `overwrite MCP server
    /// '<name>'`, remedy ` or pass --mcp-force.`), exit non-zero and leave
    /// mcp.json byte-for-byte as it was. A terminal on stdout with stdin from
    /// /dev/null asks instead, and Enter takes the `No` default with mcp.json
    /// untouched.
    #[test]
    #[serial]
    fn tty_less_mcp_remove_and_re_add_of_an_existing_server_are_refused_and_leave_mcp_json_alone() {
        let dir = fresh_config_dir("ttyless-mcp-remove-overwrite");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let (ok, out) = run_ttyless(
            &dir,
            &["--mcp-add", "probe-srv-r7m", "--", "echo", "hi"],
            "tty-less --mcp-add",
        );
        assert!(ok, "the secret-free `--mcp-add` failed:\n{out}");
        let mcp_json = dir.join("mcp.json");
        let before =
            fs::read(&mcp_json).unwrap_or_else(|_| panic!("no mcp.json after the add:\n{out}"));
        assert!(
            String::from_utf8_lossy(&before).contains("probe-srv-r7m"),
            "{out}"
        );

        let (ok, out) = run_ttyless(
            &dir,
            &["--mcp-remove", "probe-srv-r7m"],
            "tty-less --mcp-remove",
        );
        assert!(!ok, "the tty-less `--mcp-remove` exited 0:\n{out}");
        let (action, remedy) = refusal_shape(&out)
            .unwrap_or_else(|| panic!("`--mcp-remove` is not refused in the shared shape:\n{out}"));
        assert_eq!(action, "remove MCP server 'probe-srv-r7m'", "{out}");
        assert_eq!(remedy, " or pass --mcp-force.", "{out}");
        assert!(
            !out.contains("Remove MCP server"),
            "the tty-less `--mcp-remove` asked anyway:\n{out}"
        );
        assert_eq!(
            fs::read(&mcp_json).unwrap(),
            before,
            "the refused remove changed mcp.json:\n{out}"
        );

        let (ok, out) = run_ttyless(
            &dir,
            &["--mcp-add", "probe-srv-r7m", "--", "echo", "bye"],
            "tty-less --mcp-add of an existing server",
        );
        assert!(
            !ok,
            "the tty-less `--mcp-add` of an existing server exited 0:\n{out}"
        );
        let (action, remedy) = refusal_shape(&out).unwrap_or_else(|| {
            panic!("`--mcp-add` of an existing server is not refused in the shared shape:\n{out}")
        });
        assert_eq!(action, "overwrite MCP server 'probe-srv-r7m'", "{out}");
        assert_eq!(remedy, " or pass --mcp-force.", "{out}");
        assert_eq!(
            fs::read(&mcp_json).unwrap(),
            before,
            "the refused re-add changed mcp.json:\n{out}"
        );

        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            WAIT_WRAPPER,
            "/dev/null",
            " --repl --mcp-remove probe-srv-r7m",
        );
        pty.wait_for("Remove MCP server", PTY_DEADLINE)
            .expect("the piped-stdin `--mcp-remove` did not ask");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let plain = strip_ansi(&transcript);
        assert!(
            refusal_shape(&plain).is_none(),
            "the piped-stdin `--mcp-remove` was refused:\n{plain}"
        );
        assert_eq!(
            fs::read(&mcp_json).unwrap(),
            before,
            "Enter on `Remove MCP server` changed mcp.json:\n{plain}"
        );
        assert_tty_restored(&plain);
    }

    /// `--mcp-add <name> --url <url with a vault placeholder> --mcp-force` for
    /// a server that is already configured, with no terminal at all:
    /// `--mcp-force` skips the overwrite question, but the missing secret
    /// still has to be asked for, so the run is refused in the shared shape
    /// (`provision the MCP secrets for '<name>'`, remedy naming
    /// `coyote --add-secret <SECRET>`), exits non-zero and leaves mcp.json
    /// byte-for-byte as it was.
    #[test]
    #[serial]
    fn a_tty_less_mcp_add_with_a_secret_and_mcp_force_is_refused_before_touching_mcp_json() {
        let dir = fresh_config_dir("ttyless-mcp-add-secret-force");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        secure_vault_password_file(&dir);
        let (ok, out) = run_ttyless(
            &dir,
            &["--mcp-add", "forced-srv-f2k", "--", "echo", "hi"],
            "tty-less --mcp-add",
        );
        assert!(ok, "the secret-free `--mcp-add` failed:\n{out}");
        let mcp_json = dir.join("mcp.json");
        let before =
            fs::read(&mcp_json).unwrap_or_else(|_| panic!("no mcp.json after the add:\n{out}"));

        let (ok, out) = run_ttyless(
            &dir,
            &[
                "--mcp-add",
                "forced-srv-f2k",
                "--url",
                "https://h/{{FORCED_MCP_TOKEN_F2K}}",
                "--mcp-force",
            ],
            "tty-less --mcp-add --mcp-force with a vault placeholder",
        );
        assert!(
            !ok,
            "the forced tty-less `--mcp-add` needing a secret exited 0:\n{out}"
        );
        let (action, remedy) = refusal_shape(&out).unwrap_or_else(|| {
            panic!("the forced `--mcp-add` is not refused in the shared shape:\n{out}")
        });
        assert_eq!(
            action, "provision the MCP secrets for 'forced-srv-f2k'",
            "{out}"
        );
        assert!(
            remedy.contains("coyote --add-secret FORCED_MCP_TOKEN_F2K"),
            "the remedy does not name `coyote --add-secret FORCED_MCP_TOKEN_F2K`: {remedy:?}\n{out}"
        );
        assert!(
            !out.contains("Overwrite?") && !out.contains("to the vault now?"),
            "the forced run asked anyway:\n{out}"
        );
        assert_eq!(
            fs::read(&mcp_json).unwrap(),
            before,
            "the refused forced add changed mcp.json:\n{out}"
        );
    }

    /// Two more clients that authenticate with OAuth against unreachable
    /// hosts, so `--authenticate` has a pick to make and never gets a token.
    const TWO_OAUTH_CLIENTS: &str = "\x20 - type: openai-compatible\n\
    \x20   name: gateway-one-o7a\n\
    \x20   api_base: https://one.invalid/v1\n\
    \x20   auth: oauth\n\
    \x20   oauth:\n\
    \x20     client_id: one-id\n\
    \x20     device_authorization_url: https://one.invalid/device\n\
    \x20     token_url: https://one.invalid/token\n\
    \x20     flow: device_code\n\
    \x20   models:\n\
    \x20     - name: m1\n\
    \x20       max_input_tokens: 1000\n\
    \x20 - type: openai-compatible\n\
    \x20   name: gateway-two-o7b\n\
    \x20   api_base: https://two.invalid/v1\n\
    \x20   auth: oauth\n\
    \x20   oauth:\n\
    \x20     client_id: two-id\n\
    \x20     device_authorization_url: https://two.invalid/device\n\
    \x20     token_url: https://two.invalid/token\n\
    \x20     flow: device_code\n\
    \x20   models:\n\
    \x20     - name: m2\n\
    \x20       max_input_tokens: 1000\n\
    save_session: false\n";

    /// `coyote --authenticate` with two OAuth-capable clients and no terminal
    /// cannot pick one: it is refused in the shared shape naming both clients
    /// with the remedy ` or pass --authenticate <CLIENT_NAME>.`, exits
    /// non-zero and asks nothing. Naming the client skips the pick (the run
    /// then fails on its unreachable host, not on a refusal).
    #[test]
    #[serial]
    fn tty_less_authenticate_with_two_oauth_clients_is_refused_naming_both_and_the_flag_remedy() {
        let dir = fresh_config_dir("ttyless-authenticate-pick");
        let _guard = TempDirGuard(dir.clone());
        write_config(&dir, TWO_OAUTH_CLIENTS);

        let (ok, out) = run_ttyless(&dir, &["--authenticate"], "tty-less --authenticate");
        assert!(
            !ok,
            "the tty-less `--authenticate` with two OAuth clients exited 0:\n{out}"
        );
        let (action, remedy) = refusal_shape(&out).unwrap_or_else(|| {
            panic!("`--authenticate` is not refused in the shared shape:\n{out}")
        });
        assert!(
            action.contains("gateway-one-o7a") && action.contains("gateway-two-o7b"),
            "the refusal does not name both OAuth clients: {action:?}\n{out}"
        );
        assert_eq!(remedy, " or pass --authenticate <CLIENT_NAME>.", "{out}");
        assert!(
            !out.contains("to move, enter to select"),
            "the tty-less `--authenticate` drew the select anyway:\n{out}"
        );

        let (ok, out) = run_ttyless(
            &dir,
            &["--authenticate", "gateway-two-o7b"],
            "tty-less --authenticate <name>",
        );
        assert!(
            !ok,
            "`--authenticate gateway-two-o7b` against an unreachable host exited 0:\n{out}"
        );
        assert!(
            refusal_shape(&out).is_none(),
            "naming the client still hit the no-human refusal:\n{out}"
        );
        assert!(
            out.contains("two.invalid"),
            "the named client was not the one authenticated:\n{out}"
        );
    }

    // ---------------------------------------------------------------------
    // The `--install-builtins` help sentence and the clap errors around it,
    // and a tty-less `--uninstall --yes` of a bundle with several modified
    // items.
    // ---------------------------------------------------------------------

    /// The `--install-builtins` flag's help sentence as a first-time user
    /// reads it.
    const INSTALL_BUILTINS_HELP: &str = "Reinstall bundled assets for a category (asks before \
                                     overwriting your local changes unless --yes)";

    /// `--help` carries the `--install-builtins` sentence; `--install <x>
    /// --yes` is a clap error (the two flags exclude each other), as is
    /// `--install-builtins x --yes` (not a category): both exit 2 before the
    /// config dir is touched, so a typo cannot install anything.
    #[test]
    #[serial]
    fn install_builtins_help_sentence_and_the_clap_errors_around_install_with_yes() {
        let dir = fresh_config_dir("install-builtins-help-clap");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let files_before = walk(&dir);

        let (ok, help) = run_ttyless(&dir, &["--help"], "--help");
        assert!(ok, "{help}");
        assert!(
            squeezed(&help).contains(&squeezed(INSTALL_BUILTINS_HELP)),
            "`--help` does not carry the `--install-builtins` sentence `{INSTALL_BUILTINS_HELP}`:\n{help}"
        );

        for (what, args) in [
            ("`--install x --yes`", vec!["--install", "x", "--yes"]),
            (
                "`--install-builtins x --yes`",
                vec!["--install-builtins", "x", "--yes"],
            ),
        ] {
            let (status, out) = run_ttyless_output(built_binary(), &dir, &args, what);
            assert_eq!(
                status.code(),
                Some(2),
                "{what} is not a clap usage error (exit 2):\n{out}"
            );
            let plain = strip_ansi(&out);
            assert!(
                plain.contains("error:")
                    && (plain.contains("--install") || plain.contains("--yes")),
                "{what} does not report a clap error naming the flag:\n{out}"
            );
            assert!(
                refusal_shape(&out).is_none(),
                "{what} hit the no-human refusal instead of the argument check:\n{out}"
            );
        }
        assert_eq!(
            walk(&dir),
            files_before,
            "a rejected argument set still changed the config dir"
        );
    }

    /// A bundle of three roles, installed with no terminal, then two of them
    /// edited locally. `--uninstall <bundle> --yes` with no terminal asks
    /// nothing and keeps every locally modified item (the `--yes` sentence):
    /// exit 0, no refusal, no keep/delete question, the untouched role gone,
    /// both edited roles still carrying their edits, one `kept modified
    /// file` ledger line per kept item, the summary counting `kept=2`, and
    /// no cancelled-question note (no question was asked). The record keeps
    /// the kept items, so the bundle is reported partially uninstalled.
    #[test]
    #[serial]
    fn a_tty_less_uninstall_with_yes_keeps_every_modified_item_and_prints_no_question_note() {
        let dir = fresh_config_dir("ttyless-uninstall-yes-modified");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-u8k";
        let repo = dir.join("bundle-src");
        fs::create_dir_all(repo.join("roles")).unwrap();
        fs::write(repo.join("coyote-bundle.yaml"), format!("name: {name}\n")).unwrap();
        for role in ["one", "two", "three"] {
            fs::write(
                repo.join("roles").join(format!("{name}-{role}.md")),
                format!("---\nmodel: fake:fake-model\n---\nYou are role {role}.\n"),
            )
            .unwrap();
        }
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "bundle"]);

        let (ok, out) = run_ttyless(
            &dir,
            &["--install", repo.to_str().unwrap()],
            "tty-less install",
        );
        let roles = dir.join("roles");
        let one = roles.join(format!("{name}-one.md"));
        let two = roles.join(format!("{name}-two.md"));
        let three = roles.join(format!("{name}-three.md"));
        assert!(
            ok && one.exists() && two.exists() && three.exists(),
            "the tty-less install did not write the three roles:\n{out}"
        );
        let edited_one = format!("{}# edited one\n", fs::read_to_string(&one).unwrap());
        let edited_two = format!("{}# edited two\n", fs::read_to_string(&two).unwrap());
        fs::write(&one, &edited_one).unwrap();
        fs::write(&two, &edited_two).unwrap();

        let (ok, out) = run_ttyless(
            &dir,
            &["--uninstall", name, "--yes"],
            "tty-less --uninstall --yes",
        );
        assert!(ok, "`--uninstall --yes` with no terminal failed:\n{out}");
        assert!(
            refusal_shape(&out).is_none(),
            "`--yes` still refused:\n{out}"
        );
        for widget in [
            "was modified locally after install",
            "to move, enter to select",
            "keep-all",
            "? ",
        ] {
            assert!(
                !out.contains(widget),
                "`--yes` still asked (`{widget}`):\n{out}"
            );
        }
        assert!(
            !out.contains("Kept the locally modified items"),
            "a cancelled-question note was printed though no question was asked:\n{out}"
        );
        assert_eq!(
            out.matches("kept modified file").count(),
            2,
            "the ledger does not carry exactly one `kept modified file` line per kept item:\n{out}"
        );
        assert!(
            out.contains(&format!("kept modified file roles/{name}-one.md"))
                && out.contains(&format!("kept modified file roles/{name}-two.md")),
            "the ledger does not name both kept roles:\n{out}"
        );
        assert!(
            out.contains("deleted=1 kept=2"),
            "the summary does not count one deleted and two kept files:\n{out}"
        );
        assert!(
            out.contains("partially uninstalled"),
            "the bundle with kept items is not reported partially uninstalled:\n{out}"
        );
        assert!(
            !three.exists(),
            "the untouched role survived `--uninstall --yes`:\n{out}"
        );
        assert_eq!(
            fs::read_to_string(&one).ok().as_deref(),
            Some(edited_one.as_str()),
            "the first edited role lost its edit:\n{out}"
        );
        assert_eq!(
            fs::read_to_string(&two).ok().as_deref(),
            Some(edited_two.as_str()),
            "the second edited role lost its edit:\n{out}"
        );
    }

    /// `--uninstall <bundle>` without `--yes` and with no terminal at all:
    /// the refusal is the shared shape whose remedy is exactly
    /// ` or pass --yes.`, the exit code is non-zero, no `Uninstall bundle?`
    /// is painted, and every owned file and the record stay.
    #[test]
    #[serial]
    fn a_tty_less_uninstall_refusal_ends_with_the_yes_remedy() {
        let dir = fresh_config_dir("ttyless-uninstall-remedy");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-r9y";
        let repo = write_bundle_repo(&dir, name, &[]);
        let (ok, out) = run_ttyless(
            &dir,
            &["--install", repo.to_str().unwrap()],
            "tty-less install",
        );
        let role = dir.join("roles").join(format!("{name}-role.md"));
        assert!(
            ok && role.exists(),
            "the tty-less install did not proceed:\n{out}"
        );
        let files_before = walk(&dir);
        let expected = (
            format!("uninstall bundle '{name}'"),
            " or pass --yes.".to_string(),
        );

        let (ok, out) = run_ttyless(&dir, &["--uninstall", name], "tty-less uninstall");
        assert!(
            !ok,
            "a tty-less `--uninstall` without `--yes` exited 0:\n{out}"
        );
        assert_eq!(
            refusal_shape(&out),
            Some(expected),
            "the tty-less uninstall refusal is not the shared shape with the `--yes` remedy:\n{out}"
        );
        assert!(
            !out.contains("Uninstall bundle") && !out.contains("owns"),
            "the refused run painted the question or its footprint line:\n{out}"
        );
        assert_eq!(
            walk(&dir),
            files_before,
            "a refused uninstall changed the config dir"
        );
    }

    // ---------------------------------------------------------------------
    // Cold start: which shell an interactive run gets is decided by the
    // `ui:` key, then `--tui` / `--repl`, in that order of increasing
    // precedence; `COYOTE_UI` in the environment is what coyote exports to
    // its tools, not a choice. A prompt run never starts the full-screen UI.
    // ---------------------------------------------------------------------
    mod cold_start {
        use super::*;

        /// "no session" is the status line the full-screen UI paints once up.
        fn assert_full_screen_ui_then_ctrl_d(pty: &mut Pty, what: &str) {
            let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
            assert!(
                find(pty.raw(), ENTER_ALT, 0).is_some(),
                "{what} did not enter the alternate screen:\n{transcript}"
            );
            pty.read_for(Duration::from_millis(300));
            pty.send(b"\x04").unwrap();
            let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
            assert_eq!(exit_code(&transcript), 0, "{transcript}");
            assert_tty_restored(&transcript);
        }

        /// "Welcome to" opens the REPL banner.
        fn assert_plain_repl_then_ctrl_d(pty: &mut Pty, what: &str) {
            let transcript = pty.wait_for("Welcome to", PTY_DEADLINE).unwrap();
            assert!(
                find(pty.raw(), ENTER_ALT, 0).is_none(),
                "{what} entered the alternate screen:\n{transcript}"
            );
            wait_for_repl_prompt(pty, "dry-model)>", 0);
            pty.send(b"\x04").unwrap();
            let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
            assert_eq!(exit_code(&transcript), 0, "{transcript}");
            assert!(
                find(pty.raw(), ENTER_ALT, 0).is_none(),
                "{what} entered the alternate screen:\n{transcript}"
            );
            assert_tty_restored(&transcript);
        }

        #[test]
        #[serial]
        fn the_tui_flag_starts_the_full_screen_ui() {
            let dir = fresh_config_dir("tui-flag");
            let _guard = TempDirGuard(dir.clone());
            write_dry_run_config(&dir);
            let mut pty = spawn_with_args(&dir, &[], &["--tui"]);

            assert_full_screen_ui_then_ctrl_d(&mut pty, "--tui");
        }

        #[test]
        #[serial]
        fn a_tui_config_key_starts_the_full_screen_ui() {
            let dir = fresh_config_dir("tui-key");
            let _guard = TempDirGuard(dir.clone());
            write_config(&dir, "save_session: false\nui: tui\n");
            let mut pty = spawn_with_args(&dir, &[], &[]);

            assert_full_screen_ui_then_ctrl_d(&mut pty, "ui: tui");
        }

        /// `COYOTE_UI` in the environment chooses nothing: `ui: tui` still
        /// starts the full-screen UI under `COYOTE_UI=repl` or `headless`, and
        /// with no `ui:` key `COYOTE_UI=tui` still gets the plain REPL, which
        /// never touches the alternate screen.
        #[test]
        #[serial]
        fn coyote_ui_in_the_environment_does_not_choose_the_shell() {
            for env_value in ["repl", "headless"] {
                let dir = fresh_config_dir(&format!("env-ignored-{env_value}"));
                let _guard = TempDirGuard(dir.clone());
                write_config(&dir, "save_session: false\nui: tui\n");
                let mut pty = spawn_with_args(&dir, &[("COYOTE_UI", env_value)], &[]);

                assert_full_screen_ui_then_ctrl_d(
                    &mut pty,
                    &format!("ui: tui under COYOTE_UI={env_value}"),
                );
            }

            let dir = fresh_config_dir("env-ignored-tui");
            let _guard = TempDirGuard(dir.clone());
            write_dry_run_config(&dir);
            let mut pty = spawn_with_args(&dir, &[("COYOTE_UI", "tui")], &[]);

            assert_plain_repl_then_ctrl_d(&mut pty, "no ui: key under COYOTE_UI=tui");
        }

        #[test]
        #[serial]
        fn the_repl_flag_beats_a_tui_config_key() {
            let dir = fresh_config_dir("repl-flag");
            let _guard = TempDirGuard(dir.clone());
            write_config(&dir, "save_session: false\nui: tui\n");
            let mut pty = spawn_with_args(&dir, &[], &["--repl"]);

            assert_plain_repl_then_ctrl_d(&mut pty, "--repl over ui: tui");
        }

        /// [`WAIT_WRAPPER`] with the binary's stdout and stderr sent to files
        /// in the working directory while stdin keeps the tty: the shape of
        /// `coyote > out.txt` typed at a terminal.
        const STDOUT_TO_FILE_WRAPPER: &str = "'{coyote}'{args} <{stdin} >stdout.txt 2>stderr.txt & \
         cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}";

        /// A tty on stdin alone is not enough: with stdout redirected to a
        /// file the full-screen UI refuses to start (`No TTY for TUI`, exit 1),
        /// writes nothing to the file, and never opens `/dev/tty` behind the
        /// redirect to paint there instead.
        #[test]
        #[serial]
        fn the_full_screen_ui_refuses_a_redirected_stdout_with_a_tty_on_stdin() {
            let dir = fresh_config_dir("tui-stdout-redirect");
            let _guard = TempDirGuard(dir.clone());

            for (what, config, args) in [
                ("--tui", "save_session: false\n", " --tui"),
                ("ui: tui", "save_session: false\nui: tui\n", ""),
            ] {
                write_config(&dir, config);
                let _ = fs::remove_file(dir.join("stdout.txt"));
                let _ = fs::remove_file(dir.join("stderr.txt"));
                let mut pty = spawn_binary_under(
                    built_binary(),
                    &dir,
                    &[],
                    STDOUT_TO_FILE_WRAPPER,
                    "/dev/tty",
                    args,
                );
                let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
                assert_eq!(
                    exit_code(&transcript),
                    1,
                    "{what} with stdout redirected did not exit 1:\n{transcript}"
                );
                assert!(
                    find(pty.raw(), ENTER_ALT, 0).is_none(),
                    "{what} with stdout redirected painted on the tty anyway:\n{transcript}"
                );
                assert_tty_restored(&transcript);

                let stdout = fs::read(dir.join("stdout.txt")).unwrap();
                let stderr = fs::read_to_string(dir.join("stderr.txt")).unwrap();
                assert!(
                    stdout.is_empty(),
                    "{what} with stdout redirected wrote to the file: {:?}",
                    String::from_utf8_lossy(&stdout)
                );
                assert!(
                    stderr.contains("No TTY for TUI"),
                    "{what} with stdout redirected gave no tty refusal on stderr:\n{stderr}"
                );
                assert!(
                    !stderr.contains("Welcome to"),
                    "{what} with stdout redirected fell through to the REPL:\n{stderr}"
                );
            }
        }

        #[test]
        #[serial]
        fn an_interactive_start_with_no_preference_is_the_plain_repl() {
            let dir = fresh_config_dir("no-ui-preference");
            let _guard = TempDirGuard(dir.clone());
            write_dry_run_config(&dir);
            let mut pty = spawn_with_args(&dir, &[], &[]);

            assert_plain_repl_then_ctrl_d(&mut pty, "no ui preference");
        }

        /// The shell is chosen before the REPL is up, so `ui` is not a `.set`
        /// key: `.set ui tui` is refused as an unknown key, the session stays
        /// on the normal screen, and the REPL keeps running.
        #[test]
        #[serial]
        fn the_ui_key_is_not_a_dot_set_key_in_the_repl() {
            let dir = fresh_config_dir("set-ui-key");
            let _guard = TempDirGuard(dir.clone());
            write_dry_run_config(&dir);
            let mut pty = spawn_with_args(&dir, &[], &["--repl"]);

            let from = wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
            pty.send(b".set ui tui\r").unwrap();
            let since = pty
                .wait_for_after("Unknown key 'ui'", from, PTY_DEADLINE)
                .unwrap();
            assert!(
                find(pty.raw(), ENTER_ALT, 0).is_none(),
                ".set ui tui entered the alternate screen:\n{since}"
            );

            let refused = find(pty.raw(), b"Unknown key", from)
                .expect("the refusal was seen in the stripped transcript but not in the raw bytes");
            wait_for_repl_prompt(&mut pty, "dry-model)>", refused);
            pty.send(b"\x04").unwrap();
            let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
            assert_eq!(exit_code(&transcript), 0, "{transcript}");
            assert_tty_restored(&transcript);
        }
    }

    // ---------------------------------------------------------------------
    // The install conflict remedies word for word, the MCP conflict with no
    // human to ask, a piped-stdin forced MCP add, and a blank answer in the
    // first-run provider wizard.
    // ---------------------------------------------------------------------

    /// `coyote --install <bundle>` with no terminal at all, once at a local
    /// role conflict and once at an MCP server conflict: each is refused in
    /// the shared shape and the remedy is, word for word, `, with
    /// --install-force (installs), or with --yes (updates).`; the run exits
    /// non-zero and neither the local roles nor the local mcp.json change.
    #[test]
    #[serial]
    fn a_tty_less_cli_install_at_a_role_or_mcp_conflict_ends_with_the_install_force_and_yes_remedy()
    {
        let dir = fresh_config_dir("ttyless-install-remedy-cli");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-k7s";
        let repo = write_bundle_repo(&dir, name, &[]);

        // A role conflict.
        let locals = write_conflicting_local_roles(&dir, name);
        let (ok, out) = run_ttyless(
            &dir,
            &["--install", repo.to_str().unwrap()],
            "tty-less --install at a role conflict",
        );
        assert!(
            !ok,
            "the tty-less `--install` at a role conflict exited 0:\n{out}"
        );
        let (action, remedy) = refusal_shape(&out).unwrap_or_else(|| {
            panic!("the role conflict is not refused in the shared shape:\n{out}")
        });
        assert!(
            action.starts_with("overwrite local file "),
            "the role conflict refusal names {action:?}:\n{out}"
        );
        assert_eq!(
            remedy, CLI_INSTALL_REMEDY,
            "the CLI role conflict remedy is not word for word the documented one:\n{out}"
        );
        for path in &locals {
            assert_eq!(
                fs::read_to_string(path).ok().as_deref(),
                Some(LOCAL_ROLE_TEXT),
                "the refused install changed {}:\n{out}",
                path.display()
            );
        }

        // An MCP server conflict, with the roles out of the way.
        for path in &locals {
            fs::remove_file(path).unwrap();
        }
        let local_before = write_conflicting_local_mcp_server(&dir);
        let (ok, out) = run_ttyless(
            &dir,
            &["--install", repo.to_str().unwrap()],
            "tty-less --install at an MCP conflict",
        );
        assert!(
            !ok,
            "the tty-less `--install` at an MCP conflict exited 0:\n{out}"
        );
        let (action, remedy) = refusal_shape(&out).unwrap_or_else(|| {
            panic!("the MCP conflict is not refused in the shared shape:\n{out}")
        });
        assert_eq!(
            action, "merge the already-installed MCP server 'bundle-k7s-server'",
            "{out}"
        );
        assert_eq!(
            remedy, CLI_INSTALL_REMEDY,
            "the CLI MCP conflict remedy is not word for word the documented one:\n{out}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("mcp.json")).unwrap(),
            local_before,
            "the refused merge changed the local mcp.json:\n{out}"
        );
        assert!(
            !out.contains("Kept the local MCP server"),
            "a refused merge also printed the kept-server note:\n{out}"
        );
    }

    /// `.install <bundle>` in the plain REPL on a terminal with stdin from
    /// /dev/null, at an MCP server conflict: the conflict menu is painted on
    /// the terminal rather than refused, Esc keeps the local server with
    /// its definition unchanged, and the REPL goes on to `.exit` cleanly.
    #[test]
    #[serial]
    fn a_plain_repl_dot_install_at_an_mcp_conflict_with_stdin_from_dev_null_asks_and_esc_keeps_local()
     {
        let dir = fresh_config_dir("repl-install-mcp-remedy");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        write_bundle_repo(&dir, "bundle-k7s", &[]);
        let local_before = write_conflicting_local_mcp_server(&dir);

        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/null");
        let from = wait_for_repl_prompt(&mut pty, ")> ", 0);
        // The line editor repaints the prompt once per typed key, about 100 ms
        // each in a debug build, so the absolute temp path alone would eat most
        // of the deadline; the binary runs in `dir`, where the relative path
        // names the same repository.
        pty.send(b".install ./bundle-src\r").unwrap();
        pty.wait_for_after("Conflict on MCP server", from, PTY_DEADLINE)
            .unwrap_or_else(|err| panic!("`.install` at an MCP conflict did not ask: {err}"));
        pty.read_for(Duration::from_millis(500));
        let cancelled = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]));
        assert!(
            refusal_shape(&since).is_none(),
            "`.install` at an MCP conflict was refused:\n{since}"
        );
        wait_for_repl_prompt(&mut pty, ")> ", cancelled);
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(
            squeezed(&fs::read_to_string(dir.join("mcp.json")).unwrap()),
            squeezed(&local_before),
            "Esc on the MCP conflict did not keep the local server:\n{transcript}"
        );
    }

    /// `--headless --macro <name> <prompt>` on a real terminal (both ttys
    /// present, nobody declared to answer) whose macro runs `.install
    /// <bundle>` at an MCP server conflict: the conflict is not refused (a
    /// terminal is there) and not asked (nobody is); the local server is
    /// kept, exactly one note says `Kept the local MCP server
    /// 'bundle-k7s-server': nobody could be asked.`, the install completes
    /// (role installed, merge report says kept local), the macro goes on to
    /// its `.info` step and the run exits 0.
    #[test]
    #[serial]
    fn a_headless_macro_dot_install_at_an_mcp_conflict_keeps_the_local_server_saying_nobody_could_be_asked()
     {
        let dir = fresh_config_dir("headless-install-mcp-conflict");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let repo = write_bundle_repo(&dir, "bundle-k7s", &[]);
        let local_before = write_conflicting_local_mcp_server(&dir);
        fs::create_dir_all(dir.join("macros")).unwrap();
        fs::write(
            dir.join("macros").join("install-k7s.yaml"),
            format!(
                "isolated: false\nsteps:\n  - \".install {}\"\n  - \".info\"\n",
                repo.display()
            ),
        )
        .unwrap();

        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            WAIT_WRAPPER,
            "/dev/tty",
            " --headless --macro install-k7s macro-m10",
        );
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let plain = strip_ansi(&transcript);
        let note = "Kept the local MCP server 'bundle-k7s-server': nobody could be asked.";
        assert_eq!(
            plain.matches(note).count(),
            1,
            "the headless install did not print the kept-server note exactly once:\n{plain}"
        );
        assert!(
            refusal_shape(&plain).is_none(),
            "the headless install on a terminal was refused instead of keeping local:\n{plain}"
        );
        assert!(
            !plain.contains("the question was cancelled"),
            "the headless install printed the cancel wording:\n{plain}"
        );
        assert!(
            squeezed(&plain).contains("keptlocal:bundle-k7s-server"),
            "the merge report does not say the server was kept local:\n{plain}"
        );
        assert_eq!(
            exit_code(&plain),
            0,
            "the headless macro install exited non-zero:\n{plain}"
        );
        assert!(
            plain.contains("log_path"),
            "the macro did not go on to `.info` after the install:\n{plain}"
        );
        assert!(
            dir.join("roles").join("bundle-k7s-role.md").exists(),
            "the headless install did not install the bundle's role:\n{plain}"
        );
        let local_before: serde_json::Value = serde_json::from_str(&local_before).unwrap();
        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(
            merged, local_before,
            "the headless install changed the local server definition:\n{plain}"
        );
        assert!(
            find(pty.raw(), ENTER_ALT, 0).is_none(),
            "a headless run entered the alternate screen:\n{plain}"
        );
        assert_tty_restored(&plain);
    }

    /// `coyote --mcp-add x --url 'https://h/{{TOK}}' --mcp-force` typed at a
    /// terminal with stdin a pipe (`printf '' | coyote ...`), the vault
    /// configured and `x` already present: `--mcp-force` skips the overwrite
    /// question, the missing secret is asked for on the terminal (the pipe
    /// is not what answers), and `n` aborts naming `coyote --add-secret TOK`
    /// before anything is written: exit non-zero, mcp.json byte-for-byte as
    /// before, the tty left cooked.
    #[test]
    #[serial]
    fn a_piped_stdin_forced_mcp_add_with_a_secret_on_a_terminal_asks_and_n_leaves_mcp_json_alone() {
        let dir = fresh_config_dir("piped-mcp-add-secret-force");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        secure_vault_password_file(&dir);
        let (ok, out) = run_ttyless(
            &dir,
            &["--mcp-add", "x", "--", "echo", "hi"],
            "tty-less --mcp-add",
        );
        assert!(ok, "the secret-free `--mcp-add` failed:\n{out}");
        let mcp_json = dir.join("mcp.json");
        let before =
            fs::read(&mcp_json).unwrap_or_else(|_| panic!("no mcp.json after the add:\n{out}"));

        let piped_wrapper = "printf '' | '{coyote}'{args}; rc=$?; {report}";
        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            piped_wrapper,
            "/dev/null",
            " --repl --mcp-add x --url 'https://h/{{TOK}}' --mcp-force",
        );
        pty.wait_for("to the vault now?", PTY_DEADLINE)
            .expect("the piped-stdin forced `--mcp-add` did not ask for the secret");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"n\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let plain = strip_ansi(&transcript);
        assert_ne!(
            exit_code(&plain),
            0,
            "`n` on the secret question did not stop the forced `--mcp-add`:\n{plain}"
        );
        assert!(
            refusal_shape(&plain).is_none() && plain.contains("coyote --add-secret TOK"),
            "the forced add was refused, or its abort does not name the remedy:\n{plain}"
        );
        assert!(
            !plain.contains("Overwrite"),
            "`--mcp-force` still asked to overwrite:\n{plain}"
        );
        assert_eq!(
            fs::read(&mcp_json).unwrap(),
            before,
            "the aborted forced add changed mcp.json:\n{plain}"
        );
        assert!(
            !String::from_utf8_lossy(&fs::read(&mcp_json).unwrap()).contains("TOK"),
            "the aborted forced add wrote the new definition:\n{plain}"
        );
        assert_tty_restored(&plain);
    }

    /// The first run ever (no config file) on a terminal, into the vault
    /// step with the GCP provider, whose project ID has no default: Enter on
    /// the blank field is not taken; the field is asked again with `This
    /// field is required.` in front of its label (asked twice in all, not
    /// refused), and Ctrl+C on the re-asked field leaves with no config file
    /// written and the tty restored. (A value typed there would go on to a
    /// write probe against the real Secret Manager, so the case stops at the
    /// re-ask.)
    #[test]
    #[serial]
    fn a_blank_answer_in_the_first_run_vault_provider_wizard_is_reasked_with_this_field_is_required()
     {
        let dir = fresh_config_dir("first-run-provider-blank");
        let _guard = TempDirGuard(dir.clone());
        assert!(!dir.join("config.yaml").exists());

        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");
        pty.wait_for("create a new one?", PTY_DEADLINE)
            .expect("the first run did not ask to create a config file");
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        pty.wait_for("secrets provider", PTY_DEADLINE)
            .expect("the wizard did not ask which secrets provider to use");
        pty.read_for(Duration::from_millis(300));
        // The third row is the GCP provider; the select takes arrows only.
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(200));
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(200));
        pty.send(b"\r").unwrap();
        let asked_at = pty.raw().len();
        pty.wait_for("GCP project ID:", PTY_DEADLINE)
            .expect("the wizard did not ask for the GCP project ID");
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        let reasked = loop {
            pty.read_for(Duration::from_millis(200));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
            if since.contains("This field is required. GCP project ID:") {
                break since;
            }
            assert!(
                !since.contains("API Provider"),
                "the blank project ID was taken and the wizard moved on:\n{since}"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "a blank GCP project ID was not re-asked with `This field is required.`:\n{since}"
            );
        };
        assert!(
            !reasked.contains("Refusing to"),
            "the blank answer was refused instead of re-asked:\n{reasked}"
        );
        assert_eq!(
            reasked.matches("? GCP project ID:").count()
                + reasked
                    .matches("? This field is required. GCP project ID:")
                    .count(),
            2,
            "the project ID was not asked exactly twice (once plain, once re-asked):\n{reasked}"
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x03").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_ne!(
            exit_code(&transcript),
            0,
            "Ctrl+C on the re-asked field exited 0:\n{transcript}"
        );
        assert!(
            !dir.join("config.yaml").exists(),
            "the interrupted wizard wrote config.yaml:\n{transcript}"
        );
        assert_tty_restored(&transcript);
    }

    /// `coyote --auth-mcp <server>` for a remote MCP server with no terminal
    /// is refused in the shared shape before any widget or network: the
    /// server's URL points nowhere, so reaching discovery would surface as a
    /// connection error instead.
    #[test]
    #[serial]
    fn a_tty_less_auth_mcp_is_refused_in_the_shared_shape() {
        let dir = fresh_config_dir("ttyless-auth-mcp");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        fs::write(
        dir.join("mcp.json"),
        r#"{"mcpServers": {"oauth-srv-a4n": {"type": "http", "url": "http://127.0.0.1:9/mcp", "oauth": {"clientId": "cid-a4n"}}}}"#,
    )
    .unwrap();

        let (ok, out) = run_ttyless(
            &dir,
            &["--auth-mcp", "oauth-srv-a4n"],
            "tty-less --auth-mcp",
        );
        assert!(!ok, "the tty-less `--auth-mcp` exited 0:\n{out}");
        let (action, remedy) = refusal_shape(&out)
            .unwrap_or_else(|| panic!("`--auth-mcp` is not refused in the shared shape:\n{out}"));
        assert_eq!(action, "sign in to 'oauth-srv-a4n' with OAuth", "{out}");
        assert_eq!(remedy, ".", "{out}");
        assert!(
            !out.contains("Open this URL") && !out.contains("Enter the OAuth client ID"),
            "the tty-less `--auth-mcp` started the sign-in anyway:\n{out}"
        );
    }

    // ---------------------------------------------------------------------
    // Esc on the agent's `init RAG?` question cancels the agent in both
    // interfaces, and `--auth-mcp` typed at a terminal with stdin from
    // /dev/null.
    // ---------------------------------------------------------------------

    /// The error a cancelled question surfaces as.
    const CANCELED_BY_USER: &str = "Operation was canceled by the user";

    /// Under the UI, Esc on the `init RAG?` Confirm modal cancels the agent:
    /// the cancel error is shown, no rag is written, the agent is not left
    /// active (`.info agent` has no agent to show), the terminal is never
    /// released and the UI stays live.
    #[test]
    #[serial]
    fn esc_on_the_init_rag_modal_cancels_the_agent_with_the_canceled_by_user_error() {
        let dir = fresh_config_dir("agent-init-rag-esc");
        let _guard = TempDirGuard(dir.clone());
        let (mut pty, before) = open_the_init_rag_modal(&dir, "docs-agent-e1r");
        let files_before = walk(&dir);

        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_squeezed(
            &mut pty,
            CANCELED_BY_USER,
            "the cancel error after Esc on `init RAG?`",
        );
        pty.read_for(Duration::from_millis(800));
        let info_at = pty.raw().len();
        pty.send(b".info agent\r").unwrap();
        pty.read_for(Duration::from_millis(2500));
        let info = strip_ansi(&String::from_utf8_lossy(&pty.raw()[info_at..]));
        assert!(
            !squeezed(&info).contains("name:docs-agent-e1r"),
            "the agent is still active after Esc on `init RAG?`:\n{info}"
        );
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        let written = new_files_named(&dir, &files_before, "/rags/");
        assert!(
            written.is_empty(),
            "Esc on `init RAG?` still wrote a rag: {written:?}:\n{since}"
        );
        assert!(
            !since.contains(RETIRED_RAG_NOTICE),
            "the retired notice was printed after Esc:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "the `init RAG?` question released the terminal:\n{since}"
        );
        assert_ui_alive_then_quit(&mut pty, "Esc on `init RAG?`");
    }

    /// In the plain REPL, Esc on the inquire `init RAG?` confirm cancels the
    /// agent the same way: the cancel error is printed, the prompt comes
    /// back without the agent in it, and no rag is written.
    #[test]
    #[serial]
    fn esc_on_init_rag_in_the_plain_repl_cancels_the_agent_with_the_canceled_by_user_error() {
        let dir = fresh_config_dir("agent-init-rag-esc-repl");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_embeddings(&dir);
        write_agent_with_documents(&dir, "docs-agent-e2r");
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
        let files_before = walk(&dir);
        let before = pty.raw().len();
        pty.send(b".agent docs-agent-e2r\r").unwrap();
        pty.wait_for_after(INIT_RAG_QUESTION, before, PTY_DEADLINE)
            .expect("`.agent <name>` in the plain REPL did not ask `init RAG?`");
        pty.read_for(Duration::from_millis(500));
        let canceled = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        pty.wait_for_after(CANCELED_BY_USER, before, PTY_DEADLINE)
            .expect("Esc on `init RAG?` in the plain REPL did not print the cancel error");
        let info_at = wait_for_repl_prompt(&mut pty, "dry-model)>", canceled);
        pty.send(b".info agent\r").unwrap();
        pty.wait_for_after("Error: No agent", info_at, PTY_DEADLINE)
            .expect(
                "`.info agent` after Esc on `init RAG?` did not report that no agent is active",
            );
        let answered = find(pty.raw(), b"No agent", info_at)
            .expect("the error was seen in the stripped transcript but not in the raw bytes");
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        let info = strip_ansi(&String::from_utf8_lossy(&pty.raw()[info_at..]));
        assert!(
            !squeezed(&info).contains("name:docs-agent-e2r"),
            "the agent is still active after Esc on `init RAG?`:\n{since}"
        );
        let written = new_files_named(&dir, &files_before, "/rags/");
        assert!(
            written.is_empty(),
            "Esc on `init RAG?` in the plain REPL still wrote a rag: {written:?}:\n{since}"
        );

        wait_for_repl_prompt(&mut pty, "dry-model)>", answered);
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// `coyote --auth-mcp <server> </dev/null` typed at a terminal (stdout a
    /// tty, stdin not) is not refused: the sign-in goes on to discovery,
    /// which fails because the server URL points nowhere, so the run exits
    /// non-zero without the non-interactive refusal and leaves the tty
    /// cooked.
    #[test]
    #[serial]
    fn auth_mcp_on_a_terminal_with_stdin_from_dev_null_goes_past_the_terminal_gate() {
        let dir = fresh_config_dir("auth-mcp-piped-stdin");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        fs::write(
        dir.join("mcp.json"),
        r#"{"mcpServers": {"oauth-srv-p1d": {"type": "http", "url": "http://127.0.0.1:9/mcp", "oauth": {"clientId": "cid-p1d"}}}}"#,
    )
    .unwrap();
        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            WAIT_WRAPPER,
            "/dev/null",
            " --repl --auth-mcp oauth-srv-p1d",
        );
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let plain = strip_ansi(&transcript);
        assert!(
            refusal_shape(&plain).is_none(),
            "`--auth-mcp` with stdin from /dev/null was refused:\n{plain}"
        );
        assert_ne!(
            exit_code(&plain),
            0,
            "`--auth-mcp` against a server that is not there exited 0:\n{plain}"
        );
        assert_tty_restored(&plain);
    }
}

mod binary_wizards {
    //! Real-process checks of the command wizards and modals under the
    //! full-screen UI: the questions `.install`, `.uninstall`, `.update`,
    //! `.vault` and `.rag` raise become modals on a real tty and inquire
    //! prompts in the plain REPL, document loaders that stall or die are
    //! reined in, and however a case leaves the tty it ran on must come back
    //! in its cooked, echoing state (the prompter-routed menus answered from
    //! a clean config dir, and the runs with no human to ask, live in
    //! `binary_menus`, and the transcript printed on the normal screen when
    //! the UI leaves in `binary_exit` in `tui_lifecycle_and_signals.rs`).
    //! Each case runs the binary
    //! on a pseudo-terminal under a bash wrapper that reports the exit code
    //! and `stty -a` afterwards, so the module is unix-only. The cases run one
    //! at a time: each spawn forks a session leader and takes a controlling
    //! tty, and a fork landing between another test's `openpty` and its
    //! `exec` has failed the spawn with a garbled errno.
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
    use serial_test::serial;

    // ---------------------------------------------------------------------------
    // Bundle installs that reference secrets, and document loaders that are
    // slow or die under the full-screen UI.
    // ---------------------------------------------------------------------------

    /// Pumps the pty until `marker` is on screen after `from` (whitespace
    /// squeezed), answering each modal on the way with the key paired with
    /// its squeezed question in `answers`, in whatever order they come up.
    /// A modal is looked for only after the previous answer, and each answer
    /// is followed by a resize: the next question may differ from the last
    /// by a few cells, and a full repaint is what puts it on the pty whole.
    /// Returns the text since `from`.
    fn wait_for_marker_answering_modals(
        pty: &mut Pty,
        from: usize,
        marker: &str,
        answers: &[(&str, u8)],
        what: &str,
    ) -> String {
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        let mut pending: Vec<(&str, u8)> = answers.to_vec();
        let mut answered_at = from;
        let mut width = 140;
        loop {
            pty.read_for(Duration::from_millis(200));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]));
            if squeezed(&since).contains(&squeezed(marker)) {
                return since;
            }
            let recent = squeezed(&strip_ansi(&String::from_utf8_lossy(
                &pty.raw()[answered_at..],
            )));
            if let Some(index) = pending
                .iter()
                .position(|(question, _)| recent.contains(question))
            {
                let (_, key) = pending.remove(index);
                pty.read_for(HOTKEY_GAP);
                pty.send(&[key]).unwrap();
                pty.read_for(Duration::from_millis(500));
                answered_at = pty.raw().len();
                width = if width == 140 { 139 } else { 140 };
                pty.resize(60, width).unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{what}: `{marker}` never appeared (modals still unanswered: {pending:?}):\n{since}"
            );
        }
    }

    /// Files under `dir` (outside the cache and the bundle's own source
    /// repository) whose contents mention `needle`.
    fn config_files_mentioning(dir: &Path, needle: &str) -> Vec<PathBuf> {
        walk(dir)
            .into_iter()
            .filter(|path| {
                let text = path.to_string_lossy();
                !text.contains("/cache/") && !text.contains("/bundle-src/")
            })
            .filter(|path| {
                fs::read(path)
                    .map(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// The head of the notice an install prints once every missing secret
    /// has been added or deferred; the deferred names follow it.
    const DEFERRED_SECRETS_NOTICE: &str = "still required by your MCP servers";

    /// `.install <local bundle>` whose mcp.json references two secrets the
    /// vault does not hold: under the UI each `Add {{ X }} to the vault now?` is
    /// a Confirm modal; declining both, the install completes, names both
    /// secrets as deferred, leaves the merged mcp.json and the bundle record
    /// behind (nothing half-applied), adds no secret to the vault, and the
    /// UI keeps taking keys and leaves clean. Then `.install <bundle>`
    /// updates it from a source that gained a third secret and dropped a
    /// role the user had edited locally: the obsolete file's fate is a Select
    /// modal (`keep` keeps it), the secrets are declined again, the update
    /// completes, names the new secret, and keeps the edited file.
    #[test]
    #[serial]
    fn dot_install_of_a_bundle_with_secrets_defers_every_secret_the_user_declines() {
        let dir = fresh_config_dir("install-bundle-secrets");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![]);
        write_fake_llm_config(&dir, &fake);
        let name = "bundle-k7s";
        let repo = write_bundle_repo(&dir, name, &["BUNDLE_SECRET_A", "BUNDLE_SECRET_B"]);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        let files_before = walk(&dir);
        let install_at = pty.raw().len();
        let sent = std::time::Instant::now();
        pty.send(format!(".install {}\r", repo.display()).as_bytes())
            .unwrap();
        let since = wait_for_marker_answering_modals(
            &mut pty,
            install_at,
            DEFERRED_SECRETS_NOTICE,
            &[
                ("Add{{BUNDLE_SECRET_A}}tothevaultnow?", b'n'),
                ("Add{{BUNDLE_SECRET_B}}tothevaultnow?", b'n'),
            ],
            "`.install` of a bundle with secrets",
        );
        let took = sent.elapsed();
        pty.read_for(Duration::from_millis(800));
        let since_full = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
        for secret in ["BUNDLE_SECRET_A", "BUNDLE_SECRET_B"] {
            assert!(
                squeezed(&since_full).contains(secret),
                "the install did not name {secret} as deferred (took {took:?}):\n{since}"
            );
        }
        assert!(
            squeezed(&since_full).matches("tothevaultnow?").count() >= 2,
            "the install did not ask about each secret in the modal:\n{since_full}"
        );
        assert!(
            !since_full.contains("exit="),
            "`.install` quit the UI:\n{since_full}"
        );

        // Nothing half-applied: the merged mcp.json holds the bundle's server
        // and the bundle is recorded; the vault gained no secret.
        let mcp_files = config_files_mentioning(&dir, "bundle-k7s-server");
        assert!(
            mcp_files
                .iter()
                .any(|path| path.file_name().is_some_and(|f| f == "mcp.json")),
            "the merged mcp.json does not hold the bundle's server; files mentioning it: {mcp_files:?}\n{since_full}"
        );
        let recorded = new_files_named(&dir, &files_before, name);
        assert!(
            !recorded.is_empty(),
            "no file records the installed bundle `{name}`:\n{since_full}"
        );
        let installed_role = dir.join("roles").join(format!("{name}-role.md"));
        assert!(
            installed_role.exists(),
            "the bundle's role was not installed at {}:\n{since_full}",
            installed_role.display()
        );
        let secret_holders = config_files_mentioning(&dir, "BUNDLE_SECRET_A");
        assert!(
            secret_holders
                .iter()
                .all(|path| path.file_name().is_some_and(|f| f == "mcp.json")),
            "a file other than mcp.json holds BUNDLE_SECRET_A (a vault write?): {secret_holders:?}"
        );

        // `.list bundles` names the bundle and the UI is live.
        let list_at = pty.raw().len();
        pty.send(b".list bundles\r").unwrap();
        pty.wait_for_after(name, list_at, PTY_DEADLINE)
            .expect("`.list bundles` did not name the installed bundle");
        pty.read_for(Duration::from_millis(500));

        // The user edits a bundle file locally; the source then drops that
        // file and gains a third secret.
        let obsolete = dir.join("roles").join(format!("{name}-obsolete.md"));
        assert!(
            obsolete.exists(),
            "the bundle's second role was not installed"
        );
        fs::write(
            &obsolete,
            "---\nmodel: fake:fake-model\n---\nEdited locally by the user.\n",
        )
        .unwrap();
        fs::remove_file(repo.join("roles").join(format!("{name}-obsolete.md"))).unwrap();
        fs::write(
            repo.join("mcp.json"),
            bundle_mcp_json(&["BUNDLE_SECRET_A", "BUNDLE_SECRET_B", "BUNDLE_SECRET_C"]),
        )
        .unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "update"]);

        let update_at = pty.raw().len();
        pty.send(format!(".install {name}\r").as_bytes()).unwrap();
        // The repaints forced on the way re-emit the first install's notice,
        // so the update is awaited by its own deferred list: B and C are
        // adjacent rows only there.
        let since = wait_for_marker_answering_modals(
            &mut pty,
            update_at,
            "{{BUNDLE_SECRET_B}}{{BUNDLE_SECRET_C}}",
            &[
                ("Obsoletefile", b'1'),
                ("Add{{BUNDLE_SECRET_A}}tothevaultnow?", b'n'),
                ("Add{{BUNDLE_SECRET_B}}tothevaultnow?", b'n'),
                ("Add{{BUNDLE_SECRET_C}}tothevaultnow?", b'n'),
            ],
            "`.install <bundle>` update with a new secret",
        );
        pty.read_for(Duration::from_millis(800));
        let since_full = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
        assert!(
            squeezed(&since_full).contains("Add{{BUNDLE_SECRET_C}}tothevaultnow?"),
            "the update did not ask about the new secret in the modal:\n{since_full}"
        );
        assert!(
            !since_full.contains("exit="),
            "the update quit the UI:\n{since}"
        );
        assert_eq!(
            fs::read_to_string(&obsolete).ok().as_deref(),
            Some("---\nmodel: fake:fake-model\n---\nEdited locally by the user.\n"),
            "the locally edited obsolete file was not kept by the update:\n{since_full}"
        );
        let merged = config_files_mentioning(&dir, "BUNDLE_SECRET_C");
        assert!(
            merged
                .iter()
                .any(|path| path.file_name().is_some_and(|f| f == "mcp.json")),
            "the update did not merge the new secret into mcp.json: {merged:?}\n{since_full}"
        );

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("`.info` after the install and update never rendered");
        assert_eq!(fake.served(), 0, "a bundle install reached the model");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------
    // The very first run on a terminal: the config wizard's vault step with
    // the password file put somewhere other than the offered default.
    // ---------------------------------------------------------------------

    /// The first run ever (no config file) on a real terminal, answered up
    /// to the vault step with the local provider: the password-file Input
    /// offers a default path; typing a different one and a ten-plus
    /// character password creates the file THERE (mode 0600, the password
    /// as its bytes), the success line names the chosen path (`Password
    /// file '<chosen>' created.`), nothing lands at the offered default, and
    /// the wizard goes on to the API provider pick. Ctrl+C there leaves
    /// without writing a config file and with the tty restored.
    #[test]
    #[serial]
    fn the_first_run_wizard_creates_the_password_file_at_the_chosen_path_and_says_so() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fresh_config_dir("first-run-chosen-password-file");
        let _guard = TempDirGuard(dir.clone());
        assert!(!dir.join("config.yaml").exists());
        let chosen = dir.join("chosen-vault-pass");
        let password = "twelve-chars-long";

        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");
        pty.wait_for("create a new one?", PTY_DEADLINE)
            .expect("the first run did not ask to create a config file");
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        pty.wait_for("secrets provider", PTY_DEADLINE)
            .expect("the wizard did not ask which secrets provider to use");
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        pty.wait_for("create one now?", PTY_DEADLINE)
            .expect("the wizard did not offer to create the password file");
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        let path_at = pty.raw().len();
        pty.wait_for("path to the password file to create", PTY_DEADLINE)
            .expect("the wizard did not ask where to create the password file");
        // The default is painted a moment after the label, so it is waited
        // for rather than read off the first frame that shows the question.
        pty.wait_for_after("coyote_password", path_at, PTY_DEADLINE)
            .unwrap_or_else(|e| panic!("the path question shows no default path: {e:?}"));
        pty.read_for(Duration::from_millis(300));
        pty.send(format!("{}\r", chosen.display()).as_bytes())
            .unwrap();
        pty.wait_for("encrypt all vault secrets", PTY_DEADLINE)
            .expect("the wizard did not ask for the vault password");
        pty.read_for(Duration::from_millis(300));
        pty.send(format!("{password}\r").as_bytes()).unwrap();
        let created = pty
            .wait_for("created.", PTY_DEADLINE)
            .expect("the wizard never said the password file was created");
        let expected = format!("Password file '{}' created.", chosen.display());
        assert!(
            squeezed(&created).contains(&squeezed(&expected)),
            "the success line does not name the chosen path `{expected}`:\n{created}"
        );
        let default_line = created
            .lines()
            .find(|line| line.contains("created.") && line.contains("coyote_password"));
        assert!(
            default_line.is_none(),
            "the success line names the default path, not the chosen one: {default_line:?}\n{created}"
        );
        assert_eq!(
            fs::read_to_string(&chosen).ok().as_deref(),
            Some(password),
            "the chosen path does not hold the password typed:\n{created}"
        );
        assert_eq!(
            fs::metadata(&chosen).unwrap().permissions().mode() & 0o777,
            0o600,
            "the created password file is not private (0600):\n{created}"
        );
        let at_default: Vec<PathBuf> = walk(&dir)
            .into_iter()
            .filter(|p| p.to_string_lossy().contains("coyote_password"))
            .collect();
        assert!(
            at_default.is_empty(),
            "a file landed at the default password path {at_default:?} though another was chosen:\n{created}"
        );

        pty.wait_for("API Provider", PTY_DEADLINE)
            .expect("the wizard did not go on to the API provider pick");
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x03").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_ne!(
            exit_code(&transcript),
            0,
            "Ctrl+C on the provider pick exited 0:\n{transcript}"
        );
        assert!(
            !dir.join("config.yaml").exists(),
            "the interrupted wizard wrote config.yaml:\n{transcript}"
        );
        assert_tty_restored(&transcript);
    }

    /// The uninstall twin: `.uninstall <bundle>`, `y`, then Esc on the
    /// `was modified locally after install` Select modal keeps the edited
    /// file (the unmodified one is removed), the report still prints, one
    /// note says the question was cancelled, and the UI stays live.
    #[test]
    #[serial]
    fn esc_on_the_uninstall_modified_file_select_modal_keeps_the_file_and_says_so() {
        let dir = fresh_config_dir("uninstall-esc-modal");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-u8e";
        let repo = write_bundle_repo(&dir, name, &[]);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        install_bundle_without_questions(&mut pty, &repo);
        let role = dir.join("roles").join(format!("{name}-role.md"));
        let edited = dir.join("roles").join(format!("{name}-obsolete.md"));
        fs::write(&edited, EDITED_OBSOLETE).unwrap();

        let at = pty.raw().len();
        pty.send(format!(".uninstall {name}\r").as_bytes()).unwrap();
        wait_for_the_modal(
            &mut pty,
            at,
            ".uninstall",
            &[&format!("Uninstall bundle '{name}'?"), "y=yes"],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_the_modal(
            &mut pty,
            at,
            ".uninstall (modified file)",
            &["wasmodifiedlocallyafterinstall", "keep", "delete-all"],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_squeezed(
            &mut pty,
            "the question was cancelled",
            "the note after Esc on the modified-file modal",
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..]));
        let flat = squeezed(&since);
        assert_eq!(
            flat.matches("thequestionwascancelled").count(),
            1,
            "the cancelled note was printed more or less than once:\n{since}"
        );
        assert!(
            flat.contains("keptmodifiedfile") && flat.contains("partiallyuninstalled"),
            "the cancelled question did not resolve to keep with the report:\n{since}"
        );
        assert_eq!(
            fs::read_to_string(&edited).ok().as_deref(),
            Some(EDITED_OBSOLETE),
            "Esc on the modified-file modal did not keep the edited file:\n{since}"
        );
        assert!(
            !role.exists(),
            "the unmodified role survived the uninstall:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "a question released the terminal:\n{since}"
        );
        assert_ui_alive_then_quit(&mut pty, "the cancelled modified-file question");
    }

    /// Reads the pid a loader fixture wrote next to its input.
    fn loader_pid(path: &Path) -> i32 {
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }

    /// A loader that records its pid at `$1.pid` and then sleeps far past the
    /// deadline; the file-branch twin would copy `$1` to `$2` afterwards, the
    /// stdout twin would `cat` it.
    fn slow_loader(dir: &Path, file_branch: bool) -> PathBuf {
        let script = dir.join(if file_branch {
            "slow-file.sh"
        } else {
            "slow-stdout.sh"
        });
        let tail = if file_branch {
            "cp \"$1\" \"$2\"\n"
        } else {
            "cat \"$1\"\n"
        };
        fs::write(
            &script,
            format!("#!/usr/bin/env bash\necho $$ > \"$1.pid\"\nsleep 30\n{tail}"),
        )
        .unwrap();
        script
    }

    /// A loader that writes its would-be output and is then killed by a
    /// signal before it can exit cleanly.
    fn dying_loader(dir: &Path, file_branch: bool) -> PathBuf {
        let script = dir.join(if file_branch {
            "dying-file.sh"
        } else {
            "dying-stdout.sh"
        });
        let body = if file_branch {
            "echo 'PARTIAL LOADER OUTPUT d4k' > \"$2\"\nkill -KILL $$\n"
        } else {
            "echo 'PARTIAL LOADER OUTPUT d4k'\nkill -KILL $$\n"
        };
        fs::write(&script, format!("#!/usr/bin/env bash\n{body}")).unwrap();
        script
    }

    /// `.file <doc>` through a loader that sleeps: Esc ends the turn within
    /// about two seconds, the loader process is gone by then, nothing reached
    /// the model, and the UI keeps taking keys and leaves clean.
    fn esc_interrupts_a_slow_loader(scenario: &str, file_branch: bool) {
        let dir = fresh_config_dir(scenario);
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text("never reached m2x".to_string())]);
        let script = slow_loader(&dir, file_branch);
        let args = if file_branch { "$1 $2" } else { "$1" };
        write_fake_llm_config_with(
            &dir,
            &fake,
            &format!(
                "document_loaders:\n\x20 slow: 'bash {} {args}'\n",
                script.display()
            ),
        );
        let document = dir.join("doc.slow");
        fs::write(&document, "slow document body\n").unwrap();
        let pid_file = dir.join("doc.slow.pid");
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(format!(".file {} -- summarize\r", document.display()).as_bytes())
            .unwrap();
        wait_for_file(&mut pty, &pid_file, "the slow loader's pid file");
        let pid = loader_pid(&pid_file);
        assert!(alive(pid), "the loader exited before Esc");
        pty.read_for(Duration::from_millis(500));
        let pressed_at = pty.raw().len();
        let pressed = std::time::Instant::now();
        pty.send(b"\x1b").unwrap();

        let mut gone_after = None;
        let mut ended_after = None;
        let deadline = pressed + Duration::from_secs(8);
        while (gone_after.is_none() || ended_after.is_none())
            && std::time::Instant::now() < deadline
        {
            pty.read_for(Duration::from_millis(100));
            if gone_after.is_none() && !alive(pid) {
                gone_after = Some(pressed.elapsed());
            }
            if ended_after.is_none() {
                let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
                let lower = since.to_lowercase();
                if lower.contains("aborted") || lower.contains("interrupted") {
                    ended_after = Some(pressed.elapsed());
                }
            }
        }
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
        let gone_after = gone_after.unwrap_or_else(|| {
            panic!("{scenario}: the loader (pid {pid}) is still alive 8s after Esc:\n{since}")
        });
        let ended_after = ended_after.unwrap_or_else(|| {
            panic!("{scenario}: no aborted/interrupted cell followed Esc within 8s:\n{since}")
        });
        assert!(
            gone_after < Duration::from_millis(2500),
            "{scenario}: the loader outlived Esc by {gone_after:?}:\n{since}"
        );
        assert!(
            ended_after < Duration::from_millis(2500),
            "{scenario}: the turn ended {ended_after:?} after Esc:\n{since}"
        );
        assert!(
            !since.contains("exit="),
            "{scenario}: Esc quit the UI:\n{since}"
        );

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("keys were not handled after the interrupted loader");
        assert_eq!(
            fake.served(),
            0,
            "{scenario}: the interrupted document reached the model"
        );
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert!(
            !squeezed(&transcript).contains("neverreachedm2x"),
            "{scenario}: the model's reply landed after the interrupt:\n{transcript}"
        );
    }

    /// File-branch loader (`cmd $1 $2`).
    #[test]
    #[serial]
    fn esc_interrupts_a_slow_file_loader_and_kills_it() {
        esc_interrupts_a_slow_loader("esc-slow-file-loader", true);
    }

    /// Stdout-branch loader (`cmd $1`).
    #[test]
    #[serial]
    fn esc_interrupts_a_slow_stdout_loader_and_kills_it() {
        esc_interrupts_a_slow_loader("esc-slow-stdout-loader", false);
    }

    /// `.file <doc>` through a loader killed by a signal after writing its
    /// output: the turn fails (an error cell, no model call with the partial
    /// output), and the UI keeps taking keys and leaves clean.
    fn a_signal_killed_loader_is_a_failure(scenario: &str, file_branch: bool) {
        let dir = fresh_config_dir(scenario);
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text("ingested anyway r8p".to_string())]);
        let script = dying_loader(&dir, file_branch);
        let args = if file_branch { "$1 $2" } else { "$1" };
        write_fake_llm_config_with(
            &dir,
            &fake,
            &format!(
                "document_loaders:\n\x20 dying: 'bash {} {args}'\n",
                script.display()
            ),
        );
        let document = dir.join("doc.dying");
        fs::write(&document, "dying document body\n").unwrap();
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let sent_at = pty.raw().len();
        pty.send(format!(".file {} -- summarize\r", document.display()).as_bytes())
            .unwrap();
        // The turn must end one way or the other: wait until the status row no
        // longer says Working, then look at what landed.
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        loop {
            pty.read_for(Duration::from_millis(300));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[sent_at..]));
            // The failure the command module raises for a child that did not
            // exit 0 (`exited with non-zero`), squeezed because the error cell
            // wraps; no looser word, so the scenario's own name in the echoed
            // command line cannot satisfy it.
            let squeezed_since = squeezed(&since);
            if squeezed_since.contains("ingestedanywayr8p")
                || squeezed_since.contains("exitedwithnon-zero")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{scenario}: neither a failure nor a reply followed the killed loader:\n{since}"
            );
        }
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[sent_at..]));
        assert_eq!(
            fake.served(),
            0,
            "{scenario}: the output of a signal-killed loader was sent to the model:\n{since}"
        );
        assert!(
            !squeezed(&since).contains("ingestedanywayr8p"),
            "{scenario}: the model replied to a document whose loader was killed:\n{since}"
        );
        assert!(
            squeezed(&since).contains("exitedwithnon-zero"),
            "{scenario}: nothing told the user the loader failed:\n{since}"
        );
        assert!(
            !since.contains("exit="),
            "{scenario}: the failed loader quit the UI:\n{since}"
        );

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("keys were not handled after the failed loader");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// File-branch loader killed after writing `$2`.
    #[test]
    #[serial]
    fn a_signal_killed_file_loader_is_reported_as_a_failure_not_ingested() {
        a_signal_killed_loader_is_a_failure("dying-file-loader", true);
    }

    /// Stdout-branch loader killed after writing its stdout.
    #[test]
    #[serial]
    fn a_signal_killed_stdout_loader_is_reported_as_a_failure_not_ingested() {
        a_signal_killed_loader_is_a_failure("dying-stdout-loader", false);
    }

    // ---------------------------------------------------------------------------
    // Esc on the incorporate question, `.rag` with no name and a seeded temp
    // rag, a stdout loader that reads stdin outside the UI, conceal with a
    // leading zero, and a tool whose background job keeps writing after its
    // turn.
    // ---------------------------------------------------------------------------

    /// Esc on the incorporate question cancels it: no session opens (the
    /// footer still says `no session` after a full repaint, the modal is gone,
    /// nothing is written under the config dir), the UI keeps taking keys, and
    /// `.session <name>` asked again and answered with `n` seats the session
    /// empty.
    #[test]
    #[serial]
    fn esc_on_the_incorporate_question_opens_no_session_and_the_next_n_seats_it_empty() {
        let dir = fresh_config_dir("session-after-turn-esc");
        let _guard = TempDirGuard(dir.clone());
        let name = "after-turn-esc3";
        let (mut pty, _fake) = open_session_after_a_turn(&dir, name);
        let files_before = walk(&dir);

        let pressed_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(800));
        // A different size from the one `answer_and_expect_session` grows to,
        // so both repaints are forced.
        pty.resize(60, 110).unwrap();
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
        assert!(
            squeezed(&since).contains("nosession"),
            "the footer does not say `no session` after Esc on the incorporate question:\n{since}"
        );
        assert!(
            !squeezed(&since).contains("Esc=cancel"),
            "the modal is still up after Esc:\n{since}"
        );
        // The command echo `> .session <name>` sits in the transcript; the
        // footer is the last row, and the name must not be painted there.
        let on_footer: Vec<_> = painted_at(pty.raw(), name.as_bytes(), pressed_at)
            .into_iter()
            .filter(|(row, _)| *row == 60)
            .collect();
        assert!(
            on_footer.is_empty(),
            "the footer names `{name}` after Esc on the incorporate question:\n{since}"
        );
        let written = new_files_named(&dir, &files_before, name);
        assert!(
            written.is_empty(),
            "Esc on the incorporate question still wrote {written:?}:\n{since}"
        );

        // Asked again, the question comes back (the exchange is still there
        // and no session took it), and `n` seats the session empty.
        let asked_at = pty.raw().len();
        pty.send(format!(".session {name}\r").as_bytes()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            pty.read_for(Duration::from_millis(200));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
            if squeezed(&since).contains("incorporatesthelastquestionandanswer") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "`.session` after Esc did not ask about the last exchange again within 10s:\n{since}"
            );
        }
        pty.read_for(HOTKEY_GAP);
        answer_and_expect_session(pty, b"n", name, true);
    }

    /// `.rag` with no name recreates the temporary rag by asking which
    /// embedding model and documents to use, and removes the previous one
    /// only once the wizard has answered; under the UI the embedding model is
    /// asked in the Select modal while the seeded temp rag stays on disk
    /// untouched. Esc cancels it with the old temp rag still in place and
    /// byte-identical, the terminal is never released, and the UI keeps
    /// taking keys and leaves clean.
    #[test]
    #[serial]
    fn dot_rag_without_a_name_asks_in_the_select_modal_and_keeps_the_temp_rag_until_it_answers() {
        let dir = fresh_config_dir("modal-rag-temp");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_embeddings(&dir);
        let rags = dir.join("rags");
        fs::create_dir_all(&rags).unwrap();
        let temp_rag = rags.join("temp.yaml");
        let seeded = "embedding_model: dryrun:dry-embed-a\nchunk_size: 1000\nchunk_overlap: 50\ndocument_paths: []\nfiles: []\nmarker: temp-rag-seeded-t3g\n";
        fs::write(&temp_rag, seeded).unwrap();
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let before = pty.raw().len();
        pty.send(b".rag\r").unwrap();
        let modal = wait_for_the_modal(
            &mut pty,
            before,
            ".rag",
            &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
        );
        assert_eq!(
            fs::read_to_string(&temp_rag).ok().as_deref(),
            Some(seeded),
            "`.rag` removed or rewrote the temp rag {} before the modal was answered:\n{modal}",
            temp_rag.display()
        );
        esc_cancels_the_modal(&mut pty, ".rag");

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("`.info` after the cancelled `.rag` never rendered");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(
            fs::read_to_string(&temp_rag).ok().as_deref(),
            Some(seeded),
            "the cancelled `.rag` removed or rewrote the temp rag by the time the UI left:\n{transcript}"
        );
    }

    /// A stdout-branch loader (`cmd $1`) never inherits the terminal on its
    /// stdin, outside the UI as much as under it: in the plain command mode
    /// on a tty, `cat "$1" -` returns once the file is read, so the command
    /// finishes within the deadline with no key typed (a loader left reading
    /// the terminal would wait there until the deadline fails the test), the
    /// document reaches the model exactly once, the reply is printed and the
    /// command exits 0. No tighter wall-clock bound is asserted: a loaded
    /// runner stretches it.
    #[test]
    #[serial]
    fn a_stdout_loader_reading_stdin_returns_in_command_mode_on_a_tty() {
        let dir = fresh_config_dir("loader-stdin-cmd");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text("loaded document reply s7d".to_string())]);
        write_fake_llm_config_with(
            &dir,
            &fake,
            "document_loaders:\n\x20 catstdin: 'cat \"$1\" -'\n",
        );
        let document = dir.join("doc.catstdin");
        fs::write(&document, "document body s7d-body\n").unwrap();
        let args = format!(" -f '{}' 'summarize'", document.display());
        let mut pty =
            spawn_binary_under(built_binary(), &dir, &[], WAIT_WRAPPER, "/dev/tty", &args);
        let transcript = pty
            .wait_for("tty-probe-done", PTY_DEADLINE)
            .unwrap_or_else(|e| {
                panic!(
                    "the command never finished: the loader is waiting on the terminal's stdin: \
                 {e:?}\n{}",
                    strip_ansi(&String::from_utf8_lossy(pty.raw()))
                )
            });
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_eq!(
            fake.served(),
            1,
            "the document never reached the model:\n{transcript}"
        );
        assert!(
            squeezed(&transcript).contains("loadeddocumentreplys7d"),
            "the reply was not printed:\n{transcript}"
        );
        assert_tty_restored(&transcript);
    }

    /// Conceal written with a leading zero (`ESC[08m`) is still SGR 8: the
    /// word is painted, no conceal parameter reaches the tty, the sequence is
    /// never painted as text, a full repaint keeps the word, and nothing of
    /// it reaches the normal screen after Ctrl+D.
    #[test]
    #[serial]
    fn a_reply_concealed_with_a_leading_zero_shows_it_and_sends_no_conceal_to_the_tty() {
        let dir = fresh_config_dir("conceal-zero-visible");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text(
            "before c9a \x1b[08mhidden-word-c9b\x1b[0m after c9c".to_string(),
        )]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let submitted_at = pty.raw().len();
        pty.send(b"conceal please\r").unwrap();
        let transcript = wait_for_squeezed(&mut pty, "after c9c", "the reply's tail");
        assert_eq!(fake.served(), 1, "{transcript}");
        pty.read_for(Duration::from_millis(600));

        let raw = pty.raw();
        let visible = strip_ansi(&String::from_utf8_lossy(&raw[submitted_at..]));
        assert!(
            squeezed(&visible).contains("hidden-word-c9b"),
            "the word concealed with ESC[08m was not painted:\n{visible}"
        );
        assert!(
            !visible.contains("[08m") && !visible.contains("[8m") && !visible.contains("[0m"),
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

        let repaint_at = raw.len();
        pty.resize(30, 90).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let repainted = loop {
            pty.read_for(Duration::from_millis(200));
            let repainted = strip_ansi(&String::from_utf8_lossy(&pty.raw()[repaint_at..]));
            if squeezed(&repainted).contains("afterc9c") {
                break repainted;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the reply was not repainted after the resize:\n{repainted}"
            );
        };
        assert!(
            squeezed(&repainted).contains("hidden-word-c9b"),
            "the concealed word vanished on repaint:\n{repainted}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        let left_at =
            find(pty.raw(), LEAVE_ALT, 0).expect("the UI never left the alternate screen");
        let after_leave = &pty.raw()[left_at..];
        let visible_after = strip_ansi(&String::from_utf8_lossy(after_leave));
        assert!(
            !visible_after.contains("[08m") && !visible_after.contains("[8m"),
            "the conceal sequence reached the normal screen as text:\n{visible_after}"
        );
        let concealing_after: Vec<_> = sgr_params(after_leave)
            .into_iter()
            .filter(|p| is_conceal_sgr(p))
            .collect();
        assert!(
            concealing_after.is_empty(),
            "a conceal SGR reached the normal screen: {concealing_after:?}\n{visible_after}"
        );
    }

    /// A tool that leaves a background job writing to its stdout after the
    /// tool itself (and its turn) finished.
    const LINGERING_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Finish at once, leaving a job that prints later.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   ( sleep 4; echo 'late-line-one-k7a'; sleep 0.3; echo 'late-line-two-k7b'; sleep 0.3; echo 'late-line-three-k7c' ) &\n\
    \x20   echo 'fixture lingering started k7s' >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// A tool whose background job keeps writing after the turn ended does
    /// not disturb the UI: the turn completes, the UI stays live and leaves
    /// clean. The job's writes arrive after the tool's output pump was
    /// detached (the drain grace is two seconds), so the binary drops them
    /// rather than attributing them to a finished tool; should any of them
    /// be shown, they share one trailing cell (adjacent rows, no blank cell
    /// gap between the lines) rather than a cell per pipe read, as the
    /// `out_of_turn_tool_output_shares_one_bounded_cell` unit test pins
    /// in-process.
    #[test]
    #[serial]
    fn a_tool_leaving_a_lingering_writer_behind_keeps_the_ui_live_and_folds_its_output() {
        let dir = fresh_config_dir("tool-late-output");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_lingering".to_string(),
                arguments: "{}".to_string(),
            },
            Reply::Text("fixture done after lingering k7d".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_lingering.sh", LINGERING_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let submitted_at = pty.raw().len();
        pty.send(b"call the lingering tool\r").unwrap();
        wait_for_squeezed(
            &mut pty,
            "fixture done after lingering k7d",
            "the reply after the tool",
        );
        assert_eq!(fake.served(), 2);
        // Outlive the job's writes, pumping the pty.
        pty.read_for(Duration::from_millis(6000));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[submitted_at..]));
        let one = painted_at(pty.raw(), b"late-line-one-k7a", submitted_at);
        let two = painted_at(pty.raw(), b"late-line-two-k7b", submitted_at);
        let three = painted_at(pty.raw(), b"late-line-three-k7c", submitted_at);
        if let (Some(&(row_one, col_one)), Some(&(row_two, col_two))) = (one.last(), two.last()) {
            assert_eq!(
                (row_two, col_two),
                (row_one + 1, col_one),
                "the late lines did not share one cell:\n{since}"
            );
        }
        if let (Some(&(row_two, col_two)), Some(&(row_three, col_three))) =
            (two.last(), three.last())
        {
            assert_eq!(
                (row_three, col_three),
                (row_two + 1, col_two),
                "the late lines did not share one cell:\n{since}"
            );
        }
        assert!(
            !since.contains("exit="),
            "the lingering job's output quit the UI:\n{since}"
        );

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("keys were not handled after the lingering tool");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------------
    // A file-writing loader that reads stdin under the plain REPL, escapes
    // and format characters in a typed line and in an error notice, and late
    // output arriving once the exit has begun.
    // ---------------------------------------------------------------------------

    /// A `$2` loader (the file-writing branch) that copies `$1` to `$2` and
    /// then reads its stdin to the end, the way a filter that is handed a
    /// closed stdin returns at once and one handed the terminal waits for a
    /// key the user never types.
    fn file_loader_reading_stdin(dir: &Path) -> PathBuf {
        let script = dir.join("file-loader-stdin.sh");
        fs::write(&script, "#!/usr/bin/env bash\ncat \"$1\" > \"$2\"\ncat -\n").unwrap();
        script
    }

    /// `.file <doc>` in the plain REPL on a tty, through a `$2` loader whose
    /// process reads stdin after writing its output: the loader is spawned
    /// with stdin closed on this branch too, so the reply arrives within the
    /// deadline with no key typed (a loader left reading the terminal would
    /// wait there until the deadline fails the test), the document reaches
    /// the model exactly once, and `.exit` then leaves the tty cooked. No
    /// tighter wall-clock bound is asserted: a loaded runner stretches it.
    #[test]
    #[serial]
    fn dot_file_through_a_file_loader_reading_stdin_returns_at_once_in_the_plain_repl() {
        let dir = fresh_config_dir("file-loader-stdin-repl");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text("loaded document reply r9f".to_string())]);
        let script = file_loader_reading_stdin(&dir);
        write_fake_llm_config_with(
            &dir,
            &fake,
            &format!(
                "document_loaders:\n\x20 catstdinfile: 'bash {} $1 $2'\n",
                script.display()
            ),
        );
        let document = dir.join("doc.catstdinfile");
        fs::write(&document, "document body r9f-body\n").unwrap();
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        // Type only once the line editor owns the terminal: input ahead of
        // the prompt is cooked-mode typeahead the editor discards.
        wait_for_repl_prompt(&mut pty, "fake-model)>", 0);
        let started = std::time::Instant::now();
        // The line editor repaints the prompt once per typed key, about 100 ms
        // each in a debug build, so the absolute temp path alone would eat most
        // of the wait; the binary runs in `dir`, where the bare file name names
        // the same document.
        let sent = pty.raw().len();
        pty.send(b".file 'doc.catstdinfile' -- summarize\r")
            .unwrap();
        let transcript = wait_for_squeezed(
            &mut pty,
            "loaded document reply r9f",
            "the reply after the file loader (is the loader waiting on the terminal's stdin?)",
        );
        let replied = find(pty.raw(), b"r9f", sent)
            .expect("the reply was seen in the stripped transcript but not in the raw bytes");
        let took = started.elapsed();
        assert_eq!(
            fake.served(),
            1,
            "the document never reached the model (the reply came back after {took:?}):\n{transcript}"
        );
        // Let the reply finish printing and the prompt come back before
        // typing the next command.
        wait_for_repl_prompt(&mut pty, "fake-model)>", replied);
        let exit_started = std::time::Instant::now();
        pty.send(b".exit\r").unwrap();
        let transcript = pty
            .wait_for("tty-probe-done", PTY_DEADLINE * 2)
            .unwrap_or_else(|e| {
                panic!(
                    "the REPL did not exit (after {:?}): {e:?}",
                    exit_started.elapsed()
                )
            });
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A line carrying a colour SGR, a zero-width space and an OSC 8
    /// hyperlink, as pasted text would.
    const ESCAPED_WORDS: &[u8] =
    b"see \x1b[31mred\x1b[0m \xe2\x80\x8bzw \x1b]8;;http://x.invalid\x1b\\link\x1b]8;;\x1b\\ end-q3u";

    /// The same words joined into one path component (no spaces, which
    /// `.file` would read as separate paths).
    const ESCAPED_PATH: &[u8] =
    b"see-\x1b[31mred\x1b[0m-\xe2\x80\x8bzw-\x1b]8;;http://x.invalid\x1b\\link\x1b]8;;\x1b\\-end-q3u";

    /// Nothing of the raw sequences or the format character may be painted
    /// as text after `from`, and the words must be painted in order with
    /// nothing between them (`plain` is the squeezed form expected).
    fn assert_escaped_words_drawn_plain(pty: &Pty, from: usize, plain: &str, what: &str) {
        let since_raw = &pty.raw()[from..];
        let since = strip_ansi(&String::from_utf8_lossy(since_raw));
        for leaked in ["[31m", "[0m", "]8;;", "x.invalid"] {
            assert!(
                !since.contains(leaked),
                "{what}: `{leaked}` was painted as text:\n{since}"
            );
        }
        assert_eq!(
            count(since_raw, "\u{200B}".as_bytes()),
            0,
            "{what}: the zero-width space reached the terminal:\n{since}"
        );
        assert!(
            squeezed(&since).contains(plain),
            "{what}: the words were not drawn plain and in order:\n{since}"
        );
    }

    /// A pasted User line carrying a colour SGR, a zero-width space and an
    /// OSC 8 hyperlink is drawn as its plain words once submitted: no `[31m`
    /// or `]8;;` painted as text, the zero-width space never reaches the
    /// terminal, and the words sit in order. The dry-run echo of the same
    /// line is drawn plain as well, and the UI leaves clean.
    #[test]
    #[serial]
    fn a_pasted_user_line_with_escapes_and_a_zero_width_space_is_drawn_as_plain_words() {
        let dir = fresh_config_dir("scrub-user-line");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        let mut paste = b"\x1b[200~".to_vec();
        paste.extend_from_slice(ESCAPED_WORDS);
        paste.extend_from_slice(b"\x1b[201~");
        pty.send(&paste).unwrap();
        pty.wait_for("end-q3u", PTY_DEADLINE)
            .expect("the pasted text never reached the composer");
        pty.read_for(Duration::from_millis(500));
        let submitted_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        wait_for_squeezed(&mut pty, "Working", "the turn");
        // The dry-run reply echoes the line; let it land after the working
        // row so both the User cell and the reply are on screen.
        pty.read_for(Duration::from_millis(2500));
        assert_escaped_words_drawn_plain(
            &pty,
            submitted_at,
            "seeredzwlinkend-q3u",
            "the user line",
        );
        assert!(
            count(&pty.raw()[submitted_at..], b"end-q3u") >= 2,
            "the line was not drawn as both the User cell and the dry-run echo:\n{}",
            strip_ansi(&String::from_utf8_lossy(&pty.raw()[submitted_at..]))
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A command line and the error notice that echoes it (`.file` on a path
    /// that does not exist names the path) are drawn as plain words when the
    /// path carries a colour SGR, a zero-width space and an OSC 8 hyperlink:
    /// nothing of the sequences painted as text in the `> .file ...` header
    /// or in the error, the format character never sent, the UI keeps taking
    /// keys and leaves clean. The header shows the typed line; the error
    /// names the path the `.file` argument splitter hands the loader, which
    /// drops a lone backslash (REPL semantics), so the hyperlink's `ESC \`
    /// terminators reach it as bare escapes: each cuts its string short and
    /// takes the character it introduces, as on a terminal.
    #[test]
    #[serial]
    fn an_error_notice_echoing_escapes_and_a_zero_width_space_is_drawn_as_plain_words() {
        let dir = fresh_config_dir("scrub-error-notice");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        let mut paste = b"\x1b[200~.file /nonexistent-w9/".to_vec();
        paste.extend_from_slice(ESCAPED_PATH);
        paste.extend_from_slice(b"\x1b[201~");
        pty.send(&paste).unwrap();
        pty.wait_for("end-q3u", PTY_DEADLINE)
            .expect("the pasted text never reached the composer");
        pty.read_for(Duration::from_millis(500));
        let submitted_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        pty.wait_for_after("Not found", submitted_at, PTY_DEADLINE)
            .expect("no error notice for the missing path");
        pty.read_for(Duration::from_millis(800));
        let since_raw = &pty.raw()[submitted_at..];
        let since = strip_ansi(&String::from_utf8_lossy(since_raw));
        // The error names the path after `Not found '`; the command header
        // is everything painted before the error. Each is judged on its own
        // so a failure names the cell.
        let error_at = since.find("Not found '").unwrap();
        let (header, error) = since.split_at(error_at);
        let header_squeezed = squeezed(header);
        let error_squeezed = squeezed(error);
        let mut failures = Vec::new();
        for (what, text, plain) in [
            (
                "the command header",
                &header_squeezed,
                "/nonexistent-w9/see-red-zw-link-end-q3u",
            ),
            (
                "the error notice",
                &error_squeezed,
                "/nonexistent-w9/see-red-zw-inknd-q3u",
            ),
        ] {
            for leaked in ["[31m", "[0m", "]8;;", "x.invalid"] {
                if text.contains(leaked) {
                    failures.push(format!("{what}: `{leaked}` was painted as text"));
                }
            }
            if text.contains('\u{200B}') {
                failures.push(format!("{what}: the zero-width space was painted"));
            }
            if !text.contains(plain) {
                failures.push(format!(
                    "{what}: the path was not drawn plain and whole (got {text:?})"
                ));
            }
        }
        assert_eq!(
            count(since_raw, "\u{200B}".as_bytes()),
            0,
            "the zero-width space reached the terminal:\n{since}"
        );
        let error_raw_at = find(since_raw, b"Not found", 0).unwrap();
        let error_raw = String::from_utf8_lossy(
            &since_raw[error_raw_at..(error_raw_at + 240).min(since_raw.len())],
        );
        assert!(
            failures.is_empty(),
            "{}\n--- header ---\n{header}\n--- error ---\n{error}\n--- error, raw bytes ---\n{:?}",
            failures.join("\n"),
            error_raw
        );

        pty.send(b".info\r").unwrap();
        pty.wait_for("log_path", PTY_DEADLINE)
            .expect("keys were not handled after the error notice");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// The REPL's confirmation line once a session was written by name.
    const SAVED_LINE: &[u8] = b"Saved the session to";

    /// `.exit session` on a dirty temp session, answered `y` and named: the
    /// exit path's own output (the REPL's `Saved the session to '<path>'`
    /// confirmation) lands in the transcript inside the alternate screen, on
    /// a row below the `.exit session` command, the footer goes back to "no
    /// session", the UI keeps taking keys, and nothing of the line leaks to
    /// the normal screen after Ctrl+D. The transcript printed on exit would
    /// carry the line on purpose, so it is switched off here.
    #[test]
    #[serial]
    fn dot_exit_session_saving_on_y_prints_where_it_saved_in_the_transcript() {
        let dir = fresh_config_dir("exit-session-saved-line");
        let _guard = TempDirGuard(dir.clone());
        write_config(&dir, "tui_dump_on_exit: false\n");
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        open_and_dirty_a_session(&mut pty);
        let command_at = pty.raw().len();
        pty.send(b".exit session\r").unwrap();
        pty.wait_for_after("Save session?", command_at, PTY_DEADLINE)
            .unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        pty.wait_for("ession name", PTY_DEADLINE).unwrap();
        let named_at = pty.raw().len();
        answer_field_modal(&mut pty, "saved-w9-session");
        // The painter skips cells that were already blank, so the words of a
        // line can reach the stream without the spaces between them.
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        let since = loop {
            let text = strip_ansi(&String::from_utf8_lossy(&pty.raw()[named_at..]));
            if squeezed(&text).contains("Savedthesessionto") {
                break text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the save confirmation never reached the transcript:\n{text}"
            );
            pty.read_for(Duration::from_millis(200));
        };
        pty.wait_for_after("no session", named_at, PTY_DEADLINE)
            .expect("the footer did not go back to `no session` after the session exit");
        assert!(
            find(pty.raw(), LEAVE_ALT, 0).is_none(),
            "the session exit left the alternate screen:\n{since}"
        );
        // The order is read off one full repaint: the stream before it holds
        // cell diffs from several frames (the transcript scrolls as the footer
        // flips), and a needle's nearest cursor move may belong to a frame
        // other than the one that painted it.
        let repaint_at = pty.raw().len();
        pty.resize(24, 81).unwrap();
        pty.read_for(Duration::from_millis(600));
        let command_rows = painted_at(pty.raw(), b".exit session", repaint_at);
        let saved_rows = painted_at(pty.raw(), b"Saved", repaint_at);
        let (Some(&(command_row, _)), Some(&(saved_row, _))) =
            (command_rows.first(), saved_rows.last())
        else {
            panic!("the command or its confirmation was not positioned on screen:\n{since}");
        };
        assert!(
            saved_row > command_row,
            "the confirmation was drawn above its command ({command_row} / {saved_row}):\n{since}"
        );

        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", named_at, PTY_DEADLINE)
            .expect("keys were not handled after the session exit");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        let left_at = rfind(pty.raw(), LEAVE_ALT).expect("no leave sequence");
        assert!(
            find(pty.raw(), SAVED_LINE, left_at).is_none(),
            "the confirmation reached the normal screen after the UI left:\n{transcript}"
        );
        assert!(
            walk(&dir)
                .into_iter()
                .any(|p| p.file_name().is_some_and(|n| n == "saved-w9-session.yaml")),
            "no saved-w9-session.yaml was written under {dir:?}:\n{transcript}"
        );
    }

    /// `.exit` on a dirty temp session, answered `y` and named: the exit
    /// work's own output (the same `Saved the session to '<path>'` line) is
    /// not lost with the alternate screen; it is carried to the normal screen
    /// after the UI left, exactly once, before the shell gets the terminal
    /// back (above the wrapper's report), with the tty cooked and the file
    /// written.
    #[test]
    #[serial]
    fn dot_exit_saving_the_session_on_y_carries_the_saved_line_to_the_normal_screen() {
        let dir = fresh_config_dir("exit-saved-line");
        let _guard = TempDirGuard(dir.clone());
        write_config(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        open_and_dirty_a_session(&mut pty);
        let exit_at = pty.raw().len();
        pty.send(b".exit\r").unwrap();
        pty.wait_for_after("Save session?", exit_at, PTY_DEADLINE)
            .expect(".exit on a dirty session did not ask to save it");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        pty.wait_for("ession name", PTY_DEADLINE).unwrap();
        let named_at = pty.raw().len();
        answer_field_modal(&mut pty, "saved-w9-exit");
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);

        let left_at = rfind(pty.raw(), LEAVE_ALT).expect("no leave sequence");
        let saved_at = find(pty.raw(), SAVED_LINE, named_at).unwrap_or_else(|| {
            panic!("the save confirmation was lost with the screen:\n{transcript}")
        });
        assert!(
            saved_at > left_at,
            "the confirmation was drawn inside the alternate screen only, not carried out:\n{transcript}"
        );
        let report_at = find(pty.raw(), b"exit=", left_at).expect("no exit= line");
        assert!(
            saved_at < report_at,
            "the confirmation came after the shell took the terminal back:\n{transcript}"
        );
        assert_eq!(
            count(&pty.raw()[named_at..], SAVED_LINE),
            1,
            "the confirmation was printed more than once:\n{transcript}"
        );
        let after_leave = strip_ansi(&String::from_utf8_lossy(&pty.raw()[left_at..]));
        assert!(
            after_leave.contains("saved-w9-exit.yaml"),
            "the confirmation does not name the written file:\n{after_leave}"
        );
        assert!(
            walk(&dir)
                .into_iter()
                .any(|p| p.file_name().is_some_and(|n| n == "saved-w9-exit.yaml")),
            "no saved-w9-exit.yaml was written under {dir:?}:\n{transcript}"
        );
    }

    // ---------------------------------------------------------------------------
    // The remaining rerouted menus, each driven as a consumer would from a
    // clean config dir.
    // ---------------------------------------------------------------------------

    /// `.vault update <name>` under the UI: the question is the Password
    /// modal (`Enter the secret value:`), the replacement typed into it is
    /// never painted, the vault reports the update, and `.vault get` prints
    /// the new value and no longer the old one. The secret is seeded through
    /// the same modal by `.vault add`.
    #[test]
    #[serial]
    fn dot_vault_update_asks_in_the_password_modal_and_the_new_value_replaces_the_old() {
        let dir = fresh_config_dir("vault-update-modal");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        secure_vault_password_file(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let add_at = pty.raw().len();
        pty.send(b".vault add vault-secret-u5v\r").unwrap();
        wait_for_the_modal(&mut pty, add_at, ".vault add", &["Enterthesecretvalue:"]);
        answer_field_modal(&mut pty, "old-value-u5v");
        wait_for_squeezed(
            &mut pty,
            "Secret 'vault-secret-u5v' added to the vault",
            "the seeded secret's confirmation",
        );
        pty.read_for(Duration::from_millis(300));

        let update_at = pty.raw().len();
        pty.send(b".vault update vault-secret-u5v\r").unwrap();
        wait_for_the_modal(
            &mut pty,
            update_at,
            ".vault update",
            &["Enterthesecretvalue:"],
        );
        answer_field_modal(&mut pty, "new-value-u5v");
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        let updated = loop {
            pty.read_for(Duration::from_millis(200));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
            if squeezed(&since).contains("Secret'vault-secret-u5v'updatedinthevault") {
                break since;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the answered Password modal did not update the secret:\n{since}"
            );
        };
        assert!(
            !updated.contains("new-value-u5v"),
            "the replacement typed into the Password modal was painted on the screen:\n{updated}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "the Password modal released the terminal:\n{updated}"
        );

        let get_at = pty.raw().len();
        pty.send(b".vault get vault-secret-u5v\r").unwrap();
        pty.wait_for_after("new-value-u5v", get_at, PTY_DEADLINE)
            .expect("`.vault get` did not print the value the update modal collected");
        pty.read_for(Duration::from_millis(300));
        let got = strip_ansi(&String::from_utf8_lossy(&pty.raw()[get_at..]));
        assert!(
            !got.contains("old-value-u5v"),
            "`.vault get` still prints the value from before the update:\n{got}"
        );

        assert_ui_alive_then_quit(&mut pty, "the vault update");
    }

    /// `.rag attach <name>` under the UI: the first question is the driver
    /// Select modal, a closed select (only `qdrant`, no `Other (custom)`
    /// row); Enter on it moves the wizard on to the host Input modal (the
    /// answer flowed back); Esc there leaves no rag or kit behind and the
    /// terminal was never released.
    #[test]
    #[serial]
    fn dot_rag_attach_asks_for_the_driver_in_a_closed_select_modal_then_the_host_input() {
        let dir = fresh_config_dir("rag-attach-modal");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_embeddings(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let files_before = walk(&dir);
        let before = pty.raw().len();
        pty.send(b".rag attach attach-rag-u5v\r").unwrap();
        let modal = wait_for_the_modal(
            &mut pty,
            before,
            ".rag attach",
            &["Selectdriver:", "qdrant"],
        );
        assert!(
            !squeezed(&modal).contains("Other(custom)"),
            "the driver select is closed but the modal offers a custom row:\n{modal}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        let frame = repainted_frame(&mut pty, 50, 120);
        assert!(
            squeezed(&frame).contains("Host(e.g.qdrant.company.com:6333):")
                && squeezed(&frame).contains("Esc=cancel"),
            "Enter on the driver select did not move the attach wizard on to the host Input modal:\n{frame}"
        );
        assert!(
            !squeezed(&frame).contains("Selectdriver:"),
            "the driver select is still up after Enter:\n{frame}"
        );

        esc_cancels_the_modal(&mut pty, ".rag attach attach-rag-u5v");
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        for footprint in ["u5v", "/rags/"] {
            let written = new_files_named(&dir, &files_before, footprint);
            assert!(
                written.is_empty(),
                "the cancelled attach still wrote {written:?}:\n{since}"
            );
        }
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "an attach question released the terminal:\n{since}"
        );

        assert_ui_alive_then_quit(&mut pty, "the cancelled attach");
    }

    /// `.install <category>` under the UI: the overwrite warning and its
    /// `Continue?` question are the Confirm modal (the warning body is the
    /// modal's help line, not a raw-tty `[y/N]` prompt); `n` aborts with
    /// `No files were changed.` and the category directory is untouched;
    /// asked again, `y` reinstalls and reports it.
    #[test]
    #[serial]
    fn dot_install_of_a_builtin_category_asks_continue_in_the_confirm_modal() {
        let dir = fresh_config_dir("install-builtins-modal");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        let macros_dir = dir.join("macros");
        let snapshot = |dir: &Path| -> Vec<(PathBuf, u64, SystemTime)> {
            let mut rows: Vec<_> = walk(dir)
                .into_iter()
                .map(|path| {
                    let meta = fs::metadata(&path).unwrap();
                    (path, meta.len(), meta.modified().unwrap())
                })
                .collect();
            rows.sort();
            rows
        };
        let macros_before = snapshot(&macros_dir);

        let before = pty.raw().len();
        pty.send(b".install macros\r").unwrap();
        wait_for_the_modal(&mut pty, before, ".install macros", &["Continue?", "y=yes"]);
        // The warning body is long enough to overlap the banner it is painted
        // over, so the diff repaint skips the cells that already match; the
        // help line is read off a forced full repaint instead.
        let modal = repainted_frame(&mut pty, 58, 140);
        assert!(
            squeezed(&modal).contains("Reinstallingbundledmacrosoverwrites"),
            "the overwrite warning is not the modal's help line:\n{modal}"
        );
        assert!(
            !squeezed(&modal).contains("[y/N]"),
            "`Continue?` was asked on the raw tty, not in the modal:\n{modal}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"n").unwrap();
        wait_for_squeezed(
            &mut pty,
            "Aborted. No files were changed.",
            "the abort after `n` on the modal",
        );
        pty.read_for(Duration::from_millis(500));
        assert_eq!(
            snapshot(&macros_dir),
            macros_before,
            "`n` on the overwrite modal still touched the macros dir"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "the question released the terminal"
        );

        let again = pty.raw().len();
        pty.send(b".install macros\r").unwrap();
        wait_for_the_modal(&mut pty, again, ".install macros", &["Continue?", "y=yes"]);
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_squeezed(
            &mut pty,
            "Reinstalled bundled macros",
            "the reinstall report after `y` on the modal",
        );
        pty.read_for(Duration::from_millis(500));
        assert!(
            macros_dir.is_dir() && !walk(&macros_dir).is_empty(),
            "`y` on the overwrite modal reported a reinstall but left no macros under {}",
            macros_dir.display()
        );

        assert_ui_alive_then_quit(&mut pty, "the reinstall");
    }

    /// A local bundle with no secrets (`write_bundle_repo` with none), so its
    /// install asks nothing and only the uninstall menus are exercised.
    fn install_bundle_without_questions(pty: &mut Pty, repo: &Path) {
        let install_at = pty.raw().len();
        pty.send(format!(".install {}\r", repo.display()).as_bytes())
            .unwrap();
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        loop {
            pty.read_for(Duration::from_millis(200));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
            assert!(
                !squeezed(&since).contains("Esc=cancel"),
                "`.install` of a bundle without secrets or conflicts asked a question:\n{since}"
            );
            if squeezed(&since).contains("Installed:2new") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "`.install {}` did not report the installed files:\n{since}",
                repo.display()
            );
        }
        pty.read_for(Duration::from_millis(500));
    }

    /// `.uninstall <bundle>` under the UI: the question is the Confirm modal
    /// whose help line names the bundle's footprint (`owns N file(s) and M
    /// mcp.json server(s)`); `n` aborts with `nothing was changed` and every
    /// owned file stays. Asked again, `y` proceeds; a file edited locally
    /// after the install raises the `was modified locally after install`
    /// Select modal (keep / delete / keep-all / delete-all, closed: no
    /// custom row), `1` keeps it, the unmodified file is deleted, and the
    /// bundle is reported partially uninstalled with its record kept.
    #[test]
    #[serial]
    fn dot_uninstall_asks_in_the_confirm_modal_and_a_modified_file_in_the_select_modal() {
        let dir = fresh_config_dir("uninstall-modal");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-u5v";
        let repo = write_bundle_repo(&dir, name, &[]);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        install_bundle_without_questions(&mut pty, &repo);
        let role = dir.join("roles").join(format!("{name}-role.md"));
        let edited = dir.join("roles").join(format!("{name}-obsolete.md"));
        assert!(
            role.exists() && edited.exists(),
            "the bundle's roles were not installed under {}",
            dir.display()
        );
        let edited_text = "---\nmodel: fake:fake-model\n---\nEdited locally by the user.\n";
        fs::write(&edited, edited_text).unwrap();

        // `n` on the Confirm modal: nothing changes.
        let question = squeezed(&format!("Uninstall bundle '{name}'?"));
        let first = pty.raw().len();
        pty.send(format!(".uninstall {name}\r").as_bytes()).unwrap();
        let modal =
            wait_for_the_modal(&mut pty, first, ".uninstall", &[question.as_str(), "y=yes"]);
        assert!(
            squeezed(&modal).contains("owns2file(s)and1mcp.jsonserver(s)"),
            "the Confirm modal does not show the bundle's footprint as its help line:\n{modal}"
        );
        assert!(
            !squeezed(&modal).contains("(y/N)"),
            "`Uninstall bundle?` was asked on the raw tty, not in the modal:\n{modal}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"n").unwrap();
        wait_for_squeezed(
            &mut pty,
            &format!("Uninstall of '{name}' aborted; nothing was changed."),
            "the abort after `n` on the uninstall modal",
        );
        pty.read_for(Duration::from_millis(300));
        assert!(
            role.exists() && edited.exists(),
            "`n` on the uninstall modal still removed a bundle file"
        );
        let records = walk(&dir)
            .into_iter()
            .filter(|p| {
                let text = p.to_string_lossy();
                !text.contains("/bundle-src/")
                    && !text.contains("/roles/")
                    && fs::read_to_string(p).is_ok_and(|body| body.contains(name))
            })
            .count();
        assert!(records > 0, "the bundle record vanished after `n`");

        // `y`, then `1` (keep) on the modified-file Select modal.
        let second = pty.raw().len();
        pty.send(format!(".uninstall {name}\r").as_bytes()).unwrap();
        wait_for_the_modal(
            &mut pty,
            second,
            ".uninstall",
            &[question.as_str(), "y=yes"],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        let select = wait_for_the_modal(
            &mut pty,
            second,
            ".uninstall (modified file)",
            &[
                "wasmodifiedlocallyafterinstall",
                "keep",
                "delete",
                "keep-all",
                "delete-all",
            ],
        );
        assert!(
            !squeezed(&select).contains("Other(custom)"),
            "the modified-file select is closed but the modal offers a custom row:\n{select}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"1").unwrap();
        wait_for_squeezed(
            &mut pty,
            &format!("Bundle '{name}' partially uninstalled; its record keeps the remaining items"),
            "the uninstall report after keeping the modified file",
        );
        pty.read_for(Duration::from_millis(500));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[second..]));
        assert!(
            squeezed(&since).contains("keptmodifiedfile"),
            "`1` on the Select modal was not taken as `keep`:\n{since}"
        );
        assert!(
            !role.exists(),
            "the unmodified role was not deleted by the uninstall:\n{since}"
        );
        assert_eq!(
            fs::read_to_string(&edited).ok().as_deref(),
            Some(edited_text),
            "the modified role was not kept as answered:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "an uninstall question released the terminal:\n{since}"
        );

        // The kept file is still owned, so the record stays and `.list
        // bundles` keeps naming the bundle. The name is looked for after the
        // table's `Bundles:` header so the match is a table row, not the
        // command's echo or a repaint of an earlier line naming the bundle.
        let list_at = pty.raw().len();
        pty.send(b".list bundles\r").unwrap();
        let header_at = pty
            .wait_for_raw(b"Bundles:", list_at, PTY_DEADLINE)
            .expect("`.list bundles` did not print its `Bundles:` header");
        pty.wait_for_after(name, header_at, PTY_DEADLINE)
        .unwrap_or_else(|e| {
            panic!(
                "`.list bundles` no longer names the partially uninstalled bundle whose record was kept: {e:?}"
            )
        });

        assert_ui_alive_then_quit(&mut pty, "the uninstall");
    }

    /// With no terminal, the no-tty silent paths keep their polarity and the
    /// refusals keep theirs: `--install <local bundle>` with nothing to ask
    /// proceeds and installs the files; `--uninstall <bundle>` without
    /// `--yes` is refused (exit non-zero, the refusal names the
    /// non-interactive run and `--yes`) with every owned file left in place
    /// and the record kept; `--uninstall --yes` then removes them.
    #[test]
    #[serial]
    fn a_tty_less_uninstall_without_yes_is_refused_and_keeps_the_bundle() {
        let dir = fresh_config_dir("ttyless-uninstall");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-u5v";
        let repo = write_bundle_repo(&dir, name, &[]);
        let repo_arg = repo.to_str().unwrap();

        let (ok, out) = run_ttyless(&dir, &["--install", repo_arg], "tty-less install");
        let role = dir.join("roles").join(format!("{name}-role.md"));
        assert!(
            ok && role.exists(),
            "a tty-less `--install` of a bundle with nothing to ask did not proceed silently:\n{out}"
        );

        let (ok, out) = run_ttyless(&dir, &["--uninstall", name], "tty-less uninstall");
        assert!(
            !ok,
            "a tty-less `--uninstall` without `--yes` exited 0:\n{out}"
        );
        let lower = out.to_lowercase();
        assert!(
            lower.contains("non-interactive")
                || lower.contains("not a terminal")
                || lower.contains("not a tty"),
            "the refusal does not say the run is non-interactive:\n{out}"
        );
        assert!(
            out.contains("--yes"),
            "the refusal does not name `--yes` as the remedy:\n{out}"
        );
        assert!(
            role.exists(),
            "the refused tty-less uninstall still removed a bundle file:\n{out}"
        );

        let (ok, out) = run_ttyless(
            &dir,
            &["--uninstall", name, "--yes"],
            "tty-less uninstall --yes",
        );
        assert!(
            ok && !role.exists(),
            "`--uninstall --yes` with no terminal did not remove the bundle's files:\n{out}"
        );
    }

    // ---------------------------------------------------------------------
    // Wizards driven from a clean config dir: the rag wizard's Input modal
    // against its plain-REPL twin, the closed Select's key handling, and
    // typeahead on the `.update` confirm.
    // ---------------------------------------------------------------------

    /// Walks the rag wizard from the embedding Select modal to the
    /// `Set chunk size:` Input modal with Enter (the highlighted choice or the
    /// default) and returns the frame showing the chunk-size question. Every
    /// frame is forced by a resize, so `rows` alternates between two heights
    /// and the caller keeps using it for the frames that follow.
    fn walk_to_the_chunk_size_modal(pty: &mut Pty, command: &str, rows: &mut u16) -> String {
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        let mut frame = next_wizard_frame(pty, rows);
        let mut steps = 0;
        while !squeezed(&frame).contains("Setchunksize:") {
            assert!(
                !squeezed(&frame).contains("Adddocuments:"),
                "`{command}` reached `Add documents:` without asking the chunk size:\n{frame}"
            );
            assert!(
                steps < 4,
                "`{command}` did not reach `Set chunk size:` within 4 answers:\n{frame}"
            );
            pty.read_for(HOTKEY_GAP);
            pty.send(b"\r").unwrap();
            frame = next_wizard_frame(pty, rows);
            steps += 1;
        }
        frame
    }

    /// A repaint forced by flipping the height between 50 and 48 rows.
    fn next_wizard_frame(pty: &mut Pty, rows: &mut u16) -> String {
        *rows = if *rows == 50 { 48 } else { 50 };
        repainted_frame(pty, *rows, 120)
    }

    /// Typing a value into the `Set chunk size:` Input modal must mean the
    /// same as typing it at the plain REPL's `Set chunk size: (1000)`: the
    /// answer is what was typed, not the default with the typed digits
    /// appended. The wizard derives the chunk-overlay default from the
    /// accepted chunk size (1/20th of it), so the next modal shows which value
    /// was taken: `25` for 500, `50025` for 1000500.
    #[test]
    #[serial]
    fn dot_rag_chunk_size_typed_into_the_input_modal_replaces_the_default_like_the_plain_repl() {
        let dir = fresh_config_dir("rag-input-default-typed");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_embeddings(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let files_before = walk(&dir);
        let before = pty.raw().len();
        pty.send(b".rag new-rag-q8d\r").unwrap();
        wait_for_the_modal(
            &mut pty,
            before,
            ".rag new-rag-q8d",
            &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
        );
        let mut rows = 48;
        let frame = walk_to_the_chunk_size_modal(&mut pty, ".rag new-rag-q8d", &mut rows);
        assert!(
            squeezed(&frame).contains("Setchunksize:1000"),
            "the `Set chunk size:` Input modal does not show its default 1000:\n{frame}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"500\r").unwrap();
        let frame = next_wizard_frame(&mut pty, &mut rows);
        let flat = squeezed(&frame);
        assert!(
            flat.contains("Setchunkoverlay:") && flat.contains("Esc=cancel"),
            "the wizard did not move on to `Set chunk overlay:` after the chunk size was typed:\n{frame}"
        );
        assert!(
            !flat.contains("Setchunkoverlay:50025"),
            "typing 500 into the Input modal appended to the default: the wizard took chunk size \
         1000500 (overlay default 50025), where the plain REPL takes 500:\n{frame}"
        );
        assert!(
            flat.contains("Setchunkoverlay:25"),
            "the chunk-overlay default is not 25, so the chunk size taken was not the typed 500:\n{frame}"
        );

        esc_cancels_the_modal(&mut pty, ".rag new-rag-q8d");
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        for footprint in ["q8d", "/rags/"] {
            let written = new_files_named(&dir, &files_before, footprint);
            assert!(
                written.is_empty(),
                "the cancelled wizard still wrote {written:?}:\n{since}"
            );
        }
        assert_ui_alive_then_quit(&mut pty, "the cancelled wizard");
    }

    /// The plain-REPL twin of the case above, the contract the modal is held
    /// to: Enter on the Select echoes the chosen row in full (the model id
    /// with its description columns, as the wizard renders it), inquire shows
    /// the default as `(1000)`, `500` typed over it is the answer, and the
    /// next question's default is `(25)`.
    #[test]
    #[serial]
    fn dot_rag_chunk_size_typed_in_the_plain_repl_replaces_the_default() {
        let dir = fresh_config_dir("rag-input-default-typed-repl");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_embeddings(&dir);
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
        let files_before = walk(&dir);
        let before = pty.raw().len();
        pty.send(b".rag new-rag-r9e\r").unwrap();
        pty.wait_for_after("Select embedding model:", before, PTY_DEADLINE)
            .expect("`.rag <new>` in the plain REPL did not ask for the embedding model");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"\r").unwrap();
        pty.wait_for_after(
            "Select embedding model: dryrun:dry-embed-a (max-tokens:-;max-batch:100;price:-)",
            before,
            PTY_DEADLINE,
        )
        .expect("Enter on the plain REPL's Select did not echo the chosen model's full row");
        let mut answered = 0;
        loop {
            let text = pty.read_for(Duration::from_millis(800));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
            if since.contains("Set chunk size: (1000)") {
                break;
            }
            assert!(
                answered < 4,
                "the plain REPL wizard did not reach `Set chunk size: (1000)` within 4 answers:\n{text}"
            );
            pty.send(b"\r").unwrap();
            answered += 1;
        }

        let typed_at = pty.raw().len();
        pty.send(b"500\r").unwrap();
        pty.wait_for_after("Set chunk overlay:", typed_at, PTY_DEADLINE)
            .expect("the plain REPL wizard did not move on to `Set chunk overlay:`");
        // The default is painted a moment after the label, so it is waited for
        // rather than read off the first frame that shows the question.
        let since = pty
            .wait_for_after("Set chunk overlay: (25)", typed_at, PTY_DEADLINE)
            .unwrap_or_else(|e| {
                panic!(
                    "typing 500 at `Set chunk size: (1000)` did not make the chunk size 500 \
                 (the overlay default is 1/20th of it): {e:?}"
                )
            });
        assert!(
            find(pty.raw(), ENTER_ALT, 0).is_none(),
            "the plain REPL entered the alternate screen:\n{since}"
        );

        let esc_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        wait_for_repl_prompt(&mut pty, "dry-model)>", esc_at);
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        for footprint in ["r9e", "/rags/"] {
            let written = new_files_named(&dir, &files_before, footprint);
            assert!(
                written.is_empty(),
                "the cancelled `.rag` still wrote {written:?}:\n{since}"
            );
        }
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------
    // Cancelling the obsolete-file question of a bundle update.
    // ---------------------------------------------------------------------

    const EDITED_OBSOLETE: &str = "---\nmodel: fake:fake-model\n---\nEdited locally by the user.\n";
    const KEPT_OBSOLETE_NOTE: &str = "Kept the obsolete files: the question was cancelled.";

    /// Installs the secret-free bundle `name` from `repo`, edits its second
    /// role locally, then drops that role from the source and commits, so
    /// the next `.install <name>` finds an obsolete file to ask about.
    /// Returns the edited file's path.
    fn make_the_second_role_obsolete(dir: &Path, repo: &Path, name: &str) -> PathBuf {
        let obsolete = dir.join("roles").join(format!("{name}-obsolete.md"));
        assert!(
            obsolete.exists(),
            "the bundle's second role was not installed at {}",
            obsolete.display()
        );
        fs::write(&obsolete, EDITED_OBSOLETE).unwrap();
        fs::remove_file(repo.join("roles").join(format!("{name}-obsolete.md"))).unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "drop a role"]);
        obsolete
    }

    /// `.install <bundle>` updating a bundle whose source dropped a role the
    /// user edited: the obsolete file's fate is a Select modal; Esc on it
    /// keeps the file, the update still prints its summary and one note,
    /// `Kept the obsolete files: the question was cancelled.`, the terminal
    /// is never released, and the UI keeps taking keys.
    #[test]
    #[serial]
    fn esc_on_the_obsolete_file_select_modal_keeps_the_file_and_says_the_question_was_cancelled() {
        let dir = fresh_config_dir("obsolete-esc-modal");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-o8e";
        let repo = write_bundle_repo(&dir, name, &[]);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        install_bundle_without_questions(&mut pty, &repo);
        let obsolete = make_the_second_role_obsolete(&dir, &repo, name);

        let update_at = pty.raw().len();
        pty.send(format!(".install {name}\r").as_bytes()).unwrap();
        let modal = wait_for_the_modal(
            &mut pty,
            update_at,
            ".install <bundle> (obsolete file)",
            &["Obsolete file", &format!("{name}-obsolete.md"), "keep"],
        );
        assert!(
            !squeezed(&modal).contains("Other(custom)"),
            "the obsolete-file select is closed but the modal offers a custom row:\n{modal}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_squeezed(
            &mut pty,
            KEPT_OBSOLETE_NOTE,
            "the note after Esc on the obsolete-file modal",
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
        let flat = squeezed(&since);
        assert_eq!(
            flat.matches(&squeezed(KEPT_OBSOLETE_NOTE)).count(),
            1,
            "the kept-files note was printed more or less than once:\n{since}"
        );
        assert!(
            flat.contains("Installed:") || flat.contains("Updated") || flat.contains("unchanged"),
            "the cancelled question swallowed the update's summary:\n{since}"
        );
        let frame = repainted_frame(&mut pty, 58, 140);
        assert!(
            !squeezed(&frame).contains("Esc=cancel"),
            "the obsolete-file modal is still up after Esc:\n{frame}"
        );
        assert_eq!(
            fs::read_to_string(&obsolete).ok().as_deref(),
            Some(EDITED_OBSOLETE),
            "Esc on the obsolete-file modal did not keep the edited file:\n{since}"
        );
        assert!(
            dir.join("roles").join(format!("{name}-role.md")).exists(),
            "the update dropped the bundle's current role:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "the obsolete-file question released the terminal:\n{since}"
        );

        assert_ui_alive_then_quit(&mut pty, "the cancelled obsolete-file question");
    }

    /// The same update in the plain REPL: the obsolete file's fate is an
    /// inquire Select; Esc cancels it, the file is kept, the summary and
    /// the one note line follow, and the REPL goes on taking commands.
    #[test]
    #[serial]
    fn esc_on_the_obsolete_file_select_in_the_plain_repl_keeps_the_file_and_says_so() {
        let dir = fresh_config_dir("obsolete-esc-repl");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-o8r";
        let repo = write_bundle_repo(&dir, name, &[]);
        let (ok, out) = run_ttyless(
            &dir,
            &["--install", repo.to_str().unwrap()],
            "tty-less install of the bundle to update",
        );
        assert!(ok, "{out}");
        let obsolete = make_the_second_role_obsolete(&dir, &repo, name);

        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");
        let update_at = wait_for_repl_prompt(&mut pty, ")> ", 0);
        pty.send(format!(".install {name}\r").as_bytes()).unwrap();
        pty.wait_for_after("Obsolete file", update_at, PTY_DEADLINE)
            .expect("the REPL update did not ask about the obsolete file");
        pty.read_for(Duration::from_millis(500));
        let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
        assert!(
            asked.contains("keep") && !asked.contains("Other (custom)"),
            "the REPL question is not the closed keep/delete select:\n{asked}"
        );
        let cancelled = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        pty.wait_for_after(KEPT_OBSOLETE_NOTE, update_at, PTY_DEADLINE)
            .expect("the REPL did not print the kept-files note after Esc");
        pty.read_for(Duration::from_millis(500));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
        assert_eq!(
            since.matches(KEPT_OBSOLETE_NOTE).count(),
            1,
            "the kept-files note was printed more or less than once:\n{since}"
        );
        assert!(
            since.contains("Installed:")
                || since.contains("Updated")
                || since.contains("unchanged"),
            "the cancelled question swallowed the update's summary:\n{since}"
        );
        assert_eq!(
            fs::read_to_string(&obsolete).ok().as_deref(),
            Some(EDITED_OBSOLETE),
            "Esc on the REPL select did not keep the edited file:\n{since}"
        );
        assert!(
            find(pty.raw(), ENTER_ALT, 0).is_none(),
            "the plain REPL entered the alternate screen:\n{since}"
        );

        let info_at = wait_for_repl_prompt(&mut pty, ")> ", cancelled);
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("`.info` after the cancelled question never rendered");
        let shown = find(pty.raw(), b"log_path", info_at)
            .expect("`log_path` was seen in the stripped transcript but not in the raw bytes");
        wait_for_repl_prompt(&mut pty, ")> ", shown);
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A closed Select modal takes only its rows: a digit past the last row
    /// is ignored and the question stays up. A non-integer typed into the
    /// `Set chunk size:` Input modal is re-asked in the modal with the
    /// reason (`Must be an integer.`), the way the plain REPL re-asks; Esc
    /// then leaves nothing on disk.
    #[test]
    #[serial]
    fn dot_rag_ignores_a_digit_past_the_closed_select_and_reasks_a_non_integer_chunk_size_in_the_modal()
     {
        let dir = fresh_config_dir("rag-closed-select-and-reask");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_embeddings(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let files_before = walk(&dir);
        let before = pty.raw().len();
        pty.send(b".rag new-rag-t3m\r").unwrap();
        wait_for_the_modal(
            &mut pty,
            before,
            ".rag new-rag-t3m",
            &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"9").unwrap();
        let mut rows = 48;
        let frame = next_wizard_frame(&mut pty, &mut rows);
        let flat = squeezed(&frame);
        assert!(
            flat.contains("Selectembeddingmodel:") && flat.contains("Esc=cancel"),
            "`9` on a two-row closed Select closed the modal instead of being ignored:\n{frame}"
        );
        assert!(
            !flat.contains("Setchunksize:"),
            "`9` on a two-row closed Select answered it and the wizard moved on:\n{frame}"
        );

        walk_to_the_chunk_size_modal(&mut pty, ".rag new-rag-t3m", &mut rows);
        pty.read_for(HOTKEY_GAP);
        pty.send(b"abc\r").unwrap();
        let frame = next_wizard_frame(&mut pty, &mut rows);
        let flat = squeezed(&frame);
        assert!(
            flat.contains("Mustbeaninteger.")
                && flat.contains("Setchunksize:")
                && flat.contains("Esc=cancel"),
            "a non-integer chunk size was not re-asked in the modal with its reason:\n{frame}"
        );
        assert!(
            !flat.contains("Setchunkoverlay:"),
            "a non-integer chunk size was accepted and the wizard moved on:\n{frame}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "a wizard question released the terminal:\n{frame}"
        );

        esc_cancels_the_modal(&mut pty, ".rag new-rag-t3m");
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        for footprint in ["t3m", "/rags/"] {
            let written = new_files_named(&dir, &files_before, footprint);
            assert!(
                written.is_empty(),
                "the cancelled wizard still wrote {written:?}:\n{since}"
            );
        }
        assert_ui_alive_then_quit(&mut pty, "the cancelled wizard");
    }

    /// `.update` followed at once by `y` in the same burst of keys (a user
    /// typing ahead of the confirm): the `y` must not accept `Update anyway?`,
    /// so the modal is still up well past the idle gate, the update has not
    /// started, and `n` then cancels it with the binary untouched. Where the
    /// `y` lands depends on timing (dropped by the modal's idle gate, or typed
    /// into the composer before the modal opened); either way it is not an answer.
    #[test]
    #[serial]
    fn dot_update_y_typed_ahead_with_the_command_does_not_accept_the_confirm_modal() {
        let dir = fresh_config_dir("update-typeahead-y");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let binary = cargo_installed_binary(&dir);
        let binary_before = fs::metadata(&binary).unwrap();
        let mut pty = spawn_binary_under(&binary, &dir, &[], WAIT_WRAPPER, "/dev/tty", " --tui");

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        let before = pty.raw().len();
        pty.send(b".update\ry").unwrap();
        wait_for_the_modal(&mut pty, before, ".update", &["Updateanyway?", "y=yes"]);

        pty.read_for(Duration::from_millis(2500));
        let frame = repainted_frame(&mut pty, 58, 140);
        let flat = squeezed(&frame);
        assert!(
            flat.contains("Updateanyway?") && flat.contains("Esc=cancel"),
            "the `y` typed ahead with `.update` accepted the confirm modal:\n{frame}"
        );
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        assert!(
            !since.contains("Checking target-arch") && !since.contains("updating anyway"),
            "the update started on the typed-ahead `y`:\n{since}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"n").unwrap();
        wait_for_squeezed(
            &mut pty,
            "Update cancelled.",
            "the cancellation after `n` on the modal",
        );
        let binary_after = fs::metadata(&binary).unwrap();
        assert!(
            binary_after.len() == binary_before.len()
                && binary_after.modified().unwrap() == binary_before.modified().unwrap(),
            "the cancelled update touched the binary at {}",
            binary.display()
        );
        // A `y` that reached the composer before the modal opened would ride
        // along with the next command; one Backspace clears it and is a no-op
        // on an empty composer.
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x7f").unwrap();
        assert_ui_alive_then_quit(&mut pty, "the cancelled update");
    }

    // ---------------------------------------------------------------------
    // The Input modal's empty-field default, the Password modal's empty
    // re-ask, and `--update` with only stdout piped.
    // ---------------------------------------------------------------------

    /// `Set chunk size:` under the UI shows its default `1000` as a dim
    /// placeholder; Enter on the untouched field answers the default (the
    /// next question's default is 1/20th of the chunk size: `50`), and a
    /// field typed into and then backspaced back to empty answers the
    /// default too, exactly as the plain REPL's `(1000)` does. Esc on the
    /// overlay question leaves nothing on disk both times.
    #[test]
    #[serial]
    fn dot_rag_enter_on_the_empty_chunk_size_modal_takes_the_dim_placeholder_default() {
        let dir = fresh_config_dir("rag-input-empty-default");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_embeddings(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let files_before = walk(&dir);
        let mut rows = 48;
        for (name, keys, how) in [
            ("new-rag-e5d", &b"\r"[..], "Enter on the untouched field"),
            ("new-rag-e5e", &b"7\x7f\r"[..], "`7`, Backspace, Enter"),
        ] {
            let command = format!(".rag {name}");
            let before = pty.raw().len();
            pty.send(format!("{command}\r").as_bytes()).unwrap();
            wait_for_the_modal(
                &mut pty,
                before,
                &command,
                &["Selectembeddingmodel:", "dry-embed-a", "dry-embed-b"],
            );
            let frame = walk_to_the_chunk_size_modal(&mut pty, &command, &mut rows);
            assert!(
                squeezed(&frame).contains("Setchunksize:1000"),
                "`{command}`: the `Set chunk size:` Input modal does not show its default 1000:\n{frame}"
            );
            let painted_at = pty.raw().len();
            let frame = next_wizard_frame(&mut pty, &mut rows);
            assert!(
                painted_dim(pty.raw(), painted_at, b"1000"),
                "`{command}`: the default 1000 is not painted dim as a placeholder:\n{frame}\n--- raw ---\n{}",
                String::from_utf8_lossy(&pty.raw()[painted_at..]).escape_debug()
            );

            pty.read_for(HOTKEY_GAP);
            pty.send(keys).unwrap();
            let frame = next_wizard_frame(&mut pty, &mut rows);
            let flat = squeezed(&frame);
            assert!(
                flat.contains("Setchunkoverlay:") && flat.contains("Esc=cancel"),
                "`{command}`: {how} did not move the wizard on to `Set chunk overlay:`:\n{frame}"
            );
            assert!(
                flat.contains("Setchunkoverlay:50"),
                "`{command}`: {how} did not take chunk size 1000 (overlay default 50):\n{frame}"
            );
            assert!(
                !flat.contains("Setchunkoverlay:500") && !flat.contains("Setchunkoverlay:0"),
                "`{command}`: {how} took a chunk size other than the default 1000:\n{frame}"
            );

            esc_cancels_the_modal(&mut pty, &command);
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
            for footprint in [name, "/rags/"] {
                let written = new_files_named(&dir, &files_before, footprint);
                assert!(
                    written.is_empty(),
                    "`{command}`: the cancelled wizard still wrote {written:?}:\n{since}"
                );
            }
        }
        assert_ui_alive_then_quit(&mut pty, "the cancelled wizards");
    }

    /// `.vault add <name>` under the UI with Enter on an empty Password
    /// modal: nothing is stored, the question is re-asked in the modal with
    /// its reason spelled out as a sentence (the plain REPL re-asks with
    /// `A response is required.`), the terminal is never released, and Esc
    /// then leaves no vault file behind.
    #[test]
    #[serial]
    fn dot_vault_add_with_an_empty_password_modal_is_reasked_and_esc_stores_nothing() {
        let dir = fresh_config_dir("vault-add-empty-reask");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        secure_vault_password_file(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let files_before = walk(&dir);
        let before = pty.raw().len();
        pty.send(b".vault add vault-secret-e6f\r").unwrap();
        wait_for_the_modal(&mut pty, before, ".vault add", &["Enterthesecretvalue:"]);

        pty.read_for(HOTKEY_GAP);
        let entered_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        let frame = repainted_frame(&mut pty, 50, 120);
        let flat = squeezed(&frame);
        assert!(
            flat.contains("Enterthesecretvalue:") && flat.contains("Esc=cancel"),
            "Enter on the empty Password modal closed it instead of re-asking:\n{frame}"
        );
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[entered_at..]));
        assert!(
            !squeezed(&since).contains("addedtothevault"),
            "an empty secret value was stored:\n{since}"
        );
        assert!(
            flat.contains("Aresponseisrequired."),
            "the re-asked Password modal does not give the plain REPL's reason `A response is \
         required.` as a sentence:\n{frame}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "the re-asked Password modal released the terminal:\n{frame}"
        );

        esc_cancels_the_modal(&mut pty, ".vault add");
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        for footprint in ["e6f", "vault.yml"] {
            let written = new_files_named(&dir, &files_before, footprint);
            assert!(
                written.is_empty(),
                "the cancelled `.vault add` still wrote {written:?}:\n{since}"
            );
        }
        assert_ui_alive_then_quit(&mut pty, "the cancelled vault add");
    }

    // ---------------------------------------------------------------------
    // Esc on a vault-secret modal defers the rest, and a delete-all answer
    // about owned files never reaches the mcp.json servers, while keep-all
    // does.
    // ---------------------------------------------------------------------

    /// Rewrites the merged `mcp.json` so the bundle's server entry differs
    /// from the one the install recorded (its `k7s` argument gains a
    /// suffix); the uninstall then sees a locally modified server.
    fn edit_the_bundle_server_in_mcp_json(dir: &Path) -> String {
        let path = dir.join("mcp.json");
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"k7s\""),
            "the merged mcp.json does not carry the bundle server's `k7s` argument:\n{text}"
        );
        let edited = text.replace("\"k7s\"", "\"k7s-edited-locally\"");
        fs::write(&path, &edited).unwrap();
        edited
    }

    /// `.install <local bundle>` whose mcp.json references three secrets:
    /// Esc on the first `Add {{ X }} to the vault now?` modal defers THAT
    /// secret and every later one (no modal for B or C), the notice lists
    /// all three, the bundle is installed and recorded with its server
    /// merged, nothing reached the vault and the UI stays live. Asked again
    /// by `.install <bundle>` (an update from the same source), `n` on A and
    /// Esc on B leaves C unasked and all three listed once more.
    #[test]
    #[serial]
    fn esc_on_a_vault_secret_modal_defers_that_and_every_later_secret_and_keeps_the_install() {
        let dir = fresh_config_dir("install-esc-defers-secrets");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-e9s";
        let secrets = ["BUNDLE_SECRET_A", "BUNDLE_SECRET_B", "BUNDLE_SECRET_C"];
        let repo = write_bundle_repo(&dir, name, &secrets);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        let files_before = walk(&dir);
        let install_at = pty.raw().len();
        pty.send(format!(".install {}\r", repo.display()).as_bytes())
            .unwrap();
        wait_for_the_modal(
            &mut pty,
            install_at,
            ".install (first secret)",
            &["Add{{BUNDLE_SECRET_A}}tothevaultnow?", "y=yes"],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_squeezed(
            &mut pty,
            DEFERRED_SECRETS_NOTICE,
            "the deferred-secrets notice after Esc on the first secret modal",
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
        let flat = squeezed(&since);
        for later in ["BUNDLE_SECRET_B", "BUNDLE_SECRET_C"] {
            assert!(
                !flat.contains(&format!("Add{{{{{later}}}}}tothevaultnow?")),
                "Esc on the first secret modal still asked about {later}:\n{since}"
            );
        }
        assert!(
            flat.contains("{{BUNDLE_SECRET_A}}{{BUNDLE_SECRET_B}}{{BUNDLE_SECRET_C}}"),
            "the notice does not list all three deferred secrets in order:\n{since}"
        );
        assert!(
            !flat.contains("thequestionwascancelled") && !flat.contains("nobodycouldbeasked"),
            "a deferred secret printed a kept note (the summary alone reports deferred secrets):\n{since}"
        );
        assert!(!since.contains("exit="), "`.install` quit the UI:\n{since}");
        for role in ["role", "obsolete"] {
            let installed = dir.join("roles").join(format!("{name}-{role}.md"));
            assert!(
                installed.exists(),
                "the bundle's {role} file was not installed at {}:\n{since}",
                installed.display()
            );
        }
        let mcp_files = config_files_mentioning(&dir, "bundle-k7s-server");
        assert!(
            mcp_files
                .iter()
                .any(|path| path.file_name().is_some_and(|f| f == "mcp.json")),
            "the merged mcp.json does not hold the bundle's server: {mcp_files:?}\n{since}"
        );
        assert!(
            !new_files_named(&dir, &files_before, name).is_empty(),
            "no file records the installed bundle `{name}`:\n{since}"
        );
        let secret_holders = config_files_mentioning(&dir, "BUNDLE_SECRET_A");
        assert!(
            secret_holders
                .iter()
                .all(|path| path.file_name().is_some_and(|f| f == "mcp.json")),
            "a file other than mcp.json holds BUNDLE_SECRET_A (a vault write?): {secret_holders:?}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "a secret question released the terminal:\n{since}"
        );

        // The update from the same source asks about every secret again:
        // `n` defers A alone, Esc on B defers B and C together.
        let update_at = pty.raw().len();
        pty.send(format!(".install {name}\r").as_bytes()).unwrap();
        wait_for_the_modal(
            &mut pty,
            update_at,
            ".install <bundle> (first secret again)",
            &["Add{{BUNDLE_SECRET_A}}tothevaultnow?", "y=yes"],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"n").unwrap();
        // The second secret's modal differs from the first by one letter, so
        // the painter sends only that cell; a resize forces the whole frame.
        pty.read_for(Duration::from_millis(500));
        let answered_at = pty.raw().len();
        pty.resize(60, 139).unwrap();
        wait_for_the_modal(
            &mut pty,
            answered_at,
            ".install <bundle> (second secret)",
            &["Add{{BUNDLE_SECRET_B}}tothevaultnow?", "y=yes"],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_squeezed(
            &mut pty,
            "{{BUNDLE_SECRET_B}}{{BUNDLE_SECRET_C}}",
            "the deferred list of the update after `n` then Esc",
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[update_at..]));
        let flat = squeezed(&since);
        assert!(
            !flat.contains("Add{{BUNDLE_SECRET_C}}tothevaultnow?"),
            "Esc on the second secret modal still asked about the third secret:\n{since}"
        );
        assert!(
            flat.contains("{{BUNDLE_SECRET_A}}{{BUNDLE_SECRET_B}}{{BUNDLE_SECRET_C}}"),
            "the update's notice does not list all three deferred secrets:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "a secret question of the update released the terminal:\n{since}"
        );
        assert_ui_alive_then_quit(&mut pty, "the deferred secrets");
    }

    /// `.uninstall <bundle>` of a bundle whose role AND mcp.json server were
    /// both edited after install: `delete-all` on the file modal deletes the
    /// owned files but does NOT carry into the mcp.json loop, so the server
    /// gets its own `MCP server '<key>' was modified locally after install`
    /// modal. Esc there keeps the server, prints exactly one `Kept the
    /// locally modified items: the question was cancelled.` note, the
    /// summary counts `deleted=2 kept=0` files and `removed=0 kept=1`
    /// servers, the bundle is partially uninstalled and the UI stays live.
    #[test]
    #[serial]
    fn delete_all_on_the_uninstall_file_modal_does_not_carry_into_the_mcp_server_modal() {
        let dir = fresh_config_dir("uninstall-delete-all-mcp");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-d9m";
        let repo = write_bundle_repo(&dir, name, &[]);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        install_bundle_without_questions(&mut pty, &repo);
        let role = dir.join("roles").join(format!("{name}-role.md"));
        let edited = dir.join("roles").join(format!("{name}-obsolete.md"));
        fs::write(&edited, EDITED_OBSOLETE).unwrap();
        let edited_mcp = edit_the_bundle_server_in_mcp_json(&dir);

        let at = pty.raw().len();
        pty.send(format!(".uninstall {name}\r").as_bytes()).unwrap();
        wait_for_the_modal(
            &mut pty,
            at,
            ".uninstall",
            &[&format!("Uninstall bundle '{name}'?"), "y=yes"],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_the_modal(
            &mut pty,
            at,
            ".uninstall (modified file)",
            &[
                &format!("Fileroles/{name}-obsolete.mdwasmodifiedlocallyafterinstall"),
                "delete-all",
            ],
        );
        pty.read_for(HOTKEY_GAP);
        let files_answered_at = pty.raw().len();
        pty.send(b"4").unwrap();
        wait_for_the_modal(
            &mut pty,
            files_answered_at,
            ".uninstall (modified server)",
            &[
                "MCPserver'bundle-k7s-server'wasmodifiedlocallyafterinstall",
                "keep",
                "delete-all",
            ],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_squeezed(
            &mut pty,
            "the question was cancelled",
            "the note after Esc on the modified-server modal",
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..]));
        let flat = squeezed(&since);
        assert_eq!(
            flat.matches("Keptthelocallymodifieditems:thequestionwascancelled.")
                .count(),
            1,
            "the cancelled note was printed more or less than once:\n{since}"
        );
        assert!(
            !flat.contains("nobodycouldbeasked"),
            "the Esc note reads as the headless one:\n{since}"
        );
        assert!(
            flat.contains("keptserver'bundle-k7s-server'")
                && flat.contains("mcpservers:removed=0kept=1")
                && flat.contains("files:deleted=2kept=0"),
            "delete-all on the files carried into the servers, or the summary disagrees:\n{since}"
        );
        assert!(
            flat.contains("partiallyuninstalled"),
            "the bundle with a kept server is not reported partially uninstalled:\n{since}"
        );
        assert!(
            !role.exists() && !edited.exists(),
            "delete-all on the file modal left an owned file behind:\n{since}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("mcp.json")).ok().as_deref(),
            Some(edited_mcp.as_str()),
            "the kept server was rewritten or removed:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "an uninstall question released the terminal:\n{since}"
        );
        assert_ui_alive_then_quit(&mut pty, "the cancelled server question");
    }

    /// The keep-all twin: with both roles and the server edited, `keep-all`
    /// on the first file modal settles the whole run: no second file modal,
    /// no server modal, no kept note (an answer was given), two `kept
    /// modified file` lines, the server kept and the summary saying so.
    #[test]
    #[serial]
    fn keep_all_on_the_uninstall_file_modal_carries_into_the_mcp_server_loop_without_asking() {
        let dir = fresh_config_dir("uninstall-keep-all-mcp");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let name = "bundle-k9m";
        let repo = write_bundle_repo(&dir, name, &[]);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        install_bundle_without_questions(&mut pty, &repo);
        let role = dir.join("roles").join(format!("{name}-role.md"));
        let obsolete = dir.join("roles").join(format!("{name}-obsolete.md"));
        let edited_role = format!("{}# edited\n", fs::read_to_string(&role).unwrap());
        fs::write(&role, &edited_role).unwrap();
        fs::write(&obsolete, EDITED_OBSOLETE).unwrap();
        let edited_mcp = edit_the_bundle_server_in_mcp_json(&dir);

        let at = pty.raw().len();
        pty.send(format!(".uninstall {name}\r").as_bytes()).unwrap();
        wait_for_the_modal(
            &mut pty,
            at,
            ".uninstall",
            &[&format!("Uninstall bundle '{name}'?"), "y=yes"],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_the_modal(
            &mut pty,
            at,
            ".uninstall (first modified file)",
            &["wasmodifiedlocallyafterinstall", "keep-all"],
        );
        pty.read_for(HOTKEY_GAP);
        let answered_at = pty.raw().len();
        pty.send(b"3").unwrap();
        wait_for_squeezed(
            &mut pty,
            "mcp servers: removed=0 kept=1",
            "the uninstall summary after keep-all",
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[answered_at..]));
        let flat = squeezed(&since);
        assert!(
            !flat.contains("MCPserver'bundle-k7s-server'wasmodifiedlocallyafterinstall")
                && !flat.contains("Esc=cancel"),
            "keep-all did not carry into the mcp.json loop (another modal came up):\n{since}"
        );
        assert!(
            !flat.contains("Keptthelocallymodifieditems"),
            "a kept note was printed though keep-all was answered:\n{since}"
        );
        assert_eq!(
            flat.matches("keptmodifiedfile").count(),
            2,
            "the ledger does not carry one `kept modified file` line per kept role:\n{since}"
        );
        assert!(
            flat.contains("keptserver'bundle-k7s-server'")
                && flat.contains("files:deleted=0kept=2"),
            "the summary does not show every item kept:\n{since}"
        );
        assert_eq!(
            fs::read_to_string(&role).ok().as_deref(),
            Some(edited_role.as_str()),
            "keep-all lost the first role's edit:\n{since}"
        );
        assert_eq!(
            fs::read_to_string(&obsolete).ok().as_deref(),
            Some(EDITED_OBSOLETE),
            "keep-all lost the second role's edit:\n{since}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("mcp.json")).ok().as_deref(),
            Some(edited_mcp.as_str()),
            "keep-all rewrote or removed the kept server:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "an uninstall question released the terminal:\n{since}"
        );
        assert_ui_alive_then_quit(&mut pty, "keep-all on the uninstall");
    }

    // ---------------------------------------------------------------------
    // Cancelling INSIDE the vault wizard a bundle secret opens, and Esc on the
    // MCP conflict select of an install.
    // ---------------------------------------------------------------------

    /// The note the merge prints when the MCP conflict select was closed
    /// without an answer.
    const KEPT_MCP_SERVER_CANCELLED_NOTE: &str =
        "Kept the local MCP server 'bundle-k7s-server': the question was cancelled.";

    /// Typed into the vault password modal and left unsubmitted before Esc;
    /// no file may end up holding it.
    const CANCELLED_VAULT_PASSWORD: &str = "cancelled-pw-v4q";

    /// Waits for the modal `markers` describe after `from`, with a resize
    /// first so the whole frame is on the pty (a modal differing from the
    /// last by a few cells paints only those cells).
    fn next_modal_after_a_repaint(
        pty: &mut Pty,
        from: usize,
        command: &str,
        markers: &[&str],
        rows: &mut u16,
    ) -> String {
        *rows = if *rows == 60 { 59 } else { 60 };
        pty.resize(*rows, 140).unwrap();
        wait_for_the_modal(pty, from, command, markers)
    }

    /// The bundle install a vault-wizard cancel is checked against.
    struct VaultScene<'a> {
        dir: &'a Path,
        name: &'a str,
        password_file: &'a Path,
        files_before: &'a [PathBuf],
    }

    /// One install pass of `command` on a bundle with secrets A and B: `y`
    /// on `Add {{ A }} to the vault now?` opens the vault wizard, `walk`
    /// drives it up to the widget to cancel, Esc there. Then: the deferred
    /// notice lists A and B in order, B was never asked about, no password
    /// file exists at `password_file` (or anywhere new under `dir`), the
    /// bundle's files are installed, the UI is still up and the terminal was
    /// never released. Returns the text since the command.
    fn esc_inside_the_vault_wizard_defers_the_rest(
        pty: &mut Pty,
        scene: &VaultScene<'_>,
        command: &str,
        walk: &dyn Fn(&mut Pty, usize, &mut u16),
        rows: &mut u16,
        what: &str,
    ) -> String {
        let VaultScene {
            dir,
            name,
            password_file,
            files_before,
        } = *scene;
        let at = pty.raw().len();
        pty.send(format!("{command}\r").as_bytes()).unwrap();
        wait_for_the_modal(
            pty,
            at,
            &format!("{command} ({what}: first secret)"),
            &["Add{{BUNDLE_SECRET_A}}tothevaultnow?", "y=yes"],
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        pty.read_for(Duration::from_millis(500));
        let answered_at = pty.raw().len();
        walk(pty, answered_at, rows);
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_squeezed(
            pty,
            DEFERRED_SECRETS_NOTICE,
            &format!("{what}: the deferred-secrets notice after Esc inside the vault wizard"),
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..]));
        let flat = squeezed(&since);
        assert!(
            !flat.contains("Add{{BUNDLE_SECRET_B}}tothevaultnow?"),
            "{what}: Esc inside the vault wizard still asked about the later secret:\n{since}"
        );
        assert!(
            flat.contains("{{BUNDLE_SECRET_A}}{{BUNDLE_SECRET_B}}"),
            "{what}: the notice does not list both secrets as deferred in order:\n{since}"
        );
        assert!(
            !flat.contains("created.") && !flat.contains("updated."),
            "{what}: the cancelled wizard reported a password file written:\n{since}"
        );
        assert!(
            !flat.contains("Refusingto") && !flat.contains("Error:"),
            "{what}: the install failed instead of deferring the secrets:\n{since}"
        );
        assert!(
            !since.contains("exit="),
            "{what}: the install quit the UI:\n{since}"
        );
        assert!(
            !password_file.exists(),
            "{what}: a password file was written at {} though the wizard was cancelled:\n{since}",
            password_file.display()
        );
        let written: Vec<PathBuf> = walk_files(dir)
            .into_iter()
            .filter(|path| !files_before.contains(path))
            .filter(|path| {
                fs::read(path).is_ok_and(|bytes| {
                    bytes
                        .windows(CANCELLED_VAULT_PASSWORD.len())
                        .any(|window| window == CANCELLED_VAULT_PASSWORD.as_bytes())
                })
            })
            .collect();
        assert!(
            written.is_empty(),
            "{what}: a file holds the password typed before the cancel: {written:?}\n{since}"
        );
        for role in ["role", "obsolete"] {
            let installed = dir.join("roles").join(format!("{name}-{role}.md"));
            assert!(
                installed.exists(),
                "{what}: the bundle's {role} file was not installed at {}:\n{since}",
                installed.display()
            );
        }
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "{what}: a vault wizard question released the terminal:\n{since}"
        );
        since
    }

    /// [`walk`] under its harness name, for the closure above that shadows it.
    fn walk_files(dir: &Path) -> Vec<PathBuf> {
        walk(dir)
    }

    /// `.install <local bundle>` with two secrets after the vault's password
    /// file went missing while the UI was up (the boot needs it, the install
    /// runs later): `y` on `Add {{ A }} to the vault now?` opens the
    /// password-file wizard as modals. Esc on each of its three widgets
    /// in turn (the `create one now?` confirm on the install, the path
    /// Input on the first update, the password modal with a password typed
    /// but not submitted on the second) defers A and every later secret, the
    /// install or update still succeeds and lists both secrets as deferred,
    /// no password file is written, and no new file anywhere under the
    /// config dir holds the typed password.
    #[test]
    #[serial]
    fn esc_inside_the_vault_wizard_of_a_bundle_secret_defers_the_rest_and_writes_no_password_file()
    {
        let dir = fresh_config_dir("install-esc-inside-vault-wizard");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let password_file = dir.join("vault-pass");
        let name = "bundle-w2v";
        let repo = write_bundle_repo(&dir, name, &["BUNDLE_SECRET_A", "BUNDLE_SECRET_B"]);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        // The boot checked the password file; once it is gone the first `yes`
        // on a secret reaches the wizard rather than the vault.
        fs::remove_file(&password_file).unwrap();
        let files_before = walk(&dir);
        let install = format!(".install {}", repo.display());
        // The size the repaints alternate from, carried across the passes so
        // no resize is a no-op.
        let mut rows = 60u16;
        let scene = VaultScene {
            dir: &dir,
            name,
            password_file: &password_file,
            files_before: &files_before,
        };

        // First pass: Esc on the `create one now?` confirm.
        esc_inside_the_vault_wizard_defers_the_rest(
            &mut pty,
            &scene,
            &install,
            &|pty, from, rows| {
                next_modal_after_a_repaint(
                    pty,
                    from,
                    ".install (password-file confirm)",
                    &["Nopasswordfileconfigured", "createonenow?", "y=yes"],
                    rows,
                );
            },
            &mut rows,
            "Esc on the create confirm",
        );

        // Second pass (an update from the same source): Enter on the confirm,
        // Esc on the path Input.
        let update = format!(".install {name}");
        esc_inside_the_vault_wizard_defers_the_rest(
            &mut pty,
            &scene,
            &update,
            &|pty, from, rows| {
                next_modal_after_a_repaint(
                    pty,
                    from,
                    ".install <bundle> (password-file confirm)",
                    &["createonenow?", "y=yes"],
                    rows,
                );
                pty.read_for(HOTKEY_GAP);
                pty.send(b"\r").unwrap();
                pty.read_for(Duration::from_millis(500));
                let at = pty.raw().len();
                next_modal_after_a_repaint(
                    pty,
                    at,
                    ".install <bundle> (password-file path)",
                    &["Enterthepathtothepasswordfiletocreate:", "Enter=answer"],
                    rows,
                );
            },
            &mut rows,
            "Esc on the path Input",
        );

        // Third pass: Enter on the confirm, Enter on the path (the default),
        // Esc on the password modal.
        esc_inside_the_vault_wizard_defers_the_rest(
            &mut pty,
            &scene,
            &update,
            &|pty, from, rows| {
                next_modal_after_a_repaint(
                    pty,
                    from,
                    ".install <bundle> (password-file confirm, third pass)",
                    &["createonenow?", "y=yes"],
                    rows,
                );
                pty.read_for(HOTKEY_GAP);
                pty.send(b"\r").unwrap();
                pty.read_for(Duration::from_millis(500));
                let at = pty.raw().len();
                next_modal_after_a_repaint(
                    pty,
                    at,
                    ".install <bundle> (password-file path, third pass)",
                    &["Enterthepathtothepasswordfiletocreate:", "Enter=answer"],
                    rows,
                );
                answer_field_modal(pty, "");
                pty.read_for(Duration::from_millis(500));
                let at = pty.raw().len();
                next_modal_after_a_repaint(
                    pty,
                    at,
                    ".install <bundle> (vault password)",
                    &["Enterapasswordtoencryptallvaultsecrets:", "Enter=answer"],
                    rows,
                );
                pty.read_for(HOTKEY_GAP);
                pty.send(CANCELLED_VAULT_PASSWORD.as_bytes()).unwrap();
                pty.read_for(Duration::from_millis(400));
            },
            &mut rows,
            "Esc on the password modal",
        );

        assert_ui_alive_then_quit(&mut pty, "the cancelled vault wizards");
    }

    /// `.install <local bundle>` whose mcp.json defines a server the local
    /// mcp.json already holds differently: Esc on the `Conflict on MCP
    /// server` Select modal keeps the local definition, the install still
    /// completes (role installed, merge report says `kept local`), exactly
    /// one note says `Kept the local MCP server '<name>': the question was
    /// cancelled.`, nothing is refused, the terminal is never released and
    /// the UI stays live.
    #[test]
    #[serial]
    fn esc_on_the_mcp_conflict_select_modal_keeps_the_local_server_and_says_the_question_was_cancelled()
     {
        let dir = fresh_config_dir("install-mcp-conflict-esc");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let local_before = write_conflicting_local_mcp_server(&dir);
        let repo = write_bundle_repo(&dir, "bundle-k7s", &[]);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        let install_at = pty.raw().len();
        pty.send(format!(".install {}\r", repo.display()).as_bytes())
            .unwrap();
        let modal = wait_for_the_modal(
            &mut pty,
            install_at,
            ".install (MCP conflict)",
            &[
                "ConflictonMCPserver'bundle-k7s-server'",
                "keeplocal",
                "takeremote",
                "abortmerge",
            ],
        );
        assert!(
            !squeezed(&modal).contains("Other(custom)"),
            "the MCP conflict select is closed but the modal offers a custom row:\n{modal}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_squeezed(
            &mut pty,
            KEPT_MCP_SERVER_CANCELLED_NOTE,
            "the note after Esc on the MCP conflict modal",
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
        let flat = squeezed(&since);
        assert_eq!(
            flat.matches(&squeezed(KEPT_MCP_SERVER_CANCELLED_NOTE))
                .count(),
            1,
            "the kept-server note was printed more or less than once:\n{since}"
        );
        assert!(
            flat.contains("keptlocal:bundle-k7s-server"),
            "the merge report does not say the server was kept local:\n{since}"
        );
        assert!(
            !flat.contains(&squeezed(MCP_CONFLICT_REFUSAL)) && !flat.contains("Abortedmerge"),
            "Esc on the conflict modal refused or aborted the merge instead of keeping local:\n{since}"
        );
        assert!(
            !since.contains("exit="),
            "the install quit the UI:\n{since}"
        );
        let frame = repainted_frame(&mut pty, 58, 140);
        assert!(
            !squeezed(&frame).contains("Esc=cancel"),
            "the MCP conflict modal is still up after Esc:\n{frame}"
        );
        assert!(
            dir.join("roles").join("bundle-k7s-role.md").exists(),
            "the bundle's role was not installed after Esc on the conflict modal:\n{since}"
        );
        let local_before: serde_json::Value = serde_json::from_str(&local_before).unwrap();
        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(
            merged, local_before,
            "Esc on the conflict modal changed the local mcp.json:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "the conflict modal released the terminal:\n{since}"
        );
        assert_ui_alive_then_quit(&mut pty, "Esc on the MCP conflict modal");
    }
}

mod modal_extremes_and_editor_failure {
    //! Cases for the full-screen UI at the awkward corners: a Confirm modal
    //! that has to survive Ctrl-Z, SIGTERM and a window shrunk to a single
    //! row or column, and an `$EDITOR` shell-out whose editor fails. Each
    //! case runs the binary cold on a pseudo-terminal under the same bash
    //! wrappers `tests/common/tui_harness.rs` holds, so the exit code and the
    //! tty flags are reported after every exit path. Unix-only for the same
    //! reasons.
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use std::{env, fs, iter};

    use super::common::fake_llm::{Chunk, FakeLlm, Reply};
    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// Asks one yes/no question through the prompt protocol, records its
    /// own pid at `<out>.pid` first and the answer at `<out>` afterwards.
    const PID_RECORDING_CONFIRM_TOOL: &str = "#!/usr/bin/env bash\n\
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
    \x20   echo $$ > \"${out}.pid\"\n\
    \x20   v=\"$(confirm \"Really do it? m6q\")\"\n\
    \x20   printf '%s' \"${v:-cancelled}\" > \"$out\"\n\
    \x20   echo \"fixture confirm: ${v:-cancelled} m6r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// Like [`spawn_tui_under_wrapper`], with `path` as the child's whole
    /// PATH.
    fn spawn_tui_with_path(
        dir: &Path,
        extra_env: &[(&str, &str)],
        wrapper: &str,
        path: &std::ffi::OsStr,
    ) -> Pty {
        let path = path.to_str().unwrap();
        let env: Vec<(&str, &str)> = extra_env
            .iter()
            .copied()
            .chain(iter::once(("PATH", path)))
            .collect();
        spawn_tui_under(dir, &env, wrapper, "/dev/tty")
    }

    /// Every entry into the alternate screen has exactly one matching exit:
    /// the enter/leave pair is a strict mirror however many times the UI
    /// handed the terminal away and took it back.
    fn assert_alt_screen_mirrored(pty: &Pty, transcript: &str) {
        assert_eq!(
            count(pty.raw(), ENTER_ALT),
            count(pty.raw(), LEAVE_ALT),
            "alternate-screen enters and leaves do not pair up:\n{transcript}"
        );
    }

    fn pid_in(path: &Path) -> i32 {
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }

    /// The screen the UI comes back to shows its chrome again: the model and
    /// the session tokens plus whatever the composer is expected to show.
    /// Tokens are compared with whitespace squeezed out: a cell-by-cell
    /// repaint moves the cursor between words instead of writing spaces.
    fn assert_chrome_repainted_squeezed_after(
        pty: &mut Pty,
        re_entered: usize,
        model: &str,
        composer: &str,
        what: &str,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let repainted = squeezed(&since(pty, re_entered));
            let missing: Vec<&str> = [composer, model, "no session"]
                .into_iter()
                .filter(|token| !repainted.contains(&squeezed(token)))
                .collect();
            if missing.is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "after {what} the screen was not repainted; still missing {missing:?}. \
             Printable bytes since re-entry: {repainted:?}"
            );
            pty.read_for(Duration::from_millis(200));
        }
    }

    /// Starts a tool-calling turn whose tool asks a Confirm question and
    /// waits until the question is on the screen. Returns the answer file.
    fn open_confirm_modal(pty: &mut Pty, dir: &Path) -> PathBuf {
        let out = dir.join("confirm-answer");
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the confirm tool\r").unwrap();
        pty.wait_for("m6q", PTY_DEADLINE)
            .expect("the tool's confirm question never reached the screen");
        wait_for_file(pty, &out.with_extension("pid"), "the tool's pid file");
        out
    }

    fn confirm_fixture(dir: &Path) -> FakeLlm {
        let out = dir.join("confirm-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_confirm_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done m6d".to_string()),
        ]);
        write_fake_llm_config(dir, &fake);
        write_tool(dir, "fixture_confirm_rec.sh", PID_RECORDING_CONFIRM_TOOL);
        fake
    }

    // ---------------------------------------------------------------------
    // Ctrl-Z while a tool's Confirm modal is up.
    // ---------------------------------------------------------------------

    /// Ctrl-Z with a Confirm modal open hands the terminal back cooked and
    /// out of the alternate screen like any other suspend; `fg` re-enters
    /// and the modal is drawn again from state (the question is back on the
    /// screen); `y` then answers it, the tool reads a yes, the turn finishes
    /// and the exit is clean with exactly one re-entry.
    #[test]
    #[serial]
    fn ctrl_z_under_a_tools_confirm_modal_hands_the_terminal_back_and_fg_redraws_the_question() {
        let dir = fresh_config_dir("ctrl-z-confirm");
        let _guard = TempDirGuard(dir.clone());
        let fake = confirm_fixture(&dir);
        let mut pty = spawn_tui_under_wrapper(&dir, &[], JOB_CONTROL_WRAPPER);

        let out = open_confirm_modal(&mut pty, &dir);
        pty.read_for(Duration::from_millis(300));
        let before_suspend = pty.raw().len();
        let stopped_marker = suspend_and_check_handback(&mut pty, before_suspend);
        assert!(
            !out.exists(),
            "the suspend itself answered the modal:\n{}",
            since(&pty, before_suspend)
        );

        let re_entered = wait_for_re_entry(&mut pty, stopped_marker);
        wait_for_squeezed_after(&mut pty, "m6q", re_entered, "the confirm question after fg");
        assert!(
            !out.exists(),
            "the resume itself answered the modal:\n{}",
            since(&pty, stopped_marker)
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript = wait_for_squeezed_after(
            &mut pty,
            "fixture done m6d",
            re_entered,
            "the reply after the tool",
        );
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "1",
            "`y` on the resumed confirm modal did not reach the tool as a yes:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(
            count(&pty.raw()[before_suspend..], ENTER_ALT),
            1,
            "exactly one alt-screen re-entry is expected after the suspend:\n{transcript}"
        );
        assert_alt_screen_mirrored(&pty, &transcript);
    }

    // ---------------------------------------------------------------------
    // SIGTERM, twice, while a tool's Confirm modal is up.
    // ---------------------------------------------------------------------

    /// Two SIGTERMs in quick succession while a tool waits on its Confirm
    /// modal: the process leaves the alternate screen exactly once, the tty
    /// comes back cooked, the exit is orderly and bounded, and the tool that
    /// was blocked on the prompt channel is gone rather than orphaned.
    #[test]
    #[serial]
    fn two_sigterms_under_a_tools_confirm_modal_leave_once_and_take_the_tool_along() {
        let dir = fresh_config_dir("sigterm-confirm");
        let _guard = TempDirGuard(dir.clone());
        let _fake = confirm_fixture(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        let out = open_confirm_modal(&mut pty, &dir);
        let tool_pid = pid_in(&out.with_extension("pid"));
        assert!(alive(tool_pid), "the tool was gone before the signal");
        let pid = child_pid(&since(&pty, 0));
        pty.read_for(Duration::from_millis(300));
        let signalled = Instant::now();
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        std::thread::sleep(Duration::from_millis(30));
        // SAFETY: as above; ESRCH on an already-gone process is fine.
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let took = signalled.elapsed();

        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert!(
            took < Duration::from_secs(10),
            "the exit under a blocked prompt took {took:?}:\n{transcript}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            1,
            "two SIGTERMs left the alternate screen {} times:\n{transcript}",
            count(pty.raw(), LEAVE_ALT)
        );
        assert_alt_screen_mirrored(&pty, &transcript);
        assert!(
            !transcript.contains("panicked"),
            "the teardown under a blocked prompt panicked:\n{transcript}"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(tool_pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !alive(tool_pid),
            "the tool blocked on its confirm question outlived the UI (pid {tool_pid}):\n{transcript}"
        );
    }

    // ---------------------------------------------------------------------
    // Windows shrunk to one row, one column and one cell while a modal or a
    // live stream is on the screen; every widget clamps to the buffer.
    // ---------------------------------------------------------------------

    /// A Confirm modal on a window dragged to 1 row, then 1 column, then
    /// 1x1 and back: no panic (the alternate screen is never left on its
    /// own), the question is drawn again at full size, and `y` still answers
    /// it so the tool and the turn complete.
    #[test]
    #[serial]
    fn a_confirm_modal_survives_one_row_one_column_and_one_cell_windows_and_y_still_answers() {
        let dir = fresh_config_dir("confirm-extremes");
        let _guard = TempDirGuard(dir.clone());
        let fake = confirm_fixture(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        let out = open_confirm_modal(&mut pty, &dir);
        pty.read_for(Duration::from_millis(300));
        let shrink_at = pty.raw().len();
        for (rows, cols) in [(1u16, 100u16), (40, 1), (1, 1)] {
            pty.resize(rows, cols).unwrap();
            pty.read_for(Duration::from_millis(500));
            let shown = since(&pty, shrink_at);
            assert!(
                !shown.contains("panicked") && !shown.contains("exit="),
                "a {cols}x{rows} window with the confirm modal up brought the UI down:\n{shown}"
            );
        }
        assert!(
            find(pty.raw(), LEAVE_ALT, shrink_at).is_none(),
            "the UI left the alternate screen on its own while shrunk (a panic restore?):\n{}",
            since(&pty, shrink_at)
        );
        assert!(!out.exists(), "a resize answered the modal");

        let grow_at = pty.raw().len();
        pty.resize(40, 100).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "m6q",
            grow_at,
            "the confirm question after growing back",
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript = wait_for_squeezed_after(
            &mut pty,
            "fixture done m6d",
            grow_at,
            "the reply after the tool",
        );
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "1",
            "`y` after the window grew back did not reach the tool as a yes:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
    }

    /// A held stream on a window dragged to 1 row, 1 column and 1x1: the UI
    /// stays up while the deltas keep arriving into a one-cell buffer, the
    /// released tail lands once the window is back, and the exit is clean.
    #[test]
    #[serial]
    fn a_held_stream_survives_one_row_one_column_and_one_cell_windows() {
        let dir = fresh_config_dir("stream-extremes");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("head before the shrink m6h".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text(" tail after the grow m6t".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"stream and hold\r").unwrap();
        wait_for_squeezed_after(&mut pty, "head before the shrink m6h", 0, "the first delta");
        pty.read_for(Duration::from_millis(300));
        let shrink_at = pty.raw().len();
        for (rows, cols) in [(1u16, 100u16), (40, 1), (1, 1)] {
            pty.resize(rows, cols).unwrap();
            pty.read_for(Duration::from_millis(500));
        }
        // The tail arrives while the window is a single cell.
        fs::write(&release, b"").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let shown = since(&pty, shrink_at);
        assert!(
            !shown.contains("panicked") && !shown.contains("exit="),
            "a one-cell window during a stream brought the UI down:\n{shown}"
        );
        assert!(
            find(pty.raw(), LEAVE_ALT, shrink_at).is_none(),
            "the UI left the alternate screen on its own while shrunk (a panic restore?):\n{shown}"
        );

        let grow_at = pty.raw().len();
        pty.resize(40, 100).unwrap();
        let transcript = wait_for_squeezed_after(
            &mut pty,
            "tail after the grow m6t",
            grow_at,
            "the rest of the reply after growing back",
        );
        assert!(
            squeezed(&transcript).contains(&squeezed("head before the shrink m6h")),
            "the head of the reply was lost across the shrink:\n{transcript}"
        );
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("keys were not handled after the one-cell round trip");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
        assert_eq!(fake.served(), 1, "{transcript}");
    }

    // ---------------------------------------------------------------------
    // `$EDITOR` shell-outs whose editor fails.
    // ---------------------------------------------------------------------

    /// Writes a shell script at `dir/<name>` and returns its path.
    fn script(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_string()
    }

    /// After the editor has had the terminal, the UI is back with the draft
    /// still in the composer and keys handled; the draft is then cleared and
    /// the usual clean exit follows.
    fn assert_back_with_draft_and_quit(mut pty: Pty, re_entered: usize, draft: &str, what: &str) {
        assert_chrome_repainted_squeezed_after(
            &mut pty,
            re_entered,
            "dryrun:dry-model",
            draft,
            what,
        );
        // The draft must be cleared before Ctrl+D is the quit chord.
        pty.send(b"\x03").unwrap();
        pty.read_for(HOTKEY_GAP);
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .unwrap_or_else(|err| panic!("keys were not handled after {what}: {err}"));
        pty.read_for(Duration::from_millis(300));

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert!(
            !transcript.contains("panicked"),
            "{what} panicked somewhere:\n{transcript}"
        );
        assert_alt_screen_mirrored(&pty, &transcript);
    }

    /// An editor that exits non-zero without touching the file: the UI
    /// re-enters and repaints, the draft typed before Ctrl-O is still in the
    /// composer, keys work and the exit is clean.
    #[test]
    #[serial]
    fn an_editor_that_exits_non_zero_brings_the_draft_back_and_the_ui_stays_live() {
        let dir = fresh_config_dir("editor-fails");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let editor = script(&dir, "failing-editor.sh", "#!/bin/sh\nexit 3\n");
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"keep me m6k").unwrap();
        pty.wait_for("keep me m6k", PTY_DEADLINE).unwrap();
        let before = pty.raw().len();
        pty.send(b"\x0f").unwrap();
        let left = pty
            .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
            .expect("Ctrl-O did not release the terminal for the editor");
        let re_entered = wait_for_re_entry(&mut pty, left);
        assert_back_with_draft_and_quit(pty, re_entered, "keep me m6k", "a failing editor");
    }

    /// A PATH holding only what the wrappers and the binary need (bash, sh,
    /// stty, tr, sleep and the fixture `tput`): no `nano` to fall back to.
    fn path_without_an_editor(dir: &Path) -> std::ffi::OsString {
        use std::os::unix::fs::symlink;
        let bin = dir.join("bare-bin");
        fs::create_dir_all(&bin).unwrap();
        for tool in ["bash", "sh", "stty", "tr", "sleep"] {
            let real = env::split_paths(&env::var_os("PATH").unwrap_or_default())
                .map(|p| p.join(tool))
                .find(|p| p.is_file())
                .unwrap_or_else(|| panic!("{tool} is not on the PATH"));
            symlink(real, bin.join(tool)).unwrap();
        }
        let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
        env::join_paths([fixture_bin, bin]).unwrap()
    }

    /// No editor can be resolved at all (`$EDITOR` names a command that does
    /// not exist and nothing on PATH stands in for it): Ctrl-O never hands
    /// the terminal away, the refusal is a notice on the screen, the draft is
    /// intact, keys work, enters and leaves of the alternate screen pair up,
    /// and the exit is clean.
    #[test]
    #[serial]
    fn ctrl_o_without_any_resolvable_editor_refuses_on_screen_and_keeps_the_draft() {
        let dir = fresh_config_dir("editor-missing");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let editor = dir.join("no-such-editor-m6e").to_str().unwrap().to_string();
        let path = path_without_an_editor(&dir);
        let mut pty = spawn_tui_with_path(
            &dir,
            &[("EDITOR", &editor), ("VISUAL", &editor)],
            WAIT_WRAPPER,
            &path,
        );

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"keep me m6m").unwrap();
        pty.wait_for("keep me m6m", PTY_DEADLINE).unwrap();
        let before = pty.raw().len();
        pty.send(b"\x0f").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let shown = since(&pty, before);
        assert!(
            find(pty.raw(), LEAVE_ALT, before).is_none(),
            "Ctrl-O handed the terminal away with no editor to give it to:\n{shown}"
        );
        assert!(
            shown.contains("Editor not found"),
            "no refusal reached the screen when Ctrl-O had no editor:\n{shown}"
        );
        // The terminal was never handed away, so nothing forces a full
        // repaint; a real size change does, and the chrome plus the draft
        // must all be there afterwards.
        let at = pty.raw().len();
        pty.resize(39, 100).unwrap();
        pty.read_for(Duration::from_millis(300));
        assert_back_with_draft_and_quit(pty, at, "keep me m6m", "a missing editor");
    }

    /// An editor that is killed by a signal while it holds the terminal: the
    /// UI re-enters, the draft is back, keys work and the exit is clean.
    #[test]
    #[serial]
    fn an_editor_killed_by_a_signal_brings_the_draft_back_and_the_ui_stays_live() {
        let dir = fresh_config_dir("editor-killed");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let editor = script(&dir, "self-killing-editor.sh", "#!/bin/sh\nkill -KILL $$\n");
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"keep me m6s").unwrap();
        pty.wait_for("keep me m6s", PTY_DEADLINE).unwrap();
        let before = pty.raw().len();
        pty.send(b"\x0f").unwrap();
        let left = pty
            .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
            .expect("Ctrl-O did not release the terminal for the editor");
        let re_entered = wait_for_re_entry(&mut pty, left);
        assert_back_with_draft_and_quit(pty, re_entered, "keep me m6s", "a signal-killed editor");
    }
}
