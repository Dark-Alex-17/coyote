//! Real-process checks of the composer and the queue: readline and
//! text-field keys, the key-help overlay with its chords and discovery hints,
//! the queue under vi keys and headless runs, the external editor, the
//! status line and history reset. Every module drives the binary
//! through the shared pty harness in `common`, so the file is
//! unix-only.
#![cfg(unix)]

mod common;

mod queue_vi_and_headless {
    //! Black-box pins for the bottom pane's queue across an agent switch, the
    //! vi layer, Ctrl+R, backtick document commands and the headless
    //! prompter's refusal: the built binary on a pseudo-terminal and a
    //! scripted fake model where a turn has to be held open. Unix-only and
    //! serial like its sibling pty modules.
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{Chunk, FakeLlm, Reply};
    use super::common::tui_harness::*;
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

    /// A reading command typed while the model streams runs at once: its
    /// output lands while the turn is still held, and it never joins the
    /// queue.
    #[test]
    #[serial]
    fn a_dot_info_typed_mid_stream_runs_at_once() {
        let dir = fresh_config_dir("info-mid-stream");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("part one ni4 ".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("reply one ni4".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"first prompt ni4\r").unwrap();
        wait_for_squeezed_after(&mut pty, "part one ni4", turn_at, "the held stream");
        pty.read_for(Duration::from_millis(300));

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        wait_for_squeezed_after(&mut pty, "log_path", info_at, "the output of `.info`");
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        assert!(
            frame.contains("Esc to interrupt"),
            "the turn was no longer running when `.info` showed:\n{frame}"
        );
        let shown = since(&pty, info_at);
        assert!(
            !shown.contains("pending:"),
            "`.info` was queued behind the turn:\n{shown}"
        );

        // The rest of the reply goes on below the command's cell, in view.
        let release_at = pty.raw().len();
        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(&mut pty, "? = keys", release_at, "the end of the turn");
        let frame = repainted_frame_after_resize(&mut pty, 40, 100);
        let frame = squeezed(&frame);
        let (above, below) = frame
            .split_once("replyoneni4")
            .unwrap_or_else(|| panic!("the rest of the reply is out of view:\n{frame}"));
        assert!(
            above.contains("log_path") && !below.contains("log_path"),
            "the rest of the reply is not below the output of `.info`:\n{frame}"
        );
        pty.read_for(Duration::from_millis(500));
        assert_eq!(fake.served(), 1);
        quit_and_check(&mut pty);
    }

    /// A setting typed while the model streams is checked at once and
    /// confirmed for the next turn, without joining the queue; once the
    /// turn ends the session has it.
    #[test]
    #[serial]
    fn a_dot_set_typed_mid_stream_applies_from_the_next_turn() {
        let dir = fresh_config_dir("set-mid-stream");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("part one ns5 ".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("reply one ns5".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"first prompt ns5\r").unwrap();
        wait_for_squeezed_after(&mut pty, "part one ns5", turn_at, "the held stream");
        pty.read_for(Duration::from_millis(300));

        let set_at = pty.raw().len();
        pty.send(b".set temperature 0.5\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "applies from the next turn",
            set_at,
            "the confirmation of `.set`",
        );
        let shown = since(&pty, set_at);
        assert!(
            !shown.contains("pending:"),
            "`.set` was queued behind the turn:\n{shown}"
        );

        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "reply one ns5",
            set_at,
            "the end of the held reply",
        );
        pty.read_for(Duration::from_millis(500));

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        wait_for_squeezed_after(&mut pty, "log_path", info_at, "the idle `.info`");
        // Tall enough for the head of the output, where the setting is.
        let info = repainted_frame_after_resize(&mut pty, 90, 101);
        assert!(
            squeezed(&info).contains("temperature0.5"),
            "the setting typed during the turn did not reach the session:\n{info}"
        );
        quit_and_check(&mut pty);
    }

    /// `.help` typed while the model streams lists the commands at once,
    /// and a bad `.set` is refused at once: neither joins the queue, and
    /// the refused value never reaches the session.
    #[test]
    #[serial]
    fn a_dot_help_and_a_bad_dot_set_mid_stream_answer_at_once() {
        let dir = fresh_config_dir("help-badset-mid-stream");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("part one hb7 ".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("reply one hb7".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"first prompt hb7\r").unwrap();
        wait_for_squeezed_after(&mut pty, "part one hb7", turn_at, "the held stream");

        let help_at = pty.raw().len();
        pty.send(b".help\r").unwrap();
        wait_for_squeezed_after(&mut pty, "Exit REPL", help_at, "the `.help` listing");
        let set_at = pty.raw().len();
        pty.send(b".set temperature abc\r").unwrap();
        wait_for_squeezed_after(&mut pty, "Invalid value", set_at, "the refusal of `.set`");
        let shown = since(&pty, help_at);
        assert!(
            !shown.contains("pending:"),
            "`.help` or the bad `.set` was queued behind the turn:\n{shown}"
        );
        assert!(
            !squeezed(&since(&pty, set_at)).contains(&squeezed("applies from the next turn")),
            "the bad `.set` was confirmed for the next turn:\n{shown}"
        );
        assert!(
            !squeezed(&shown).contains("replyonehb7"),
            "the turn ended before the commands answered, so they did not run during it"
        );

        let release_at = pty.raw().len();
        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(&mut pty, "? = keys", release_at, "the end of the turn");
        let frame = squeezed(&repainted_frame_after_resize(&mut pty, 40, 101));
        let (above, _) = frame
            .split_once("replyonehb7")
            .unwrap_or_else(|| panic!("the rest of the reply is out of view:\n{frame}"));
        assert!(
            above.contains(&squeezed("Invalid value")),
            "the rest of the reply is not below the command cells:\n{frame}"
        );
        pty.read_for(Duration::from_millis(500));
        assert_eq!(fake.served(), 1, "a command reached the model");
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        wait_for_squeezed_after(&mut pty, "log_path", info_at, "the idle `.info`");
        let info = repainted_frame_after_resize(&mut pty, 90, 101);
        let info = squeezed(&info);
        let listing = info
            .rsplit_once(">.info")
            .map_or(info.as_str(), |(_, listing)| listing);
        assert!(
            listing.contains("temperaturenull"),
            "the refused value changed the session's temperature:\n{listing}"
        );
        quit_and_check(&mut pty);
    }

    /// `.model` typed while the model streams changes the model, so it
    /// waits for the turn: the `pending:` strip shows it until the turn
    /// ends.
    #[test]
    #[serial]
    fn a_dot_model_typed_mid_stream_waits_for_the_turn() {
        let dir = fresh_config_dir("model-mid-stream");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("part one mq3 ".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("reply one mq3".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"first prompt mq3\r").unwrap();
        wait_for_squeezed_after(&mut pty, "part one mq3", turn_at, "the held stream");

        let model_at = pty.raw().len();
        pty.send(b".model nosuch:model-mq3\r").unwrap();
        wait_for_squeezed_after(&mut pty, "pending:", model_at, "the queued `.model`");
        let shown = since(&pty, model_at);
        assert!(
            !squeezed(&shown).contains("replyonemq3"),
            "the turn ended before `.model` showed as pending:\n{shown}"
        );

        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(&mut pty, "reply one mq3", model_at, "the end of the reply");
        wait_until_no_pending(&mut pty);
        assert_eq!(fake.served(), 1, "`.model` reached the model as a prompt");
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

    /// A line sent while the model waits in `job__wait` ends the wait at
    /// once: the model's next request carries the line and the wait's
    /// `user_message` status long before the job could finish, and the job
    /// keeps running.
    #[test]
    #[serial]
    fn a_line_sent_during_a_job_wait_reaches_the_model_and_the_job_keeps_running() {
        let dir = fresh_config_dir("wait-user-message");
        let _guard = TempDirGuard(dir.clone());
        let job_pid = dir.join("job.pid");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "job__start".to_string(),
                arguments: r#"{"tool":"fixture_sleep","arguments":{}}"#.to_string(),
            },
            Reply::ToolCall {
                name: "job__wait".to_string(),
                arguments: "{}".to_string(),
            },
            Reply::Text("saw it w4t".to_string()),
        ]);
        write_fake_llm_config_with(&dir, &fake, "enabled_tools:\n  - fixture_sleep\n");
        write_tool(&dir, "fixture_sleep.sh", &sleeping_tool(&job_pid));
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        pty.send(b"start it and wait\r").unwrap();
        wait_for_file(&mut pty, &job_pid, "the job's pid file");
        let pid: i32 = fs::read_to_string(&job_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _sleepers = Sleepers(vec![job_pid.clone()]);
        let deadline = Instant::now() + PTY_DEADLINE;
        while fake.served() < 2 {
            assert!(
                Instant::now() < deadline,
                "the model never called job__wait"
            );
            pty.read_for(Duration::from_millis(100));
        }
        pty.read_for(Duration::from_millis(800));
        assert_eq!(fake.served(), 2, "the wait ended before the line was sent");

        let sent = Instant::now();
        pty.send(b"look at this m5q\r").unwrap();
        wait_for_squeezed_after(&mut pty, "saw it w4t", 0, "the reply after the wait");
        assert!(
            sent.elapsed() < Duration::from_secs(10),
            "the line waited {:?} for the wait to end",
            sent.elapsed()
        );
        // The third request answers the wait; a later one may be the
        // engine's reminder about the job still running.
        let bodies = fake.bodies();
        let after_wait = bodies.get(2).expect("no request followed the wait");
        assert!(
            after_wait.contains("look at this m5q"),
            "the line did not reach the model with the wait's result: {after_wait}"
        );
        // The tool result is JSON inside the request's JSON, so its quotes
        // arrive escaped; the notification event says `event`, not `status`.
        assert!(
            after_wait.contains(r#"\"status\":\"user_message\""#),
            "the wait did not say why it ended: {after_wait}"
        );
        assert!(alive(pid), "the job died with the interrupted wait");
        quit_clearing_any_draft(&mut pty);
    }

    /// A line sent while a real tool runs waits for it: the tool is not
    /// stopped for it and the model is not asked again. Esc then stops
    /// the turn and sends the held line at once, as the next turn, rather
    /// than dropping it.
    #[test]
    #[serial]
    fn esc_sends_a_line_held_behind_a_running_tool() {
        let dir = fresh_config_dir("esc-sends-held");
        let _guard = TempDirGuard(dir.clone());
        let tool_pid = dir.join("tool.pid");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_sleep".to_string(),
                arguments: "{}".to_string(),
            },
            Reply::Text("answered the held line n8x".to_string()),
        ]);
        write_fake_llm_config_with(&dir, &fake, "enabled_tools:\n  - fixture_sleep\n");
        write_tool(&dir, "fixture_sleep.sh", &sleeping_tool(&tool_pid));
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        pty.send(b"run the slow tool\r").unwrap();
        wait_for_file(&mut pty, &tool_pid, "the tool's pid file");
        let pid: i32 = fs::read_to_string(&tool_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _sleepers = Sleepers(vec![tool_pid.clone()]);
        pty.send(b"held line h3d\r").unwrap();
        wait_for_squeezed_after(&mut pty, "pending: 1", 0, "the held line's strip");
        pty.read_for(Duration::from_millis(1500));
        assert_eq!(
            fake.served(),
            1,
            "the held line asked the model while the tool ran"
        );
        assert!(alive(pid), "the held line stopped the running tool");

        let esc_at = pty.raw().len();
        pty.send(ESC).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "answered the held line n8x",
            esc_at,
            "the held line's turn after Esc",
        );
        let bodies = fake.bodies();
        let next = bodies.get(1).expect("Esc sent nothing to the model");
        assert!(
            next.contains("held line h3d"),
            "Esc did not send the held line: {next}"
        );
        assert_eq!(
            next.matches("held line h3d").count(),
            1,
            "the held line reached the model twice: {next}"
        );
        let shown = since(&pty, esc_at);
        assert!(
            !shown.contains("Dropped"),
            "Esc dropped the held line:\n{shown}"
        );
        quit_clearing_any_draft(&mut pty);
    }
}

