//! Real-process checks of the transcript pager and the thread roster:
//! threads, the transcript, footer counts, the help popup and completion,
//! popup colours, scrolling and cell boundaries. Every module drives the
//! binary through the shared pty harness in `common`, so the file is
//! unix-only.
#![cfg(unix)]

mod common;

mod transcript {
    //! Transcript cases for the full-screen UI: what the real binary shows a
    //! first-time user in the scrollback, exercised on a pseudo-terminal from
    //! a cold start (fresh config and cache dirs, a dry-run model, or the
    //! scripted endpoint when a turn has to be recorded). Unix-only for the
    //! same reasons as the lifecycle suite; cases run one at a time because
    //! each takes a controlling tty.
    //!
    //! Covered here and nowhere else in `tests/`:
    //! - the welcome block leaves for the first reply, not for typing, and
    //!   names the model;
    //! - the welcome block carries no colour under `NO_COLOR`;
    //! - `.undo` removes the last exchange's cells back to the previous user
    //!   cell (against a recording session: a dry-run turn is never saved);
    //! - `.undo` with nothing to undo, a terminal shrunk to a few cells, and
    //!   an emoji/VS16 prompt leave the UI alive and quitting cleanly;
    //! - undoing the only exchange stops at the command cell before it and
    //!   leaves that cell in place;
    //! - the "Thinking for Ns" status row is live while a reasoning delta is
    //!   held;
    //! - `Ctrl+X` shows and hides the Todo panel;
    //! - scrolling up raises the follow chip, `End` and a submitted line
    //!   clear it;
    //! - a tool call is one cell that finishes in place (its start and end
    //!   events pair up), never a running cell plus a finished one;
    //! - a finished tool row reads `name(args)  elapsed  ok`, folds its output
    //!   away, and opens/closes on `Enter` once `Ctrl+Up` focused it; `Esc`
    //!   drops the focus and a draft makes `Enter` submit instead;
    //! - a failed tool row's header ends in `failed: <first line>` of the
    //!   error; the opened row carries the rest;
    //! - cells that land while the view is scrolled up count on the follow
    //!   chip and `End` brings them into view;
    //! - `todo__*` calls never take a transcript row; the Todo panel shows them;
    //! - Shift+Up and a mouse wheel notch detach the view like PgUp does; PgDn
    //!   walks back to the bottom, and Shift+G jumps there only while the
    //!   composer is empty;
    //! - a streamed `<think>` span never shows in the assistant cell, during
    //!   the stream or after it;
    //! - a folded header cuts a long argument to its budget;
    //! - a bookkeeping call (`job__list`) takes one line with no status;
    //! - a mouse click on a folded tool row opens it and a second click folds
    //!   it again;
    //! - a narrower terminal re-wraps the cells in place without duplicating
    //!   a row;
    //! - Shift+G with the view at the bottom is plain typing, before a scroll
    //!   and right after a jump;
    //! - a think span whose `</think>` arrives late (after a hold, or only at
    //!   the end of a reasoning-only reply) never shows;
    //! - Esc over a focused row mid-turn interrupts the turn;
    //! - Ctrl+X, PgUp and End keep working under a tool's Confirm modal and the
    //!   Input modal, and leave the question up;
    //! - a folded tool header at forty columns is one row ending in `ok`.
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use super::common::fake_llm::{Chunk, FakeLlm, HOLD_LIMIT, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
    use serial_test::serial;

    const PAGE_UP: &[u8] = b"\x1b[5~";
    const PAGE_DOWN: &[u8] = b"\x1b[6~";
    const END: &[u8] = b"\x1b[F";
    const SHIFT_UP: &[u8] = b"\x1b[1;2A";
    const CTRL_UP: &[u8] = b"\x1b[1;5A";
    const CTRL_L: &[u8] = b"\x0c";
    /// One SGR-encoded wheel-up notch at column 50, row 5.
    const WHEEL_UP: &[u8] = b"\x1b[<64;50;5M";

    const CHIP: &str = "End=follow";

    /// Ctrl+L clears the screen and repaints everything, so the bytes after it
    /// are the whole visible frame as it stands. Returns that frame squeezed.
    fn ctrl_l_repaint(pty: &mut Pty) -> String {
        let at = pty.raw().len();
        pty.send(CTRL_L).unwrap();
        pty.read_for(Duration::from_secs(1));
        squeezed(&strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..])))
    }

    /// Ctrl+L, then the frame split into screen rows by following the
    /// cursor-position sequences the repaint carries. Rows are 1-based as a
    /// terminal reports them; each is the squeezed text written on it.
    fn repaint_rows(pty: &mut Pty) -> Vec<(u16, String)> {
        let at = pty.raw().len();
        pty.send(CTRL_L).unwrap();
        pty.read_for(Duration::from_secs(1));
        let text = String::from_utf8_lossy(&pty.raw()[at..]).into_owned();
        let chars: Vec<char> = text.chars().collect();
        let mut rows: std::collections::BTreeMap<u16, String> = Default::default();
        let mut row: u16 = 1;
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != '\x1b' {
                if !chars[i].is_whitespace() && !chars[i].is_control() {
                    rows.entry(row).or_default().push(chars[i]);
                }
                i += 1;
                continue;
            }
            match chars.get(i + 1) {
                Some('[') => {
                    let start = i + 2;
                    let mut j = start;
                    while matches!(chars.get(j), Some(c) if ('\x30'..='\x3f').contains(c)) {
                        j += 1;
                    }
                    while matches!(chars.get(j), Some(c) if ('\x20'..='\x2f').contains(c)) {
                        j += 1;
                    }
                    let fin = chars.get(j).copied();
                    if matches!(fin, Some('H') | Some('f')) {
                        let params: String = chars[start..j].iter().collect();
                        row = params
                            .split(';')
                            .next()
                            .and_then(|r| r.parse().ok())
                            .unwrap_or(1)
                            .max(1);
                    }
                    i = if fin.is_some() { j + 1 } else { j };
                }
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    i += 2;
                    while i < chars.len() && chars[i] != '\x07' && chars[i] != '\x1b' {
                        i += 1;
                    }
                    i += 1;
                }
                Some(_) => i += 2,
                None => i += 1,
            }
        }
        rows.into_iter().collect()
    }

    fn boot(scenario: &str, extra_env: &[(&str, &str)]) -> (TempDirGuard, Pty) {
        let dir = fresh_config_dir(scenario);
        let guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, extra_env);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        (guard, pty)
    }

    fn boot_with_fake(scenario: &str, fake: &FakeLlm) -> (TempDirGuard, Pty) {
        let dir = fresh_config_dir(scenario);
        let guard = TempDirGuard(dir.clone());
        write_fake_llm_config(&dir, fake);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        (guard, pty)
    }

    /// Ctrl+D on the empty composer; the UI has to leave cleanly and hand the
    /// terminal back as it found it.
    fn quit(mut pty: Pty) {
        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_D).unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// The first SGR sequence in `raw` that sets a foreground or background
    /// colour (basic, bright, 256 or truecolour). Bold, dim, italic,
    /// underline and reset are not colours and stay allowed.
    fn has_colour_sgr(raw: &[u8]) -> Option<String> {
        let text = String::from_utf8_lossy(raw);
        let mut rest: &str = &text;
        while let Some(at) = rest.find("\x1b[") {
            let after = &rest[at + 2..];
            let end = after
                .find(|c: char| ('\x40'..='\x7e').contains(&c))
                .unwrap_or(after.len());
            let (params, fin) = (&after[..end], after.get(end..end + 1).unwrap_or(""));
            if fin == "m" {
                for p in params.split(';') {
                    if let Ok(n) = p.parse::<u16>()
                        && matches!(n, 30..=38 | 40..=48 | 90..=97 | 100..=107)
                    {
                        return Some(format!("\\x1b[{params}m"));
                    }
                }
            }
            rest = &after[end.min(after.len())..];
        }
        None
    }

    // ---------------------------------------------------------------------
    // The welcome block fills the empty transcript on boot and leaves for
    // good once the first reply is in. The dry-run echo renders the prompt's
    // markdown, so the asterisk-free form of the prompt can only come from
    // the assistant cell, never from the composer echo or the user cell.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn the_welcome_block_leaves_after_the_first_reply() {
        let dir = fresh_config_dir("welcome");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);

        let booted = pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        let booted = squeezed(&strip_ansi(&booted));
        assert!(
            booted.contains(&squeezed("Tip: Press Ctrl-C")),
            "the welcome block did not show the interrupt hint on boot:\n{booted}"
        );
        assert!(
            booted.contains(&squeezed(concat!("coyote ", env!("CARGO_PKG_VERSION")))),
            "the welcome block did not show the wordmark and version on boot:\n{booted}"
        );

        let before = pty.raw().len();
        pty.send(b"**bye** welcome-marker\r").unwrap();
        wait_for_squeezed_after(&mut pty, "bye welcome-marker", before, "the first reply");
        pty.read_for(Duration::from_millis(500));

        let repainted = ctrl_l_repaint(&mut pty);
        assert!(
            repainted.contains(&squeezed("bye welcome-marker")),
            "the repaint after the first reply does not show the reply:\n{repainted}"
        );
        assert!(
            !repainted.contains(&squeezed("Tip: Press Ctrl-C")),
            "the welcome block is still on screen after the first reply:\n{repainted}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // The welcome block carries the same facts the REPL banner prints (name
    // and version, model, interrupt hint, .help pointer) and typing alone
    // does not dismiss it.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn the_welcome_block_names_the_model_and_survives_typing() {
        let (_guard, mut pty) = boot("welcome-typing", &[]);

        let frame = ctrl_l_repaint(&mut pty);
        for fact in [
            concat!("coyote ", env!("CARGO_PKG_VERSION")),
            "model: dryrun:dry-model",
            "Tip: Press Ctrl-C",
            "\".help\"",
        ] {
            assert!(
                frame.contains(&squeezed(fact)),
                "the welcome block lacks {fact:?} on boot:\n{frame}"
            );
        }

        pty.send(b"typed-but-not-sent").unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains(&squeezed("typed-but-not-sent")),
            "the composer did not echo the typed text:\n{frame}"
        );
        assert!(
            frame.contains(&squeezed("Tip: Press Ctrl-C")),
            "typing (without Enter) dismissed the welcome block:\n{frame}"
        );

        let before = pty.raw().len();
        pty.send(b"\r").unwrap();
        wait_for_squeezed_after(&mut pty, "typed-but-not-sent", before, "the first prompt");
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(&squeezed("Tip: Press Ctrl-C")),
            "the welcome block is still on screen after the first submitted line:\n{frame}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // Under NO_COLOR the welcome frame carries no colour SGR.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn the_welcome_block_has_no_colour_under_no_color() {
        let (_guard, mut pty) = boot("welcome-no-color", &[("NO_COLOR", "1")]);

        let at = pty.raw().len();
        pty.send(CTRL_L).unwrap();
        pty.read_for(Duration::from_secs(1));
        let raw = &pty.raw()[at..];
        let text = strip_ansi(&String::from_utf8_lossy(raw));
        assert!(
            squeezed(&text).contains(&squeezed("Tip: Press Ctrl-C")),
            "the welcome block is missing under NO_COLOR:\n{text}"
        );
        if let Some(seq) = has_colour_sgr(raw) {
            panic!("NO_COLOR is set but the frame carries a colour SGR {seq}:\n{text}");
        }

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // Inside a session, `.undo` takes the second exchange (its `> ` user cell
    // and its reply) out of the transcript and leaves the first. The undone
    // prompt comes back as composer prefill, the REPL's long-standing `.undo`
    // semantics, so only the `> `-prefixed cell is asserted gone. A dry-run
    // turn is never saved to the session (`record_chat_completion` skips
    // it), so the scripted endpoint answers.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn undo_removes_the_last_exchange_and_keeps_the_one_before() {
        let fake = FakeLlm::start(vec![
            Reply::Text("reply-alpha-xx".to_string()),
            Reply::Text("reply-bravo-yy".to_string()),
        ]);
        let (_guard, mut pty) = boot_with_fake("undo", &fake);

        pty.send(b".session scratch\r").unwrap();
        pty.read_for(Duration::from_secs(1));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains(".scratch."),
            "the status row does not name the scratch session:\n{frame}"
        );

        let before = pty.raw().len();
        pty.send(b"alphamarker one\r").unwrap();
        wait_for_squeezed_after(&mut pty, "reply-alpha-xx", before, "the first reply");
        pty.read_for(Duration::from_millis(500));

        let before = pty.raw().len();
        pty.send(b"bravomarker two\r").unwrap();
        wait_for_squeezed_after(&mut pty, "reply-bravo-yy", before, "the second reply");
        pty.read_for(Duration::from_millis(500));

        let frame = ctrl_l_repaint(&mut pty);
        for cell in [
            ">alphamarkerone",
            "reply-alpha-xx",
            ">bravomarkertwo",
            "reply-bravo-yy",
        ] {
            assert!(
                frame.contains(cell),
                "{cell} should be on screen before .undo:\n{frame}"
            );
        }

        pty.send(b".undo\r").unwrap();
        pty.read_for(Duration::from_secs(1));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(">bravomarkertwo"),
            ".undo left the second exchange's user cell on screen:\n{frame}"
        );
        assert!(
            !frame.contains("reply-bravo-yy"),
            ".undo left the second exchange's reply cell on screen:\n{frame}"
        );
        assert!(
            frame.contains(">alphamarkerone") && frame.contains("reply-alpha-xx"),
            ".undo removed more than the last exchange:\n{frame}"
        );
        assert!(
            frame.contains("bravomarkertwo"),
            ".undo did not hand the undone prompt back to the composer:\n{frame}"
        );

        // Ctrl+C clears the undone prompt out of the composer; Ctrl+D would
        // not quit over it.
        pty.send(CTRL_C).unwrap();
        pty.read_for(Duration::from_millis(300));
        quit(pty);
    }

    // =====================================================================
    // The scroll paths the page keys do not cover, the think span of a
    // streamed reply, the folded header's argument budget, a bookkeeping
    // call's single line, a mouse click on a folded row, and a resize that
    // must re-wrap without duplicating rows.
    // =====================================================================

    // ---------------------------------------------------------------------
    // Shift+Up detaches the view like PgUp does and raises the chip; enough
    // PgDn presses walk back to the bottom and clear it without End.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn shift_up_raises_the_chip_and_page_down_walks_back_to_the_bottom() {
        let (_guard, mut pty) = boot("shift-up", &[]);
        overflow_a_short_terminal(&mut pty);

        pty.read_for(HOTKEY_GAP);
        let before = pty.raw().len();
        pty.send(SHIFT_UP).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "End = follow",
            before,
            "the follow chip after Shift+Up",
        );

        let mut frame = String::new();
        for _ in 0..6 {
            pty.send(PAGE_DOWN).unwrap();
            pty.read_for(Duration::from_millis(400));
            frame = ctrl_l_repaint(&mut pty);
            if !frame.contains(CHIP) {
                break;
            }
        }
        assert!(
            !frame.contains(CHIP),
            "six PgDn presses did not bring the view back to the bottom:\n{frame}"
        );
        assert!(
            frame.contains("fillerline05"),
            "the bottom of the transcript is not in view after PgDn:\n{frame}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // A mouse wheel notch upward scrolls the view and raises the chip; End
    // clears it again.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn the_mouse_wheel_raises_the_chip_and_end_clears_it() {
        let (_guard, mut pty) = boot("wheel", &[]);
        overflow_a_short_terminal(&mut pty);

        pty.read_for(HOTKEY_GAP);
        let before = pty.raw().len();
        pty.send(WHEEL_UP).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "End = follow",
            before,
            "the follow chip after a wheel-up notch",
        );

        pty.send(END).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(CHIP),
            "End did not clear the follow chip after a wheel scroll:\n{frame}"
        );
        assert!(
            frame.contains("fillerline05"),
            "End did not bring the newest output back into view:\n{frame}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // Shift+G is the keyboard's other way back to the bottom, but only from
    // an empty composer: with a draft the letter is typed and the view stays
    // where it is; once the draft is gone the same key clears the chip.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn shift_g_follows_the_bottom_only_from_an_empty_composer() {
        let (_guard, mut pty) = boot("shift-g", &[]);
        overflow_a_short_terminal(&mut pty);

        let before = pty.raw().len();
        pty.send(PAGE_UP).unwrap();
        wait_for_squeezed_after(&mut pty, "End = follow", before, "the chip after PgUp");

        pty.send(b"draftG").unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains(CHIP),
            "G typed into a draft moved the view:\n{frame}"
        );
        assert!(
            frame.contains("draftG"),
            "G typed into a draft did not reach the composer:\n{frame}"
        );

        pty.send(CTRL_C).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"G").unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(CHIP),
            "Shift+G on the empty composer did not follow the bottom (the chip is still up):\n{frame}"
        );
        assert!(
            frame.contains("fillerline05"),
            "Shift+G did not bring the newest output back into view:\n{frame}"
        );
        assert!(
            !frame.contains("draftG"),
            "the cleared draft is still on screen:\n{frame}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // A reasoning model streams its thinking as a `<think>` span ahead of
    // the answer. The assistant cell shows the answer alone: no tag and no
    // word of the reasoning, during the stream or once the reply is done.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn a_streamed_think_span_never_shows_in_the_assistant_cell() {
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Reasoning("weighing the options v4w".to_string()),
            Chunk::Text("the answer n7p".to_string()),
        ])]);
        let (_guard, mut pty) = boot_with_fake("think-span", &fake);

        let before = pty.raw().len();
        pty.send(b"think first\r").unwrap();
        let streamed = wait_for_squeezed_after(
            &mut pty,
            "the answer n7p",
            before,
            "the answer after the think span",
        );
        assert!(
            !streamed.contains("<think>") && !streamed.contains("</think>"),
            "a think tag was painted while the reply streamed:\n{streamed}"
        );
        assert!(
            !squeezed(&streamed).contains(&squeezed("weighing the options")),
            "the reasoning was painted while the reply streamed:\n{streamed}"
        );

        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains(&squeezed("the answer n7p")),
            "the answer is not on screen:\n{frame}"
        );
        assert!(
            !frame.contains("<think>") && !frame.contains("</think>"),
            "a think tag is on screen after the reply finished:\n{frame}"
        );
        assert!(
            !frame.contains(&squeezed("weighing the options")),
            "the reasoning is on screen after the reply finished:\n{frame}"
        );

        quit(pty);
    }

    /// A tool that takes a free-text note besides its marker and echoes a
    /// fixed line, so the folded header has long arguments to cut.
    const NOTE_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Take a note.\n\
    # @option --note! The note text.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   echo \"note taken t6y\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    // ---------------------------------------------------------------------
    // The folded header carries `name(short args)`: a long argument is cut
    // to the header's budget, never spilled whole across the row, and the
    // row stays one folded line that ends in its status.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn long_tool_args_are_cut_short_in_the_folded_header() {
        let dir = fresh_config_dir("short-args");
        let _guard = TempDirGuard(dir.clone());
        let note: String = (0..40).map(|i| format!("seg{i:02}xx")).collect();
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_note".to_string(),
                arguments: format!("{{\"note\": \"{note}\"}}"),
            },
            Reply::Text("note reply h3d".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_note.sh", NOTE_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b"take a long note\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "note reply h3d",
            0,
            "the reply after the note tool",
        );
        pty.read_for(Duration::from_millis(500));

        let rows = repaint_rows(&mut pty);
        let header = folded_header_row(&rows, "fixture_note");
        assert!(
            header.contains("seg00xx"),
            "the folded header shows none of the arguments: {header:?}"
        );
        assert!(
            !header.contains("seg39xx"),
            "the folded header spilled the whole 320-character argument: {header:?}"
        );
        assert_header_is_name_args_elapsed(
            header_before_status(header, "fixture_note", "ok"),
            "fixture_note",
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // A bookkeeping call (`job__list` here; `memory__*`, `skill__*` and
    // `agent__list_*` are the same class) takes one dim line: the name is on
    // screen once, with neither a running nor a failed status attached.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn a_bookkeeping_call_takes_one_line() {
        let dir = fresh_config_dir("bookkeeping");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "job__list".to_string(),
                arguments: "{}".to_string(),
            },
            Reply::Text("listing reply b8k".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b"list the jobs\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "listing reply b8k",
            0,
            "the reply after the bookkeeping call",
        );
        pty.read_for(Duration::from_millis(500));

        let frame = ctrl_l_repaint(&mut pty);
        let names = frame.matches("job__list").count();
        assert_eq!(
            names, 1,
            "a bookkeeping call should take exactly one line naming it (saw {names}):\n{frame}"
        );
        assert!(
            !frame.contains("failed"),
            "the bookkeeping line reads as a failure:\n{frame}"
        );
        assert_eq!(fake.served(), 2, "{frame}");

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // A mouse click on a folded tool row focuses and opens it. The row's
    // screen position is not known from outside, so the click lands on the
    // row the repaint put the header on; a second click folds it again.
    // ---------------------------------------------------------------------

    const ECHO_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Say something.\n\
    # @option --word! The word.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   echo \"clicked output c4v\" >&2\n\
    \x20   echo \"clicked output c4v\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    #[test]
    #[serial]
    fn a_mouse_click_on_the_folded_tool_row_opens_it() {
        let dir = fresh_config_dir("click");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_echo".to_string(),
                arguments: "{\"word\": \"hi\"}".to_string(),
            },
            Reply::Text("echo reply m9s".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_echo.sh", ECHO_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b"say hi\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "echo reply m9s",
            0,
            "the reply after the echo tool",
        );
        pty.read_for(Duration::from_millis(500));

        let output = squeezed("clicked output c4v");
        let folded = ctrl_l_repaint(&mut pty);
        assert!(
            folded.contains("fixture_echo("),
            "no folded header names the echo tool:\n{folded}"
        );
        assert!(
            !folded.contains(&output),
            "the tool row is already open before any click:\n{folded}"
        );
        let raw = String::from_utf8_lossy(pty.raw()).into_owned();
        assert!(
            raw.contains("\x1b[?1000h") || raw.contains("\x1b[?1002h"),
            "the UI never asked the terminal for mouse reports"
        );
        assert!(
            raw.contains("\x1b[?1006h"),
            "the UI never asked the terminal for SGR-encoded mouse reports"
        );

        let rows = repaint_rows(&mut pty);
        let header_row = rows
            .iter()
            .find(|(_, text)| text.contains("fixture_echo("))
            .map(|(row, _)| *row)
            .unwrap_or_else(|| panic!("no screen row carries the tool header: {rows:?}"));

        // A left click (press and release) on column 4 of the header row.
        // An escape sequence counts as a hotkey only once the keyboard has
        // been idle; the repaint before the click was a keystroke.
        let click = format!("\x1b[<0;4;{header_row}M\x1b[<0;4;{header_row}m");
        pty.read_for(HOTKEY_GAP);
        pty.send(click.as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(400));
        let opened = ctrl_l_repaint(&mut pty);
        assert!(
            opened.contains(&output),
            "a left click on the tool header's screen row {header_row} did not open the folded \
         tool row; rows were {rows:?}; frame:\n{opened}"
        );
        assert!(
            opened.contains("fixture_echo("),
            "the opened row lost its header:\n{opened}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(click.as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(400));
        let refolded = ctrl_l_repaint(&mut pty);
        assert!(
            !refolded.contains(&output),
            "a second click on row {header_row} did not fold the row again:\n{refolded}"
        );

        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(300));
        quit(pty);
    }

    // ---------------------------------------------------------------------
    // Mouse capture is always on and asks for drags too, so the UI can
    // select text itself; Alt+M no longer switches it off.
    // ---------------------------------------------------------------------

    const ALT_M: &[u8] = b"\x1bm";

    #[test]
    #[serial]
    fn mouse_capture_stays_on_and_reports_drags() {
        let (_guard, mut pty) = boot("mouse-toggle", &[]);
        let raw = String::from_utf8_lossy(pty.raw()).into_owned();
        for mode in ["\x1b[?1000h", "\x1b[?1002h", "\x1b[?1006h"] {
            assert!(raw.contains(mode), "the UI never asked for {mode:?}");
        }
        assert!(
            !raw.contains("\x1b[?1003h"),
            "the UI turned on any-motion tracking"
        );
        let booted = ctrl_l_repaint(&mut pty);
        assert!(
            !booted.contains("mouse"),
            "the footer still names a mouse mode:\n{booted}"
        );

        let at = pty.raw().len();
        pty.read_for(HOTKEY_GAP);
        pty.send(ALT_M).unwrap();
        pty.read_for(Duration::from_millis(400));
        let after = String::from_utf8_lossy(&pty.raw()[at..]).into_owned();
        assert!(
            !after.contains("\x1b[?1000l"),
            "Alt+M switched the mouse reports off: {after:?}"
        );
        // Whatever the unbound chord left in the composer goes, so Ctrl+D
        // quits rather than deleting.
        pty.send(b"\x15").unwrap();

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // A narrower terminal re-wraps the cells in place: every marker is on
    // screen exactly as often as before (user cell plus dry-run echo), never
    // a duplicated row, and growing back restores the original layout.
    // The terminal is tall enough that the whole transcript stays in view
    // at every width (the dry-run echo repeats the prompt plus a preamble).
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn a_resize_rewraps_without_duplicating_rows() {
        let (_guard, mut pty) = boot("rewrap", &[]);
        pty.resize(70, 100).unwrap();
        pty.read_for(Duration::from_millis(500));

        let filler = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor";
        for n in 1..=2 {
            let before = pty.raw().len();
            let marker = format!("rewrapmark{n:02}");
            pty.send(format!("{marker} {filler} {filler}\r").as_bytes())
                .unwrap();
            wait_for_squeezed_after(&mut pty, &marker, before, "a long prompt");
            pty.read_for(Duration::from_millis(400));
        }
        pty.read_for(Duration::from_millis(500));

        let wide = ctrl_l_repaint(&mut pty);
        let counts: Vec<usize> = (1..=2)
            .map(|n| wide.matches(&format!("rewrapmark{n:02}")).count())
            .collect();
        assert!(
            counts.iter().all(|&c| c == 2),
            "each exchange should show its marker twice (user cell and echo) at 100 columns, saw {counts:?}:\n{wide}"
        );

        for (rows, cols) in [(70u16, 50u16), (70, 100), (70, 64), (70, 100)] {
            pty.resize(rows, cols).unwrap();
            pty.read_for(Duration::from_millis(700));
            let frame = ctrl_l_repaint(&mut pty);
            for n in 1..=2 {
                let marker = format!("rewrapmark{n:02}");
                let seen = frame.matches(&marker).count();
                assert_eq!(
                    seen, 2,
                    "{marker} shows {seen} times after a resize to {rows}x{cols} (expected 2):\n{frame}"
                );
            }
            assert!(
                !frame.contains(CHIP),
                "a resize detached the view from the bottom:\n{frame}"
            );
        }

        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_UP).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(300));
        quit(pty);
    }

    // =====================================================================
    // Collapsed rows and the focus cursor: what a finished tool looks like
    // folded, how it opens, and what wins when a draft is in the composer.
    // =====================================================================

    /// The squeezed text strictly between the first `start` and the first
    /// `end` after it.
    fn between<'a>(frame: &'a str, start: &str, end: &str) -> Option<&'a str> {
        let from = frame.find(start)? + start.len();
        let to = frame[from..].find(end)? + from;
        Some(&frame[from..to])
    }

    /// The one squeezed screen row carrying `name(`: the tool's folded
    /// header.
    fn folded_header_row<'a>(rows: &'a [(u16, String)], name: &str) -> &'a str {
        let needle = format!("{name}(");
        let mut headers = rows.iter().filter(|(_, text)| text.contains(&needle));
        let (_, header) = headers
            .next()
            .unwrap_or_else(|| panic!("no screen row carries the {name} header: {rows:?}"));
        assert!(
            headers.next().is_none(),
            "the {name} header is on more than one row: {rows:?}"
        );
        header
    }

    /// The `args)<elapsed>` part of a folded header, read from its right
    /// end: the row has to end in `status`, so the text in between can be
    /// checked without a search for the status word landing inside a path.
    fn header_before_status<'a>(header: &'a str, name: &str, status: &str) -> &'a str {
        let body = header
            .strip_suffix(status)
            .unwrap_or_else(|| panic!("the {name} header does not end in {status:?}: {header:?}"));
        let needle = format!("{name}(");
        let from = body
            .find(&needle)
            .unwrap_or_else(|| panic!("the {name} header lost its name: {header:?}"))
            + needle.len();
        &body[from..]
    }

    /// Checks a folded tool header squeezed to `name(args)<elapsed><status>`:
    /// the piece after the closing paren must be a duration (`0s`, `12s`,
    /// `1m3s` once squeezed) and nothing else, which is also what keeps an
    /// error message out of the header.
    fn assert_header_is_name_args_elapsed(header: &str, name: &str) {
        let args_end = header
            .rfind(')')
            .unwrap_or_else(|| panic!("no closing paren in the {name} header {header:?}"));
        let elapsed = &header[args_end + 1..];
        let digits = elapsed.trim_end_matches('s');
        assert!(
            digits.starts_with(|c: char| c.is_ascii_digit())
                && digits
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == 'h' || c == 'm')
                && elapsed.ends_with('s'),
            "the {name} header should read name(args) elapsed status, got {header:?} \
         (after the args: {elapsed:?})"
        );
    }

    /// Same shape as `STDERR_SLEEP_TOOL`, but it writes an error and exits non-zero
    /// once it has marked its start.
    const FAIL_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Fail on purpose.\n\
    # @option --marker! Path prefix for the start file.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   local marker\n\
    \x20   marker=\"$(jq -r '.marker' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   : > \"${marker}-started\"\n\
    \x20   echo \"boom-error-q8x\" >&2\n\
    \x20   exit 1\n\
    }\n";

    // ---------------------------------------------------------------------
    // A finished tool row is one folded line, `name(args)  elapsed  ok`, with
    // the tool's output out of sight. Ctrl+Up focuses it, Enter opens it
    // (output visible), Enter folds it again, Esc drops the focus so a bare
    // Enter opens nothing, and with a draft typed Enter submits the draft
    // even while a row is focused.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn a_finished_tool_row_is_one_folded_line_that_opens_on_enter_when_focused() {
        let dir = fresh_config_dir("tool-focus");
        let _guard = TempDirGuard(dir.clone());
        let marker = dir.join("tool");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_sleep".to_string(),
                arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
            },
            Reply::Text("fixture done tail-m4e".to_string()),
            Reply::Text("after focus reply-n2p".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", STDERR_SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-started"),
            "the tool's start marker",
        );
        fs::write(dir.join("tool-release"), b"").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "fixture done tail-m4e",
            0,
            "the reply after the tool",
        );
        pty.read_for(Duration::from_millis(500));

        let rows = repaint_rows(&mut pty);
        let header = folded_header_row(&rows, "fixture_sleep");
        assert_header_is_name_args_elapsed(
            header_before_status(header, "fixture_sleep", "ok"),
            "fixture_sleep",
        );
        let folded: String = rows.iter().map(|(_, text)| text.as_str()).collect();
        let output = squeezed("fixture slept p3t");
        assert!(
            !folded.contains(&output),
            "the folded tool row still shows the tool's output:\n{folded}"
        );

        // Focus the row and open it.
        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_UP).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        let opened = ctrl_l_repaint(&mut pty);
        assert!(
            opened.contains(&output),
            "Enter on the focused tool row did not open it (no tool output on screen):\n{opened}"
        );
        assert!(
            opened.contains("fixture_sleep("),
            "the opened row lost its header:\n{opened}"
        );

        // Enter again folds it.
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        let refolded = ctrl_l_repaint(&mut pty);
        assert!(
            !refolded.contains(&output),
            "a second Enter did not fold the opened row:\n{refolded}"
        );

        // Esc drops the focus: a bare Enter now opens nothing.
        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        let unfocused = ctrl_l_repaint(&mut pty);
        assert!(
            !unfocused.contains(&output),
            "Enter after Esc still toggled the row, so Esc did not clear the focus:\n{unfocused}"
        );
        assert!(
            !unfocused.contains("fakellm:noreplyscripted"),
            "a bare Enter on the empty composer submitted a turn:\n{unfocused}"
        );

        // A focused row plus a draft: Enter submits the draft.
        pty.send(CTRL_UP).unwrap();
        pty.read_for(Duration::from_millis(300));
        let before = pty.raw().len();
        pty.send(b"draft wins k7q\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "after focus reply-n2p",
            before,
            "the reply to the draft sent over a focused row",
        );
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains(">draftwinsk7q"),
            "the draft did not become a user cell:\n{frame}"
        );
        assert!(
            !frame.contains(&output),
            "Enter with a draft toggled the focused row instead of only submitting:\n{frame}"
        );
        assert_eq!(fake.served(), 3, "{frame}");

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // A tool that exits non-zero folds to `name(args)  elapsed  failed: <first
    // line of the error>`; what the tool wrote to stderr is there once the
    // row is opened.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn a_failed_tool_row_says_failed_and_keeps_the_error_for_the_opened_row() {
        let dir = fresh_config_dir("erroring");
        let _guard = TempDirGuard(dir.clone());
        let marker = dir.join("tool");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_fail".to_string(),
                arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
            },
            Reply::Text("after failure reply-f3w".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_fail.sh", FAIL_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b"call the failing tool\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "after failure reply-f3w",
            0,
            "the reply after the failed tool",
        );
        pty.read_for(Duration::from_millis(500));

        let rows = repaint_rows(&mut pty);
        let header = folded_header_row(&rows, "fixture_fail");
        let status = squeezed("failed: Tool call 'fixture_fail' exited with code 1");
        assert_header_is_name_args_elapsed(
            header_before_status(header, "fixture_fail", &status),
            "fixture_fail",
        );
        assert!(
            !header.contains("boom"),
            "the folded header carries the tool's stderr: {header:?}"
        );
        let folded: String = rows.iter().map(|(_, text)| text.as_str()).collect();
        let errors_folded = folded.matches("boom-error-q8x").count();

        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_UP).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        let opened = ctrl_l_repaint(&mut pty);
        assert!(
            opened.matches("boom-error-q8x").count() > errors_folded,
            "the opened failed row does not show the tool's error (folded: {errors_folded}):\n{opened}"
        );

        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(300));
        quit(pty);
    }

    // ---------------------------------------------------------------------
    // With the view scrolled up during a turn, the cells that land (the
    // tool's end, the reply) do not drag the view down: the follow chip
    // counts them as new, and End brings them into view and clears the chip.
    // ---------------------------------------------------------------------

    /// The count a follow chip `N new below  End = follow` carries at the
    /// right end of a squeezed screen row, if the row ends in one.
    fn follow_chip_count(row: &str) -> Option<usize> {
        let before = row.strip_suffix("End=follow")?.strip_suffix("newbelow")?;
        before
            .rsplit(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    }

    #[test]
    #[serial]
    fn cells_landing_while_scrolled_up_count_on_the_follow_chip_until_end() {
        let dir = fresh_config_dir("scrolled-landing");
        let _guard = TempDirGuard(dir.clone());
        let marker = dir.join("tool");
        let mut replies: Vec<Reply> = (1..=5)
            .map(|n| Reply::Text(format!("fillerreply{n:02}")))
            .collect();
        replies.push(Reply::ToolCall {
            name: "fixture_sleep".to_string(),
            arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
        });
        replies.push(Reply::Text("late reply tail-v6c".to_string()));
        let fake = FakeLlm::start(replies);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", STDERR_SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.resize(14, 100).unwrap();
        pty.read_for(Duration::from_millis(500));
        for n in 1..=5 {
            let before = pty.raw().len();
            pty.send(format!("fillerline{n:02}\r").as_bytes()).unwrap();
            wait_for_squeezed_after(
                &mut pty,
                &format!("fillerreply{n:02}"),
                before,
                "a filler reply",
            );
            pty.read_for(Duration::from_millis(300));
        }

        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-started"),
            "the tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        let before = pty.raw().len();
        pty.send(PAGE_UP).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "End = follow",
            before,
            "the follow chip after scrolling up mid-turn",
        );

        fs::write(dir.join("tool-release"), b"").unwrap();
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        let late = squeezed("late reply tail-v6c");
        let scrolled = loop {
            pty.read_for(Duration::from_millis(300));
            let rows = repaint_rows(&mut pty);
            let frame: String = rows.iter().map(|(_, text)| text.as_str()).collect();
            if let Some(count) = rows.iter().find_map(|(_, text)| follow_chip_count(text)) {
                assert!(count > 0, "the follow chip counts nothing new:\n{frame}");
                break frame;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the follow chip never counted the cells that landed while scrolled up:\n{frame}"
            );
        };
        assert!(
            !scrolled.contains(&late),
            "the reply pulled the scrolled view down to the bottom:\n{scrolled}"
        );

        pty.send(END).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains(&late),
            "End did not bring the late reply into view:\n{frame}"
        );
        assert!(
            !frame.contains("End=follow"),
            "End did not clear the follow chip:\n{frame}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // todo__* calls take no transcript row of their own; what they set shows
    // up in the Todo panel (Ctrl+X).
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn todo_calls_take_no_transcript_row_and_fill_the_todo_panel() {
        let dir = fresh_config_dir("todo-hidden");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "todo__init".to_string(),
                arguments: "{\"goal\": \"walk the dog w7k\"}".to_string(),
            },
            Reply::ToolCall {
                name: "todo__add".to_string(),
                arguments: "{\"task\": \"feed the cat z3r\"}".to_string(),
            },
            Reply::Text("todo noted r4d".to_string()),
        ]);
        write_fake_llm_config_with(&dir, &fake, "auto_continue: true\nmax_auto_continues: 0\n");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b"plan the chores\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "todo noted r4d",
            0,
            "the reply after the todo calls",
        );
        pty.read_for(Duration::from_millis(800));

        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains("todo__init") && !frame.contains("todo__add"),
            "a todo call took a transcript row:\n{frame}"
        );
        assert!(
            !frame.contains("feedthecatz3r"),
            "the todo item is on screen before the panel is opened:\n{frame}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let panel = ctrl_l_repaint(&mut pty);
        assert!(
            panel.contains("Todo"),
            "Ctrl+X did not open the Todo panel:\n{panel}"
        );
        assert!(
            panel.contains("feedthecatz3r"),
            "the Todo panel does not list the item the todo__add call set:\n{panel}"
        );

        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(300));
        quit(pty);
    }

    /// A todo list longer than the panel scrolls in the real binary:
    /// Alt+PgDn pages it and the top border then counts the lines above,
    /// and a wheel notch over the panel moves it back a line.
    #[test]
    #[serial]
    fn a_long_todo_list_scrolls_with_alt_page_down_and_the_wheel() {
        const ALT_PAGE_DOWN: &[u8] = b"\x1b[6;3~";
        const WHEEL_UP_OVER_THE_PANEL: &[u8] = b"\x1b[<64;85;4M";
        let dir = fresh_config_dir("todo-scroll");
        let _guard = TempDirGuard(dir.clone());
        let mut replies = vec![Reply::ToolCall {
            name: "todo__init".to_string(),
            arguments: "{\"goal\": \"a long list\"}".to_string(),
        }];
        replies.extend((1..=14).map(|n| Reply::ToolCall {
            name: "todo__add".to_string(),
            arguments: format!("{{\"task\": \"item {n:02} of the list\"}}"),
        }));
        replies.push(Reply::Text("listed m2t".to_string()));
        let fake = FakeLlm::start(replies);
        write_fake_llm_config_with(&dir, &fake, "auto_continue: true\nmax_auto_continues: 0\n");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(16, 100).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b"make a list\r").unwrap();
        wait_for_squeezed_after(&mut pty, "listed m2t", 0, "the reply after the todo calls");
        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let panel = ctrl_l_repaint(&mut pty);
        assert!(
            panel.contains("item01") && panel.contains("below)"),
            "the panel does not show the first items with a count below:\n{panel}"
        );

        pty.send(ALT_PAGE_DOWN).unwrap();
        pty.read_for(Duration::from_millis(500));
        let paged = ctrl_l_repaint(&mut pty);
        assert!(
            !paged.contains("item01") && paged.contains("above)"),
            "Alt+PgDn did not page the panel:\n{paged}"
        );

        pty.send(WHEEL_UP_OVER_THE_PANEL).unwrap();
        pty.read_for(Duration::from_millis(500));
        let wheeled = ctrl_l_repaint(&mut pty);
        assert_ne!(
            above_count(&paged),
            above_count(&wheeled),
            "a wheel notch over the panel did not move it:\n{wheeled}"
        );
        quit(pty);
    }

    /// The `(+N above)` count on the todo panel's top border.
    fn above_count(frame: &str) -> Option<String> {
        let end = frame.find("above)")?;
        let start = frame[..end].rfind("(+")?;
        Some(frame[start..end].to_string())
    }

    /// Alt+PgUp undoes Alt+PgDn on an open todo panel: the first items are
    /// back on screen and the top border no longer counts lines above.
    #[test]
    #[serial]
    fn alt_page_up_scrolls_a_paged_todo_list_back_to_its_top() {
        const ALT_PAGE_DOWN: &[u8] = b"\x1b[6;3~";
        const ALT_PAGE_UP: &[u8] = b"\x1b[5;3~";
        let dir = fresh_config_dir("todo-scroll-up");
        let _guard = TempDirGuard(dir.clone());
        let mut replies = vec![Reply::ToolCall {
            name: "todo__init".to_string(),
            arguments: "{\"goal\": \"a long list\"}".to_string(),
        }];
        replies.extend((1..=14).map(|n| Reply::ToolCall {
            name: "todo__add".to_string(),
            arguments: format!("{{\"task\": \"item {n:02} of the list\"}}"),
        }));
        replies.push(Reply::Text("listed u3p".to_string()));
        let fake = FakeLlm::start(replies);
        write_fake_llm_config_with(&dir, &fake, "auto_continue: true\nmax_auto_continues: 0\n");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(16, 100).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b"make a list\r").unwrap();
        wait_for_squeezed_after(&mut pty, "listed u3p", 0, "the reply after the todo calls");
        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        pty.send(ALT_PAGE_DOWN).unwrap();
        pty.read_for(Duration::from_millis(500));
        let paged = ctrl_l_repaint(&mut pty);
        assert!(
            !paged.contains("item01") && paged.contains("above)"),
            "Alt+PgDn did not page the panel:\n{paged}"
        );

        pty.send(ALT_PAGE_UP).unwrap();
        pty.read_for(Duration::from_millis(500));
        let back = ctrl_l_repaint(&mut pty);
        assert!(
            back.contains("item01") && !back.contains("above)") && back.contains("below)"),
            "Alt+PgUp did not scroll the panel back to its top:\n{back}"
        );
        quit(pty);
    }

    /// `.threads` typed while a tool runs opens the thread picker at once:
    /// it is not held for the turn's end, the tool keeps running, and the
    /// model is not asked again.
    #[test]
    #[serial]
    fn dot_threads_mid_turn_opens_the_picker_at_once() {
        const PICKER: &str = "Enter open  Esc back";
        let dir = fresh_config_dir("threads-mid-turn");
        let _guard = TempDirGuard(dir.clone());
        let tool_pid = dir.join("tool.pid");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_sleep".to_string(),
                arguments: "{}".to_string(),
            },
            Reply::Text("done after the tool r6k".to_string()),
        ]);
        write_fake_llm_config_with(&dir, &fake, "enabled_tools:\n  - fixture_sleep\n");
        write_tool(&dir, "fixture_sleep.sh", &sleeping_tool(&tool_pid));
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        pty.send(b"run the slow tool\r").unwrap();
        wait_for_file(&mut pty, &tool_pid, "the tool's pid file");
        let _sleepers = Sleepers(vec![tool_pid.clone()]);
        let pid: i32 = fs::read_to_string(&tool_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        pty.read_for(Duration::from_millis(500));

        pty.send(b".threads\r").unwrap();
        let frame = repainted_frame_showing(
            &mut pty,
            PICKER,
            "`.threads` mid-turn did not open the picker at once",
        );
        assert!(
            !squeezed(&frame).contains(&squeezed("pending: 1")),
            "`.threads` was held as a queued line:\n{frame}"
        );
        assert_eq!(fake.served(), 1, "`.threads` reached the model");
        assert!(alive(pid), "`.threads` stopped the running tool");

        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"q").unwrap();
        pty.read_for(Duration::from_millis(300));
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        wait_for_squeezed_after(&mut pty, "done after the tool r6k", 0, "the turn's end");
        quit(pty);
    }

    /// `.session <name>` on a session of thousands of messages, from inside
    /// the TUI, shows its newest reply within a bounded time, and the top
    /// of the transcript view is the session's very first message.
    #[test]
    #[serial]
    fn dot_session_on_a_large_session_is_bounded_and_reaches_the_first_message() {
        const EXCHANGES: usize = 2_500;
        let dir = fresh_config_dir("big-dot-session");
        let _guard = TempDirGuard(dir.clone());
        // The exit dump of the whole history is not what this case checks,
        // and on a loaded host its 5,000 messages outlast the quit deadline.
        write_dry_run_config_with(&dir, "tui_dump_on_exit: false\n");
        write_session(&dir, "huge", EXCHANGES);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        let at = pty.raw().len();
        let started = std::time::Instant::now();
        pty.send(b".session huge\r").unwrap();
        pty.wait_for_after("previous conversation", at, PTY_DEADLINE)
            .expect("`.session` never showed the history's divider");
        let elapsed = started.elapsed();
        eprintln!("`.session` on {} messages took {elapsed:?}", EXCHANGES * 2);
        assert!(
            elapsed < Duration::from_secs(15),
            "`.session` on {} messages took {elapsed:?}",
            EXCHANGES * 2
        );
        repainted_frame_showing(
            &mut pty,
            "answer 02499",
            "the newest reply is not on screen after `.session`",
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
            "question 00000",
            "the transcript view's top is not the session's first message",
        );
        pty.send(b"q").unwrap();
        pty.read_for(Duration::from_millis(300));
        // Scrolling the transcript itself (no pager) up to the top reaches it too.
        // About four rows an exchange and twenty a page: 1,000 pages is
        // twice what the history needs.
        for n in 0..1_000 {
            pty.send(PAGE_UP).unwrap();
            if n % 100 == 99 {
                pty.read_for(Duration::from_millis(100));
            }
        }
        repainted_frame_showing(
            &mut pty,
            "question 00000",
            "PageUp in the transcript never reached the first message",
        );
        quit(pty);
    }

    // ---------------------------------------------------------------------
    // With the reasoning delta held open on the wire, the "Thinking for Ns"
    // status row is on screen well before the hold limit, and it is gone
    // once the answer lands.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn the_thinking_status_row_is_live_during_a_held_reasoning_delta() {
        let dir_for_release = fresh_config_dir("thinking-release");
        let _release_guard = TempDirGuard(dir_for_release.clone());
        let release = dir_for_release.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Reasoning("pondering deeply q9z".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("final answer k2m".to_string()),
        ])]);
        let (_guard, mut pty) = boot_with_fake("thinking", &fake);

        let started = std::time::Instant::now();
        pty.send(b"think hard\r").unwrap();
        let seen = wait_for_squeezed_after(&mut pty, "Thinking for", 0, "the live thinking row");
        assert!(
            started.elapsed() < HOLD_LIMIT,
            "the Thinking row appeared only after the fake gave up holding the stream:\n{seen}"
        );

        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "final answer k2m",
            0,
            "the answer after reasoning",
        );
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(&squeezed("Thinking for")),
            "the Thinking row is still up after the answer landed:\n{frame}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // Ctrl+X shows the Todo panel and hides it again.
    // ---------------------------------------------------------------------

    /// A session file of `exchanges` user lines and replies, numbered so
    /// the first and the last can be told apart.
    fn write_session(dir: &Path, name: &str, exchanges: usize) {
        let sessions = dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let mut yaml = String::from("model: dryrun:dry-model\nmessages:\n");
        for n in 0..exchanges {
            yaml.push_str(&format!(
                "- role: user\n  content: 'question {n:05}'\n\
                 - role: assistant\n  content: 'answer {n:05}, a reply of a few words'\n"
            ));
        }
        fs::write(sessions.join(format!("{name}.yaml")), yaml).unwrap();
    }

    /// A session resumed at launch with thousands of messages shows all of
    /// its history, as the REPL does: the newest reply and the divider
    /// under it come up within a bounded time, and the top of the
    /// transcript view is the very first message.
    #[test]
    #[serial]
    fn a_large_session_resumed_at_launch_shows_its_whole_history() {
        const EXCHANGES: usize = 2_500;
        let dir = fresh_config_dir("big-session");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        write_session(&dir, "big", EXCHANGES);

        let started = std::time::Instant::now();
        let mut pty = spawn_tui_with_args(&dir, &[], &["-s", "big"]);
        pty.wait_for("previous conversation", PTY_DEADLINE)
            .expect("the resumed history never showed its divider");
        let elapsed = started.elapsed();
        eprintln!(
            "first frame of a {}-message session after {elapsed:?}",
            EXCHANGES * 2
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "a {}-message session took {elapsed:?} to show",
            EXCHANGES * 2
        );
        let frame = repainted_frame_showing(
            &mut pty,
            "answer 02499",
            "the newest reply is not on the first screen",
        );
        assert!(!frame.contains("question 00000"), "{frame}");

        // The transcript view still lays the whole root thread out when it
        // opens; this bounds that first layout of the long history.
        let opened_at = pty.raw().len();
        let opening = std::time::Instant::now();
        pty.send(b"\x14").unwrap();
        pty.wait_for_after("root thread", opened_at, PTY_DEADLINE)
            .expect("Ctrl+T did not open the transcript view");
        let opened = opening.elapsed();
        eprintln!("the transcript view opened on the long history after {opened:?}");
        assert!(
            opened < Duration::from_secs(10),
            "the transcript view took {opened:?} to open on a {}-message history",
            EXCHANGES * 2
        );
        repainted_frame_showing(
            &mut pty,
            "Thread: root thread",
            "Ctrl+T did not open the pager",
        );
        pty.send(b"g").unwrap();
        repainted_frame_showing(
            &mut pty,
            "question 00000",
            "the transcript view's top is not the session's first message",
        );
        pty.send(b"q").unwrap();
        pty.read_for(Duration::from_millis(300));
        quit(pty);
    }

    /// `.session <name>` inside the TUI shows the session's history as
    /// rows, the oldest message included, rather than as the command's
    /// printed output.
    #[test]
    #[serial]
    fn dot_session_shows_the_resumed_history_as_rows() {
        let dir = fresh_config_dir("dot-session-history");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        write_session(&dir, "small", 3);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b".session small\r").unwrap();
        let frame = repainted_frame_showing(
            &mut pty,
            "previous conversation",
            "`.session` did not show the history's divider",
        );
        for shown in [
            "> question 00000",
            "answer 00000",
            "> question 00002",
            "answer 00002",
        ] {
            assert!(
                squeezed(&frame).contains(&squeezed(shown)),
                "{shown:?} is not shown:\n{frame}"
            );
        }
        assert!(
            !frame.contains("You:"),
            "the REPL's printed replay leaked in:\n{frame}"
        );
        quit(pty);
    }

    /// With nothing running and no child ever started, `.threads` opens the
    /// thread picker straight away, and so does `t` in the tasks pop-up;
    /// leaving the picker and the transcript view opened from the pop-up
    /// brings the pop-up back.
    #[test]
    #[serial]
    fn dot_threads_and_t_in_the_tasks_popup_open_the_thread_picker() {
        const PICKER: &str = "Enter open  Esc back";
        let (_guard, mut pty) = boot("threads-cmd", &[]);

        pty.send(b".threads\r").unwrap();
        let picker =
            repainted_frame_showing(&mut pty, PICKER, "`.threads` did not open the picker");
        assert!(
            squeezed(&picker).contains(&squeezed("No child sessions yet")),
            "the picker does not say no child has started:\n{picker}"
        );
        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"q").unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(ALT_T).unwrap();
        repainted_frame_showing(
            &mut pty,
            "Esc = close",
            "Alt+T did not open the tasks pop-up",
        );
        pty.send(b"t").unwrap();
        repainted_frame_showing(
            &mut pty,
            PICKER,
            "`t` in the tasks pop-up did not open the picker",
        );
        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"q").unwrap();
        repainted_frame_showing(
            &mut pty,
            "Esc = close",
            "leaving the transcript view did not bring the tasks pop-up back",
        );
        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(300));
        quit(pty);
    }

    #[test]
    #[serial]
    fn ctrl_x_shows_and_hides_the_todo_panel() {
        let (_guard, mut pty) = boot("ctrl-x", &[]);

        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains("Todo"),
            "the Todo panel is open before Ctrl+X was pressed:\n{frame}"
        );

        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains("Todo"),
            "Ctrl+X did not open the Todo panel:\n{frame}"
        );

        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains("Todo"),
            "a second Ctrl+X did not close the Todo panel:\n{frame}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // Once the transcript overflows a short terminal, PgUp detaches the view
    // from the bottom and the chip offers "End = follow"; End clears it. A
    // submitted line follows the bottom again on its own, so the reply it
    // gets is never out of sight.
    // ---------------------------------------------------------------------

    /// Fills a 14-row terminal past one screen with dry-run exchanges, so
    /// there is something to scroll, and checks the chip is still down.
    fn overflow_a_short_terminal(pty: &mut Pty) {
        pty.resize(14, 100).unwrap();
        pty.read_for(Duration::from_millis(500));
        for n in 1..=5 {
            let before = pty.raw().len();
            let marker = format!("fillerline{n:02}");
            pty.send(format!("{marker}\r").as_bytes()).unwrap();
            wait_for_squeezed_after(pty, &marker, before, "a filler prompt");
            pty.read_for(Duration::from_millis(300));
        }
        let frame = ctrl_l_repaint(pty);
        assert!(
            !frame.contains(CHIP),
            "the follow chip is up while the view follows the bottom:\n{frame}"
        );
    }

    #[test]
    #[serial]
    fn follow_chip_appears_on_scroll_up_and_clears_on_end() {
        let (_guard, mut pty) = boot("follow-chip", &[]);
        overflow_a_short_terminal(&mut pty);

        let before = pty.raw().len();
        pty.send(PAGE_UP).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "End = follow",
            before,
            "the follow chip after scrolling up",
        );

        pty.send(END).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains("End=follow"),
            "End did not clear the follow chip:\n{frame}"
        );
        assert!(
            frame.contains("fillerline05"),
            "End did not bring the newest output into view:\n{frame}"
        );

        pty.send(PAGE_UP).unwrap();
        pty.read_for(Duration::from_millis(500));
        let before = pty.raw().len();
        pty.send(b"latemarker\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "latemarker",
            before,
            "a prompt sent while scrolled up",
        );
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains("End=follow"),
            "the view did not follow the line just sent:\n{frame}"
        );
        assert!(
            frame.contains("latemarker"),
            "the line sent while scrolled up is out of sight:\n{frame}"
        );

        quit(pty);
    }

    // =====================================================================
    // Edge cases: tool-cell pairing, invalid input, tiny terminals, wide
    // graphemes, undo to an empty transcript.
    // =====================================================================

    /// A tool that marks its start at `<marker>-started`, then waits (at most
    /// eight seconds) for `<marker>-release` to appear before finishing. Its
    /// result goes to `LLM_OUTPUT` (a file the runner reads back for the
    /// model); the progress line on stderr is what the transcript can show.
    const STDERR_SLEEP_TOOL: &str = "#!/usr/bin/env bash\n\
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
    \x20   echo \"fixture slept p3t\" >&2\n\
    \x20   echo \"fixture slept p3t\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    // ---------------------------------------------------------------------
    // A tool call is one cell: its start and end events land in the same
    // row. Observable from the outside: while the tool runs its cell names
    // it; once it has finished and the model replied, the same name is on
    // screen exactly once, not in a second cell.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn a_tool_call_is_one_cell_that_finishes_in_place() {
        let dir = fresh_config_dir("tool-cell");
        let _guard = TempDirGuard(dir.clone());
        let marker = dir.join("tool");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_sleep".to_string(),
                arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
            },
            Reply::Text("fixture done tail-r7q".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", STDERR_SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-started"),
            "the tool's start marker",
        );
        pty.read_for(Duration::from_millis(500));
        let running = ctrl_l_repaint(&mut pty);
        assert!(
            running.contains("fixture_sleep"),
            "no cell names the running tool:\n{running}"
        );

        fs::write(dir.join("tool-release"), b"").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "fixture done tail-r7q",
            0,
            "the reply after the tool",
        );
        pty.read_for(Duration::from_millis(500));
        let finished = ctrl_l_repaint(&mut pty);
        let names = finished.matches("fixture_sleep").count();
        assert_eq!(
            names, 1,
            "the finished tool should be one cell naming it once (saw {names}); \
         while running the frame was:\n{running}\nafter:\n{finished}"
        );
        assert!(
            finished.contains(&squeezed("fixture done tail-r7q")),
            "the model's reply after the tool is missing:\n{finished}"
        );
        assert_eq!(fake.served(), 2, "{finished}");

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // Invalid input: `.undo` on an empty transcript (nothing recorded) must
    // not take the UI down; the process is still there, still takes keys,
    // and still quits with the tty restored.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn undo_with_nothing_to_undo_leaves_the_ui_alive() {
        let (_guard, mut pty) = boot("undo-empty", &[]);

        pty.send(b".undo\r").unwrap();
        pty.read_for(Duration::from_secs(1));
        pty.send(b".undo\r").unwrap();
        pty.read_for(Duration::from_secs(1));

        let before = pty.raw().len();
        pty.send(b"still-alive-marker\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "still-alive-marker",
            before,
            "a prompt after a refused .undo",
        );
        pty.read_for(Duration::from_millis(500));

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // Every widget clamps to the buffer. With the welcome block up and the
    // Todo panel open, the terminal is shrunk to a handful of cells and then
    // to a single cell; nothing may panic, a key typed under every size
    // still reaches the composer, and once the terminal grows back both
    // widgets are drawn again and the draft typed along the way is
    // submitted whole.
    //
    // After a shrink the composer's own viewport (ratatui-textarea) may stay
    // scrolled right, so the draft can look blank until it is edited or
    // sent; the draft itself is intact, which is what the submitted user
    // cell asserts.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn tiny_terminal_sizes_clamp_the_welcome_block_and_todo_panel() {
        let (_guard, mut pty) = boot("tiny", &[]);

        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains("Todo"),
            "Ctrl+X did not open the Todo panel:\n{frame}"
        );

        // One distinct digit per size, the first a control resize that is
        // not tiny at all.
        for (digit, rows, cols) in [
            (b"1", 20u16, 70u16),
            (b"2", 6, 20),
            (b"3", 3, 8),
            (b"4", 2, 4),
            (b"5", 1, 1),
            (b"6", 5, 40),
        ] {
            pty.resize(rows, cols).unwrap();
            pty.read_for(Duration::from_millis(400));
            pty.send(digit).unwrap();
            pty.read_for(Duration::from_millis(400));
            let since = strip_ansi(&String::from_utf8_lossy(pty.raw()));
            assert!(
                !since.contains("panicked") && !since.contains("exit="),
                "the UI died at {rows}x{cols}:\n{since}"
            );
        }

        pty.resize(24, 80).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains(&squeezed("Tip: Press Ctrl-C")),
            "the welcome block did not come back after growing the terminal:\n{frame}"
        );
        assert!(
            frame.contains("Todo"),
            "the Todo panel did not come back after growing the terminal:\n{frame}"
        );

        let before = pty.raw().len();
        pty.send(b"\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            ">123456",
            before,
            "the draft typed across the resizes, as the user cell",
        );
        pty.read_for(Duration::from_millis(500));

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // A prompt carrying a VS16 sequence and a wide emoji reaches the user
    // cell and the dry-run echo with both graphemes intact.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn emoji_and_vs16_prompts_echo_whole() {
        let (_guard, mut pty) = boot("emoji", &[]);

        let before = pty.raw().len();
        pty.send("\u{2714}\u{FE0F} check \u{1F44D} emoji-marker-v5\r".as_bytes())
            .unwrap();
        wait_for_squeezed_after(&mut pty, "emoji-marker-v5", before, "the emoji prompt");
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains('\u{2714}') && frame.contains('\u{1F44D}'),
            "the emoji graphemes did not survive into the transcript:\n{frame}"
        );
        assert!(
            frame.matches("emoji-marker-v5").count() >= 2,
            "the user cell and the echo should both carry the prompt:\n{frame}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // "Undo removes cells back to the last user cell", at the boundary where
    // the previous user cell is a command (`.session scratch`) rather than a
    // prompt: the only exchange goes, the command cell stays, and the undone
    // prompt is handed back to the composer. The welcome block does not
    // return: a submitted line (the command) is still in the transcript.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn undoing_the_only_exchange_stops_at_the_command_cell_before_it() {
        let fake = FakeLlm::start(vec![Reply::Text("reply-solo-zz".to_string())]);
        let (_guard, mut pty) = boot_with_fake("undo-to-empty", &fake);

        pty.send(b".session scratch\r").unwrap();
        pty.read_for(Duration::from_secs(1));

        let before = pty.raw().len();
        pty.send(b"solomarker one\r").unwrap();
        wait_for_squeezed_after(&mut pty, "reply-solo-zz", before, "the only reply");
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(&squeezed("Tip: Press Ctrl-C")),
            "the welcome block is up with a turn on screen:\n{frame}"
        );

        pty.send(b".undo\r").unwrap();
        pty.read_for(Duration::from_secs(1));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains("reply-solo-zz") && !frame.contains(">solomarkerone"),
            ".undo left the only exchange on screen:\n{frame}"
        );
        assert!(
            frame.contains(">.sessionscratch"),
            ".undo removed the command cell before the exchange:\n{frame}"
        );
        assert!(
            !frame.contains(&squeezed("Tip: Press Ctrl-C")),
            "the welcome block came back over the command cell:\n{frame}"
        );
        assert!(
            frame.contains("solomarkerone"),
            ".undo did not hand the undone prompt back to the composer:\n{frame}"
        );

        pty.send(CTRL_C).unwrap();
        pty.read_for(Duration::from_millis(300));
        quit(pty);
    }

    // =====================================================================
    // Key gating and hidden spans: Shift+G at the bottom, a think span that
    // closes late, Esc over a focused row mid-turn, the view keys under a
    // modal, and a folded header at forty columns.
    // =====================================================================

    // ---------------------------------------------------------------------
    // Shift+G jumps to the bottom only while the view is scrolled AND
    // the composer is empty. With the view at the bottom the letter is
    // plain typing, before any scroll and again right after a jump.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn shift_g_types_the_letter_while_the_view_follows_the_bottom() {
        let (_guard, mut pty) = boot("shift-g-unscrolled", &[]);
        overflow_a_short_terminal(&mut pty);

        // Unscrolled, empty composer: G is a letter.
        pty.send(b"Gx7").unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains("Gx7"),
            "Shift+G on an unscrolled empty composer was swallowed instead of typed:\n{frame}"
        );
        assert!(
            !frame.contains(CHIP),
            "typing G raised the follow chip:\n{frame}"
        );
        pty.send(CTRL_C).unwrap();
        pty.read_for(Duration::from_millis(300));

        // Scrolled, empty composer: G jumps back and types nothing.
        let before = pty.raw().len();
        pty.send(PAGE_UP).unwrap();
        wait_for_squeezed_after(&mut pty, "End = follow", before, "the chip after PgUp");
        pty.send(b"G").unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(CHIP),
            "Shift+G while scrolled on an empty composer did not follow the bottom:\n{frame}"
        );
        assert!(
            !frame.contains(">G"),
            "Shift+G both jumped and typed its letter:\n{frame}"
        );

        // Right after the jump the view follows again: G is a letter once more.
        pty.send(b"Gx7").unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains("Gx7"),
            "Shift+G right after a jump was swallowed instead of typed:\n{frame}"
        );

        pty.send(CTRL_C).unwrap();
        pty.read_for(Duration::from_millis(300));
        quit(pty);
    }

    // ---------------------------------------------------------------------
    // The streaming cell hides a `<think>` span whose closing tag has
    // not arrived yet. With the stream held between the reasoning and the
    // answer, the open span is off screen while held, and the late
    // `</think>` leaves neither tag nor reasoning behind.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn an_open_think_span_stays_hidden_until_its_late_close_arrives() {
        let dir_for_release = fresh_config_dir("late-think-release");
        let _release_guard = TempDirGuard(dir_for_release.clone());
        let release = dir_for_release.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Reasoning("weighing the options v4w".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("the answer n7p".to_string()),
        ])]);
        let (_guard, mut pty) = boot_with_fake("late-think", &fake);

        let started = std::time::Instant::now();
        pty.send(b"think then hold\r").unwrap();
        wait_for_squeezed_after(&mut pty, "Thinking for", 0, "the live thinking row");
        assert!(
            started.elapsed() < HOLD_LIMIT,
            "the Thinking row appeared only after the fake gave up holding the stream"
        );
        pty.read_for(Duration::from_millis(500));
        let held = ctrl_l_repaint(&mut pty);
        assert!(
            !held.contains("<think>") && !held.contains("</think>"),
            "a think tag is on screen while the span is still open:\n{held}"
        );
        assert!(
            !held.contains(&squeezed("weighing the options")),
            "the reasoning of an open think span is on screen while held:\n{held}"
        );

        fs::write(&release, b"").unwrap();
        let after =
            wait_for_squeezed_after(&mut pty, "the answer n7p", 0, "the answer after the hold");
        assert!(
            !after.contains("<think>") && !after.contains("</think>"),
            "a think tag was painted when the late close arrived:\n{after}"
        );
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains(&squeezed("the answer n7p")),
            "the answer is not on screen:\n{frame}"
        );
        assert!(
            !frame.contains("<think>") && !frame.contains("</think>"),
            "a think tag is on screen after the late close:\n{frame}"
        );
        assert!(
            !frame.contains(&squeezed("weighing the options")),
            "the reasoning is on screen after the late close:\n{frame}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // The other late close: a reply that is reasoning and nothing else
    // gets its `</think>` only at `[DONE]`. Nothing of the span survives on
    // screen and the UI takes the next turn.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn a_reasoning_only_reply_leaves_no_think_span_behind() {
        let fake = FakeLlm::start(vec![
            Reply::Streamed(vec![Chunk::Reasoning(
                "only reasoning here z3k".to_string(),
            )]),
            Reply::Text("second reply p4m".to_string()),
        ]);
        let (_guard, mut pty) = boot_with_fake("think-only", &fake);

        pty.send(b"reason only\r").unwrap();
        let deadline = std::time::Instant::now() + PTY_DEADLINE;
        loop {
            pty.read_for(Duration::from_millis(300));
            if fake.served() >= 1 {
                let frame = ctrl_l_repaint(&mut pty);
                if !frame.contains(&squeezed("Thinking for"))
                    && !frame.contains(&squeezed("Esc to interrupt"))
                {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the reasoning-only turn never finished"
            );
        }
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains("<think>") && !frame.contains("</think>"),
            "a think tag is on screen after a reasoning-only reply:\n{frame}"
        );
        assert!(
            !frame.contains(&squeezed("only reasoning here")),
            "the reasoning is on screen after a reasoning-only reply:\n{frame}"
        );

        let before = pty.raw().len();
        pty.send(b"and now answer\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "second reply p4m",
            before,
            "the next turn's reply",
        );
        assert_eq!(fake.served(), 2);

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // While a turn runs, Esc interrupts it even with a transcript row
    // focused: the interrupted marker lands, the stream stops painting, and
    // the focus walk is intact afterwards (Ctrl+Up, Enter opens the row).
    // Whether the focus itself survives that Esc is not pinned here: the
    // contract says the interrupt comes first, not what becomes of the focus.
    // A recording session is opened first so the interrupted marker, not an
    // "Aborted." error, closes the turn (the lifecycle suite's precedent).
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn esc_with_a_focused_row_interrupts_the_running_turn_first() {
        let dir = fresh_config_dir("esc-focus-running");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_echo".to_string(),
                arguments: "{\"word\": \"hi\"}".to_string(),
            },
            Reply::Text("echo reply m9s".to_string()),
            Reply::Streamed(vec![
                Chunk::Text("held head h5t".to_string()),
                Chunk::WaitFor(release.clone()),
                Chunk::Text("held tail never shown".to_string()),
            ]),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_echo.sh", ECHO_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b".session scratch\r").unwrap();
        pty.read_for(Duration::from_secs(1));

        pty.send(b"say hi\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "echo reply m9s",
            0,
            "the reply after the echo tool",
        );
        pty.read_for(Duration::from_millis(500));
        let output = squeezed("clicked output c4v");

        let before = pty.raw().len();
        pty.send(b"stream and hold\r").unwrap();
        wait_for_squeezed_after(&mut pty, "held head h5t", before, "the first delta");
        pty.wait_for("Esc to interrupt", PTY_DEADLINE)
            .expect("the status row never offered Esc to interrupt");

        // Focus the tool row mid-turn, then Esc.
        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_UP).unwrap();
        pty.read_for(HOTKEY_GAP);
        let pressed_at = pty.raw().len();
        pty.send(ESC).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        loop {
            pty.read_for(Duration::from_millis(200));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
            if since.to_lowercase().contains("interrupted") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Esc with a row focused did not interrupt the running turn:\n{since}"
            );
        }
        fs::write(&release, b"").unwrap();
        pty.read_for(Duration::from_secs(1));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(&squeezed("Esc to interrupt")),
            "the turn is still running after Esc:\n{frame}"
        );
        assert!(
            !frame.contains(&squeezed("held tail never shown")),
            "the stream kept painting after the interrupt:\n{frame}"
        );
        assert!(
            !frame.contains(&output),
            "the tool row opened on its own around the interrupt:\n{frame}"
        );

        // The focus walk is intact once the turn is over.
        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_UP).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        let opened = ctrl_l_repaint(&mut pty);
        assert!(
            opened.contains(&output),
            "Ctrl+Up then Enter after the interrupt did not open the tool row:\n{opened}"
        );
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        let unfocused = ctrl_l_repaint(&mut pty);
        assert!(
            !unfocused.contains(&output),
            "Esc on the idle UI did not clear the focus:\n{unfocused}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // Keys that only move the view belong to the transcript, not the modal:
    // with a tool's Confirm question up, Ctrl+X toggles the Todo panel, PgUp
    // raises the follow chip and End clears it, and the question is still
    // there to answer afterwards.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn view_keys_work_under_a_tools_confirm_modal() {
        let dir = fresh_config_dir("confirm-view-keys");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("confirm-answer");
        let mut replies: Vec<Reply> = (1..=5)
            .map(|n| Reply::Text(format!("fillerreply{n:02}")))
            .collect();
        replies.push(Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        });
        replies.push(Reply::Text("fixture done z2v".to_string()));
        let fake = FakeLlm::start(replies);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_confirm_rec.sh", RECORDING_CONFIRM_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.resize(14, 100).unwrap();
        pty.read_for(Duration::from_millis(500));
        for n in 1..=5 {
            let before = pty.raw().len();
            pty.send(format!("fillerline{n:02}\r").as_bytes()).unwrap();
            wait_for_squeezed_after(
                &mut pty,
                &format!("fillerreply{n:02}"),
                before,
                "a filler reply",
            );
            pty.read_for(Duration::from_millis(300));
        }

        pty.send(b"call the confirm tool\r").unwrap();
        pty.wait_for("q9d-q", PTY_DEADLINE)
            .expect("the tool's confirm question never reached the screen");
        pty.read_for(HOTKEY_GAP);

        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains("Todo"),
            "Ctrl+X under the Confirm modal did not open the Todo panel:\n{frame}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains("Todo"),
            "a second Ctrl+X under the Confirm modal did not close the Todo panel:\n{frame}"
        );

        pty.read_for(HOTKEY_GAP);
        let before = pty.raw().len();
        pty.send(PAGE_UP).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "End = follow",
            before,
            "the follow chip after PgUp under the Confirm modal",
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(END).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(CHIP),
            "End under the Confirm modal did not clear the follow chip:\n{frame}"
        );
        assert!(
            frame.contains("q9d-q"),
            "the Confirm question is gone after the view keys:\n{frame}"
        );
        assert!(
            !out.exists(),
            "a view key answered the Confirm question: {:?}",
            fs::read_to_string(&out)
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        wait_for_squeezed_after(&mut pty, "fixture done z2v", 0, "the reply after the tool");
        assert_eq!(fs::read_to_string(&out).unwrap().trim(), "1");

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // The same view keys under the Input modal an agent variable raises;
    // Esc then cancels the question and the UI is still alive.
    // ---------------------------------------------------------------------

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

    #[test]
    #[serial]
    fn view_keys_work_under_the_input_modal() {
        let dir = fresh_config_dir("input-view-keys");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        write_agent_needing_a_variable(&dir, "vars-agent-g4r");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        overflow_a_short_terminal(&mut pty);

        pty.send(b".agent vars-agent-g4r\r").unwrap();
        wait_for_squeezed_after(&mut pty, "Enter = answer", 0, "the Input modal");
        pty.read_for(HOTKEY_GAP);

        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            frame.contains("Todo"),
            "Ctrl+X under the Input modal did not open the Todo panel:\n{frame}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(CTRL_X).unwrap();
        pty.read_for(Duration::from_millis(500));

        pty.read_for(HOTKEY_GAP);
        let before = pty.raw().len();
        pty.send(PAGE_UP).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "End = follow",
            before,
            "the follow chip after PgUp under the Input modal",
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(END).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(CHIP),
            "End under the Input modal did not clear the follow chip:\n{frame}"
        );
        assert!(
            frame.contains(&squeezed("Enter = answer")),
            "the Input question is gone after the view keys:\n{frame}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_secs(1));
        let frame = ctrl_l_repaint(&mut pty);
        assert!(
            !frame.contains(&squeezed("Enter = answer")),
            "Esc did not close the Input modal:\n{frame}"
        );

        quit(pty);
    }

    // ---------------------------------------------------------------------
    // A folded tool header at forty columns is still one row that ends in
    // its status: the state token is what the narrow width keeps, the
    // arguments are what it cuts.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn a_folded_tool_header_keeps_its_status_at_forty_columns() {
        let dir = fresh_config_dir("forty-cols");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_echo".to_string(),
                arguments: "{\"word\": \"a-rather-long-argument-value-q2w\"}".to_string(),
            },
            Reply::Text("echo reply m9s".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_echo.sh", ECHO_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        pty.send(b"say it\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "echo reply m9s",
            0,
            "the reply after the echo tool",
        );
        pty.read_for(Duration::from_millis(500));

        pty.resize(24, 40).unwrap();
        pty.read_for(Duration::from_millis(800));
        let rows = repaint_rows(&mut pty);
        let headers: Vec<&(u16, String)> = rows
            .iter()
            .filter(|(_, text)| text.contains("fixture_echo"))
            .collect();
        assert_eq!(
            headers.len(),
            1,
            "the tool name should be on exactly one row at 40 columns: {rows:?}"
        );
        let (row, header) = headers[0];
        assert!(
            header.ends_with("ok"),
            "the folded header on row {row} lost its status token at 40 columns: {header:?}; rows {rows:?}"
        );
        let args_and_elapsed = between(header, "fixture_echo(", "ok").unwrap_or_else(|| {
            panic!("no `fixture_echo(...) ... ok` header on row {row}: {header:?}")
        });
        assert_header_is_name_args_elapsed(args_and_elapsed, "fixture_echo");
        let frame: String = rows.iter().map(|(_, t)| t.as_str()).collect();
        assert!(
            !frame.contains(&squeezed("clicked output c4v")),
            "the row opened on its own at 40 columns:\n{frame}"
        );

        quit(pty);
    }
}

