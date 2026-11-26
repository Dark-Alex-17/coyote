//! Real-process checks of tool turns and the bottom pane: the questions
//! a tool asks through the prompt protocol, the composer's colours and
//! the prompt modals, and turns driven by a scripted model with tools
//! running. Every module drives the binary through the shared pty
//! harness in `common`, so the file is unix-only.
#![cfg(unix)]

mod common;

mod binary_tools {
    //! Real-process checks of the questions a tool asks through the prompt
    //! protocol: its confirm, list, checkbox and input questions as modals
    //! under the UI and inquire prompts in the plain REPL, and what the tool
    //! gets back for a typed, exact-option or blank answer on the custom row.
    //! Next to them: the confirm a rebuild over a failing document asks, the
    //! select an MCP server conflict asks during `.install`, the `init RAG?`
    //! question an agent with documents asks when it starts, and the `.rag`
    //! wizard and macro steps that reach those questions.
    //! After them: the Ctrl+T pager over a tool's whole output with its find
    //! and thread picker, the Alt+T roster cancelling a background job, and both
    //! with nothing to show, and the key help that `?` opens from the status
    //! row's hint. What the UI prints on the normal screen when it leaves is
    //! checked in `binary_exit` in `tui_lifecycle_and_signals.rs`.
    //! Each case runs the binary on a pseudo-terminal under a bash wrapper
    //! that reports the exit code and `stty -a` afterwards, so the module is
    //! unix-only. The cases run one at a time: each spawn forks a session
    //! leader and takes a controlling tty, and a fork landing between another
    //! test's `openpty` and its `exec` has failed the spawn with a garbled
    //! errno.
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
    use serial_test::serial;

    // ---------------------------------------------------------------------------
    // A tool question carrying invisible format characters, a reply in palette
    // grey, a rebuild whose document fails to load, and an MCP conflict on
    // `.install` under the UI and with stdout redirected.
    // ---------------------------------------------------------------------------

    /// The UTF-8 of each format character the question below carries, so the
    /// raw terminal stream can be searched for them.
    const FORMAT_CHARACTERS: [(&str, &[u8]); 5] = [
        ("U+202E right-to-left override", "\u{202E}".as_bytes()),
        ("U+202C pop directional formatting", "\u{202C}".as_bytes()),
        ("U+200B zero-width space", "\u{200B}".as_bytes()),
        ("U+200F right-to-left mark", "\u{200F}".as_bytes()),
        ("U+FEFF byte-order mark", "\u{FEFF}".as_bytes()),
    ];

    /// A confirm tool whose question wraps the thing being approved in a
    /// right-to-left override (which would display `txt.safe` reversed), pads
    /// it with a zero-width space and ends in a right-to-left mark and a
    /// byte-order mark.
    fn format_character_confirm_tool() -> String {
        RECORDING_CONFIRM_TOOL.replace(
            "Really do it? q9d-q",
            "Delete \u{202E}txt.safe\u{202C} \u{200B}k7m\u{200F}\u{FEFF}? q9d-q",
        )
    }

    /// A tool-authored question carrying bidi overrides and zero-width
    /// characters is drawn in its stored order with each one spelled out as
    /// `\u{...}`: none of the characters reaches the terminal (where a real
    /// emulator would reorder or hide the text around them), `y` still
    /// answers the tool as a yes, and nothing of them is left on the normal
    /// screen after the UI leaves.
    #[test]
    #[serial]
    fn a_tools_confirm_question_with_format_characters_shows_them_spelled_out() {
        let dir = fresh_config_dir("tool-confirm-format-chars");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("confirm-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_confirm_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done f8c".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_confirm_rec.sh",
            &format_character_confirm_tool(),
        );
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let submitted_at = pty.raw().len();
        pty.send(b"call the confirm tool\r").unwrap();
        pty.wait_for("q9d-q", PTY_DEADLINE)
            .expect("the tool's confirm question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[submitted_at..]));
        assert!(
            squeezed(&since)
                .contains(r"Delete\u{202e}txt.safe\u{202c}\u{200b}k7m\u{200f}\u{feff}?q9d-q"),
            "the question was not drawn in its stored order with the format characters spelled out:\n{since}"
        );
        for (name, bytes) in FORMAT_CHARACTERS {
            assert!(
                find(pty.raw(), bytes, submitted_at).is_none(),
                "{name} reached the terminal while the modal was up:\n{since}"
            );
        }

        pty.send(b"y").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done f8c", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "1",
            "`y` on the scrubbed question did not reach the tool as a yes:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        for (name, bytes) in FORMAT_CHARACTERS {
            assert!(
                find(pty.raw(), bytes, submitted_at).is_none(),
                "{name} reached the terminal by the time the UI left:\n{transcript}"
            );
        }
    }

    /// A reply coloured with the palette index 8 (`ESC[38;5;8m`, the grey
    /// most prompts use for de-emphasis) is painted as colour: the word is
    /// visible, the sequence is not drawn as text, the palette foreground is
    /// what reaches the terminal, and nothing conceals. The `8` inside an
    /// extended colour is a colour argument, not the conceal attribute.
    #[test]
    #[serial]
    fn a_reply_in_palette_grey_is_painted_as_colour_not_stripped_as_conceal() {
        let dir = fresh_config_dir("palette-grey");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text(
            "before g8a \x1b[38;5;8mgrey-word-g8b\x1b[0m after g8c".to_string(),
        )]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let submitted_at = pty.raw().len();
        pty.send(b"grey please\r").unwrap();
        let transcript = wait_for_squeezed(&mut pty, "after g8c", "the reply's tail");
        assert_eq!(fake.served(), 1, "{transcript}");
        pty.read_for(Duration::from_millis(600));

        let raw = pty.raw();
        let visible = strip_ansi(&String::from_utf8_lossy(&raw[submitted_at..]));
        assert!(
            squeezed(&visible).contains("grey-word-g8b"),
            "the grey word was not painted:\n{visible}"
        );
        assert!(
            !visible.contains("[38;5;8m") && !visible.contains("[0m"),
            "the colour sequence was painted as text:\n{visible}"
        );
        let params = sgr_params(&raw[submitted_at..]);
        assert!(
            params.iter().any(|p| p.windows(3).any(|w| w == [38, 5, 8])),
            "the palette-grey foreground never reached the terminal (stripped as conceal?); SGRs seen: {params:?}\n{visible}"
        );
        let concealing: Vec<_> = params.iter().filter(|p| is_conceal_sgr(p)).collect();
        assert!(
            concealing.is_empty(),
            "a conceal SGR reached the tty: {concealing:?}\n{visible}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// The dry-run config with two embedding models and a document loader
    /// for the `broken` extension that always fails. Returns the loader.
    fn write_dry_run_config_with_a_failing_loader(dir: &Path) -> PathBuf {
        let script = dir.join("broken-loader.sh");
        fs::write(
            &script,
            "#!/usr/bin/env bash\necho 'loader refused r8l' >&2\nexit 1\n",
        )
        .unwrap();
        write_config(
            dir,
            &format!(
                "\x20     - name: dry-embed-a\n\
             \x20       type: embedding\n\
             \x20       max_input_tokens: 2048\n\
             \x20       default_chunk_size: 1000\n\
             \x20       max_batch_size: 100\n\
             save_session: false\n\
             document_loaders:\n\
             \x20 broken: 'bash {} $1'\n",
                script.display()
            ),
        );
        script
    }

    /// A named yaml rag over one document whose loader fails, with no files
    /// indexed yet, so `.rag <name>` loads it and `.rebuild rag` has to load
    /// the document.
    fn seed_rag_with_a_failing_document(dir: &Path, name: &str) -> (PathBuf, String) {
        let document = dir.join("doc.broken");
        fs::write(&document, "body\n").unwrap();
        let rags = dir.join("rags");
        fs::create_dir_all(&rags).unwrap();
        let rag_path = rags.join(format!("{name}.yaml"));
        let seeded = format!(
            "driver: yaml\n\
         embedding_model: dryrun:dry-embed-a\n\
         chunk_size: 1000\n\
         chunk_overlap: 50\n\
         top_k: 5\n\
         document_paths:\n\
         - '{}'\n\
         files: {{}}\n\
         marker: rag-survives-r8r\n",
            document.display()
        );
        fs::write(&rag_path, &seeded).unwrap();
        (rag_path, seeded)
    }

    /// `.rebuild rag` over a document whose loader fails: the sync's
    /// `Some documents failed to load. Continue?` is the Confirm modal under
    /// the UI (not a raw-tty `[y/N]` prompt, not an abort naming the plain
    /// REPL); `n` aborts with `Aborted: some documents failed to load` after
    /// the load and warning rows for the document. The rag file is left
    /// exactly as it was, the terminal is never released, and the UI keeps
    /// taking keys and leaves clean.
    #[test]
    #[serial]
    fn dot_rebuild_rag_with_a_failing_document_asks_continue_in_the_confirm_modal_and_n_aborts() {
        let dir = fresh_config_dir("rebuild-rag-failed-doc");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_a_failing_loader(&dir);
        let name = "kb-w8";
        let (rag_path, seeded) = seed_rag_with_a_failing_document(&dir, name);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 120).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(format!(".rag {name}\r").as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(1500));
        let before = pty.raw().len();
        pty.send(b".rebuild rag\r").unwrap();
        let modal = wait_for_the_modal(
            &mut pty,
            before,
            ".rebuild rag",
            &["Somedocumentsfailedtoload.Continue?", "y=yes"],
        );
        assert!(
            !modal.contains("No RAG"),
            "`.rag {name}` did not seat the rag, so the rebuild had nothing to work on:\n{modal}"
        );
        let flat_modal = squeezed(&modal);
        assert!(
            !flat_modal.contains("[y/N]") && !flat_modal.contains("(y/N)"),
            "`Continue?` was asked on the raw tty, not in the modal:\n{modal}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"n").unwrap();
        wait_for_squeezed(
            &mut pty,
            "Aborted: some documents failed to load",
            "the abort after `n` on the modal",
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        let squeezed_since = squeezed(&since);
        // The load row names the document's full path, which a Command cell
        // cuts at the pane's edge; the row's head and the scenario dir are
        // enough to place it.
        assert!(
            squeezed_since.contains("Load/") && squeezed_since.contains("rebuild-rag-failed-doc"),
            "the load row for the document was not printed before the question:\n{since}"
        );
        assert!(
            // The warning row leads with the warning sign (U+26A0).
            squeezed_since.contains("loaderrefusedr8l") || squeezed_since.contains("\u{26a0}"),
            "the warning row for the failed document was not printed:\n{since}"
        );
        assert!(
            !squeezed_since.contains(&squeezed("Re-run in the plain REPL")),
            "the abort after `n` still names the plain REPL as the way to choose:\n{since}"
        );
        assert!(
            !squeezed_since.contains(&squeezed("Re-run in a terminal")),
            "the abort says `Re-run in a terminal`, but the UI is on a terminal:\n{since}"
        );
        assert!(
            !since.contains("exit="),
            "the aborted rebuild quit the UI:\n{since}"
        );
        assert_eq!(
            fs::read_to_string(&rag_path).unwrap(),
            seeded,
            "the aborted rebuild rewrote the rag file:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "the aborted rebuild released the terminal:\n{since}"
        );

        let transcript = assert_ui_alive_then_quit(&mut pty, "the aborted rebuild");
        assert_eq!(
            fs::read_to_string(&rag_path).unwrap(),
            seeded,
            "the rag file changed by the time the UI left:\n{transcript}"
        );
    }

    /// `y` on the same modal lets the rebuild go on past the failed
    /// document; with no other document the corpus is empty, so the rebuild
    /// reports `No RAG files` instead of the abort, the rag file is left as
    /// it was, and the UI keeps taking keys and leaves clean.
    #[test]
    #[serial]
    fn dot_rebuild_rag_with_a_failing_document_and_y_goes_on_to_report_the_empty_corpus() {
        let dir = fresh_config_dir("rebuild-rag-failed-doc-y");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with_a_failing_loader(&dir);
        let name = "kb-w8";
        let (rag_path, seeded) = seed_rag_with_a_failing_document(&dir, name);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 120).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(format!(".rag {name}\r").as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(1500));
        let before = pty.raw().len();
        pty.send(b".rebuild rag\r").unwrap();
        wait_for_the_modal(
            &mut pty,
            before,
            ".rebuild rag",
            &["Somedocumentsfailedtoload.Continue?", "y=yes"],
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_squeezed(
            &mut pty,
            "No RAG files",
            "the empty-corpus report after `y` on the modal",
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        assert!(
            !squeezed(&since).contains(&squeezed("Aborted: some documents failed to load")),
            "`y` on the modal still aborted the rebuild:\n{since}"
        );
        assert!(
            !since.contains("exit="),
            "the rebuild over an empty corpus quit the UI:\n{since}"
        );
        assert_eq!(
            fs::read_to_string(&rag_path).unwrap(),
            seeded,
            "the rebuild over an empty corpus rewrote the rag file:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "the rebuild over an empty corpus released the terminal:\n{since}"
        );

        assert_ui_alive_then_quit(&mut pty, "the rebuild over an empty corpus");
    }

    /// `.install <local bundle>` whose `mcp.json` defines a server the local
    /// `mcp.json` already holds differently: under the UI the menu that
    /// resolves the conflict is the Select modal with the four choices,
    /// `1` keeps the local definition, the install completes without a
    /// refusal, the local server definition is untouched, the UI keeps
    /// taking keys and leaves clean.
    #[test]
    #[serial]
    fn dot_install_with_a_conflicting_mcp_server_asks_in_the_select_modal_and_keep_local_keeps_it()
    {
        let dir = fresh_config_dir("install-mcp-conflict-tui");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![]);
        write_fake_llm_config(&dir, &fake);
        let local_before = write_conflicting_local_mcp_server(&dir);
        let repo = write_bundle_repo(&dir, "bundle-k7s", &[]);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        let install_at = pty.raw().len();
        let sent = std::time::Instant::now();
        pty.send(format!(".install {}\r", repo.display()).as_bytes())
            .unwrap();
        let modal = wait_for_the_modal(
            &mut pty,
            install_at,
            ".install",
            &[
                "ConflictonMCPserver'bundle-k7s-server'",
                "keeplocal",
                "takeremote",
                "bundle-k7s-server-remote",
                "abortmerge",
            ],
        );
        let took = sent.elapsed();
        assert!(
            took < Duration::from_secs(10),
            "the menu took {took:?} to come up:\n{modal}"
        );
        assert!(
            !squeezed(&modal).contains(&squeezed(MCP_CONFLICT_REFUSAL)),
            "the merge was refused instead of asked in the modal:\n{modal}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"1").unwrap();
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        loop {
            pty.read_for(Duration::from_millis(200));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
            if squeezed(&since).contains("keptlocal:bundle-k7s-server") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "`keep local` did not let the merge report the kept server:\n{since}"
            );
        }
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
        let squeezed_since = squeezed(&since);
        assert!(
            !squeezed_since.contains(&squeezed(MCP_CONFLICT_REFUSAL)),
            "the merge was refused after the menu was answered:\n{since}"
        );
        assert!(
            !squeezed_since.contains(&squeezed(TERMINAL_HINT)),
            "the install under the UI points at another UI:\n{since}"
        );
        assert!(
            !since.contains("exit="),
            "the install quit the UI:\n{since}"
        );
        let installed_role = dir.join("roles").join("bundle-k7s-role.md");
        assert!(
            installed_role.exists(),
            "the bundle's role was not installed at {}:\n{since}",
            installed_role.display()
        );
        // The merge writes mcp.json back formatted; the server definition is
        // what `keep local` keeps.
        let local_before: serde_json::Value = serde_json::from_str(&local_before).unwrap();
        let merged = |dir: &Path| -> serde_json::Value {
            serde_json::from_str(&fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap()
        };
        assert_eq!(
            merged(&dir),
            local_before,
            "`keep local` changed the local mcp.json:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "the conflict menu released the terminal:\n{since}"
        );

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("`.info` after the install never rendered");
        assert_eq!(fake.served(), 0, "a bundle install reached the model");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(
            merged(&dir),
            local_before,
            "the local mcp.json changed by the time the UI left:\n{transcript}"
        );
    }

    /// The same conflict with stdout redirected: `coyote --install <bundle>`
    /// with a tty on stdin but stdout a file (the shape of `coyote --install
    /// x > log`), no UI involved. The menu cannot be shown without a terminal
    /// on stdout, so the merge is refused in the shared no-tty shape, naming
    /// the server and ending with the CLI remedy word for word (not the plain
    /// REPL's, which is not what stands in the way), the command fails, and
    /// the local server definition is untouched.
    #[test]
    #[serial]
    fn an_install_with_stdout_redirected_and_a_conflicting_mcp_server_is_refused_naming_a_terminal()
    {
        let dir = fresh_config_dir("install-mcp-conflict-stdout-file");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![]);
        write_fake_llm_config(&dir, &fake);
        let local_before = write_conflicting_local_mcp_server(&dir);
        let repo = write_bundle_repo(&dir, "bundle-k7s", &[]);
        let captured = dir.join("stdout.log");
        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            &wait_wrapper_with_stdout_to(&captured),
            "/dev/tty",
            &format!(" --repl --install '{}'", repo.display()),
        );

        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let file = String::from_utf8_lossy(&fs::read(&captured).unwrap_or_default()).to_string();
        let everything = format!("{transcript}\n{file}");
        let (action, remedy) = refusal_shape(&everything).unwrap_or_else(|| {
        panic!(
            "the install with an MCP conflict and stdout redirected was not refused in the shared shape:\n--- terminal ---\n{transcript}\n--- stdout file ---\n{file}"
        )
    });
        assert!(
            action.starts_with("merge the already-installed MCP server 'bundle-k7s-server'"),
            "the refusal does not name the MCP merge and the server: {action:?}\n{everything}"
        );
        assert_eq!(
            remedy, CLI_INSTALL_REMEDY,
            "the refusal does not end with the documented remedy:\n{everything}"
        );
        assert_ne!(
            exit_code(&transcript),
            0,
            "a refused install exited 0:\n{everything}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("mcp.json")).unwrap(),
            local_before,
            "the refused merge changed the local mcp.json:\n{everything}"
        );
        assert!(
            find(pty.raw(), ENTER_ALT, 0).is_none(),
            "an install with stdout redirected entered the alternate screen:\n{transcript}"
        );
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------------
    // A tool's list question keeps its arrow help and its custom row, and the
    // asset-overwrite warning of `.install` is an Enter-answered confirm.
    // ---------------------------------------------------------------------------

    /// A tool that asks the human to pick one of three entries through the
    /// prompt helpers' `list` and records the index (or typed text) it got
    /// back, and next to it the helper's exit status: 0 for an index, 2 for
    /// typed text.
    const RECORDING_LIST_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Ask the user to pick one entry and record the index.\n\
    # @option --out! Where to write the answer.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    # shellcheck disable=SC1090\n\
    source \"$LLM_PROMPT_UTILS_FILE\"\n\
    \n\
    main() {\n\
    \x20   local out v rc=0\n\
    \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   v=\"$(list \"Pick one entry q7l-q\" alpha-q7l beta-q7l gamma-q7l)\" || rc=$?\n\
    \x20   printf '%s' \"$rc\" > \"$out-rc\"\n\
    \x20   printf '%s' \"${v:-cancelled}\" > \"$out\"\n\
    \x20   echo \"fixture picked: ${v:-cancelled} q7l-r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// The scripted model: one call of the list tool, then a closing line.
    fn list_tool_turn(dir: &Path) -> (FakeLlm, PathBuf) {
        let out = dir.join("list-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_list_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done q7l".to_string()),
        ]);
        write_fake_llm_config(dir, &fake);
        write_tool(dir, "fixture_list_rec.sh", RECORDING_LIST_TOOL);
        (fake, out)
    }