mod readline_and_text_field_keys {
    //! Real-process checks of the readline chords and a question's text field,
    //! sent as the terminal encodes them: Ctrl+W, Ctrl+Y, Alt+B, Ctrl+K, Alt+F,
    //! Ctrl+_, Alt+D and Ctrl+U edit the composer; in a question's text field
    //! `?` is typed rather than opening the key help, F1-F12 type nothing, and
    //! Ctrl+X shows the todo panel without eating the field's text or closing
    //! the question. Each case runs the binary on a pseudo-terminal, so the
    //! module is unix-only, and the cases run one at a time.
    use std::time::Duration;

    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// A row only the key help shows.
    const HELP_MARK: &str = "Suspend to the shell";

    const CTRL_E: &[u8] = b"\x05";
    const CTRL_K: &[u8] = b"\x0b";
    const CTRL_U: &[u8] = b"\x15";
    const CTRL_W: &[u8] = b"\x17";
    const CTRL_Y: &[u8] = b"\x19";
    /// What a terminal sends for Ctrl+_ and, in most, for Ctrl+/ as well.
    const CTRL_UNDERSCORE: &[u8] = b"\x1f";
    const ALT_B: &[u8] = b"\x1bb";
    const ALT_D: &[u8] = b"\x1bd";
    const ALT_F: &[u8] = b"\x1bf";
    const HOME: &[u8] = b"\x1b[H";