mod binary_threads {
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{Chunk, FakeLlm, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
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

    /// The threads pop-up's band of a repainted frame: from its title down to
    /// the key legend on its bottom border, so a word elsewhere on screen does
    /// not count as a pop-up row's.
    fn thread_picker_band(frame: &str) -> &str {
        let title = frame
            .rfind("Threads")
            .unwrap_or_else(|| panic!("the pop-up's title is not on screen:\n{frame}"));
        let band = &frame[title..];
        &band[..band.find("Enter open").unwrap_or(band.len())]
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
        fn walk_children(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk_children(&path, out);
                } else if path.parent().and_then(Path::file_name) == Some("children".as_ref()) {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk_children(dir, &mut out);
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
        let roster =
            repainted_frame_showing(&mut pty, "Esc = close", "Alt+T did not open the roster");
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
        let roster =
            repainted_frame_showing(&mut pty, "Esc = close", "Alt+T did not open the roster");
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
            row_after(frame, "kid  ").is_some_and(|row| row.contains("cancelled"))
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

    /// `o` on the kid's row opens its thread over the tasks pop-up, and `q`
    /// on the thread brings the pop-up back, title and way out in sight.
    #[test]
    #[serial]
    fn closing_the_kids_thread_returns_to_the_tasks_popup() {
        let scene = scene("thr-return", kid_mark_then_held, "");
        let mut pty = spawn_tui(&scene.dir, &scene.log_env());
        engage_and_spawn(&mut pty, &scene);

        pty.send(ALT_T).unwrap();
        let roster =
            repainted_frame_showing(&mut pty, "Esc = close", "Alt+T did not open the roster");
        assert!(
            roster_band(&roster).contains("kid"),
            "no kid row to open:\n{roster}"
        );
        pty.send(b"o").unwrap();
        let thread = repainted_frame_showing(
            &mut pty,
            "Thread: main > kid",
            "o did not open the kid's thread",
        );
        assert!(
            !thread.contains("Esc = close"),
            "the tasks pop-up shows over the thread:\n{thread}"
        );
        pty.send(b"q").unwrap();
        let back = repainted_frame_showing(
            &mut pty,
            "Esc = close",
            "closing the thread did not bring the tasks pop-up back",
        );
        assert!(
            back.contains("\u{250c}Tasks") && roster_band(&back).contains("kid"),
            "the tasks pop-up came back without its title or the kid's row:\n{back}"
        );
        pty.send(b"\x1b").unwrap();
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
            "Enter open  Esc back",
            "Tab did not open the picker",
        );
        let kid_row = row_after(thread_picker_band(&picker), "kid  ").unwrap_or_else(|| {
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
            row_after(frame, "kid  ").is_some_and(|row| row.contains("completed"))
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

    /// The kid's thread, opened while the kid sits in its gated tool and
    /// before it saved that round, shows the call as a running tool row
    /// named after the tool; opening the gate turns the row ok with no
    /// key pressed.
    #[test]
    #[serial]
    fn the_kids_thread_shows_its_running_tool_row_live_and_then_ok() {
        let scene = scene("thr-live-row", kid_in_a_blocking_tool, "");
        let mut pty = spawn_tui(&scene.dir, &scene.log_env());
        engage_and_spawn(&mut pty, &scene);
        wait_for_file(
            &mut pty,
            &scene.dir.join("kid.gated"),
            "the kid's gate tool",
        );

        pty.send(b"\x14").unwrap();
        repainted_frame_showing(
            &mut pty,
            "Thread: root thread",
            "Ctrl+T did not open the pager",
        );
        pty.send(b"\t").unwrap();
        let picker = repainted_frame_showing(
            &mut pty,
            "Enter open  Esc back",
            "Tab did not open the picker",
        );
        assert!(
            row_after(thread_picker_band(&picker), "kid").is_some(),
            "the picker does not list the kid:\n{picker}"
        );
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        let (running, thread) = frame_until(&mut pty, Duration::from_secs(15), |frame| {
            row_after(frame, "kid_gate(").is_some_and(|row| row.contains("running"))
        });
        assert!(
            running,
            "the kid's thread does not show its gated call running:\n{thread}\nlog: {}",
            scene.log.display()
        );

        fs::write(&scene.kid_gate, "").unwrap();
        let (finished, after) = frame_until(&mut pty, Duration::from_secs(15), |frame| {
            row_after(frame, "kid_gate(").is_some_and(|row| row.contains(" ok"))
        });
        assert!(
            finished,
            "the kid's gated call did not turn ok in its open thread:\n{after}"
        );
        pty.send(b"q").unwrap();
        let transcript = leave(&mut pty, &scene);
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
    }

    /// Once the kid has saved its rounds, Ctrl+Down and Enter in its open
    /// thread open its gated call on what the call returned, which the
    /// tool wrote to its output file rather than printed as it ran. The
    /// fake model gives every call one id, so the gated call heads the
    /// thread in the mark call's place.
    #[test]
    #[serial]
    fn the_kids_thread_opens_a_finished_call_on_its_saved_output() {
        let scene = scene("thr-open-row", kid_in_a_blocking_tool, "");
        let mut pty = spawn_tui(&scene.dir, &scene.log_env());
        engage_and_spawn(&mut pty, &scene);
        wait_for_file(
            &mut pty,
            &scene.dir.join("kid.gated"),
            "the kid's gate tool",
        );

        pty.send(b"\x14").unwrap();
        repainted_frame_showing(
            &mut pty,
            "Thread: root thread",
            "Ctrl+T did not open the pager",
        );
        pty.send(b"\t").unwrap();
        repainted_frame_showing(&mut pty, "Threads", "Tab did not open the picker");
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        fs::write(&scene.kid_gate, "").unwrap();
        let (saved, thread) = frame_until(&mut pty, Duration::from_secs(20), |frame| {
            squeezed(frame).contains("kiddonez4p")
        });
        assert!(
            saved,
            "the kid's thread did not read back its saved reply:\n{thread}\nlog: {}",
            scene.log.display()
        );

        pty.send(b"\x1b[1;5B").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        let (opened, open) = frame_until(&mut pty, Duration::from_secs(10), |frame| {
            frame
                .lines()
                .skip_while(|line| !line.contains("\u{25BE} kid_gate("))
                .take(3)
                .any(|line| line.contains("kid gate open k2g"))
        });
        assert!(
            opened,
            "Enter on the focused gated call does not show its output:\n{open}"
        );
        pty.send(b"q").unwrap();
        let transcript = leave(&mut pty, &scene);
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
    }

    /// The kid as a graph agent: node `scout` calls the mark tool, node
    /// `waiter` the gated one.
    fn write_graph_kid(dir: &Path) {
        let kid = dir.join("agents").join("kid");
        let _ = fs::remove_file(kid.join("config.yaml"));
        fs::write(
            kid.join("graph.yaml"),
            "name: kid\n\
             description: Does the kid task in two nodes.\n\
             version: \"1.0\"\n\
             model: kidfake:fake-model\n\
             global_tools:\n\
             \x20 - kid_mark.sh\n\
             \x20 - kid_gate.sh\n\
             start: scout\n\
             nodes:\n\
             \x20 scout:\n\
             \x20   type: llm\n\
             \x20   prompt: \"Scout {{initial_prompt}}\"\n\
             \x20   tools: [kid_mark]\n\
             \x20   next: waiter\n\
             \x20 waiter:\n\
             \x20   type: llm\n\
             \x20   prompt: \"Wait for the gate\"\n\
             \x20   tools: [kid_gate]\n\
             \x20   next: done\n\
             \x20 done: { type: end, output: \"graph kid over g9k\" }\n",
        )
        .unwrap();
    }

    /// A spawned graph agent's thread, opened while its second node sits in
    /// the gated call, shows one row per node it ran, in start order, with
    /// the calls folded under them; opening the running node's row shows
    /// its call, live.
    #[test]
    #[serial]
    fn a_graph_kids_thread_shows_a_row_per_node_with_its_calls_folded_under() {
        let scene = scene(
            "thr-graph-nodes",
            |_| {
                vec![
                    Reply::ToolCall {
                        name: "kid_mark".to_string(),
                        arguments: "{}".to_string(),
                    },
                    Reply::Text("scout done g1n".to_string()),
                    Reply::ToolCall {
                        name: "kid_gate".to_string(),
                        arguments: "{}".to_string(),
                    },
                    Reply::Text("waiter done g2n".to_string()),
                ]
            },
            "",
        );
        write_graph_kid(&scene.dir);
        let mut pty = spawn_tui(&scene.dir, &scene.log_env());
        engage_and_spawn(&mut pty, &scene);
        wait_for_file(
            &mut pty,
            &scene.dir.join("kid.gated"),
            "the kid's gate tool",
        );

        pty.send(b"\x14").unwrap();
        repainted_frame_showing(
            &mut pty,
            "Thread: root thread",
            "Ctrl+T did not open the pager",
        );
        pty.send(b"\t").unwrap();
        repainted_frame_showing(
            &mut pty,
            "Enter open  Esc back",
            "Tab did not open the picker",
        );
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        let (nodes, thread) = frame_until(&mut pty, Duration::from_secs(15), |frame| {
            let scout = frame.find("scout");
            let waiter = frame.find("waiter");
            matches!((scout, waiter), (Some(s), Some(w)) if s < w)
        });
        assert!(
            nodes,
            "the graph kid's thread does not show a `scout` then a `waiter` node row:\n{thread}\nlog: {}",
            scene.log.display()
        );
        assert_eq!(
            thread.matches("kid_gate").count(),
            1,
            "the waiter node's call is not folded under its node row (only its header names it):\n{thread}"
        );

        pty.send(b"\x1b[1;5A").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        let (opened, open) = frame_until(&mut pty, Duration::from_secs(10), |frame| {
            frame.matches("kid_gate").count() >= 2
                && row_after(frame, "kid_gate").is_some_and(|row| row.contains("running"))
        });
        assert!(
            opened,
            "opening the focused node row does not show its running call:\n{open}"
        );

        // The kid finishes and saves: the live rows and the saved session
        // merge into one row per node and one row per call, none doubled.
        fs::write(&scene.kid_gate, "").unwrap();
        let (settled, after) = frame_until(&mut pty, Duration::from_secs(20), |frame| {
            row_after(frame, "node waiter").is_some_and(|row| row.contains("completed"))
        });
        assert!(
            settled,
            "the waiter node did not turn completed in the open thread:\n{after}"
        );
        for (needle, want) in [("node scout", 1), ("node waiter", 1), ("kid_gate", 2)] {
            assert_eq!(
                after.matches(needle).count(),
                want,
                "`{needle}` shows {} times once the kid saved, not {want}:\n{after}",
                after.matches(needle).count()
            );
        }
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
            "Enter open  Esc back",
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
            "Enter open  Esc back",
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
            "Enter open  Esc back",
            "Tab did not open the picker",
        );
        assert!(
            squeezed(thread_picker_band(&picker)).contains("running"),
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
        let child =
            child.unwrap_or_else(|| panic!("no picker row opened the second kid:\n{picker}"));
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
                repainted_frame_showing(
                    pty,
                    "Thread: root thread",
                    "Ctrl+T did not open the pager",
                );
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
    fn one_tool_turn(
        dir: &Path,
        name: &str,
        file: &str,
        tool: &str,
        reply: &str,
    ) -> (Pty, FakeLlm) {
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
        let end =
            repainted_frame_showing(&mut pty, "pq-120", "G did not show the last output line");
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
        let gk_gate_holder =
            std::env::temp_dir().join(format!("thr-gk-gate-{}", std::process::id()));
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
}

mod footer_counts_help_popup_completion_and_scroll {
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
    //! pseudo-terminal, so the module is unix-only, and the cases run one at a
    //! time.
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{Chunk, FakeLlm, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
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
        let (_, idle_frame) =
            frame_until(&mut pty, Duration::from_secs(20), |f| f.contains("agents"));
        let idle = footer(&idle_frame);
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
            question.contains("EscCancelthequestion")
                && question.contains("Ctrl+CCancelthequestion"),
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
        let at = wait_for_repl_prompt(&mut pty, "dry-model)>", 0);
        pty.send(b".ag").unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\t").unwrap();
        // The line editor asks for the cursor position, which a pty never
        // answers, so the completion lands only after its timeout.
        let tabbed = pty
            .wait_for_after(".agent", at, PTY_DEADLINE)
            .unwrap_or_default();
        pty.send(b"\r").unwrap();
        let answered = pty
            .wait_for_after("Usage: .agent", at, REPL_PROMPT_DEADLINE)
            .is_ok();
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
        assert!(
            answered,
            ".agent with no name printed no usage line:\n{out}"
        );

        let usage = find(pty.raw(), b"Usage: .agent", at).unwrap();
        let at = wait_for_repl_prompt(&mut pty, "dry-model)>", usage);
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
        let interrupted = pty.raw().len();
        pty.send(b"\x03").unwrap();
        pty.wait_for_after("To exit, press Ctrl+D", interrupted, REPL_PROMPT_DEADLINE)
            .expect("Ctrl+C did not print the exit hint");
        let hint = find(pty.raw(), b"To exit, press Ctrl+D", interrupted).unwrap();
        wait_for_repl_prompt(&mut pty, "dry-model)>", hint);
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
        read_repaint(pty, from, Duration::from_millis(600));
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
}

mod popup_colours_scroll_hold_and_child_jobs {
    //! Real-process checks written from the TUI's user-facing contract, not
    //! its code, for what the footer, completion and scrolling cases next door
    //! leave open: the command popup paints a command's name and its
    //! description in two different colours, the description in the yellow
    //! the plain REPL's menu uses, and paints neither under `NO_COLOR`; a
    //! wheel notch down scrolls back one row; a view scrolled up holds still
    //! while the reply below it keeps streaming; and the footer's `jobs` count
    //! follows a background job a spawned sub-agent starts. Each case runs the
    //! binary on a pseudo-terminal, so the module is unix-only, and the cases
    //! run one at a time.
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{Chunk, FakeLlm, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
    use serial_test::serial;

    const CTRL_L: &[u8] = b"\x0c";
    const END: &[u8] = b"\x1b[F";
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

    /// A full repaint, which leaves the scroll position alone.
    fn ctrl_l_frame(pty: &mut Pty) -> String {
        pty.read_for(Duration::from_millis(400));
        let from = pty.raw().len();
        pty.send(CTRL_L).unwrap();
        pty.read_for(Duration::from_millis(600));
        strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]))
    }

    /// The topmost transcript row of a full repaint.
    fn top_row(pty: &mut Pty) -> u32 {
        let frame = ctrl_l_frame(pty);
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
        // The status row offers Esc to interrupt only while the turn runs, so
        // its absence means the whole reply has arrived under the held view.
        let deadline = Instant::now() + PTY_DEADLINE;
        loop {
            let frame = ctrl_l_frame(&mut pty);
            if !squeezed(&frame).contains("Esctointerrupt") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the reply never finished:\n{frame}"
            );
        }
        let after = top_row(&mut pty);
        assert_eq!(
            after, held,
            "the scrolled view moved while the reply streamed on: row-{held:03} -> row-{after:03}"
        );
        pty.send(END).unwrap();
        let bottom = ctrl_l_frame(&mut pty);
        let bottom = squeezed(&bottom);
        assert!(
            bottom.contains("row-160") && bottom.contains("allrowssent"),
            "End did not show the end of the reply:\n{bottom}"
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
}

mod scroll_bursts_and_cell_boundaries {
    //! Real-process checks written from the TUI's user-facing contract, not
    //! its code, for scrolling cases the modules next door leave open: a burst
    //! of wheel notches that lands in one read scrolls exactly as many rows as
    //! there were notches, Shift+Down scrolls back one row and End jumps back
    //! to the newest output; and scrolling a notch at a time through a
    //! transcript of several turns never jumps more than one row, even where
    //! one reply ends and the next prompt begins. Each case runs the binary on
    //! a pseudo-terminal, so the module is unix-only, and the cases run one at a
    //! time.
    use std::time::Duration;

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
    use serial_test::serial;

    const CTRL_L: &[u8] = b"\x0c";
    const WHEEL_UP: &[u8] = b"\x1b[<64;20;5M";
    const SHIFT_DOWN: &[u8] = b"\x1b[1;2B";
    const END: &[u8] = b"\x1b[F";

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

    /// The topmost and bottommost transcript rows of a full repaint that
    /// leaves the scroll position alone.
    fn span(pty: &mut Pty) -> (u32, u32) {
        pty.read_for(Duration::from_millis(300));
        let from = pty.raw().len();
        pty.send(CTRL_L).unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]));
        let rows = visible_rows(&frame);
        assert!(
            !rows.is_empty(),
            "Ctrl+L repainted no transcript rows:\n{frame}"
        );
        (*rows.iter().min().unwrap(), *rows.iter().max().unwrap())
    }

    fn rows(range: std::ops::RangeInclusive<u32>) -> String {
        range.map(|n| format!("row-{n:03}\n")).collect()
    }

    #[test]
    #[serial]
    fn a_burst_of_wheel_notches_scrolls_one_row_each_and_end_returns() {
        let dir = fresh_config_dir("wheel-burst");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text(format!("```\n{}```\n", rows(1..=150)))]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(30, 100).unwrap();
        pty.send(b"print the rows\r").unwrap();
        pty.wait_for("row-150", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(1500));
        let (bottom, _) = span(&mut pty);

        pty.send(&WHEEL_UP.repeat(12)).unwrap();
        let (burst, _) = span(&mut pty);
        assert_eq!(
            burst,
            bottom - 12,
            "twelve notches in one write did not scroll twelve rows: row-{bottom:03} -> row-{burst:03}"
        );

        pty.send(SHIFT_DOWN).unwrap();
        let (down, _) = span(&mut pty);
        assert_eq!(down, burst + 1, "Shift+Down did not scroll back one row");

        pty.send(END).unwrap();
        let (end, _) = span(&mut pty);
        assert_eq!(end, bottom, "End did not jump back to the newest output");
        leave(pty);
    }

    #[test]
    #[serial]
    fn notch_by_notch_scrolling_across_turns_never_jumps() {
        let dir = fresh_config_dir("wheel-cells");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::Text(format!("```\n{}```\n", rows(1..=12))),
            Reply::Text(format!("```\n{}```\n", rows(13..=24))),
            Reply::Text(format!("```\n{}```\n", rows(25..=36))),
            Reply::Text(format!("```\n{}```\n", rows(37..=48))),
        ]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(30, 100).unwrap();
        for (n, last) in [
            (1, "row-012"),
            (2, "row-024"),
            (3, "row-036"),
            (4, "row-048"),
        ] {
            pty.send(format!("turn {n}\r").as_bytes()).unwrap();
            pty.wait_for(last, PTY_DEADLINE).unwrap();
            pty.read_for(Duration::from_millis(800));
        }
        let mut at = span(&mut pty);
        let start = at;
        let mut moved_steps = 0;
        for step in 0..30 {
            pty.send(WHEEL_UP).unwrap();
            let next = span(&mut pty);
            let (top_d, bot_d) = (at.0 - next.0, at.1 - next.1);
            assert!(
                next.0 <= at.0 && next.1 <= at.1 && top_d <= 1 && bot_d <= 1,
                "notch {step} jumped: rows {at:?} -> {next:?} (from {start:?})"
            );
            if next != at {
                moved_steps += 1;
            }
            at = next;
            if at.0 == 1 {
                break;
            }
        }
        assert!(
            moved_steps >= 15,
            "thirty notches moved the labelled rows only {moved_steps} times: {start:?} -> {at:?}"
        );
        leave(pty);
    }
}