    /// The `list` exit status the recording tool saw for the answer at `out`.
    fn list_exit_status(out: &Path) -> String {
        fs::read_to_string(format!("{}-rc", out.display())).unwrap()
    }

    /// A tool's `list` question in the plain REPL is the inquire select it
    /// always was: the three entries and the `Other (custom)` row are listed
    /// under the question with the `to move, enter to select` help, a typed
    /// letter does not filter the list (Down then Enter still lands on the
    /// second entry, not on the only one starting with that letter), the
    /// index flows back to the tool and the turn completes.
    #[test]
    #[serial]
    fn a_tools_list_question_in_the_plain_repl_is_an_open_select_keeping_the_arrow_help() {
        let dir = fresh_config_dir("tool-list-repl");
        let _guard = TempDirGuard(dir.clone());
        let (fake, out) = list_tool_turn(&dir);
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        let asked_at = wait_for_repl_prompt(&mut pty, "fake-model)>", 0);
        pty.send(b"call the list tool\r").unwrap();
        pty.wait_for_after("q7l-q", asked_at, PTY_DEADLINE)
            .expect("the tool's list question never reached the terminal");
        pty.wait_for_after("gamma-q7l", asked_at, PTY_DEADLINE)
            .expect("the last entry of the list never reached the terminal");
        pty.read_for(Duration::from_millis(500));
        let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
        for entry in ["alpha-q7l", "beta-q7l", "gamma-q7l"] {
            assert!(
                asked.contains(entry),
                "the list question does not offer `{entry}`:\n{asked}"
            );
        }
        assert!(
            asked.contains("Other (custom)"),
            "a tool's list question does not offer the free-text row:\n{asked}"
        );
        assert!(
            asked.contains("to move, enter to select"),
            "the tool's list question lost its arrow help:\n{asked}"
        );
        assert!(
            find(pty.raw(), ENTER_ALT, 0).is_none(),
            "the plain REPL entered the alternate screen:\n{asked}"
        );

        // `g` would narrow a filtering select to `gamma-q7l`; here it is
        // ignored, so Down then Enter picks the second entry.
        pty.send(b"g").unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(400));
        let answered = pty.raw().len();
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "1",
            "Down then Enter on the list did not reach the tool as the second entry (a typed \
         letter must not filter the list):\n{transcript}"
        );
        assert_eq!(
            list_exit_status(&out),
            "0",
            "the chosen entry did not reach the tool with the index status:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        wait_for_repl_prompt(&mut pty, "fake-model)>", answered);
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// The same tool's `list` question under the UI is the Select modal with
    /// the tool's entries and the `Other (custom)` row, the terminal is never
    /// released, Down then Enter hands the tool the second entry's index, and
    /// on a second call text typed on the custom row flows back to the tool
    /// verbatim.
    #[test]
    #[serial]
    fn a_tools_list_question_under_the_ui_takes_typed_text_from_the_custom_row() {
        let dir = fresh_config_dir("tool-list-modal");
        let _guard = TempDirGuard(dir.clone());
        let picked = dir.join("list-answer-picked");
        let out = dir.join("list-answer");
        let call = |out: &Path| Reply::ToolCall {
            name: "fixture_list_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        };
        let fake = FakeLlm::start(vec![
            call(&picked),
            Reply::Text("fixture picked first q7l".to_string()),
            call(&out),
            Reply::Text("fixture done q7l".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_list_rec.sh", RECORDING_LIST_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        let ask = |pty: &mut Pty, line: &[u8]| {
            let asked_at = pty.raw().len();
            pty.send(line).unwrap();
            wait_for_the_modal(
                pty,
                asked_at,
                "the list tool",
                &[
                    "q7l-q",
                    "alpha-q7l",
                    "beta-q7l",
                    "gamma-q7l",
                    "Other (custom)",
                    "Up/Down=move",
                ],
            );
            pty.read_for(HOTKEY_GAP);
        };

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        ask(&mut pty, b"call the list tool\r");
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &picked, "the tool's recorded index");
        let transcript = wait_for_squeezed(
            &mut pty,
            "fixture picked first q7l",
            "the reply after the first call",
        );
        assert_eq!(
            fs::read_to_string(&picked).unwrap(),
            "1",
            "Down then Enter in the modal did not reach the tool as the second entry:\n{transcript}"
        );
        assert_eq!(
            list_exit_status(&picked),
            "0",
            "the chosen entry did not reach the tool with the index status:\n{transcript}"
        );

        pty.read_for(Duration::from_millis(300));
        ask(&mut pty, b"call the list tool again\r");
        for _ in 0..3 {
            pty.send(b"\x1b[B").unwrap();
            pty.read_for(Duration::from_millis(400));
        }
        pty.send(b"\r").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"typed-by-hand-x7q").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "typed-by-hand-x7q",
            "the text typed on the custom row did not reach the tool verbatim:\n{transcript}"
        );
        assert_eq!(
            list_exit_status(&out),
            "2",
            "typed text did not reach the tool with the custom-answer status:\n{transcript}"
        );
        assert_eq!(fake.served(), 4, "{transcript}");
        assert!(
            fake.bodies()[3].contains("fixture picked: typed-by-hand-x7q q7l-r"),
            "the tool's output handed to the model does not carry the typed text:\n{:?}",
            fake.bodies()
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// `.install <category>` in the plain REPL: the overwrite warning is
    /// followed by the inquire `Continue? (y/N)` confirm, which Enter
    /// answers - a bare `y` on its own does nothing until Enter follows
    /// (the single-key `[y/N]` read is gone), and a bare Enter takes the
    /// `No` default: `Aborted. No files were changed.` and the category
    /// directory is untouched.
    #[test]
    #[serial]
    fn dot_install_of_a_builtin_category_in_the_plain_repl_confirms_on_enter_not_on_a_bare_key() {
        let dir = fresh_config_dir("install-builtins-repl");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let macros_dir = dir.join("macros");
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
        // The built-in macros are seeded on startup; the question must not
        // touch them from here on.
        let files_before = walk(&macros_dir);
        let before = pty.raw().len();
        pty.send(b".install macros\r").unwrap();
        pty.wait_for_after("Continue?", before, PTY_DEADLINE)
            .expect("`.install macros` in the plain REPL did not ask `Continue?`");
        pty.read_for(Duration::from_millis(500));
        let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        // The warning body is a plain `println!` ahead of the question; the
        // terminal may wrap it, so it is looked for with whitespace removed.
        let flat_asked = squeezed(&asked);
        let warning_at = flat_asked
            .find("WARNING:Reinstallingbundledmacrosoverwrites")
            .unwrap_or_else(|| panic!("the overwrite warning was not printed:\n{asked}"));
        assert!(
            warning_at < flat_asked.find("Continue?").unwrap(),
            "the overwrite warning came after the `Continue?` question:\n{asked}"
        );
        assert!(
            asked.contains("(y/N)"),
            "the `Continue?` question is not the inquire confirm with its `(y/N)` legend:\n{asked}"
        );

        // A bare `y` is typed, not acted on.
        pty.send(b"y").unwrap();
        let after_y = pty.read_for(Duration::from_millis(1500));
        let flat = squeezed(&strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..])));
        assert!(
            !flat.contains("Aborted")
                && !flat.contains("Installed")
                && !flat.contains("Reinstalled"),
            "a bare `y` without Enter already settled the `Continue?` question:\n{after_y}"
        );
        assert_eq!(
            walk(&macros_dir),
            files_before,
            "a bare `y` without Enter already changed the macros directory"
        );

        // Backspace takes the `y` back; Enter then answers with the default.
        pty.send(b"\x7f").unwrap();
        pty.read_for(Duration::from_millis(300));
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

    // ---------------------------------------------------------------------
    // A tool's open select answered with typed text in the plain REPL and
    // from a Python tool under the UI, two MCP conflicts where the first is
    // cancelled, a tool input question whose default is the empty string,
    // and a tool checkbox question in both interfaces.
    // ---------------------------------------------------------------------

    /// Text a user might type on the custom row: inner spaces, both quote
    /// kinds, a dollar sign and a backslash, none of which may be expanded,
    /// escaped or dropped on the way to the tool.
    const TYPED_CUSTOM_ANSWER: &str = "my \"own\" pick $HOME 'x11' back\\slash";

    /// `list` in the plain REPL: the `Other (custom)` row opens the
    /// `Custom response:` text prompt, and the text typed there reaches the
    /// bash tool verbatim (the helper prints it as is and exits 2) and the
    /// model sees it in the tool's output.
    #[test]
    #[serial]
    fn a_tools_list_question_in_the_plain_repl_opens_custom_response_and_passes_the_typed_text_verbatim()
     {
        let dir = fresh_config_dir("tool-list-repl-custom");
        let _guard = TempDirGuard(dir.clone());
        let (fake, out) = list_tool_turn(&dir);
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        let asked_at = wait_for_repl_prompt(&mut pty, "fake-model)>", 0);
        pty.send(b"call the list tool\r").unwrap();
        pty.wait_for_after("Other (custom)", asked_at, PTY_DEADLINE)
            .expect("the tool's list question never offered the custom row");
        pty.read_for(Duration::from_millis(500));
        for _ in 0..3 {
            pty.send(b"\x1b[B").unwrap();
            pty.read_for(Duration::from_millis(300));
        }
        let custom_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        pty.wait_for_after("Custom response:", custom_at, PTY_DEADLINE)
            .expect("Enter on `Other (custom)` did not open the `Custom response:` prompt");
        pty.read_for(Duration::from_millis(400));
        pty.send(TYPED_CUSTOM_ANSWER.as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(400));
        let answered = pty.raw().len();
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            TYPED_CUSTOM_ANSWER,
            "the text typed at `Custom response:` did not reach the tool verbatim:\n{transcript}"
        );
        assert_eq!(
            list_exit_status(&out),
            "2",
            "typed text did not reach the tool with the custom-answer status:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");
        // The tool's output reaches the model as the `output` field of the
        // JSON object in the tool message's content.
        let tool_output = format!("fixture picked: {TYPED_CUSTOM_ANSWER} q7l-r");
        let body: serde_json::Value = serde_json::from_str(&fake.bodies()[1]).unwrap();
        let handed: Vec<String> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| message["content"].as_str())
            .filter_map(|content| serde_json::from_str::<serde_json::Value>(content).ok())
            .filter_map(|content| content["output"].as_str().map(str::to_string))
            .collect();
        assert!(
            handed.iter().any(|output| output.trim_end() == tool_output),
            "the tool's output handed to the model does not carry the typed text verbatim: \
         {handed:?}"
        );

        wait_for_repl_prompt(&mut pty, "fake-model)>", answered);
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A Python tool that loads the prompt helpers by the path the shim
    /// exports, asks one `select`, and records the Python type and value of
    /// the answer.
    const RECORDING_PY_SELECT_TOOL: &str = r#"import importlib.util
import os


def run(out: str) -> str:
    """Ask the user to pick one entry and record the answer type.

    Args:
        out (str): Where to write the answer.

    Returns:
        str: The answer the user gave.
    """
    spec = importlib.util.spec_from_file_location("prompt_utils", os.environ["LLM_PROMPT_UTILS_PY"])
    pu = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(pu)
    v = pu.select("Pick one entry p5y-q", ["alpha-p5y", "beta-p5y"])
    with open(out, "w") as f:
        f.write(type(v).__name__ + ":" + str(v))
    return "fixture picked: " + str(v) + " p5y-r"