    /// xterm's encodings of F1-F12.
    const FUNCTION_KEYS: [&[u8]; 12] = [
        b"\x1bOP",
        b"\x1bOQ",
        b"\x1bOR",
        b"\x1bOS",
        b"\x1b[15~",
        b"\x1b[17~",
        b"\x1b[18~",
        b"\x1b[19~",
        b"\x1b[20~",
        b"\x1b[21~",
        b"\x1b[23~",
        b"\x1b[24~",
    ];

    /// Ctrl+C clears a non-empty composer and quits from an empty one, so the
    /// Ctrl+D after it may find the UI already gone.
    fn leave(mut pty: Pty) {
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(300));
        let _ = pty.send(b"\x04");
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert!(!transcript.contains("panicked"), "{transcript}");
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    fn shows(frame: &str, needle: &str) -> bool {
        squeezed(frame).contains(&squeezed(needle))
    }

    /// Sends `keys` and reads the composer row off the next full repaint.
    fn step(pty: &mut Pty, keys: &[u8]) -> String {
        pty.send(keys).unwrap();
        pty.read_for(Duration::from_millis(300));
        composer_row(&flipped_frame(pty))
    }

    #[test]
    #[serial]
    fn readline_chords_edit_the_composer() {
        let dir = fresh_config_dir("readline-chords");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"alpha beta gamma").unwrap();
        repainted_frame_showing(&mut pty, "alpha beta gamma", "the composer took no text");