"#;

    /// The Python helper's `select` under the UI: the Select modal offers the
    /// custom row, and text typed there comes back from `select` as a `str`
    /// holding exactly that text (an index is an `int`).
    #[test]
    #[serial]
    fn a_python_tools_select_under_the_ui_returns_the_typed_custom_text_as_a_str() {
        let dir = fresh_config_dir("tool-py-select-custom");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("py-select-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_py_select".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done p5y".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_py_select.py", RECORDING_PY_SELECT_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let asked_at = pty.raw().len();
        pty.send(b"call the python select tool\r").unwrap();
        wait_for_the_modal(
            &mut pty,
            asked_at,
            "the python select tool",
            &["p5y-q", "alpha-p5y", "beta-p5y", "Other (custom)"],
        );
        pty.read_for(HOTKEY_GAP);
        for _ in 0..2 {
            pty.send(b"\x1b[B").unwrap();
            pty.read_for(Duration::from_millis(400));
        }
        pty.send(b"\r").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"  typed by hand p5y ").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the python tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done p5y", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "str:  typed by hand p5y ",
            "the Python `select` did not return the typed text as a str:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A bundle repo whose mcp.json ships two servers, and a local mcp.json
    /// that holds both under different definitions: two conflicts in a row.
    fn two_mcp_conflicts(dir: &Path) -> (PathBuf, String) {
        let repo = write_bundle_repo(dir, "bundle-k7s", &[]);
        fs::write(
        repo.join("mcp.json"),
        "{\"mcpServers\": {\
         \"bundle-k7s-server\": {\"type\": \"stdio\", \"command\": \"echo\", \"args\": [\"k7s\"], \"env\": {}}, \
         \"bundle-k7s-second\": {\"type\": \"stdio\", \"command\": \"echo\", \"args\": [\"k7s-2\"], \"env\": {}}}}\n",
    )
    .unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "two servers"]);
        let local = "{\"mcpServers\": {\
                 \"bundle-k7s-server\": {\"type\": \"stdio\", \"command\": \"printf\", \"args\": [\"local-1-k7s\"], \"env\": {}}, \
                 \"bundle-k7s-second\": {\"type\": \"stdio\", \"command\": \"printf\", \"args\": [\"local-2-k7s\"], \"env\": {}}}}\n";
        fs::write(dir.join("mcp.json"), local).unwrap();
        (repo, local.to_string())
    }

    /// Checks what follows Esc on the first of two MCP conflicts: the second
    /// conflict is never asked, exactly one `question was cancelled` note,
    /// both servers kept local, the role installed, nothing refused, and
    /// the local mcp.json unchanged.
    fn assert_both_kept_local_after_one_esc(dir: &Path, local_before: &str, since: &str) {
        let flat = squeezed(since);
        assert!(
            !flat.contains("ConflictonMCPserver'bundle-k7s-second'"),
            "the second conflict was asked after the first was cancelled:\n{since}"
        );
        assert_eq!(
            flat.matches("thequestionwascancelled").count(),
            1,
            "expected exactly one `the question was cancelled` note:\n{since}"
        );
        assert!(
            !flat.contains("Refusingto")
                && !flat.contains("Abortedmerge")
                && !flat.contains("Error:"),
            "Esc on the first conflict refused, aborted or failed the install:\n{since}"
        );
        assert!(
            dir.join("roles").join("bundle-k7s-role.md").exists(),
            "the bundle's role was not installed:\n{since}"
        );
        let before: serde_json::Value = serde_json::from_str(local_before).unwrap();
        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(
            merged, before,
            "Esc on the first conflict changed a local server definition:\n{since}"
        );
    }

    /// Under the UI: Esc on the first of two MCP conflict modals keeps both
    /// servers local with one note and never raises the second modal.
    #[test]
    #[serial]
    fn esc_on_the_first_of_two_mcp_conflict_modals_keeps_both_local_with_one_note() {
        let dir = fresh_config_dir("install-two-mcp-conflicts-esc");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let (repo, local_before) = two_mcp_conflicts(&dir);
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
            ".install (first MCP conflict)",
            &["ConflictonMCPserver", "keeplocal"],
        );
        let first_is_server = squeezed(&modal).contains("ConflictonMCPserver'bundle-k7s-server'");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_squeezed(
            &mut pty,
            "the question was cancelled.",
            "the note after Esc on the first MCP conflict modal",
        );
        wait_for_file(
            &mut pty,
            &dir.join("roles").join("bundle-k7s-role.md"),
            "the bundle's role",
        );
        pty.read_for(Duration::from_millis(2500));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
        let since = if first_is_server {
            since
        } else {
            since
                .replace("bundle-k7s-server", "SWAP")
                .replace("bundle-k7s-second", "bundle-k7s-server")
                .replace("SWAP", "bundle-k7s-second")
        };
        assert_both_kept_local_after_one_esc(&dir, &local_before, &since);
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "the conflict modal released the terminal:\n{since}"
        );
        assert_ui_alive_then_quit(&mut pty, "Esc on the first of two MCP conflicts");
    }

    /// In the plain REPL: Esc on the first of two inquire conflict selects
    /// keeps both servers local with one note and never asks the second.
    #[test]
    #[serial]
    fn esc_on_the_first_of_two_mcp_conflicts_in_the_plain_repl_keeps_both_local_with_one_note() {
        let dir = fresh_config_dir("install-two-mcp-conflicts-esc-repl");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let (repo, local_before) = two_mcp_conflicts(&dir);
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        pty.wait_for("dry-model)>", REPL_PROMPT_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(300));
        let install_at = pty.raw().len();
        // The line editor repaints the prompt once per typed key, so the
        // absolute temp path alone would eat most of the deadline; the binary
        // runs in `dir`, where the relative path names the same repository.
        let relative = repo
            .strip_prefix(&dir)
            .expect("the bundle repository lives under the test dir");
        pty.send(format!(".install ./{}\r", relative.display()).as_bytes())
            .unwrap();
        pty.wait_for_after("Conflict on MCP server", install_at, PTY_DEADLINE)
            .expect("`.install` did not ask about the first MCP conflict");
        pty.read_for(Duration::from_millis(600));
        let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
        let first_is_server = asked.contains("Conflict on MCP server 'bundle-k7s-server'");
        let cancelled = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        wait_for_squeezed(
            &mut pty,
            "the question was cancelled.",
            "the note after Esc on the first MCP conflict",
        );
        wait_for_file(
            &mut pty,
            &dir.join("roles").join("bundle-k7s-role.md"),
            "the bundle's role",
        );
        pty.read_for(Duration::from_millis(2500));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
        let since = if first_is_server {
            since
        } else {
            since
                .replace("bundle-k7s-server", "SWAP")
                .replace("bundle-k7s-second", "bundle-k7s-server")
                .replace("SWAP", "bundle-k7s-second")
        };
        assert_both_kept_local_after_one_esc(&dir, &local_before, &since);
        wait_for_repl_prompt(&mut pty, "dry-model)>", cancelled);
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A tool that asks one Input question over the prompt channel with
    /// `"default": ""` on the wire and records the answer it gets back.
    const EMPTY_DEFAULT_INPUT_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Ask the user for a label and record the answer.\n\
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
    \x20   id=\"e0d-$$\"\n\
    \x20   line=\"$(jq -cn --arg id \"$id\" '{coyote_prompt:{id:$id,action:\"input\",question:\"Label e0d-q\",options:[],default:\"\"}}')\"\n\
    \x20   printf '%s\\n' \"$line\" >&\"$_COYOTE_PROMPT_OUT\"\n\
    \x20   while IFS= read -r reply <\"$_COYOTE_PROMPT_REPLY_PATH\"; do\n\
    \x20       if printf '%s' \"$reply\" | jq -e --arg id \"$id\" '.id == $id' >/dev/null 2>&1; then break; fi\n\
    \x20   done\n\
    \x20   v=\"$(printf '%s' \"$reply\" | jq -r 'if (.answer | type) == \"string\" then \"[\" + .answer + \"]\" else \"cancelled\" end')\"\n\
    \x20   printf '%s' \"$v\" > \"$out\"\n\
    \x20   echo \"fixture label: $v e0d-r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    fn empty_default_turn(dir: &Path) -> (FakeLlm, PathBuf) {
        let out = dir.join("label-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_label_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done e0d".to_string()),
        ]);
        write_fake_llm_config(dir, &fake);
        write_tool(dir, "fixture_label_rec.sh", EMPTY_DEFAULT_INPUT_TOOL);
        (fake, out)
    }

    /// `"default": ""` in the plain REPL is no default: the inquire prompt
    /// shows no `(...)` default hint after the question, and typed text is
    /// the answer.
    #[test]
    #[serial]
    fn a_tools_input_question_with_an_empty_default_shows_no_default_hint_in_the_plain_repl() {
        let dir = fresh_config_dir("tool-input-empty-default-repl");
        let _guard = TempDirGuard(dir.clone());
        let (fake, out) = empty_default_turn(&dir);
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        let asked_at = wait_for_repl_prompt(&mut pty, "fake-model)>", 0);
        pty.send(b"call the label tool\r").unwrap();
        pty.wait_for_after("Label e0d-q", asked_at, PTY_DEADLINE)
            .expect("the tool's Input question never reached the terminal");
        pty.read_for(Duration::from_millis(600));
        let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
        let line = asked
            .lines()
            .rev()
            .find(|line| line.contains("Label e0d-q"))
            .unwrap()
            .to_string();
        let after = line.split("Label e0d-q").nth(1).unwrap_or("");
        assert!(
            !after.contains('(') && !after.contains(')'),
            "`\"default\": \"\"` painted a default hint after the question: {line:?}\n{asked}"
        );
        pty.send(b"typed-e0d").unwrap();
        pty.read_for(Duration::from_millis(300));
        let answered = pty.raw().len();
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done e0d", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "[typed-e0d]",
            "{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        wait_for_repl_prompt(&mut pty, "fake-model)>", answered);
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// `"default": ""` under the UI is no default: no dim placeholder text is
    /// painted in the Input modal, and Enter on the untouched field answers
    /// the empty string (nothing substituted for it).
    #[test]
    #[serial]
    fn a_tools_input_question_with_an_empty_default_paints_no_placeholder_under_the_ui() {
        let dir = fresh_config_dir("tool-input-empty-default-tui");
        let _guard = TempDirGuard(dir.clone());
        let (fake, out) = empty_default_turn(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let asked_at = pty.raw().len();
        pty.send(b"call the label tool\r").unwrap();
        let modal = wait_for_the_modal(
            &mut pty,
            asked_at,
            "the label tool",
            &["Labele0d-q", "Enter=answer"],
        );
        assert!(
            !squeezed(&modal).contains("\"\"") && !squeezed(&modal).contains("default:"),
            "the empty default is rendered as visible text in the modal:\n{modal}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done e0d", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "[]",
            "Enter on the untouched modal with `\"default\": \"\"` did not answer the empty string:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A tool that asks one `checkbox` question and records the indices it
    /// got back.
    const RECORDING_CHECKBOX_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Ask the user to tick entries and record the indices.\n\
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
    \x20   v=\"$(checkbox \"Tick entries c3b-q\" alpha-c3b beta-c3b)\"\n\
    \x20   printf '%s' \"${v:-cancelled}\" > \"$out\"\n\
    \x20   echo \"fixture ticked: ${v:-cancelled} c3b-r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// The custom row is for selects only: a tool's `checkbox` question,
    /// in the plain REPL and under the UI, lists the entries with no
    /// `Other (custom)` row.
    #[test]
    #[serial]
    fn a_tools_checkbox_question_stays_closed_with_no_custom_row_in_either_interface() {
        for ui in ["repl", "tui"] {
            let dir = fresh_config_dir(&format!("tool-checkbox-closed-{ui}"));
            let _guard = TempDirGuard(dir.clone());
            let out = dir.join("checkbox-answer");
            let fake = FakeLlm::start(vec![
                Reply::ToolCall {
                    name: "fixture_checkbox_rec".to_string(),
                    arguments: format!("{{\"out\": \"{}\"}}", out.display()),
                },
                Reply::Text("fixture done c3b".to_string()),
            ]);
            write_fake_llm_config(&dir, &fake);
            write_tool(&dir, "fixture_checkbox_rec.sh", RECORDING_CHECKBOX_TOOL);
            let mut pty = spawn_binary_under(
                built_binary(),
                &dir,
                &[],
                WAIT_WRAPPER,
                "/dev/tty",
                &format!(" --{ui}"),
            );
            let asked_at = if ui == "repl" {
                wait_for_repl_prompt(&mut pty, "fake-model)>", 0)
            } else {
                pty.wait_for("no session", PTY_DEADLINE).unwrap();
                pty.read_for(Duration::from_millis(300));
                pty.raw().len()
            };
            pty.send(b"call the checkbox tool\r").unwrap();
            if ui == "repl" {
                pty.wait_for_after("beta-c3b", asked_at, PTY_DEADLINE)
                    .expect("the checkbox question never reached the terminal");
            } else {
                wait_for_the_modal(
                    &mut pty,
                    asked_at,
                    "the checkbox tool",
                    &["c3b-q", "alpha-c3b", "beta-c3b"],
                );
            }
            pty.read_for(Duration::from_millis(800));
            let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
            assert!(
                !squeezed(&asked).contains("Other(custom)"),
                "[{ui}] a tool's checkbox question offers the custom row:\n{asked}"
            );
            pty.read_for(HOTKEY_GAP);
            let answered = pty.raw().len();
            pty.send(b"\x1b").unwrap();
            wait_for_file(&mut pty, &out, "the tool's recorded answer");
            wait_for_squeezed(&mut pty, "fixture done c3b", "the reply after the tool");
            assert_eq!(
                fs::read_to_string(&out).unwrap(),
                "cancelled",
                "[{ui}] Esc on the checkbox question did not reach the tool as a cancel"
            );
            if ui == "repl" {
                wait_for_repl_prompt(&mut pty, "fake-model)>", answered);
                pty.send(b".exit\r").unwrap();
            } else {
                pty.read_for(Duration::from_millis(300));
                pty.send(b"\x04").unwrap();
            }
            let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
            assert_eq!(exit_code(&transcript), 0, "[{ui}] {transcript}");
            assert_tty_restored(&transcript);
            assert_eq!(fake.served(), 2, "[{ui}] {transcript}");
        }
    }

    // ---------------------------------------------------------------------------
    // `Agent::init`'s "init RAG?" question. An agent with `documents:`
    // attached asks whether to build its knowledge base when it starts; under
    // the UI the question is the Confirm modal, in the plain REPL it is the
    // inquire confirm it always was, and without a terminal the agent starts
    // without RAG, silently. Then a tty-less macro step that would ask, and
    // the `.rag` wizard's questions under the UI and in the plain REPL.
    // ---------------------------------------------------------------------------

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
        let info = painted_after(&mut pty, info_at, |text| {
            let flat = squeezed(text);
            flat.contains("name:docs-agent-n7q") && flat.contains("documents:-notes.md")
        })
        .unwrap_or_else(|since| since);
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
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
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
        wait_for_repl_prompt(&mut pty, "docs-agent-r7q>>", answered_at);
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
        let mut pty = spawn_binary_under(
            built_binary(),
            &dir,
            &[],
            &wait_wrapper_with_stdout_to(&captured),
            "/dev/null",
            " --agent docs-agent-h7q --headless 'say hello-sentinel-h7q'",
        );

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
            run_ttyless_status(built_binary(), &dir, &["--info"]);
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
        let at = repaint_until(&mut pty, 50, 120, |f| {
            let flat = squeezed(f);
            RAG_WIZARD_FOLLOW_UPS.iter().any(|q| flat.contains(q)) && flat.contains("Esc=cancel")
        });
        let mut frame = since(&pty, at);
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
            squeezed(&frame).contains("Setchunksize:1000")
                && squeezed(&frame).contains("Esc=cancel"),
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
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
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
        wait_for_repl_prompt(&mut pty, "dry-model)>", esc_at);
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        for footprint in ["r2p", "/rags/"] {
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
    // What the bash `list` helper hands back for the three kinds of typed
    // answer: text that looks like an index, the exact text of an offered
    // entry, and a blank answer under the UI and in the plain REPL; and the
    // Python `select` for a blank one.
    // ---------------------------------------------------------------------

    /// Opens the custom row of the recording list tool under the UI, types
    /// `typed` there, submits it and returns the recorded answer and status.
    fn list_custom_answer_under_the_ui(scenario: &str, typed: &[u8]) -> (String, String, String) {
        let dir = fresh_config_dir(scenario);
        let _guard = TempDirGuard(dir.clone());
        let (fake, out) = list_tool_turn(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let asked_at = pty.raw().len();
        pty.send(b"call the list tool\r").unwrap();
        wait_for_the_modal(
            &mut pty,
            asked_at,
            "the list tool",
            &["q7l-q", "alpha-q7l", "gamma-q7l", "Other (custom)"],
        );
        pty.read_for(HOTKEY_GAP);
        for _ in 0..3 {
            pty.send(b"\x1b[B").unwrap();
            pty.read_for(Duration::from_millis(400));
        }
        pty.send(b"\r").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(typed).unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
        assert_eq!(fake.served(), 2, "{transcript}");
        let answer = fs::read_to_string(&out).unwrap();
        let status = list_exit_status(&out);

        pty.send(b"\x04").unwrap();
        let end = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&end), 0, "{end}");
        assert_tty_restored(&end);
        (answer, status, transcript)
    }

    /// A typed `1` on the custom row is the user's own text, not index 1: the
    /// helper prints `1` and exits 2, so the tool can tell it from a pick.
    #[test]
    #[serial]
    fn a_typed_one_on_the_lists_custom_row_reaches_the_tool_as_text_with_status_two() {
        let (answer, status, transcript) =
            list_custom_answer_under_the_ui("tool-list-typed-one", b"1");
        assert_eq!(answer, "1", "{transcript}");
        assert_eq!(
            status, "2",
            "a typed `1` was taken for index 1 (status 0) instead of custom text:\n{transcript}"
        );
    }

    /// Typing an offered entry's exact text counts as choosing it: the helper
    /// prints that entry's index and exits 0.
    #[test]
    #[serial]
    fn typing_an_offered_entrys_exact_text_on_the_custom_row_chooses_that_entry() {
        let (answer, status, transcript) =
            list_custom_answer_under_the_ui("tool-list-typed-entry", b"beta-q7l");
        assert_eq!(
            answer, "1",
            "the exact text of the second entry did not come back as its index:\n{transcript}"
        );
        assert_eq!(status, "0", "{transcript}");
    }

    /// A whitespace-only answer on the custom row is a cancel: the helper
    /// prints nothing and exits 1.
    #[test]
    #[serial]
    fn a_blank_answer_on_the_lists_custom_row_cancels_with_status_one() {
        let (answer, status, transcript) =
            list_custom_answer_under_the_ui("tool-list-typed-blank", b"   ");
        assert_eq!(
            answer, "cancelled",
            "a blank custom answer printed something:\n{transcript}"
        );
        assert_eq!(
            status, "1",
            "a blank custom answer did not cancel with status 1:\n{transcript}"
        );
    }

    /// The same blank answer in the plain REPL's `Custom response:` prompt
    /// cancels the same way.
    #[test]
    #[serial]
    fn a_blank_custom_response_in_the_plain_repl_cancels_the_list_with_status_one() {
        let dir = fresh_config_dir("tool-list-repl-blank");
        let _guard = TempDirGuard(dir.clone());
        let (fake, out) = list_tool_turn(&dir);
        let mut pty = spawn_repl_under(&dir, &[], WAIT_WRAPPER, "/dev/tty");

        let asked_at = wait_for_repl_prompt(&mut pty, "fake-model)>", 0);
        pty.send(b"call the list tool\r").unwrap();
        pty.wait_for_after("Other (custom)", asked_at, PTY_DEADLINE)
            .expect("the tool's list question never offered the custom row");
        pty.read_for(Duration::from_millis(500));
        for _ in 0..3 {
            pty.send(b"\x1b[B").unwrap();
            pty.read_for(Duration::from_millis(300));
        }
        let custom_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        pty.wait_for_after("Custom response:", custom_at, PTY_DEADLINE)
            .expect("Enter on `Other (custom)` did not open the `Custom response:` prompt");
        pty.read_for(Duration::from_millis(400));
        pty.send(b"  ").unwrap();
        pty.read_for(Duration::from_millis(400));
        let answered = pty.raw().len();
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "cancelled",
            "{transcript}"
        );
        assert_eq!(
            list_exit_status(&out),
            "1",
            "a blank `Custom response:` did not cancel with status 1:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        wait_for_repl_prompt(&mut pty, "fake-model)>", answered);
        pty.send(b".exit\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE * 2).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// The Python helper's `select` returns `None` for a blank custom answer.
    #[test]
    #[serial]
    fn a_python_tools_select_returns_none_for_a_blank_custom_answer() {
        let dir = fresh_config_dir("tool-py-select-blank");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("py-select-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_py_select".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done p5y".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_py_select.py", RECORDING_PY_SELECT_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let asked_at = pty.raw().len();
        pty.send(b"call the python select tool\r").unwrap();
        wait_for_the_modal(
            &mut pty,
            asked_at,
            "the python select tool",
            &["p5y-q", "alpha-p5y", "beta-p5y", "Other (custom)"],
        );
        pty.read_for(HOTKEY_GAP);
        for _ in 0..2 {
            pty.send(b"\x1b[B").unwrap();
            pty.read_for(Duration::from_millis(400));
        }
        pty.send(b"\r").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"   ").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the python tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done p5y", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "NoneType:None",
            "the Python `select` did not return None for a blank custom answer:\n{transcript}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------------
    // The pager over a tool's whole output.
    // ---------------------------------------------------------------------------

    /// A tool that writes forty numbered lines to stderr, twice what a tool
    /// row keeps.
    const FORTY_LINES_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Print forty numbered lines.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   for i in $(seq -w 1 40); do echo \"pg-$i\" >&2; done\n\
    \x20   echo \"fixture printed forty p9g\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// Ctrl+T opens the pager over the screen with the tool's whole output,
    /// the lines its row in the transcript dropped included, and `q` closes
    /// it with the UI still taking keys.
    #[test]
    #[serial]
    fn ctrl_t_pages_a_tools_whole_output_and_q_closes_the_pager() {
        let dir = fresh_config_dir("pager");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_forty".to_string(),
                arguments: "{}".to_string(),
            },
            Reply::Text("fixture done p9g".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_forty.sh", FORTY_LINES_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"print forty lines\r").unwrap();
        wait_for_squeezed(&mut pty, "fixture done p9g", "the reply after the tool");
        pty.read_for(Duration::from_millis(500));
        let at = repaint_until(&mut pty, 30, 100, |f| f.contains("fixture_forty({})"));
        let inline = since(&pty, at);
        assert!(
            inline.contains("fixture_forty({})"),
            "the tool row is not in the transcript:\n{inline}"
        );
        assert!(
            !inline.contains("pg-01"),
            "the transcript shows a line its tool row drops:\n{inline}"
        );

        pty.send(b"\x14").unwrap();
        repainted_frame_showing(
            &mut pty,
            "Thread: root thread",
            "Ctrl+T did not open the pager",
        );
        pty.send(b"g").unwrap();
        repainted_frame_showing(
            &mut pty,
            "pg-01",
            "the pager did not show the first line of the tool's output",
        );

        pty.send(b"q").unwrap();
        let frame =
            repainted_frame_showing(&mut pty, "no session", "q did not give the screen back");
        assert!(
            !squeezed(&frame).contains("Thread:rootthread"),
            "q left the pager on the screen:\n{frame}"
        );
        pty.send(b"still here").unwrap();
        repainted_frame_showing(
            &mut pty,
            "still here",
            "keys did not reach the composer after the pager closed",
        );
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------------
    // The pager's find and thread picker, and the roster over a background
    // job.
    // ---------------------------------------------------------------------------

    /// In the pager `/` finds a line of the tool's output that is off the
    /// screen, Tab lists the root thread, Esc goes back to the pager, and Esc
    /// again gives the screen back.
    #[test]
    #[serial]
    fn the_pager_finds_a_line_and_tab_lists_the_root_thread() {
        let dir = fresh_config_dir("pager-find");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_forty".to_string(),
                arguments: "{}".to_string(),
            },
            Reply::Text("fixture done f5d".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_forty.sh", FORTY_LINES_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"print forty lines\r").unwrap();
        wait_for_squeezed(&mut pty, "fixture done f5d", "the reply after the tool");
        pty.read_for(Duration::from_millis(500));

        pty.send(b"\x14").unwrap();
        repainted_frame_showing(
            &mut pty,
            "Thread: root thread",
            "Ctrl+T did not open the pager",
        );
        pty.send(b"g").unwrap();
        let top = repainted_frame_showing(&mut pty, "pg-01", "g did not go to the top");
        assert!(
            !top.contains("pg-37"),
            "pg-37 already on the top page:\n{top}"
        );
        pty.send(b"/pg-37").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        let found =
            repainted_frame_showing(&mut pty, "pg-37", "find did not bring pg-37 on screen");
        // The repaint is the screen's rows in order with the escapes gone:
        // the find field closed on Enter, so the last row is the key legend
        // again and a `pg-37` ahead of it is a row of the text.
        let legend = found
            .rfind("j/k scroll")
            .unwrap_or_else(|| panic!("Enter did not give the key legend back:\n{found}"));
        assert!(
            found[..legend].contains("pg-37") && !found.contains("/pg-37"),
            "pg-37 is not in the pager's text:\n{found}"
        );
        assert!(
            !found.contains("No match"),
            "the find reported no match:\n{found}"
        );

        pty.send(b"\t").unwrap();
        let picker = repainted_frame_showing(
            &mut pty,
            "Enter/o open  Esc back",
            "Tab did not open the picker",
        );
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(400));
        let back = repainted_frame_showing(
            &mut pty,
            "Thread: root thread",
            "Esc did not go back to the pager",
        );
        assert!(
            !squeezed(&back).contains(&squeezed("Enter/o open  Esc back")),
            "Esc left the picker up:\n{back}"
        );
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(400));
        let frame = repainted_frame_showing(&mut pty, "no session", "Esc did not close the pager");
        assert!(
            !squeezed(&frame).contains("Thread:rootthread"),
            "the pager is still up after Esc:\n{frame}\npicker was:\n{picker}"
        );
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A job the model started in the background and the user left running
    /// by interrupting the turn shows on the Alt+T roster as running; `x` on its
    /// row cancels it, so its process goes away and the row stops saying
    /// running.
    #[test]
    #[serial]
    fn alt_t_lists_a_job_left_running_and_x_cancels_it() {
        let dir = fresh_config_dir("roster-cancel");
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
        pty.read_for(Duration::from_millis(1200));
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let pid: i32 = fs::read_to_string(&job_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(alive(pid), "the job died with the interrupted turn");

        pty.send(ALT_T).unwrap();
        let roster =
            repainted_frame_showing(&mut pty, "fixture_sleep", "Alt+T did not list the job");
        let band = roster_band(&roster);
        assert!(
            band.contains("fixture_sleep") && band.contains("running"),
            "the roster does not list the job as running: {band:?}\n{roster}"
        );
        pty.send(b"x").unwrap();
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        while alive(pid) && std::time::Instant::now() < deadline {
            pty.read_for(Duration::from_millis(200));
        }
        let gone = !alive(pid);
        assert!(
            gone,
            "x did not stop the job's process {pid}; roster was {band:?}"
        );
        let open = repainted_frame_showing(&mut pty, "Esc = close", "the tasks pop-up closed");
        let band = roster_band(&open);
        assert!(
            !band.contains("fixture_sleep"),
            "the roster still lists the cancelled job: {band:?}"
        );

        // The pop-up covers part of the transcript, so the job row is read
        // once it is closed.
        pty.send(b"\x1b").unwrap();
        let after = repainted_frame_showing(&mut pty, "Cancelled job_", "x reported no cancel");
        let at = after
            .find("job fixture_sleep")
            .unwrap_or_else(|| panic!("the transcript lost the job's row:\n{after}"));
        let job_row: String = after[at..].chars().take(60).collect();
        assert!(
            job_row.contains("cancelled"),
            "the job row does not say cancelled after x: {job_row:?}\n{after}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------------
    // The pager and roster with nothing in them.
    // ---------------------------------------------------------------------------

    /// Every pager key on a first start with no turn at all: Ctrl+T, the
    /// scroll keys, a find with no match, Tab and Esc. The UI stays up, the
    /// composer takes keys afterwards and Ctrl+D leaves with exit 0.
    #[test]
    #[serial]
    fn ctrl_t_and_every_pager_key_on_an_empty_transcript_keep_the_ui_up() {
        let dir = fresh_config_dir("pager-empty");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"\x14").unwrap();
        pty.read_for(Duration::from_millis(400));
        for keys in [
            &b"j"[..],
            b"k",
            b"G",
            b"g",
            b"\x1b[6~",
            b"\x1b[5~",
            b"/nothing-here-w2z",
            b"\r",
            b"n",
            b"N",
            b"\t",
            b"\x1b[B",
            b"\r",
        ] {
            pty.send(keys).unwrap();
            pty.read_for(Duration::from_millis(150));
        }
        for _ in 0..3 {
            pty.send(b"\x1b").unwrap();
            pty.read_for(Duration::from_millis(400));
        }
        let frame = repainted_frame_showing(&mut pty, "no session", "the UI did not come back");
        assert!(
            !squeezed(&frame).contains("Thread:rootthread"),
            "Esc left the pager up:\n{frame}"
        );
        pty.send(b"alive after the pager c8v").unwrap();
        wait_for_squeezed(
            &mut pty,
            "alive after the pager c8v",
            "keys did not reach the composer",
        );
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert!(!transcript.contains("panicked"), "{transcript}");
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// Alt+T with no task and no question shows the empty roster; `x` and `o`
    /// on it change nothing, Esc closes it and the composer takes keys.
    #[test]
    #[serial]
    fn alt_t_with_nothing_running_shows_the_empty_roster_and_x_and_o_are_harmless() {
        let dir = fresh_config_dir("roster-empty");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(ALT_T).unwrap();
        repainted_frame_showing(
            &mut pty,
            "(no tasks and no questions)",
            "Alt+T did not show the empty roster",
        );
        pty.send(b"x").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"o").unwrap();
        pty.read_for(Duration::from_millis(300));
        let still = repainted_frame_showing(
            &mut pty,
            "(no tasks and no questions)",
            "x or o closed the empty roster or opened something",
        );
        assert!(
            !squeezed(&still).contains("Thread:"),
            "o on the empty roster opened the pager:\n{still}"
        );
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"typed after the roster h5n").unwrap();
        wait_for_squeezed(
            &mut pty,
            "typed after the roster h5n",
            "keys did not reach the composer after Esc",
        );
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// The status row names `?`; on the empty composer it opens the key
    /// help over the whole screen, Esc closes it, and the composer then
    /// takes keys, a `?` after text among them, while the status row stops
    /// offering it.
    #[test]
    #[serial]
    fn question_mark_opens_the_key_help_the_status_row_names_and_esc_closes_it() {
        let dir = fresh_config_dir("key-help");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let welcome = repainted_frame_showing(
            &mut pty,
            "? = keys",
            "the status row does not name the key help",
        );
        assert!(
            squeezed(&welcome).contains(&squeezed(
                "? = keys  Alt+T = tasks  Alt+S = threads  Ctrl+X = todo"
            )),
            "the status row does not name the help key and the panel chords:\n{welcome}"
        );
        pty.send(b"?").unwrap();
        let help = repainted_frame_showing(
            &mut pty,
            "Suspend to the shell",
            "? did not open the key help",
        );
        assert!(
            !squeezed(&help).contains("no session"),
            "the key help does not cover the screen:\n{help}"
        );
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(400));
        let closed =
            repainted_frame_showing(&mut pty, "no session", "Esc did not close the key help");
        assert!(
            !squeezed(&closed).contains("Suspend to the shell"),
            "the key help is still up after Esc:\n{closed}"
        );
        pty.send(b"why? h7k").unwrap();
        wait_for_squeezed(
            &mut pty,
            "why? h7k",
            "a ? after text did not reach the composer",
        );
        let typed = repainted_frame_showing(&mut pty, "why? h7k", "the draft is not on screen");
        assert!(
            squeezed(&typed).contains(&squeezed("Alt+T = tasks  Alt+S = threads  Ctrl+X = todo"))
                && !squeezed(&typed).contains(&squeezed("? = keys")),
            "the status row still offers ? over a draft:\n{typed}"
        );
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }
}

mod bottom_pane {
    //! Real-process checks of the bottom pane: the composer field's surface
    //! and command colours as the terminal sees them, and the three prompt
    //! modals a tool can ask for through the prompt channel (select, checkbox,
    //! password). Colours and backgrounds are read from the raw escape stream,
    //! so the module runs the binary on a pseudo-terminal and is unix-only. The
    //! cases run one at a time: each spawn forks a session leader and takes a
    //! controlling tty.
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use super::common::fake_llm::{Chunk, FakeLlm, Reply};
    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

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

    /// The SGR parameter lists written between the cursor positioning that
    /// started the run of cells `at` belongs to and `at` itself: the style
    /// the painter set up for the text at `at` after a full repaint, when
    /// every row starts with its own cursor move.
    fn sgr_before(raw: &[u8], at: usize) -> Vec<Vec<u16>> {
        let (start, _, _) = cursor_position_before(raw, at)
            .unwrap_or_else(|| panic!("no cursor position precedes offset {at}"));
        sgr_params(&raw[start..at])
    }

    /// The background the lists leave in effect, read in order: the
    /// parameters of the last `48;5;N` or `48;2;r;g;b`, or `None` when no
    /// background was set or a later `49` or `0` reset it.
    fn background_after(params: &[Vec<u16>]) -> Option<Vec<u16>> {
        let mut background = None;
        for list in params {
            let mut i = 0;
            while i < list.len() {
                match list[i] {
                    38 | 48 | 58 => {
                        let len = match list.get(i + 1) {
                            Some(5) => 3,
                            Some(2) => 5,
                            _ => 1,
                        };
                        let end = (i + len).min(list.len());
                        if list[i] == 48 && len > 1 {
                            background = Some(list[i..end].to_vec());
                        }
                        i = end;
                        continue;
                    }
                    0 | 49 => background = None,
                    _ => {}
                }
                i += 1;
            }
        }
        background
    }

    /// Whether the lists set the indexed foreground `index` (`38;5;index`);
    /// the crossterm backend writes the named ANSI colours this way.
    fn has_indexed_foreground(params: &[Vec<u16>], index: u16) -> bool {
        params
            .iter()
            .any(|p| p.windows(3).any(|w| w == [38, 5, index]))
    }

    /// The palette slots the crossterm backend writes for the field's
    /// `Color::Green` and `Color::Red`.
    const GREEN: u16 = 2;
    const RED: u16 = 1;

    /// A tool that asks through one of the prompt helpers (`call` is the
    /// helper invocation) and records the answer it got at `out`, so a test
    /// sees the value the modal produced rather than only the model's
    /// follow-up. The helper prints the same shape it does on a terminal:
    /// `list` the option index, `checkbox` the checked indices, `password`
    /// the text.
    fn recording_prompt_tool(call: &str) -> String {
        format!(
            "#!/usr/bin/env bash\n\
         \n\
         # @describe Ask the user through a prompt helper and record the answer.\n\
         # @option --out! Where to write the answer.\n\
         \n\
         # @env LLM_OUTPUT=/dev/stdout The output path\n\
         \n\
         # shellcheck disable=SC1090\n\
         source \"$LLM_PROMPT_UTILS_FILE\"\n\
         \n\
         main() {{\n\
         \x20   local out v\n\
         \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
         \x20   v=\"$({call})\"\n\
         \x20   printf '%s' \"${{v:-cancelled}}\" > \"$out\"\n\
         \x20   echo \"fixture prompt answered z2r\" >> \"$LLM_OUTPUT\"\n\
         }}\n"
        )
    }

    /// A turn whose first reply calls `tool` with `{"out": out}` and whose
    /// second is the text `fixture done z2s`.
    fn tool_call_then_done(tool: &str, out: &Path) -> FakeLlm {
        FakeLlm::start(vec![
            Reply::ToolCall {
                name: tool.to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done z2s".to_string()),
        ])
    }

    /// Lets the model's follow-up land, then leaves with Ctrl+D and checks the
    /// exit code.
    fn finish_after_tool(pty: &mut Pty) {
        wait_for_squeezed(pty, "fixture done z2s", "the reply after the tool");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
    }

    /// The palette lists what the context allows from the first key: with
    /// function calling on, the `.tool` verbs are in the band before any
    /// command has run, under either prefix. The full completer parity is
    /// pinned by the unit tests of the shared table.
    #[test]
    #[serial]
    fn the_cold_start_palette_lists_the_tool_verbs_under_either_prefix() {
        for (prefix, typed, rows) in [
            ("slash", "/tool", ["/tool enable", "/tool disable"]),
            ("dot", ".tool", [".tool enable", ".tool disable"]),
        ] {
            let dir = fresh_config_dir(&format!("cold-palette-{prefix}"));
            let _guard = TempDirGuard(dir.clone());
            write_config(
                &dir,
                &format!(
                    "save_session: false\n\
                 function_calling_support: true\n\
                 command_prefix: {prefix}\n"
                ),
            );
            let mut pty = spawn_tui(&dir, &[]);

            pty.wait_for("no session", PTY_DEADLINE).unwrap();
            pty.send(typed.as_bytes()).unwrap();
            pty.wait_for("Enable a single tool", PTY_DEADLINE)
                .unwrap_or_else(|err| {
                    panic!("the band never listed the tool commands under {prefix}: {err}")
                });
            // The band pads the name to the description column, so the rows
            // are matched with whitespace squeezed out on both sides.
            let [enable, disable] = rows.map(|row| row.split_whitespace().collect::<String>());
            let repainted = repaint_showing(&mut pty, 40, 101, &[&enable, &disable]);
            let frame = since(&pty, repainted);
            squeezed_positions(&frame, [enable.as_str(), disable.as_str()]);

            // Ctrl+C closes the band first; the second one clears the draft.
            pty.send(b"\x03").unwrap();
            pty.read_for(HOTKEY_GAP);
            pty.send(b"\x03").unwrap();
            pty.read_for(HOTKEY_GAP);
            pty.send(b"\x04").unwrap();
            let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
            assert_eq!(exit_code(&transcript), 0, "{transcript}");
        }
    }

    /// The field is its own surface: the cells of its text rows are painted
    /// on a background, and the transcript rows above the rule on a
    /// different one. The screen is made tall enough for the `.info` table
    /// so the `> .info` header is still on screen to compare against.
    #[test]
    #[serial]
    fn the_composer_rows_carry_a_background_distinct_from_the_transcript_rows() {
        let dir = fresh_config_dir("field-bg");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(80, 100).unwrap();
        pty.read_for(Duration::from_millis(300));
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"probe-bg-q9d").unwrap();
        pty.read_for(Duration::from_millis(300));
        let repainted = repaint_showing(&mut pty, 80, 101, &["probe-bg-q9d", "> .info"]);

        let raw = pty.raw();
        let probe_at = find(raw, b"probe-bg-q9d", repainted).unwrap_or_else(|| {
            panic!(
                "the typed probe was not repainted:\n{}",
                since(&pty, repainted)
            )
        });
        let header_at = find(raw, b"> .info", repainted).unwrap_or_else(|| {
            panic!(
                "the `> .info` header was not repainted:\n{}",
                since(&pty, repainted)
            )
        });
        let probe_style = sgr_before(raw, probe_at);
        let Some(probe_background) = background_after(&probe_style) else {
            panic!(
                "the field's text row was painted without a background: {probe_style:?}\n{}",
                since(&pty, repainted)
            );
        };
        let header_style = sgr_before(raw, header_at);
        let Some(header_background) = background_after(&header_style) else {
            panic!(
                "the transcript row was painted without a background: {header_style:?}\n{}",
                since(&pty, repainted)
            );
        };
        assert_ne!(
            probe_background,
            header_background,
            "the transcript row carries the field's background\n{}",
            since(&pty, repainted)
        );
        let probe_rows = painted_at(raw, b"probe-bg-q9d", repainted);
        let header_rows = painted_at(raw, b"> .info", repainted);
        let (Some(&(probe_row, _)), Some(&(header_row, _))) =
            (probe_rows.first(), header_rows.first())
        else {
            panic!("the probe or the header was not positioned: {probe_rows:?} / {header_rows:?}");
        };
        assert!(
            probe_row > header_row,
            "the field (row {probe_row}) is not below the transcript (row {header_row})"
        );

        // The draft must be cleared before Ctrl+D is the quit chord.
        pty.send(b"\x03").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
    }

    /// `TuiPrompter` renders a tool's select request as the modal; a digit
    /// jumps to that row and, once the idle gate has passed, accepts it. The
    /// helper hands the tool the option's index.
    #[test]
    #[serial]
    fn a_tools_select_request_is_the_modal_and_a_digit_picks_the_row() {
        let dir = fresh_config_dir("tool-select");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("select-answer");
        let fake = tool_call_then_done("fixture_select_rec", &out);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_select_rec.sh",
            &recording_prompt_tool("list \"Pick one q9d-s\" alpha beta gamma"),
        );
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the select tool\r").unwrap();
        pty.wait_for("q9d-s", PTY_DEADLINE)
            .expect("the tool's select question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"2").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "1",
            "`2` on the select modal did not reach the tool as beta's index"
        );
        finish_after_tool(&mut pty);
    }

    /// A checkbox request: Space ticks the row under the cursor, the arrows
    /// move it, Enter sends the ticked set; the helper hands the tool the
    /// checked indices.
    #[test]
    #[serial]
    fn a_tools_checkbox_request_ticks_rows_and_sends_them() {
        let dir = fresh_config_dir("tool-checkbox");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("checkbox-answer");
        let fake = tool_call_then_done("fixture_checkbox_rec", &out);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_checkbox_rec.sh",
            &recording_prompt_tool("checkbox \"Tick some q9d-c\" alpha beta gamma"),
        );
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the checkbox tool\r").unwrap();
        pty.wait_for("q9d-c", PTY_DEADLINE)
            .expect("the tool's checkbox question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        // One key per write: an escape sequence split across reads would be
        // taken for a bare Esc and cancel the modal.
        let keys: [&[u8]; 5] = [b" ", b"\x1b[B", b"\x1b[B", b" ", b"\r"];
        for key in keys {
            pty.send(key).unwrap();
            pty.read_for(Duration::from_millis(100));
        }
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "0 2",
            "ticking alpha and gamma did not reach the tool as their indices"
        );
        finish_after_tool(&mut pty);
    }

    /// A tool's password request names the tool and masks what is typed:
    /// the screen shows a run of mask characters and never the text, which
    /// still reaches the tool whole on Enter.
    #[test]
    #[serial]
    fn a_tools_password_request_masks_the_field_and_sends_the_text() {
        let dir = fresh_config_dir("tool-password");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("password-answer");
        let fake = tool_call_then_done("fixture_password_rec", &out);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_password_rec.sh",
            &recording_prompt_tool("password \"Token q9d-p\""),
        );
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the password tool\r").unwrap();
        pty.wait_for("q9d-p", PTY_DEADLINE)
            .expect("the tool's password question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        let modal_at = pty.raw().len();
        pty.send(b"s3cret").unwrap();
        pty.read_for(HOTKEY_GAP);
        let repainted =
            repaint_showing(&mut pty, 40, 101, &["fixture_password_rec asks:", "******"]);
        let field = since(&pty, repainted);
        assert!(
            field.contains("fixture_password_rec asks:"),
            "the password modal did not name the tool:\n{field}"
        );
        assert!(
            field.contains("******"),
            "the password field showed no mask run:\n{field}"
        );
        let shown = since(&pty, modal_at);
        assert!(
            !shown.contains("s3cret"),
            "the typed password reached the screen:\n{shown}"
        );

        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        assert_eq!(fs::read_to_string(&out).unwrap().trim(), "s3cret");
        assert!(
            !since(&pty, modal_at).contains("s3cret"),
            "the password reached the screen after it was sent:\n{}",
            since(&pty, modal_at)
        );
        finish_after_tool(&mut pty);
    }

    /// The field colours the command word as it is typed: green for a
    /// command the REPL knows, red for one it does not. Typing a command
    /// opens the palette, whose rows also spell the command, so the band is
    /// closed with Esc before the frame is searched for the field's copy.
    #[test]
    #[serial]
    fn a_typed_command_is_painted_green_and_an_unknown_one_red() {
        let dir = fresh_config_dir("command-colours");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b".info").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(300));
        let repainted = repaint_showing(&mut pty, 40, 101, &[".info"]);
        assert!(
            !since(&pty, repainted).contains("Show system info"),
            "the palette band is still up:\n{}",
            since(&pty, repainted)
        );
        let at = find(pty.raw(), b".info", repainted)
            .unwrap_or_else(|| panic!("`.info` was not repainted:\n{}", since(&pty, repainted)));
        let style = sgr_before(pty.raw(), at);
        assert!(
            has_indexed_foreground(&style, GREEN),
            "a known command was not painted green: {style:?}\n{}",
            since(&pty, repainted)
        );

        pty.send(b"\x7f\x7f\x7f\x7f\x7f").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b".nosuch").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(300));
        let repainted = repaint_showing(&mut pty, 40, 100, &[".nosuch"]);
        let at = find(pty.raw(), b".nosuch", repainted)
            .unwrap_or_else(|| panic!("`.nosuch` was not repainted:\n{}", since(&pty, repainted)));
        let style = sgr_before(pty.raw(), at);
        assert!(
            has_indexed_foreground(&style, RED),
            "an unknown command was not painted red: {style:?}\n{}",
            since(&pty, repainted)
        );

        pty.send(b"\x03").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
    }

    /// Esc with the palette up closes the band and nothing else: the typed
    /// prefix stays in the field and no turn is interrupted.
    #[test]
    #[serial]
    fn esc_closes_the_palette_without_interrupting_or_clearing_focus() {
        let dir = fresh_config_dir("palette-esc");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b".in").unwrap();
        pty.wait_for("Show system info", PTY_DEADLINE)
            .expect("the palette band never showed the `.info` row");
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(300));
        let repainted = repaint_showing(&mut pty, 40, 101, &[".in"]);
        let frame = since(&pty, repainted);
        assert!(
            !frame.contains("Show system info") && !frame.contains(".info"),
            "the palette band survived Esc:\n{frame}"
        );
        assert!(
            frame.contains(".in"),
            "Esc cleared the typed prefix:\n{frame}"
        );

        pty.send(b"\x03").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
    }

    /// A streamed reply held open at `release` with no tool call in it, so
    /// the driver has no round at which to hand a queued line to the model.
    fn held_stream(release: &Path) -> Reply {
        Reply::Streamed(vec![
            Chunk::Text("first half q9d-m".to_string()),
            Chunk::WaitFor(release.to_path_buf()),
            Chunk::Text(" tail q9d-t".to_string()),
        ])
    }

    /// Starts a turn on the held stream and queues `line` behind it; the
    /// preview shows the line held and the model has had its one request.
    fn queue_behind_held_stream(pty: &mut Pty, fake: &FakeLlm, line: &[u8]) {
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"stream and hold\r").unwrap();
        wait_for_squeezed(pty, "first half q9d-m", "the first delta");
        pty.send(line).unwrap();
        pty.wait_for("pending: 1", PTY_DEADLINE)
            .expect("the preview never showed the queued line");
        assert_eq!(fake.served(), 1, "the queued line went out mid-stream");
    }

    /// Where each marker first appears in `frame` with whitespace squeezed
    /// out, so the order of transcript cells can be compared no matter how
    /// the rows wrapped.
    fn squeezed_positions<const N: usize>(frame: &str, markers: [&str; N]) -> [usize; N] {
        let squeezed: String = frame.chars().filter(|c| !c.is_whitespace()).collect();
        markers.map(|marker| {
            squeezed
                .find(marker)
                .unwrap_or_else(|| panic!("`{marker}` is not on the screen:\n{frame}"))
        })
    }

    /// A prompt queued while the model streams, with no tool round for the
    /// driver to hand it over at, becomes the next turn's prompt once the
    /// reply ends: the second request goes out only after the first reply,
    /// the line sits as a user cell between the two replies, and the
    /// preview is gone.
    #[test]
    #[serial]
    fn a_queued_prompt_the_model_never_got_becomes_the_next_prompt() {
        let dir = fresh_config_dir("queued-next-prompt");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![
            held_stream(&release),
            Reply::Text("second reply q9d-s".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);
        queue_behind_held_stream(&mut pty, &fake, b"follow-up q9d-n\r");

        fs::write(&release, "").unwrap();
        wait_for_squeezed(
            &mut pty,
            "second reply q9d-s",
            "the reply to the queued prompt",
        );
        assert_eq!(fake.served(), 2);
        pty.read_for(Duration::from_millis(500));
        let repainted = repaint_showing(
            &mut pty,
            40,
            101,
            &["tail q9d-t", "> follow-up q9d-n", "second reply q9d-s"],
        );
        let frame = since(&pty, repainted);
        let [tail, header, second] =
            squeezed_positions(&frame, ["tailq9d-t", ">follow-upq9d-n", "secondreplyq9d-s"]);
        assert!(
            tail < header && header < second,
            "the queued line is not a prompt between the two replies:\n{frame}"
        );
        assert!(
            !frame.contains("pending:"),
            "the preview outlived the queued line:\n{frame}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
    }

    /// A command queued while the model streams never goes to the model: it
    /// runs as a turn of its own once the reply ends, its header and output
    /// after the reply, with no second request.
    #[test]
    #[serial]
    fn a_queued_command_runs_as_its_own_turn_after_the_reply() {
        let dir = fresh_config_dir("queued-command");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![held_stream(&release)]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);
        queue_behind_held_stream(&mut pty, &fake, b".clear images\r");

        let released_at = pty.raw().len();
        fs::write(&release, "").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "No pending images.",
            released_at,
            "the queued `.clear images` never ran",
        );
        pty.read_for(Duration::from_millis(500));
        let repainted = repaint_showing(
            &mut pty,
            80,
            40,
            &["tail q9d-t", "> .clear images", "No pending images."],
        );
        let frame = since(&pty, repainted);
        let [tail, header, output] =
            squeezed_positions(&frame, ["tailq9d-t", ">.clearimages", "Nopendingimages."]);
        assert!(
            tail < header && header < output,
            "the queued command did not run as its own turn after the reply:\n{frame}"
        );
        assert_eq!(fake.served(), 1, "a command reached the model");
        assert!(
            !frame.contains("pending:"),
            "the preview outlived the queued command:\n{frame}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
    }
}

mod binary {
    //! Real-process checks of the full-screen UI while it works: turns driven
    //! by a scripted model (a tool's confirm modal, typing while a tool runs,
    //! a signal mid-turn), paced streams painted before they end, the
    //! interrupt keys at rest and mid-turn, the confirm modal's shifted
    //! answers and clipped questions, fenced and editor-backed commands, the
    //! `!shell` arm, and the pty harness's own escape stripper. Each case
    //! runs the binary on a pseudo-terminal under a bash wrapper that reports
    //! the exit code and `stty -a` afterwards, so the module is unix-only. How
    //! the UI starts, leaves and hands the terminal back lives in
    //! `binary_lifecycle` in `tui_lifecycle_and_signals.rs`; the command
    //! menus' modals, the plain REPL's
    //! type-ahead and the runs with no human to ask live in
    //! `binary_menus` in `tui_modals.rs`.
    //! The cases run one at a time: each spawn forks a session leader and takes
    //! a controlling tty, and a fork landing between another test's `openpty`
    //! and its `exec` has failed the spawn with a garbled errno.
    use std::fs;
    use std::process::Command;
    use std::time::Duration;

    use super::common::fake_llm::{Chunk, FakeLlm, HOLD_LIMIT, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
    use serial_test::serial;

    // ---------------------------------------------------------------------
    // Scenarios driven by a scripted model: a real tool call in the
    // full-screen UI, so the prompt modal, the busy UI and the signal arm are
    // seen on a pty rather than through a TestBackend.
    // ---------------------------------------------------------------------

    /// A tool that marks its start at `<marker>-started`, then waits (at most
    /// eight seconds) for `<marker>-release` to appear before finishing.
    const MARKED_SLEEP_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Wait until released.\n\
    # @option --marker! Path prefix for the start and release files.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   local marker\n\
    \x20   marker=\"$(jq -r '.marker' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   : > \"${marker}-started\"\n\
    \x20   for _ in $(seq 1 160); do\n\
    \x20       [ -e \"${marker}-release\" ] && break\n\
    \x20       sleep 0.05\n\
    \x20   done\n\
    \x20   echo \"fixture slept p3t\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// A turn that does not stop when told to: the UI waits out
    /// `EXIT_CANCEL_WAIT` and then ends the process without it, still with
    /// the tty restored, the alternate screen left and a notice saying what
    /// was left behind. The exit is bounded: the wrapper's report lands
    /// within ten seconds of the kill.
    ///
    /// Nothing a user can configure makes such a turn any more: an aborted
    /// loader is killed and an aborted tool is SIGKILLed, so the turn is the
    /// binary's `COYOTE_TUI_STUBBORN_TURN` hook, which keeps the work task
    /// on its thread past the deadline after the reply has streamed.
    /// `sigterm_during_a_running_tool_still_leaves_the_tty_cooked` is the
    /// cooperating twin.
    #[test]
    #[serial]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "the stubborn-turn hook is compiled out of release builds"
    )]
    fn sigterm_during_a_turn_that_ignores_the_abort_leaves_it_behind_in_bounded_time() {
        let dir = fresh_config_dir("sigterm-stubborn-turn");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text("reply before the stall v7q".to_string())]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[("COYOTE_TUI_STUBBORN_TURN", "1")]);

        let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let pid = child_pid(&transcript);
        pty.send(b"say something\r").unwrap();
        wait_for_squeezed(
            &mut pty,
            "reply before the stall v7q",
            "the reply of the turn that then stalls",
        );
        let signalled_at = pty.raw().len();
        let started = std::time::Instant::now();
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let took = started.elapsed();

        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert!(
            find(pty.raw(), LEAVE_ALT, signalled_at.saturating_sub(16)).is_some(),
            "SIGTERM during the stubborn turn never left the alternate screen:\n{transcript}"
        );
        assert!(
            transcript.contains("did not stop within"),
            "no notice said the turn was left behind:\n{transcript}"
        );
        let exit_line =
            find(pty.raw(), b"exit=", signalled_at).expect("the wrapper reported the exit");
        assert_cursor_shown_before(pty.raw(), exit_line, "the exit report");
        assert!(
            took < Duration::from_secs(10),
            "the exit was not bounded by the deadline: {took:?}"
        );
    }

    /// `TuiPrompter` renders a tool's `Confirm` request as the modal and
    /// answers it: a tool asking through the prompt channel on a real tty
    /// shows its question in the UI, `y` reaches the tool as a `1` and lets
    /// it finish, the model's follow-up reply lands in the transcript, and no
    /// raw protocol line reaches the screen.
    #[test]
    #[serial]
    fn a_tools_confirm_request_is_the_modal_and_y_lets_the_tool_finish() {
        let dir = fresh_config_dir("tool-confirm");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("confirm-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_confirm_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done z2v".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_confirm_rec.sh", RECORDING_CONFIRM_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the confirm tool\r").unwrap();
        pty.wait_for("q9d-q", PTY_DEADLINE)
            .expect("the tool's confirm question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture done z2v", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "1",
            "`y` on the confirm modal did not reach the tool as a yes:\n{transcript}"
        );
        assert!(
            !transcript.contains("coyote_prompt"),
            "a raw prompt-protocol line reached the screen:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// Keys are handled and frames drawn while a work task sleeps: with a tool
    /// blocked mid-turn, typed text shows up in the composer; once the tool is
    /// released the turn finishes, and Enter then submits the typed-ahead text
    /// as the next turn (it was neither lost nor swallowed).
    #[test]
    #[serial]
    fn typing_while_a_tool_sleeps_reaches_the_composer_and_submits_afterwards() {
        let dir = fresh_config_dir("tool-sleep-keys");
        let _guard = TempDirGuard(dir.clone());
        let marker = dir.join("tool");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_sleep".to_string(),
                arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
            },
            Reply::Text("fixture done tail-k4q".to_string()),
            Reply::Text("third reply m8n".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", MARKED_SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-started"),
            "the sleep tool's start marker",
        );
        let typed_at = pty.raw().len();
        let typed = std::time::Instant::now();
        pty.send(b"typeahead-q9z").unwrap();
        // Keystrokes that straddle a frame boundary are painted as two cell
        // runs with a cursor move between them, so the stripped transcript need
        // not hold the marker contiguously. Ctrl+L repaints the whole screen in
        // one frame, which also proves a key is handled while the tool sleeps.
        pty.send(b"\x0c").unwrap();
        pty.wait_for_after("typeahead-q9z", typed_at, PTY_DEADLINE)
            .expect("text typed while the tool slept was not drawn in the composer");
        let drawn_after = typed.elapsed();
        // The tool only ends when released (or after eight seconds on its
        // own); the second model call is the sign the turn has moved on.
        assert!(
            !dir.join("tool-release").exists() && fake.served() == 1,
            "text typed while the tool ran was drawn only {drawn_after:?} later, after the \
         turn had finished (model calls served: {}); frames must be drawn and keys \
         handled while a work task sleeps",
            fake.served()
        );

        fs::write(dir.join("tool-release"), b"").unwrap();
        wait_for_squeezed(
            &mut pty,
            "fixture done tail-k4q",
            "the reply after the tool",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        let transcript = wait_for_squeezed(
            &mut pty,
            "third reply m8n",
            "the reply to the typed-ahead prompt",
        );
        assert_eq!(fake.served(), 3, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// SIGTERM while a work task is still running (a tool blocked mid-turn)
    /// still runs the leave path: the alternate screen is left, the tty is
    /// cooked and the exit is orderly. The abort kills the tool, so the exit
    /// comes well inside `EXIT_CANCEL_WAIT` (5 s) rather than waiting out the
    /// tool's own eight-second give-up; a turn that did miss that deadline
    /// would be left behind and the process would still exit, which
    /// `sigterm_during_a_turn_that_ignores_the_abort_leaves_it_behind_in_bounded_time`
    /// pins.
    #[test]
    #[serial]
    fn sigterm_during_a_running_tool_still_leaves_the_tty_cooked() {
        let dir = fresh_config_dir("sigterm-tool");
        let _guard = TempDirGuard(dir.clone());
        let marker = dir.join("tool");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_sleep".to_string(),
                arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
            },
            Reply::Text("fixture done after sigterm".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", MARKED_SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let pid = child_pid(&transcript);
        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-started"),
            "the sleep tool's start marker",
        );
        let signalled_at = pty.raw().len();
        let started = std::time::Instant::now();
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let took = started.elapsed();

        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert!(
            find(pty.raw(), LEAVE_ALT, signalled_at.saturating_sub(16)).is_some(),
            "SIGTERM during the tool never left the alternate screen:\n{transcript}"
        );
        let exit_line =
            find(pty.raw(), b"exit=", signalled_at).expect("the wrapper reported the exit");
        assert_cursor_shown_before(pty.raw(), exit_line, "the exit report");
        assert!(
            took < Duration::from_secs(5),
            "the exit waited the tool out instead of aborting it: {took:?}"
        );
    }

    /// Two turns in a row: the second prompt gets its own rendered reply and
    /// the first turn's cells are still there, so the transcript grows rather
    /// than being replaced.
    #[test]
    #[serial]
    fn a_second_turn_renders_after_the_first_without_losing_it() {
        let dir = fresh_config_dir("two-turns");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"**alpha** one-1q\r").unwrap();
        wait_for_squeezed(&mut pty, "alpha one-1q", "the first rendered reply");
        pty.read_for(Duration::from_millis(300));
        let second_at = pty.raw().len();
        pty.send(b"**beta** two-2w\r").unwrap();
        let transcript = wait_for_squeezed(&mut pty, "beta two-2w", "the second rendered reply");
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[second_at..]));
        assert!(
            since.contains("**beta**"),
            "the second prompt never reached the screen as a user cell:\n{since}"
        );
        assert!(
            transcript.contains("**alpha**"),
            "the first turn vanished from the transcript:\n{transcript}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// The tty check names stdin and stdout only: `coyote 2> log` is a normal
    /// run. The UI comes up, Ctrl+D leaves cleanly, and nothing of the
    /// alternate screen lands in the stderr file.
    #[test]
    #[serial]
    fn stderr_redirected_to_a_file_is_not_a_tty_refusal() {
        let dir = fresh_config_dir("stderr-file");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let captured = dir.join("stderr.log");
        let wrapper = format!(
            "'{{coyote}}'{{args}} <{{stdin}} 2>'{}' & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {{report}}",
            captured.display()
        );
        let mut pty = spawn_tui_under(&dir, &[], &wrapper, "/dev/tty");

        pty.wait_for("no session", PTY_DEADLINE)
            .expect("the UI did not come up with stderr redirected to a file");
        pty.send(b".info\r").unwrap();
        pty.wait_for("log_path", PTY_DEADLINE).unwrap();
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert!(
            !transcript.contains("No TTY for TUI"),
            "a stderr redirect was treated as a missing tty:\n{transcript}"
        );
        let file = fs::read(&captured).unwrap_or_default();
        assert!(
            find(&file, ENTER_ALT, 0).is_none() && find(&file, LEAVE_ALT, 0).is_none(),
            "alt-screen escapes were written into the stderr file: {:?}",
            String::from_utf8_lossy(&file)
        );
    }

    /// A window dragged down to almost nothing and back: no panic, the chrome
    /// returns at full size and keys still work.
    #[test]
    #[serial]
    fn a_tiny_window_does_not_crash_the_ui_and_growing_back_repaints() {
        let dir = fresh_config_dir("tiny-resize");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        for (rows, cols) in [(3u16, 12u16), (1, 1), (2, 40)] {
            pty.resize(rows, cols).unwrap();
            pty.read_for(Duration::from_millis(400));
        }
        let before = pty.raw().len();
        pty.resize(40, 100).unwrap();
        assert_repainted_after(&mut pty, before, "growing back from a 1x1 window");

        pty.send(b".info\r").unwrap();
        pty.wait_for("log_path", PTY_DEADLINE)
            .expect("keys were not handled after the tiny-window round trip");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            1,
            "the alternate screen was left more than once (a panic restore?):\n{transcript}"
        );
    }

    // ---------------------------------------------------------------------
    // Paced streams: the fake holds the SSE response open between two deltas
    // so the screen can be read while the model is still "talking".
    // ---------------------------------------------------------------------

    /// A streamed reply renders progressively: the first delta is on screen
    /// while the stream is still open (the fake has not sent the second delta
    /// yet), and the second delta joins it once released. The whole reply
    /// must not wait for the stream to end before it is painted.
    #[test]
    #[serial]
    fn a_streamed_reply_is_painted_before_the_stream_ends() {
        let dir = fresh_config_dir("paced-stream");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("stream-release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("first half h4x".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text(" second half j9k".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"stream something\r").unwrap();
        let started = std::time::Instant::now();
        let transcript = wait_for_squeezed(&mut pty, "first half h4x", "the first delta");
        let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            started.elapsed() < HOLD_LIMIT,
            "the first delta appeared only after the fake gave up holding the stream"
        );
        assert!(
            !squeezed.contains("secondhalfj9k"),
            "the second delta was on screen before the fake sent it:\n{transcript}"
        );
        // Give a late paint a chance before deciding; the second delta has
        // not been sent, so it must still be absent.
        let settled = pty.read_for(Duration::from_millis(600));
        assert!(
            !settled
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
                .contains("secondhalfj9k"),
            "the second delta reached the screen before the fake sent it:\n{settled}"
        );

        fs::write(&release, b"").unwrap();
        let transcript = wait_for_squeezed(&mut pty, "second half j9k", "the second delta");
        assert_eq!(fake.served(), 1, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// `exit()` is idempotent on the panic path too: the hook restores the
    /// terminal and nothing afterwards (a guard drop, the normal leave path)
    /// leaves the alternate screen or disables bracketed paste a second time.
    #[test]
    #[serial]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "the panic hook is compiled out of release builds"
    )]
    fn a_panic_at_start_leaves_the_alternate_screen_exactly_once() {
        let dir = fresh_config_dir("panic-once");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[("COYOTE_TUI_PANIC_AT_START", "1")]);

        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 101, "{transcript}");
        assert_tty_restored(&transcript);
        let raw = pty.raw();
        assert_eq!(
            count(raw, ENTER_ALT),
            1,
            "the alternate screen must be entered exactly once:\n{transcript}"
        );
        assert_eq!(
            count(raw, LEAVE_ALT),
            1,
            "the alternate screen must be left exactly once on a panic:\n{transcript}"
        );
        assert_eq!(
            count(raw, PASTE_OFF),
            1,
            "bracketed paste must be disabled exactly once on a panic:\n{transcript}"
        );
        if let Some(push) = find_keyboard_push(raw, 0) {
            let pop = rfind_keyboard_pop(raw).expect("flags pushed but never popped on panic");
            assert!(push < pop, "{transcript}");
        }
    }

    const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
    const SYNC_END: &[u8] = b"\x1b[?2026l";

    /// A reasoning model's stream renders progressively too: with the
    /// reasoning delta held open on the wire the turn is visibly busy (the
    /// live `Thinking for Ns` status row offers the interrupt), the reasoning
    /// text itself stays hidden like the REPL hides a think span, and the
    /// answer is not on screen; once released the answer lands without the
    /// reasoning text and the status row stops ticking (it repaints once a
    /// second while it lives).
    #[test]
    #[serial]
    fn a_reasoning_stream_keeps_the_turn_busy_until_the_answer_is_sent() {
        let dir = fresh_config_dir("reasoning-stream");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("thinking-release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Reasoning("pondering the question v5w".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("thought-out reply c3d".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"think about it\r").unwrap();
        let started = std::time::Instant::now();
        let transcript = wait_for_squeezed(&mut pty, "Thinking for", "the live thinking row");
        assert!(
            started.elapsed() < HOLD_LIMIT,
            "the Thinking row appeared only after the fake gave up holding the stream"
        );
        assert!(
            transcript.contains("Esc to interrupt"),
            "no busy status row was shown while the model was reasoning:\n{transcript}"
        );
        let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            !squeezed.contains("ponderingthequestionv5w"),
            "the reasoning text was painted into the transcript:\n{transcript}"
        );
        assert!(
            !squeezed.contains("thought-outreplyc3d"),
            "the answer was on screen before the fake sent it:\n{transcript}"
        );
        let settled = pty.read_for(Duration::from_millis(600));
        assert!(
            !settled
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
                .contains("thought-outreplyc3d"),
            "the answer reached the screen before the fake sent it:\n{settled}"
        );

        fs::write(&release, b"").unwrap();
        wait_for_squeezed(
            &mut pty,
            "thought-out reply c3d",
            "the answer after reasoning",
        );
        pty.read_for(Duration::from_millis(400));
        let answered_at = pty.raw().len();
        pty.read_for(Duration::from_millis(1600));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[answered_at..]));
        assert!(
            !since.contains("Esc to interrupt") && !since.contains("Working"),
            "the busy status row kept repainting after the answer arrived:\n{since}"
        );
        let whole: String = strip_ansi(&String::from_utf8_lossy(pty.raw()))
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            !whole.contains("ponderingthequestionv5w"),
            "the reasoning text reached the screen at some point during the turn"
        );
        assert_eq!(fake.served(), 1);

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// Frames are drawn inside synchronized updates on the real tty: every
    /// begin has its end, the pairs never nest or interleave, the first begin
    /// comes after the alternate screen is entered and the last end before it
    /// is left.
    #[test]
    #[serial]
    fn every_frame_on_the_tty_is_wrapped_in_a_synchronized_update() {
        let dir = fresh_config_dir("sync-updates");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b".info\r").unwrap();
        pty.wait_for("log_path", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);

        let raw = pty.raw();
        let enter_alt = find(raw, ENTER_ALT, 0).expect("the alternate screen was never entered");
        let leave_alt = rfind(raw, LEAVE_ALT).expect("the alternate screen was never left");
        let begins = count(raw, SYNC_BEGIN);
        let ends = count(raw, SYNC_END);
        assert!(
            begins >= 2,
            "fewer than two frames were drawn inside synchronized updates ({begins})"
        );
        assert_eq!(
            begins, ends,
            "synchronized updates were not balanced (begin={begins}, end={ends})"
        );

        // Walk the stream: begins and ends must strictly alternate.
        let mut at = 0usize;
        let mut open = false;
        let mut first_begin = None;
        let mut last_end = None;
        while let Some(next) = [find(raw, SYNC_BEGIN, at), find(raw, SYNC_END, at)]
            .into_iter()
            .flatten()
            .min()
        {
            if raw[next..].starts_with(SYNC_BEGIN) {
                assert!(
                    !open,
                    "a synchronized update began at {next} while one was open"
                );
                first_begin.get_or_insert(next);
                open = true;
            } else {
                assert!(open, "a synchronized update ended at {next} with none open");
                open = false;
                last_end = Some(next);
            }
            at = next + 1;
        }
        assert!(!open, "the last synchronized update was never ended");
        let first_begin = first_begin.unwrap();
        let last_end = last_end.unwrap();
        assert!(
            enter_alt < first_begin,
            "a frame was drawn before the alternate screen was entered \
         (begin={first_begin}, enter={enter_alt})"
        );
        assert!(
            last_end < leave_alt,
            "a frame ended after the alternate screen was left (end={last_end}, leave={leave_alt})"
        );
    }

    /// The status row offers "Esc to interrupt" while a turn runs. Pressing
    /// Esc on a stream the model is still holding ends the turn at once: the
    /// next command is handled well before the model would have let go, the
    /// rest of the reply never lands, a fresh turn gets its own request, and
    /// the UI leaves cleanly.
    #[test]
    #[serial]
    fn esc_interrupts_a_held_stream_and_the_ui_keeps_taking_keys() {
        let dir = fresh_config_dir("esc-interrupt");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("never-released");
        let fake = FakeLlm::start(vec![
            Reply::Streamed(vec![
                Chunk::Text("first half int-a1".to_string()),
                Chunk::WaitFor(release),
                Chunk::Text(" tail int-b2".to_string()),
            ]),
            Reply::Text("fresh reply after the interrupt s2".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"stream and hold\r").unwrap();
        wait_for_squeezed(&mut pty, "first half int-a1", "the first delta");
        pty.wait_for("Esc to interrupt", PTY_DEADLINE)
            .expect("the status row never offered Esc to interrupt");
        let pressed = std::time::Instant::now();
        pty.send(b"\x1b").unwrap();
        // A command typed right after the interrupt runs as its own turn; had
        // the stream still been running it would queue behind the fake's
        // eight-second hold. `.clear images` is one that waits for a running
        // turn, where `.info` would run beside it.
        pty.read_for(Duration::from_millis(500));
        let info_at = pty.raw().len();
        pty.send(b".clear images\r").unwrap();
        let transcript = wait_for_squeezed_after(
            &mut pty,
            "No pending images.",
            info_at,
            "keys were not handled after the interrupt",
        );
        assert!(
            pressed.elapsed() < Duration::from_secs(6),
            "the command ran only after the fake gave up holding the stream, not on Esc"
        );
        let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            !squeezed.contains("tailint-b2"),
            "the interrupted stream's tail still reached the screen:\n{transcript}"
        );

        pty.read_for(Duration::from_millis(300));
        pty.send(b"next prompt\r").unwrap();
        let transcript = wait_for_squeezed(
            &mut pty,
            "fresh reply after the interrupt s2",
            "the reply to the turn after the interrupt",
        );
        assert_eq!(fake.served(), 2, "{transcript}");
        let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            !squeezed.contains("tailint-b2"),
            "the interrupted stream's tail landed later:\n{transcript}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A tool that records the `COYOTE_UI` it was given, the value a tool
    /// branches on to learn which surface answers its prompts.
    const UI_RECORD_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Record the UI the tool runs under.\n\
    # @option --out! Path to write the UI name to.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   local out\n\
    \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   printf '%s' \"${COYOTE_UI:-unset}\" > \"$out\"\n\
    \x20   echo \"fixture recorded ui w7e\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// Tools run from the full-screen UI are told `COYOTE_UI=tui`.
    #[test]
    #[serial]
    fn a_tool_run_from_the_tui_sees_coyote_ui_tui() {
        let dir = fresh_config_dir("tool-ui-env");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("ui-seen");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_ui".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done r4t".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_ui.sh", UI_RECORD_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the ui tool\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's COYOTE_UI record");
        wait_for_squeezed(&mut pty, "fixture done r4t", "the reply after the tool");
        assert_eq!(fs::read_to_string(&out).unwrap().trim(), "tui");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// `TuiPrompter` answers a Confirm modal both ways: `n` is a real "no"
    /// (the tool reads `0`, not a cancellation), the turn still finishes with
    /// the model's follow-up, and the UI keeps taking keys afterwards.
    #[test]
    #[serial]
    fn declining_a_tools_confirm_with_n_gives_the_tool_a_no_and_the_turn_still_finishes() {
        let dir = fresh_config_dir("tool-confirm-no");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("confirm-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_confirm_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture declined t6k".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_confirm_rec.sh", RECORDING_CONFIRM_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the confirm tool\r").unwrap();
        pty.wait_for("q9d-q", PTY_DEADLINE)
            .expect("the tool's confirm question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"n").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture declined t6k", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "0",
            "`n` on the confirm modal did not reach the tool as a no:\n{transcript}"
        );
        assert!(
            !transcript.contains("coyote_prompt"),
            "a raw prompt-protocol line reached the screen:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        pty.send(b".info\r").unwrap();
        pty.wait_for("log_path", PTY_DEADLINE)
            .expect("keys were not handled after the declined confirm");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// On a terminal that answers the keyboard-enhancement probe (kitty,
    /// ghostty, wezterm, foot) the flags are pushed after alt-screen entry
    /// and popped before it is left, and with them on, a key *release* the
    /// terminal reports in the kitty encoding must not type into the
    /// composer (`KeyEventKind::Press` only) while a kitty-encoded press
    /// still does.
    #[test]
    #[serial]
    fn a_terminal_answering_the_keyboard_probe_gets_flags_pushed_and_releases_are_ignored() {
        let dir = fresh_config_dir("kitty-flags");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.leave_keyboard_probe_unanswered();

        let probe = pty
            .wait_for_raw(b"\x1b[?u", 0, PTY_DEADLINE)
            .expect("the keyboard-enhancement support probe (CSI ? u) was never sent");
        // A kitty-protocol terminal answers "flags currently 0" and then the
        // primary device attributes the probe pairs the query with (CSI c).
        pty.wait_for_raw(b"\x1b[c", probe, PTY_DEADLINE)
            .expect("the probe never asked for primary device attributes");
        pty.send(b"\x1b[?0u\x1b[?62;22c").unwrap();
        if let Err(err) = pty.wait_for("no session", PTY_DEADLINE) {
            panic!(
                "the UI never came up after the terminal answered the keyboard probe: {err}\n\
             raw bytes since the probe: {:?}",
                String::from_utf8_lossy(&pty.raw()[probe..])
            );
        }

        let raw = pty.raw();
        let enter_alt = find(raw, ENTER_ALT, 0).expect("the alternate screen was never entered");
        let push = find_keyboard_push(raw, 0)
            .expect("the terminal answered the probe but the flags were never pushed");
        assert!(
            enter_alt < push,
            "flags pushed before alt-screen entry (enter={enter_alt}, push={push})"
        );
        assert!(probe < push, "flags pushed before the probe was sent");

        pty.send(b"qz7").unwrap();
        pty.read_for(Duration::from_millis(300));
        // kitty encoding: CSI <codepoint> ; <mods>:<event-type> u; type 3 is release.
        pty.send(b"\x1b[107;1:3u").unwrap();
        pty.read_for(Duration::from_millis(300));
        // ...and type 1 is press.
        pty.send(b"\x1b[109;1:1u").unwrap();
        pty.read_for(Duration::from_millis(300));
        // Submitting shows the composer's exact contents twice: the prompt echo
        // and the dry-run reply, both painted whole rather than cell by cell.
        // Counted from the submit on, so the composer's own paints of the
        // typed text before Enter do not stand in for them.
        let submitted_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        let text = loop {
            pty.read_for(Duration::from_millis(200));
            let text = strip_ansi(&String::from_utf8_lossy(&pty.raw()[submitted_at..]));
            let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                !squeezed.contains("qz7k"),
                "a kitty key *release* typed into the composer:\n{text}"
            );
            if squeezed.matches("qz7m").count() >= 2 {
                break text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a kitty-encoded key press did not reach the composer (or Enter did not submit it):\n{text}"
            );
        };
        assert!(
            !text.contains("qz7mk") && !text.contains("qz7km"),
            "a kitty key *release* typed into the composer:\n{text}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        let raw = pty.raw();
        let leave_alt = rfind(raw, LEAVE_ALT).expect("the alternate screen was never left");
        let pop = rfind_keyboard_pop(raw).expect("flags pushed but never popped");
        assert!(
            push < pop && pop < leave_alt,
            "flags must be popped before leaving the alternate screen (push={push}, pop={pop}, leave={leave_alt})"
        );
    }

    /// A user who turns streaming off in their config still gets the reply:
    /// a non-streamed assistant message (one JSON body, no SSE) lands in the
    /// transcript as a rendered cell, the busy status row clears, and the UI
    /// keeps taking keys afterwards.
    #[test]
    #[serial]
    fn a_non_streamed_reply_lands_in_the_transcript_and_the_ui_stays_live() {
        let dir = fresh_config_dir("no-stream");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::Text("whole reply at once w4n".to_string()),
            Reply::Text("second whole reply w4o".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        // Same config with streaming turned off at the top level.
        let config = dir.join("config.yaml");
        let text = fs::read_to_string(&config).unwrap();
        assert!(
            text.contains("stream: true\n"),
            "fixture config lost its stream key"
        );
        fs::write(&config, text.replace("stream: true\n", "stream: false\n")).unwrap();
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"first\r").unwrap();
        wait_for_squeezed(
            &mut pty,
            "whole reply at once w4n",
            "the non-streamed reply",
        );
        assert_eq!(fake.served(), 1);

        // The turn is over: the busy row must stop repainting.
        pty.read_for(Duration::from_millis(400));
        let settled_at = pty.raw().len();
        pty.read_for(Duration::from_millis(1600));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[settled_at..]));
        assert!(
            !since.contains("Esc to interrupt") && !since.contains("Working"),
            "the busy status row kept repainting after a non-streamed reply:\n{since}"
        );

        // And a second turn still works, with the first still on screen.
        pty.send(b"second\r").unwrap();
        let text = wait_for_squeezed(&mut pty, "second whole reply w4o", "the second reply");
        let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            squeezed.contains("wholereplyatoncew4n"),
            "the first reply vanished from the transcript once the second arrived:\n{text}"
        );
        assert_eq!(fake.served(), 2);

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// OSC 8 hyperlinks and other raw escapes are stripped before the rendered
    /// markdown reaches `ansi-to-tui`: a reply carrying a link shows the link's
    /// text in the transcript, no OSC 8 introducer reaches the tty and no
    /// fragment of one (`]8;;`) is painted as visible text.
    #[test]
    #[serial]
    fn a_reply_with_a_link_shows_its_text_and_leaks_no_osc_8() {
        let dir = fresh_config_dir("osc8-link");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text(
            "see [the fixture page r2x](https://example.invalid/r2x-page) for more".to_string(),
        )]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let submitted_at = pty.raw().len();
        pty.send(b"link please\r").unwrap();
        let transcript = wait_for_squeezed(&mut pty, "the fixture page r2x", "the link's text");
        assert_eq!(fake.served(), 1, "{transcript}");
        // Let the reply settle fully before inspecting the bytes.
        pty.read_for(Duration::from_millis(600));

        let raw = pty.raw();
        let settled_at = raw.len();
        assert!(
            find(raw, b"\x1b]8;", 0).is_none(),
            "an OSC 8 sequence reached the tty through the frame:\n{}",
            strip_ansi(&String::from_utf8_lossy(raw))
        );
        let visible = strip_ansi(&String::from_utf8_lossy(raw));
        assert!(
            !visible.contains("]8;;"),
            "an OSC 8 fragment was painted as text:\n{visible}"
        );
        // Past the startup colour queries the UI writes no operating-system
        // commands of its own, so any `ESC ]` after the submit came through
        // from the reply.
        if let Some(at) = find(raw, b"\x1b]", submitted_at) {
            let context = &raw[at.saturating_sub(40)..(at + 80).min(raw.len())];
            panic!(
                "an OSC introducer reached the tty at {at}: {:?}",
                String::from_utf8_lossy(context)
            );
        }

        // The UI is still live afterwards.
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", settled_at, PTY_DEADLINE)
            .expect("`.info` after the linked reply never rendered");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------
    // The interrupt keys at rest and mid-turn, and what the normal screen
    // shows once the UI has left.
    // ---------------------------------------------------------------------

    /// Ctrl+C on an idle, empty composer quits exactly as Ctrl+D does: the
    /// alternate screen is left once, the process exits 0, the tty is cooked.
    #[test]
    #[serial]
    fn ctrl_c_on_an_idle_empty_composer_quits_like_ctrl_d() {
        let dir = fresh_config_dir("ctrl-c-idle-empty");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x03").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            1,
            "Ctrl+C on an idle empty composer must leave the alternate screen exactly once:\n{transcript}"
        );
    }

    /// Ctrl+C while the composer holds a draft clears the draft and nothing
    /// else: the UI stays up, the draft is never submitted (Enter afterwards
    /// starts no turn and the model sees no request), and the composer still
    /// takes a command.
    #[test]
    #[serial]
    fn ctrl_c_with_a_draft_clears_the_composer_and_does_not_quit() {
        let dir = fresh_config_dir("ctrl-c-draft");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text("must never be requested k4m".to_string())]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"draft to discard q9x").unwrap();
        wait_for_painted_after(
            &mut pty,
            "draft to discard q9x",
            0,
            "the typed draft never reached the composer",
        );
        pty.read_for(HOTKEY_GAP);
        let cleared_at = pty.raw().len();
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[cleared_at..]));
        assert!(
            !since.contains("exit="),
            "Ctrl+C with a draft in the composer quit the UI:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "Ctrl+C with a draft in the composer left the alternate screen:\n{since}"
        );

        // Enter now must find an empty composer: no turn, no request.
        pty.send(b"\r").unwrap();
        pty.read_for(HOTKEY_GAP);
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[cleared_at..]));
        assert!(
            !since.contains("Working"),
            "Enter after Ctrl+C started a turn, so the draft was not cleared:\n{since}"
        );
        assert_eq!(
            fake.served(),
            0,
            "the discarded draft reached the model:\n{since}"
        );

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("keys were not handled after Ctrl+C cleared the draft");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(fake.served(), 0, "{transcript}");
    }

    /// Ctrl+D with a draft in the composer is not the quit chord: the UI
    /// stays up and the draft stays put; once Ctrl+C has cleared it, Ctrl+D
    /// leaves as usual.
    #[test]
    #[serial]
    fn ctrl_d_with_a_draft_does_not_quit() {
        let dir = fresh_config_dir("ctrl-d-draft");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"draft kept on ctrl-d w2v").unwrap();
        wait_for_painted_after(
            &mut pty,
            "draft kept on ctrl-d w2v",
            0,
            "the typed draft never reached the composer",
        );
        pty.read_for(HOTKEY_GAP);
        let pressed_at = pty.raw().len();
        pty.send(b"\x04").unwrap();
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
        assert!(
            !since.contains("exit="),
            "Ctrl+D with a draft in the composer quit the UI:\n{since}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "Ctrl+D with a draft in the composer left the alternate screen:\n{since}"
        );

        pty.send(b"\x03").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// Ctrl+C while a turn runs interrupts it as Esc does, and drops any
    /// held lines besides: the held stream ends at once, the process does
    /// NOT exit, the rest of the reply never lands, and the next command is
    /// handled well before the model would have let go.
    #[test]
    #[serial]
    fn ctrl_c_during_a_held_stream_interrupts_the_turn_and_the_ui_stays_up() {
        let dir = fresh_config_dir("ctrl-c-interrupt");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("never-released");
        let fake = FakeLlm::start(vec![
            Reply::Streamed(vec![
                Chunk::Text("first half cc-a1".to_string()),
                Chunk::WaitFor(release),
                Chunk::Text(" tail cc-b2".to_string()),
            ]),
            Reply::Text("fresh reply after ctrl-c s3".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"stream and hold\r").unwrap();
        wait_for_squeezed(&mut pty, "first half cc-a1", "the first delta");
        pty.wait_for("Esc to interrupt", PTY_DEADLINE)
            .expect("the status row never offered Esc to interrupt");
        let pressed = std::time::Instant::now();
        let pressed_at = pty.raw().len();
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(500));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
        assert!(
            !since.contains("exit="),
            "Ctrl+C during a running turn quit the UI instead of interrupting the turn:\n{since}"
        );

        // `.clear images` waits for a running turn, so its output shows the
        // interrupt ended the turn.
        let info_at = pty.raw().len();
        pty.send(b".clear images\r").unwrap();
        let transcript = wait_for_squeezed_after(
            &mut pty,
            "No pending images.",
            info_at,
            "keys were not handled after the Ctrl+C interrupt",
        );
        assert!(
            pressed.elapsed() < Duration::from_secs(6),
            "the command ran only after the fake gave up holding the stream, not on Ctrl+C"
        );
        let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            !squeezed.contains("tailcc-b2"),
            "the interrupted stream's tail still reached the screen:\n{transcript}"
        );

        pty.read_for(Duration::from_millis(300));
        pty.send(b"next prompt\r").unwrap();
        let transcript = wait_for_squeezed(
            &mut pty,
            "fresh reply after ctrl-c s3",
            "the reply to the turn after the interrupt",
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// With a session open, Esc on a held stream behaves as the REPL's Ctrl+C
    /// does: a "Response interrupted" notice lands in the transcript, the
    /// partial text already streamed is kept, both survive a full repaint
    /// once the UI is idle again, and the rest of the reply never arrives.
    #[test]
    #[serial]
    fn esc_mid_stream_leaves_an_interrupted_cell_in_the_transcript() {
        let dir = fresh_config_dir("esc-interrupted-cell");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("never-released");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("partial text before esc e5p".to_string()),
            Chunk::WaitFor(release),
            Chunk::Text(" tail never shown e5q".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        // The partial-text path needs a session; without one the streaming
        // call fails the turn with "Aborted." (the REPL does the same).
        pty.send(b".session\r").unwrap();
        pty.wait_for("temp", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(500));
        pty.send(b"stream and hold\r").unwrap();
        wait_for_squeezed(&mut pty, "partial text before esc e5p", "the first delta");
        pty.wait_for("Esc to interrupt", PTY_DEADLINE)
            .expect("the status row never offered Esc to interrupt");
        let pressed_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        let since = loop {
            pty.read_for(Duration::from_millis(200));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
            if since.to_lowercase().contains("interrupted") {
                break since;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no cell said the turn was interrupted after Esc:\n{since}"
            );
        };
        assert!(
            !since.contains("exit="),
            "Esc during a running turn quit the UI:\n{since}"
        );
        // A resize forces a full repaint: the partial text and the interrupted
        // cell must both still be on the idle screen.
        pty.read_for(Duration::from_millis(800));
        let repaint_at = pty.raw().len();
        pty.resize(38, 100).unwrap();
        pty.wait_for_after("temp", repaint_at, PTY_DEADLINE)
            .expect("the screen was not repainted after the resize");
        let repainted = painted_after(&mut pty, repaint_at, |text| {
            squeezed(text).contains("partialtextbeforeesce5p")
                && text.to_lowercase().contains("interrupted")
        })
        .unwrap_or_else(|since| since);
        let squeezed: String = repainted.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            squeezed.contains("partialtextbeforeesce5p"),
            "the partial text streamed before Esc vanished from the transcript:\n{repainted}"
        );
        assert!(
            repainted.to_lowercase().contains("interrupted"),
            "the interrupted cell did not survive to the idle screen:\n{repainted}"
        );
        assert!(
            !squeezed.contains("tailnevershowne5q"),
            "the interrupted stream's tail reached the screen:\n{repainted}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// Without a session, Esc on a held stream fails the turn the way the
    /// REPL's Ctrl+C does there: an "Aborted." error cell, no partial text
    /// kept, and the UI stays up and keeps taking keys.
    #[test]
    #[serial]
    fn esc_mid_stream_without_a_session_shows_the_aborted_error_and_stays_up() {
        let dir = fresh_config_dir("esc-aborted-no-session");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("never-released");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("partial before esc n8a".to_string()),
            Chunk::WaitFor(release),
            Chunk::Text(" tail never shown n8b".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"stream and hold\r").unwrap();
        wait_for_squeezed(&mut pty, "partial before esc n8a", "the first delta");
        pty.wait_for("Esc to interrupt", PTY_DEADLINE)
            .expect("the status row never offered Esc to interrupt");
        let pressed_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        let since = pty
            .wait_for_after("Aborted.", pressed_at, Duration::from_secs(6))
            .expect("no Aborted. error followed Esc on a session-less stream");
        assert!(
            !since.contains("exit="),
            "Esc during a running turn quit the UI:\n{since}"
        );

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        let transcript = pty
            .wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("keys were not handled after the interrupt");
        let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            !squeezed.contains("tailnevershownn8b"),
            "the interrupted stream's tail reached the screen:\n{transcript}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A lone Esc on an idle, empty composer is not a quit: the UI stays on
    /// the alternate screen and keeps taking keys.
    #[test]
    #[serial]
    fn esc_on_an_idle_empty_composer_does_not_quit() {
        let dir = fresh_config_dir("esc-idle");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(HOTKEY_GAP);
        let pressed_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(1000));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
        assert!(!since.contains("exit="), "a lone Esc quit the UI:\n{since}");
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            0,
            "a lone Esc left the alternate screen:\n{since}"
        );

        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("keys were not handled after a lone Esc");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------
    // Shifted answers on the confirm modal, a narrow screen, fenced commands
    // and the editor-backed commands the UI refuses.
    // ---------------------------------------------------------------------

    /// A confirm tool whose question is long enough to need several rows on
    /// a narrow screen.
    const LONG_QUESTION: &str = "Really continue with this long question that needs several rows at forty columns? q9d-wide";

    /// The recording confirm tool asking [`LONG_QUESTION`] instead.
    fn long_question_confirm_tool() -> String {
        RECORDING_CONFIRM_TOOL.replace("Really do it? q9d-q", LONG_QUESTION)
    }

    /// The yes/no modal takes a shifted answer too: `Y` is a yes (the tool
    /// reads `1`) and `N` is a no (`0`), exactly like their lowercase twins.
    /// Two turns in one session, so the second modal is also proof the UI
    /// re-arms after the first.
    #[test]
    #[serial]
    fn uppercase_y_and_n_answer_a_tools_confirm_like_their_lowercase_twins() {
        let dir = fresh_config_dir("tool-confirm-upper");
        let _guard = TempDirGuard(dir.clone());
        let out_yes = dir.join("confirm-answer-yes");
        let out_no = dir.join("confirm-answer-no");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_confirm_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out_yes.display()),
            },
            Reply::Text("fixture upper yes u7y".to_string()),
            Reply::ToolCall {
                name: "fixture_confirm_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out_no.display()),
            },
            Reply::Text("fixture upper no u7n".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_confirm_rec.sh", RECORDING_CONFIRM_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the confirm tool\r").unwrap();
        pty.wait_for("q9d-q", PTY_DEADLINE)
            .expect("the tool's confirm question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"Y").unwrap();
        wait_for_file(&mut pty, &out_yes, "the tool's recorded answer to Y");
        let transcript = wait_for_squeezed(&mut pty, "fixture upper yes u7y", "the reply after Y");
        assert_eq!(
            fs::read_to_string(&out_yes).unwrap().trim(),
            "1",
            "`Y` on the confirm modal did not reach the tool as a yes:\n{transcript}"
        );

        let second = pty.raw().len();
        pty.send(b"call the confirm tool again\r").unwrap();
        pty.wait_for_after("q9d-q", second, PTY_DEADLINE)
            .expect("the second confirm question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"N").unwrap();
        wait_for_file(&mut pty, &out_no, "the tool's recorded answer to N");
        let transcript = wait_for_squeezed(&mut pty, "fixture upper no u7n", "the reply after N");
        assert_eq!(
            fs::read_to_string(&out_no).unwrap().trim(),
            "0",
            "`N` on the confirm modal did not reach the tool as a no:\n{transcript}"
        );
        assert!(
            !transcript.contains("coyote_prompt"),
            "a raw prompt-protocol line reached the screen:\n{transcript}"
        );
        assert_eq!(fake.served(), 4, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// On a 40-column screen a confirm question longer than the width is
    /// wrapped, not clipped: every character of it is on screen when the
    /// modal is up, and `y` still answers it.
    #[test]
    #[serial]
    fn a_long_confirm_question_is_fully_visible_on_a_forty_column_screen() {
        let dir = fresh_config_dir("tool-confirm-narrow");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("confirm-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_confirm_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture narrow done n4w".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_confirm_rec.sh",
            &long_question_confirm_tool(),
        );
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(24, 40).unwrap();
        pty.read_for(Duration::from_millis(500));
        let before = pty.raw().len();
        pty.send(b"call the confirm tool\r").unwrap();
        let modal = pty
            .wait_for_after("q9d-wide", before, PTY_DEADLINE)
            .expect("the tool's confirm question never reached the 40-column screen");
        // Let the modal finish painting before reading it back.
        pty.read_for(Duration::from_millis(500));
        let modal = format!(
            "{modal}{}",
            strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]))
        );
        let shown = squeezed(&modal);
        assert!(
            shown.contains(&squeezed(LONG_QUESTION)),
            "the confirm question was clipped on a 40-column screen; printable bytes since the \
         prompt was sent:\n{modal}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let transcript = wait_for_squeezed(
            &mut pty,
            "fixture narrow done n4w",
            "the reply after the tool",
        );
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "1",
            "{transcript}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A `.command` wrapped in the REPL's `:::` multi-line fence is still a
    /// command: pasted as three lines and submitted, `.info` yields its table
    /// in the transcript instead of going to the model as a prompt (a dry-run
    /// reply would only echo the fence back). The table is taller than the
    /// screen, so the rows checked are its last ones, on a screen taller
    /// than the default so they all fit above the composer.
    #[test]
    #[serial]
    fn a_fenced_dot_command_runs_as_a_command_and_not_as_a_prompt() {
        let dir = fresh_config_dir("fenced-command");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        repaint(&mut pty, 60, 100);
        pty.read_for(Duration::from_millis(300));
        let before = pty.raw().len();
        pty.send(b"\x1b[200~:::\r.info\r:::\x1b[201~").unwrap();
        pty.wait_for_after(".info", before, PTY_DEADLINE)
            .expect("the pasted fence never reached the composer");
        pty.read_for(Duration::from_millis(300));
        // Three pasted lines, three stacked composer rows.
        let fences = painted_at(pty.raw(), b":::", before);
        let command = painted_at(pty.raw(), b".info", before);
        let (Some(&(open, col)), Some(&(close, _)), Some(&(middle, _))) =
            (fences.first(), fences.last(), command.last())
        else {
            panic!("the fenced paste was not positioned on screen");
        };
        assert_eq!(
            middle,
            open + 1,
            "fences at {fences:?}, command at {command:?}"
        );
        assert_eq!(close, open + 2, "fences at {fences:?}");
        assert_eq!(command.last().map(|p| p.1), Some(col), "{command:?}");
        let submitted = pty.raw().len();
        pty.send(b"\r").unwrap();
        let since = pty
            .wait_for_after("log_path", submitted, PTY_DEADLINE)
            .expect("the fenced `.info` did not run as a command");
        assert!(
            since.contains("config_dir") && since.contains("dry_run"),
            "the `.info` table's tail is incomplete:\n{since}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A `.list` table is fitted to the screen: on a narrow screen a long
    /// description wraps inside its cell, so the table's right-hand edge is
    /// painted rather than cut off. The probe agent makes sure one row is
    /// far wider than the screen whatever agents the install ships.
    #[test]
    #[serial]
    fn a_dot_list_table_wraps_its_cells_to_fit_a_narrow_screen() {
        let dir = fresh_config_dir("list-wrap");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let agent_dir = dir.join("agents").join("wrapprobe");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join("config.yaml"),
            "name: wrapprobe\n\
             description: a probe agent whose description is far too long to sit on \
             one row of a sixty column screen so it must wrap\n\
             version: 0.1.0\n\
             instructions: Reply briefly.\n",
        )
        .unwrap();
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 60).unwrap();
        pty.read_for(Duration::from_millis(500));

        let at = pty.raw().len();
        pty.send(b".list agents\r").unwrap();
        // The bottom border's column join is painted whether or not the
        // table fits, so it marks the listing as drawn either way.
        pty.wait_for_after("\u{2534}", at, PTY_DEADLINE)
            .expect("`.list agents` did not draw its table");
        // One column wider, so the resize is real and the whole screen is
        // painted again; the table was fitted to sixty.
        let at = repaint_until(&mut pty, 60, 61, |f| f.contains('\u{2534}'));
        let frame = since(&pty, at);
        let bottom = frame
            .rfind('\u{2534}')
            .unwrap_or_else(|| panic!("the table's bottom border is off the screen:\n{frame}"));
        let closed = frame[bottom..].chars().skip(1).find(|c| *c != '\u{2500}') == Some('\u{2518}');
        assert!(
            closed && frame.contains('\u{2524}'),
            "the table's right-hand edge was cut off instead of its cells wrapping:\n{frame}"
        );
        assert_ui_alive_then_quit(&mut pty, "`.list agents`");
    }

    /// `.skill <name>` scaffolds the skill file and opens it in `$EDITOR` on
    /// the lent terminal: the alternate screen is left for the editor, the
    /// editor is handed the skill file's path, the file exists afterwards,
    /// and the UI comes back repainted and taking keys.
    #[test]
    #[serial]
    fn dot_skill_scaffolds_the_file_and_opens_it_on_the_lent_terminal() {
        let dir = fresh_config_dir("skill-editor");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let launched = dir.join("editor-launched");
        let editor = recording_editor(&dir, &launched);
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let files_before = walk(&dir);
        let before = pty.raw().len();
        pty.send(b".skill probe-skill-zq7\r").unwrap();
        let left = pty
            .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
            .expect("`.skill` did not release the terminal for the editor");
        let re_entered = wait_for_re_entry(&mut pty, left);
        assert_repainted_after(&mut pty, re_entered, "the `.skill` editor");
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));

        let edited = fs::read_to_string(&launched)
            .unwrap_or_else(|e| panic!("`.skill` never launched the editor ({e}):\n{since}"));
        let opened = edited.lines().next().unwrap_or_default();
        assert!(
            opened.contains("probe-skill-zq7") && opened.contains("SKILL.md"),
            "the editor was not handed the skill file: {edited:?}\n{since}"
        );
        assert!(
            edited.lines().nth(1) == Some("existed"),
            "the skill file {opened:?} was not scaffolded before the editor opened: {edited:?}\n{since}"
        );
        let body = fs::read_to_string(opened).unwrap();
        assert!(
            body.starts_with("---\n") && body.contains("description:"),
            "the skill file {opened:?} holds {body:?}, not the scaffold:\n{since}"
        );
        let skill_files = new_files_named(&dir, &files_before, "probe-skill-zq7");
        assert!(
            !skill_files.is_empty(),
            "no probe-skill-zq7 file appeared under {dir:?}:\n{since}"
        );

        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", re_entered, PTY_DEADLINE)
            .expect("keys were not handled after the `.skill` editor returned");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------
    // The pty harness itself: the escape stripper every assertion above
    // reads through, and the descriptor hygiene of the spawn.
    // ---------------------------------------------------------------------

    #[test]
    fn csi_with_intermediate_bytes_is_dropped_whole() {
        // Kitty keyboard pop (`ESC [ < 1 u`, also without its count) and a
        // DEC private mode reset (`ESC [ ? 2004 l`), plus a space
        // intermediate variant.
        assert_eq!(strip_ansi("a\x1b[<1ub"), "ab");
        assert_eq!(strip_ansi("a\x1b[<ub"), "ab");
        assert_eq!(strip_ansi("a\x1b[?2004lb"), "ab");
        assert_eq!(strip_ansi("a\x1b[2 qb"), "ab");
        assert_eq!(strip_ansi("a\x1b[1;31mb\x1b[0m"), "ab");
        // A CSI cut short by the next escape does not swallow that escape.
        assert_eq!(strip_ansi("a\x1b[31\x1b[0mb"), "ab");
    }

    #[test]
    fn non_csi_escapes_with_an_intermediate_are_dropped_whole() {
        assert_eq!(strip_ansi("x\x1b(By"), "xy");
        assert_eq!(strip_ansi("x\x1b=y\x1b>z"), "xyz");
        assert_eq!(strip_ansi("x\x1b]0;title\x07y\x1b]8;;\x1b\\z"), "xyz");
    }

    #[test]
    fn a_truncated_escape_at_the_end_does_not_panic() {
        assert_eq!(strip_ansi("tail\x1b"), "tail");
        assert_eq!(strip_ansi("tail\x1b["), "tail");
        assert_eq!(strip_ansi("tail\x1b]"), "tail");
        assert_eq!(strip_ansi("tail\x1b("), "tail");
    }

    /// Control strings other than OSC are consumed whole as well: a DCS, APC,
    /// PM or SOS payload is not screen text.
    #[test]
    fn control_string_payloads_are_not_screen_text() {
        assert_eq!(strip_ansi("a\x1bPq#0;2;0;0;0\x1b\\b"), "ab");
        assert_eq!(strip_ansi("a\x1b_Gf=100\x1b\\b"), "ab");
        assert_eq!(strip_ansi("a\x1b^privacy\x07b"), "ab");
        assert_eq!(strip_ansi("a\x1bXstart\x1b\\b"), "ab");
        assert_eq!(strip_ansi("tail\x1bP"), "tail");
        // An unterminated string ends at the next bare ESC, which then starts
        // its own sequence: the text after it is kept.
        assert_eq!(strip_ansi("a\x1b]8;;broken\x1b[31mb\x1b[0m"), "ab");
    }

    /// The conceal oracle reads `8` only as its own parameter: the palette
    /// index or channel value 8 inside an extended colour is colour.
    #[test]
    fn the_conceal_oracle_steps_over_extended_colour_arguments() {
        assert!(is_conceal_sgr(&[8]));
        assert!(is_conceal_sgr(&[1, 8]));
        assert!(is_conceal_sgr(&[38, 5, 8, 8]));
        assert!(is_conceal_sgr(&[48, 2, 8, 8, 8, 8]));
        assert!(!is_conceal_sgr(&[38, 5, 8]));
        assert!(!is_conceal_sgr(&[48, 5, 8, 1]));
        assert!(!is_conceal_sgr(&[58, 2, 8, 8, 8]));
        assert!(!is_conceal_sgr(&[0]));
        assert!(!is_conceal_sgr(&[28]));
    }

    // ---------------------------------------------------------------------
    // The clipped confirm modal, the hint on a narrow screen, the
    // editor-backed commands and the `!shell` arm under the UI, and the
    // `.info` capture under CPU load.
    // ---------------------------------------------------------------------

    /// A `Remove '<path>'?` question whose path alone spans 13 rows at 40
    /// columns (520 chars, broken at the width), so a 20-row screen cannot
    /// show it whole.
    fn clipped_question() -> String {
        let path: String = (0..13)
            .map(|i| format!("/component-{i:02}-{}", "x".repeat(24)))
            .collect();
        format!("q9d-clip Remove '{path}'? q9d-clip-end")
    }

    /// The recording confirm tool asking [`clipped_question`] instead.
    fn clipped_question_confirm_tool() -> String {
        RECORDING_CONFIRM_TOOL.replace("Really do it? q9d-q", &clipped_question())
    }

    /// Every token of the modal's hint once the question fits, whitespace
    /// squeezed out. A tool confirm with no default of its own starts on No,
    /// so Enter answers no.
    const HINT_TOKENS: [&str; 5] = [
        "y=yes",
        "n=no",
        "a=yesforthissession",
        "Enter=no",
        "Esc=cancel",
    ];

    /// Starts the UI at 40x20 with the clipped-question confirm tool, submits
    /// the prompt and returns once the modal's clip marker is on screen,
    /// handing back the printable bytes since the prompt was sent.
    fn open_clipped_modal(pty: &mut Pty) -> String {
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(20, 40).unwrap();
        pty.read_for(Duration::from_millis(500));
        let before = pty.raw().len();
        pty.send(b"call the confirm tool\r").unwrap();
        pty.wait_for_after("q9d-clip", before, PTY_DEADLINE)
            .expect("the tool's confirm question never reached the 40x20 screen");
        pty.wait_for_after("more lines", before, PTY_DEADLINE)
            .expect("the clipped question showed no `(+N more lines` marker");
        pty.read_for(Duration::from_millis(500));
        strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]))
    }

    /// A question taller than the screen is marked `(+N more lines; enlarge
    /// the terminal)` and `y` does nothing while it is clipped: the tool gets
    /// no answer, and the legend offers only Esc. Once the window is enlarged
    /// the whole question is visible, the full hint is back and `y` answers
    /// it.
    #[test]
    #[serial]
    fn a_clipped_confirm_question_refuses_y_until_the_terminal_is_enlarged() {
        let dir = fresh_config_dir("confirm-clipped-y");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("confirm-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_confirm_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture clipped done c7y".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_confirm_rec.sh",
            &clipped_question_confirm_tool(),
        );
        let mut pty = spawn_tui(&dir, &[]);

        let modal = open_clipped_modal(&mut pty);
        let shown = squeezed(&modal);
        assert!(
            shown.contains("(+") && shown.contains("morelines;enlargetheterminal)"),
            "no `(+N more lines; enlarge the terminal)` marker on the clipped modal:\n{modal}"
        );
        assert!(
            !shown.contains("q9d-clip-end"),
            "the question's tail was painted although the marker says it is clipped:\n{modal}"
        );
        assert!(
            shown.contains("Esc=cancelenlargetheterminaltoanswer"),
            "the clipped legend is missing from the 40-column modal:\n{modal}"
        );
        assert!(
            !shown.contains("y=yes"),
            "the clipped legend still offers an approving key:\n{modal}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        let after_y = pty.read_for(Duration::from_secs(2));
        assert!(
            !out.exists(),
            "`y` answered the confirm while its question was clipped (the tool recorded {:?}):\n{after_y}",
            fs::read_to_string(&out).unwrap_or_default()
        );
        assert_eq!(
            fake.served(),
            1,
            "the turn moved on while the modal was clipped:\n{after_y}"
        );

        let grown_at = pty.raw().len();
        pty.resize(50, 100).unwrap();
        let grown = pty
            .wait_for_after("q9d-clip-end", grown_at, PTY_DEADLINE)
            .expect("enlarging the terminal did not reveal the rest of the question");
        assert!(
            !squeezed(&grown).contains("morelines;enlarge"),
            "the clip marker is still shown after the whole question fits:\n{grown}"
        );
        // `read_for` hands back the whole screen so far; the approving keys
        // were never offered before the question fit.
        let grown = squeezed(&pty.read_for(HOTKEY_GAP));
        for token in HINT_TOKENS {
            assert!(
                grown.contains(token),
                "the hint token {token:?} did not come back once the question fit:\n{grown}"
            );
        }
        pty.send(b"y").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer after enlarging");
        let transcript = wait_for_squeezed(
            &mut pty,
            "fixture clipped done c7y",
            "the reply after the tool",
        );
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "1",
            "`y` on the fully visible modal did not reach the tool as a yes:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// Esc still cancels a clipped confirm: the tool reads no yes, the turn
    /// finishes with the model's follow-up and the UI keeps taking keys.
    #[test]
    #[serial]
    fn esc_still_cancels_a_clipped_confirm_question() {
        let dir = fresh_config_dir("confirm-clipped-esc");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("confirm-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_confirm_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture clipped esc c7e".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_confirm_rec.sh",
            &clipped_question_confirm_tool(),
        );
        let mut pty = spawn_tui(&dir, &[]);

        let _modal = open_clipped_modal(&mut pty);
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer to Esc");
        let transcript =
            wait_for_squeezed(&mut pty, "fixture clipped esc c7e", "the reply after Esc");
        let answer = fs::read_to_string(&out).unwrap();
        assert_ne!(
            answer.trim(),
            "1",
            "Esc on the clipped modal reached the tool as a yes:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        pty.send(b".info\r").unwrap();
        pty.wait_for("log_path", PTY_DEADLINE)
            .expect("keys were not handled after Esc on the clipped modal");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// `.role <new-name>` asks `Create a new role?` in a modal; `y` opens the
    /// new role's file in `$EDITOR` on the lent terminal, and the UI comes
    /// back taking keys.
    #[test]
    #[serial]
    fn dot_role_for_a_new_role_asks_in_the_modal_and_opens_the_editor_on_y() {
        let dir = fresh_config_dir("role-modal");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let launched = dir.join("editor-launched");
        let editor = recording_editor(&dir, &launched);
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let before = pty.raw().len();
        pty.send(b".role new-role-xk3\r").unwrap();
        pty.wait_for_after("Create a new role?", before, PTY_DEADLINE)
            .expect("`.role <new>` did not ask `Create a new role?` in a modal");
        assert!(
            !launched.exists(),
            "the editor opened before the question was answered"
        );
        pty.read_for(HOTKEY_GAP);
        let answered = pty.raw().len();
        pty.send(b"y").unwrap();
        let left = pty
            .wait_for_raw(LEAVE_ALT, answered, PTY_DEADLINE)
            .expect("`y` did not release the terminal for the role editor");
        let re_entered = wait_for_re_entry(&mut pty, left);
        assert_repainted_after(&mut pty, re_entered, "the `.role` editor");
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        let edited = fs::read_to_string(&launched)
            .unwrap_or_else(|e| panic!("`y` never launched the editor ({e}):\n{since}"));
        assert!(
            edited
                .lines()
                .next()
                .is_some_and(|path| path.contains("new-role-xk3")),
            "the editor was not handed the new role's file: {edited:?}\n{since}"
        );
        assert!(
            edited.lines().nth(1) == Some("missing"),
            "the new role's file existed before its editor opened: {edited:?}\n{since}"
        );
        let role_files = new_files_named(&dir, &[], "new-role-xk3");
        assert!(
            role_files.iter().any(|path| path.is_file()),
            "`y` opened the editor but wrote no role file (new files: {role_files:?}):\n{since}"
        );

        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", re_entered, PTY_DEADLINE)
            .expect("keys were not handled after the role editor returned");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// `.edit session` on a live named session writes it to disk and opens
    /// its yaml in `$EDITOR` on the lent terminal; the UI comes back taking
    /// keys.
    #[test]
    #[serial]
    fn dot_edit_session_opens_the_editor_on_the_lent_terminal() {
        let dir = fresh_config_dir("edit-session-editor");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let launched = dir.join("editor-launched");
        let editor = recording_editor(&dir, &launched);
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b".session named-sess-xk3\r").unwrap();
        pty.wait_for("named-sess-xk3", PTY_DEADLINE)
            .expect("the named session never showed in the footer");
        pty.read_for(Duration::from_millis(500));
        let files_before = walk(&dir);
        let before = pty.raw().len();
        pty.send(b".edit session\r").unwrap();
        let left = pty
            .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
            .expect("`.edit session` did not release the terminal for the editor");
        let re_entered = wait_for_re_entry(&mut pty, left);
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));

        let edited = fs::read_to_string(&launched).unwrap_or_else(|e| {
            panic!("`.edit session` never launched the editor ({e}):\n{since}")
        });
        let opened = edited.lines().next().unwrap_or_default();
        assert!(
            opened.contains("named-sess-xk3"),
            "the editor was not handed the session file: {edited:?}\n{since}"
        );
        assert!(
            edited.lines().nth(1) == Some("existed"),
            "the session file {opened:?} was not written before the editor opened: {edited:?}\n{since}"
        );
        let session_files = new_files_named(&dir, &files_before, "named-sess-xk3");
        assert!(
            !session_files.is_empty(),
            "no named-sess-xk3 session file appeared under {dir:?}:\n{since}"
        );

        pty.wait_for_after("Type a prompt", re_entered, PTY_DEADLINE)
            .expect("the composer was not repainted after the session editor returned");
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", re_entered, PTY_DEADLINE)
            .expect("keys were not handled after the session editor returned");
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// A `!shell` line is a command, not a message for the model: it runs on
    /// the lent terminal (alternate screen left, the child's output on the
    /// normal screen, alternate screen re-entered) and the fake model is
    /// never asked.
    #[test]
    #[serial]
    fn a_bang_shell_line_runs_on_the_lent_terminal_and_never_reaches_the_model() {
        let dir = fresh_config_dir("bang-shell");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text(
            "fixture must not be served b4s".to_string(),
        )]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        let before = pty.raw().len();
        pty.send(b"!echo bang-probe-q9d\r").unwrap();
        let left = pty
            .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
            .expect("`!echo` did not release the terminal for the shell");
        let re_entered = wait_for_re_entry(&mut pty, left);
        // The typed line echoes in the composer before the send, so only the
        // window between the leave and the re-entry can hold the child's own
        // output.
        let on_the_normal_screen =
            strip_ansi(&String::from_utf8_lossy(&pty.raw()[left..re_entered]));
        assert!(
            on_the_normal_screen.contains("bang-probe-q9d"),
            "the shell's output did not land on the normal screen:\n{on_the_normal_screen}"
        );
        pty.read_for(Duration::from_millis(800));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        assert_eq!(
            fake.served(),
            0,
            "`!echo` was sent to the model as a prompt:\n{since}"
        );
        assert!(
            !since.contains("b4s"),
            "the model's reply landed for a `!shell` line:\n{since}"
        );

        wait_until_idle(&mut pty, "the turn of the `!shell` line");
        pty.send(b".info\r").unwrap();
        wait_for_painted_after(
            &mut pty,
            "log_path",
            re_entered,
            "keys were not handled after the shell returned",
        );
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(fake.served(), 0, "{transcript}");
    }

    /// Busy loops on every core for the life of the guard.
    struct CpuLoad(Vec<std::process::Child>);

    impl CpuLoad {
        fn start() -> CpuLoad {
            let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
            let children = (0..cores * 2)
                .filter_map(|_| {
                    Command::new("sh")
                        .arg("-c")
                        // Counted, so a loop that outlives its test (a killed
                        // harness) still burns out on its own within a minute.
                        .arg("i=0; while [ $i -lt 40000000 ]; do i=$((i+1)); done")
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .ok()
                })
                .collect();
            CpuLoad(children)
        }
    }

    impl Drop for CpuLoad {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Under CPU load the `.info` table still lands in the Command cell
    /// directly under `> .info`: the capture waits for its drain thread
    /// rather than calling the pipe quiet early and routing the table into a
    /// later notice a row lower. Three cold starts, each on a fresh screen so
    /// the row placement is unambiguous.
    #[test]
    #[serial]
    fn dot_info_lands_in_its_command_cell_under_cpu_load() {
        let load = CpuLoad::start();
        for round in 0..3 {
            let dir = fresh_config_dir(&format!("info-under-load-{round}"));
            let _guard = TempDirGuard(dir.clone());
            write_dry_run_config(&dir);
            let mut pty = spawn_tui(&dir, &[]);

            pty.wait_for("no session", PTY_DEADLINE).unwrap();
            pty.resize(80, 100).unwrap();
            pty.read_for(Duration::from_millis(300));
            let info_at = pty.raw().len();
            pty.send(b".info\r").unwrap();
            pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
                .unwrap_or_else(|e| {
                    panic!("round {round}: the `.info` table never appeared: {e:?}")
                });
            pty.read_for(Duration::from_millis(800));
            let printable = strip_ansi(&String::from_utf8_lossy(&pty.raw()[info_at..]));
            let header_rows = painted_at(pty.raw(), b"> .info", info_at);
            let &(header_row, 1) = header_rows.first().unwrap_or_else(|| {
                panic!("round {round}: `> .info` was never painted at column one:\n{printable}")
            }) else {
                panic!("round {round}: `> .info` was painted mid-row: {header_rows:?}");
            };
            let first_rows = painted_at(pty.raw(), b"model", info_at);
            assert!(
                first_rows.contains(&(header_row + 1, 1)),
                "round {round}: under load the table's first row was not painted directly under \
             `> .info` (row {header_row}); `model` was painted at {first_rows:?}:\n{printable}"
            );

            pty.send(b"\x04").unwrap();
            let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
            assert_eq!(exit_code(&transcript), 0, "round {round}: {transcript}");
            assert_tty_restored(&transcript);
        }
        drop(load);
    }
}