        assert_eq!(
            step(&mut pty, CTRL_W),
            "alpha beta",
            "Ctrl+W did not cut the last word"
        );
        assert_eq!(
            step(&mut pty, CTRL_Y),
            "alpha beta gamma",
            "Ctrl+Y did not paste the cut word"
        );
        pty.send(ALT_B).unwrap();
        pty.read_for(Duration::from_millis(150));
        pty.send(ALT_B).unwrap();
        pty.read_for(Duration::from_millis(150));
        assert_eq!(
            step(&mut pty, CTRL_K),
            "alpha",
            "Alt+B twice then Ctrl+K did not cut `beta gamma`"
        );
        assert_eq!(
            step(&mut pty, CTRL_Y),
            "alpha beta gamma",
            "Ctrl+Y did not paste the killed text"
        );
        pty.send(HOME).unwrap();
        pty.read_for(Duration::from_millis(150));
        pty.send(ALT_F).unwrap();
        pty.read_for(Duration::from_millis(150));
        // The wiki promises "move a word", not emacs end-of-word: accept the
        // cursor landing at either edge of the gap after `alpha`.
        let moved = step(&mut pty, b"Z");
        assert!(
            moved == "alphaZ beta gamma" || moved == "alpha Zbeta gamma",
            "Alt+F did not move one word forward: {moved:?}"
        );
        assert_eq!(
            step(&mut pty, CTRL_UNDERSCORE),
            "alpha beta gamma",
            "Ctrl+_ did not undo the typed Z"
        );
        pty.send(HOME).unwrap();
        pty.read_for(Duration::from_millis(150));
        assert_eq!(
            step(&mut pty, ALT_D),
            "beta gamma",
            "Alt+D did not cut the word after the cursor"
        );
        pty.send(CTRL_E).unwrap();
        pty.read_for(Duration::from_millis(150));
        pty.send(ALT_B).unwrap();
        pty.read_for(Duration::from_millis(150));
        assert_eq!(
            step(&mut pty, CTRL_U),
            "gamma",
            "Ctrl+U did not cut back to the start of the line"
        );
        leave(pty);
    }

    #[test]
    #[serial]
    fn in_a_text_field_question_mark_types_ctrl_x_shows_the_todo_and_function_keys_do_nothing() {
        let dir = fresh_config_dir("text-field-keys");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b".temp-role you are the temp role g4r\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        let before = pty.raw().len();
        pty.send(b".save role\r").unwrap();
        pty.wait_for_after("Role name:", before, PTY_DEADLINE)
            .expect("`.save role` did not ask `Role name:`");
        pty.read_for(HOTKEY_GAP);

        pty.send(b"?").unwrap();
        for key in FUNCTION_KEYS {
            pty.send(key).unwrap();
            pty.read_for(Duration::from_millis(100));
        }
        pty.send(b"one two").unwrap();
        pty.read_for(Duration::from_millis(400));
        let frame = flipped_frame(&mut pty);
        assert!(
            !shows(&frame, HELP_MARK),
            "`?` in the text field opened the key help:\n{frame}"
        );
        assert!(
            shows(&frame, "?one two"),
            "`?` was not typed into the field, or an F-key left debris:\n{frame}"
        );

        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = flipped_frame(&mut pty);
        assert!(
            frame.contains("Todo"),
            "Ctrl+X in the text field did not show the todo panel:\n{frame}"
        );
        assert!(
            shows(&frame, "?one two") && shows(&frame, "Role name:"),
            "Ctrl+X ate the field text or closed the question:\n{frame}"
        );
        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(300));
        esc_cancels_the_modal(&mut pty, ".save role");
        leave(pty);
    }
}

mod key_help_and_chords {
    //! Real-process checks of the TUI's key discovery and panel chords, written
    //! from the keys contract rather than the dispatcher: no function key does
    //! anything (F1-F12 on the empty composer open no panel, no roster and no
    //! key help, and type nothing); Alt+T closes the roster it opened, and `?`
    //! with the roster open does not open the key help; Ctrl+X opens the todo
    //! panel without eating the composer's text, and Ctrl+E is still the
    //! composer's end of line; the plain REPL does not know `.keys`. F1-F12
    //! also leave the roster, the key help and the pager as they are; `?` in
    //! the history search band opens no help; under a Select modal Ctrl+X
    //! shows the todo panel while Alt+T, Ctrl+G, `?` and the function keys do
    //! nothing. Each case runs the binary on a pseudo-terminal, so the module
    //! is unix-only, and the cases run one at a time.
    use std::time::Duration;

    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
    use serial_test::serial;

    const CTRL_E: &[u8] = b"\x05";
    const HOME: &[u8] = b"\x1b[H";
    /// A row only the key help shows.
    const HELP_MARK: &str = "Suspend to the shell";
    /// The roster's legend.
    const ROSTER_MARK: &str = "Esc = close";

    /// xterm's encodings of F1-F12.
    const FUNCTION_KEYS: [&[u8]; 12] = [
        b"\x1bOP",
        b"\x1bOQ",
        b"\x1bOR",
        b"\x1bOS",
        b"\x1b[15~",
        b"\x1b[17~",
        b"\x1b[18~",
        b"\x1b[19~",
        b"\x1b[20~",
        b"\x1b[21~",
        b"\x1b[23~",
        b"\x1b[24~",
    ];

    fn boot(scenario: &str, extra: &str) -> (TempDirGuard, Pty) {
        let dir = fresh_config_dir(scenario);
        let guard = TempDirGuard(dir.clone());
        write_config(&dir, &format!("save_session: false\n{extra}"));
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(400));
        (guard, pty)
    }

    /// Ctrl+C clears a non-empty composer and quits from an empty one, so the
    /// Ctrl+D after it may find the UI already gone.
    fn leave(mut pty: Pty) {
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(300));
        let _ = pty.send(b"\x04");
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert!(!transcript.contains("panicked"), "{transcript}");
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    fn assert_help_closed(pty: &mut Pty, by: &str) {
        pty.read_for(Duration::from_millis(400));
        let frame = repainted_frame_showing(
            pty,
            "no session",
            &format!("{by} did not close the key help"),
        );
        assert!(
            !squeezed(&frame).contains(&squeezed(HELP_MARK)),
            "the key help is still up after {by}:\n{frame}"
        );
    }

    #[test]
    #[serial]
    fn function_keys_do_nothing_on_the_empty_composer() {
        let (_guard, mut pty) = boot("fkeys", "");
        for key in FUNCTION_KEYS {
            pty.send(key).unwrap();
            pty.read_for(Duration::from_millis(150));
        }
        let frame = flipped_frame(&mut pty);
        let flat = squeezed(&frame);
        assert!(
            !flat.contains(&squeezed(ROSTER_MARK)),
            "an F-key opened the roster:\n{frame}"
        );
        assert!(
            !frame.contains("Todo"),
            "an F-key opened the todo panel:\n{frame}"
        );
        assert!(
            !flat.contains(&squeezed(HELP_MARK)),
            "an F-key opened the key help:\n{frame}"
        );
        pty.send(b"q4z").unwrap();
        let typed = repainted_frame_showing(&mut pty, "q4z", "the composer stopped taking keys");
        assert_eq!(
            composer_row(&typed),
            "q4z",
            "an F-key left text in the composer:\n{typed}"
        );
        leave(pty);
    }

    #[test]
    #[serial]
    fn alt_t_toggles_the_roster_and_question_mark_over_it_opens_no_help() {
        let (_guard, mut pty) = boot("roster-toggle", "");
        pty.send(ALT_T).unwrap();
        repainted_frame_showing(&mut pty, ROSTER_MARK, "Alt+T did not open the roster");

        pty.send(b"?").unwrap();
        pty.read_for(Duration::from_millis(400));
        let frame = flipped_frame(&mut pty);
        assert!(
            !squeezed(&frame).contains(&squeezed(HELP_MARK)),
            "? over the open roster opened the key help:\n{frame}"
        );

        pty.send(ALT_T).unwrap();
        pty.read_for(Duration::from_millis(400));
        let frame = flipped_frame(&mut pty);
        assert!(
            !squeezed(&frame).contains(&squeezed(ROSTER_MARK)),
            "a second Alt+T did not close the roster:\n{frame}"
        );
        leave(pty);
    }

    #[test]
    #[serial]
    fn ctrl_x_keeps_the_composer_text_and_ctrl_e_is_still_end_of_line() {
        let (_guard, mut pty) = boot("ctrl-x-text", "");
        pty.send(b"bc").unwrap();
        pty.read_for(Duration::from_millis(200));
        pty.send(HOME).unwrap();
        pty.send(b"a").unwrap();
        pty.read_for(Duration::from_millis(200));
        pty.send(CTRL_E).unwrap();
        pty.send(b"d9v").unwrap();
        repainted_frame_showing(
            &mut pty,
            "abcd9v",
            "Ctrl+E did not move to the end of the line",
        );

        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = flipped_frame(&mut pty);
        assert!(
            frame.contains("Todo"),
            "Ctrl+X with text did not open the todo panel:\n{frame}"
        );
        assert!(
            squeezed(&frame).contains("abcd9v"),
            "Ctrl+X ate the composer's text:\n{frame}"
        );
        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = flipped_frame(&mut pty);
        assert!(
            !frame.contains("Todo"),
            "a second Ctrl+X did not close the todo panel:\n{frame}"
        );
        leave(pty);
    }

    #[test]
    #[serial]
    fn the_plain_repl_does_not_know_dot_keys() {
        let dir = fresh_config_dir("repl-keys");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");
        let at = wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
        pty.send(b".keys\r").unwrap();
        pty.wait_for_after("Unknown command", at, REPL_PROMPT_DEADLINE)
            .expect("the REPL did not refuse .keys as an unknown command");
        let out = strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..]));
        assert!(
            !out.contains(HELP_MARK),
            "the REPL printed key help:\n{out}"
        );
        assert!(
            find(pty.raw(), ENTER_ALT, 0).is_none(),
            "the REPL entered the alternate screen"
        );
        let refused = find(pty.raw(), b"Unknown command", at).unwrap();
        wait_for_repl_prompt(&mut pty, "dry-model)>", refused);
        pty.send(b".exit\r").unwrap();
        let transcript = pty
            .wait_for("tty-probe-done", REPL_PROMPT_DEADLINE)
            .unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
    }

    // ---------------------------------------------------------------------
    // Function keys over the roster, the key help and the pager; `?` in a
    // band with an empty composer; Alt+T and Ctrl+X under a prompt modal.
    // ---------------------------------------------------------------------

    const CTRL_T: &[u8] = b"\x14";
    /// The pager's header for the root thread.
    const PAGER_MARK: &str = "Thread: root thread";

    fn send_function_keys(pty: &mut Pty) {
        for key in FUNCTION_KEYS {
            pty.send(key).unwrap();
            pty.read_for(Duration::from_millis(150));
        }
        pty.read_for(Duration::from_millis(300));
    }

    /// F1-F12 leave whatever is open as it is: the roster stays up and opens
    /// no key help, the key help stays up, and the pager stays up (Ctrl+X in
    /// the pager does nothing either). None of them is taken for an Esc.
    #[test]
    #[serial]
    fn function_keys_leave_the_roster_the_key_help_and_the_pager_as_they_are() {
        let (_guard, mut pty) = boot("fkeys-over", "");

        pty.send(ALT_T).unwrap();
        repainted_frame_showing(&mut pty, ROSTER_MARK, "Alt+T did not open the roster");
        send_function_keys(&mut pty);
        let frame = flipped_frame(&mut pty);
        assert!(
            squeezed(&frame).contains(&squeezed(ROSTER_MARK)),
            "an F-key closed the roster:\n{frame}"
        );
        assert!(
            !squeezed(&frame).contains(&squeezed(HELP_MARK)),
            "an F-key over the roster opened the key help:\n{frame}"
        );
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(400));

        pty.send(b".keys\r").unwrap();
        repainted_frame_showing(&mut pty, HELP_MARK, ".keys did not open the key help");
        send_function_keys(&mut pty);
        let frame = flipped_frame(&mut pty);
        assert!(
            squeezed(&frame).contains(&squeezed(HELP_MARK)),
            "an F-key closed the key help:\n{frame}"
        );
        pty.send(b"q").unwrap();
        assert_help_closed(&mut pty, "q");

        pty.send(CTRL_T).unwrap();
        repainted_frame_showing(&mut pty, PAGER_MARK, "Ctrl+T did not open the pager");
        send_function_keys(&mut pty);
        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(400));
        let frame = flipped_frame(&mut pty);
        assert!(
            squeezed(&frame).contains(&squeezed(PAGER_MARK)),
            "an F-key or Ctrl+X closed the pager:\n{frame}"
        );
        assert!(
            !frame.contains("Todo"),
            "Ctrl+X in the pager opened the todo panel:\n{frame}"
        );
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(400));
        repainted_frame_showing(&mut pty, "no session", "Esc did not close the pager");
        leave(pty);
    }

    /// `?` with the composer empty but the history search band open is a
    /// search character, not the key help.
    #[test]
    #[serial]
    fn question_mark_in_the_history_search_band_opens_no_help() {
        let (_guard, mut pty) = boot("search-help", "");
        pty.send(CTRL_R).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"?").unwrap();
        pty.read_for(Duration::from_millis(400));
        let frame = flipped_frame(&mut pty);
        assert!(
            !squeezed(&frame).contains(&squeezed(HELP_MARK)),
            "? in the history search band opened the key help:\n{frame}"
        );
        assert!(
            squeezed(&frame).contains(&squeezed("history: ?")),
            "the history search band closed or did not take ? as its query:\n{frame}"
        );
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(400));
        leave(pty);
    }

    /// Under a Select modal `?` and F1-F12 leave the modal up and open no key
    /// help, Alt+T opens no roster (the modal has the keyboard), the unbound
    /// Ctrl+G does nothing either, and Ctrl+X still shows the todo panel
    /// beside the question.
    #[test]
    #[serial]
    fn under_a_select_modal_ctrl_x_shows_the_todo_and_alt_t_and_question_mark_do_nothing() {
        let dir = fresh_config_dir("modal-chords");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_embeddings(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(400));
        let before = pty.raw().len();
        pty.send(b".rag new-rag-m3c\r").unwrap();
        wait_for_the_modal(&mut pty, before, ".rag new-rag-m3c", &["dry-embed-a"]);
        pty.read_for(HOTKEY_GAP);

        pty.send(b"?").unwrap();
        send_function_keys(&mut pty);
        pty.send(ALT_T).unwrap();
        pty.send(b"\x07").unwrap();
        pty.read_for(Duration::from_millis(400));
        let frame = flipped_frame(&mut pty);
        let flat = squeezed(&frame);
        assert!(
            flat.contains("Esc=cancel") && flat.contains("dry-embed-a"),
            "?, an F-key, Alt+T or Ctrl+G closed the modal:\n{frame}"
        );
        assert!(
            !flat.contains(&squeezed(HELP_MARK)),
            "? or an F-key under the modal opened the key help:\n{frame}"
        );
        assert!(
            !flat.contains(&squeezed(ROSTER_MARK)),
            "Alt+T or Ctrl+G under the modal opened the roster:\n{frame}"
        );

        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = flipped_frame(&mut pty);
        assert!(
            frame.contains("Todo"),
            "Ctrl+X under the modal did not show the todo panel:\n{frame}"
        );
        assert!(
            squeezed(&frame).contains("Esc=cancel"),
            "Ctrl+X closed the modal:\n{frame}"
        );
        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(300));
        esc_cancels_the_modal(&mut pty, ".rag new-rag-m3c");
        leave(pty);
    }
}

mod key_help_discovery {
    //! Real-process checks of how the TUI points at its keys: function keys
    //! held with a modifier (Shift+F1, Ctrl+F2, Alt+F4, Ctrl+F5, Shift+F12)
    //! do nothing and leave no escape debris, and the idle tasks pop-up
    //! shows its title, holds the keys typed over it and closes. Each case
    //! runs the binary on a pseudo-terminal, so the module is unix-only, and
    //! the cases run one at a time.
    use std::time::Duration;

    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// A row only the key help shows.
    const HELP_MARK: &str = "Suspend to the shell";
    /// The roster's legend.
    const ROSTER_MARK: &str = "Esc = close";
    const IDLE_HINT: &str = "? = keys  Alt+T = tasks  Alt+S = threads  Ctrl+X = todo";
    const TEXT_HINT: &str = "Alt+T = tasks  Alt+S = threads  Ctrl+X = todo";

    fn boot(scenario: &str, extra: &str) -> (TempDirGuard, Pty) {
        let dir = fresh_config_dir(scenario);
        let guard = TempDirGuard(dir.clone());
        write_config(&dir, &format!("save_session: false\n{extra}"));
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(400));
        (guard, pty)
    }

    /// Ctrl+C clears a non-empty composer and quits from an empty one, so the
    /// Ctrl+D after it may find the UI already gone.
    fn leave(mut pty: Pty) {
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(300));
        let _ = pty.send(b"\x04");
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert!(!transcript.contains("panicked"), "{transcript}");
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    fn shows(frame: &str, needle: &str) -> bool {
        squeezed(frame).contains(&squeezed(needle))
    }

    #[test]
    #[serial]
    fn function_keys_held_with_a_modifier_do_nothing_either() {
        let (_guard, mut pty) = boot("discovery-modified-fkeys", "");
        // Shift+F1, Ctrl+F2, Alt+F4, Ctrl+F5 and Shift+F12 as xterm sends
        // them. Alt+F3 would be `\x1b[1;3R`, which is also a cursor position
        // report.
        for key in [
            &b"\x1b[1;2P"[..],
            b"\x1b[1;5Q",
            b"\x1b[1;3S",
            b"\x1b[15;5~",
            b"\x1b[24;2~",
        ] {
            pty.send(key).unwrap();
            pty.read_for(Duration::from_millis(150));
        }
        let frame = flipped_frame(&mut pty);
        for (mark, what) in [(ROSTER_MARK, "the roster"), (HELP_MARK, "the key help")] {
            assert!(
                !shows(&frame, mark),
                "a modified F-key opened {what}:\n{frame}"
            );
        }
        assert!(
            !frame.contains("Todo"),
            "a modified F-key opened the todo panel:\n{frame}"
        );
        pty.send(b"k7q").unwrap();
        let frame = repainted_frame_showing(&mut pty, "k7q", "the composer stopped taking keys");
        assert!(
            shows(&frame, TEXT_HINT),
            "the UI lost its hint row:\n{frame}"
        );
        assert_eq!(
            composer_row(&frame),
            "k7q",
            "a modified F-key left escape debris in the composer:\n{frame}"
        );
        leave(pty);
    }

    /// With nothing running, Alt+T opens the tasks pop-up over the
    /// transcript with its title and way out; it takes the keys typed
    /// while it is up, and Esc or a second Alt+T closes it.
    #[test]
    #[serial]
    fn the_idle_tasks_popup_shows_its_title_holds_focus_and_closes() {
        let (_guard, mut pty) = boot("discovery-tasks-popup", "");
        pty.read_for(HOTKEY_GAP);
        pty.send(ALT_T).unwrap();
        let open = repainted_frame_showing(&mut pty, ROSTER_MARK, "Alt+T did not open the pop-up");
        assert!(
            open.contains("\u{250c}Tasks"),
            "the tasks pop-up has no `Tasks` title on its border:\n{open}"
        );
        assert!(
            shows(&open, "Type a prompt or a .command"),
            "the composer is not in view under the pop-up:\n{open}"
        );

        pty.send(b"z7").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(ESC).unwrap();
        let closed = repainted_frame_showing(&mut pty, IDLE_HINT, "Esc did not close the pop-up");
        assert!(
            !closed.contains("\u{250c}Tasks") && !shows(&closed, ROSTER_MARK),
            "the tasks pop-up is still up after Esc:\n{closed}"
        );
        assert!(
            !composer_row(&closed).contains("z7"),
            "keys typed over the pop-up reached the composer:\n{closed}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(ALT_T).unwrap();
        repainted_frame_showing(&mut pty, ROSTER_MARK, "Alt+T did not reopen the pop-up");
        pty.read_for(HOTKEY_GAP);
        pty.send(ALT_T).unwrap();
        let closed = repainted_frame_showing(&mut pty, IDLE_HINT, "Alt+T did not close the pop-up");
        assert!(
            !closed.contains("\u{250c}Tasks"),
            "a second Alt+T left the pop-up up:\n{closed}"
        );
        leave(pty);
    }
}

mod editor_status_history_reset_and_bare_agent {
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
    //! round is needed. Unix-only and serial like its sibling pty modules.
    use std::fs;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::Pty;
    use super::common::tui_harness::*;
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
}
