//! Real-process checks of commands typed during a turn, the welcome
//! screen and the footer, and the elapsed time a long turn shows. Every
//! module drives the binary through the shared pty harness in `common`,
//! so the file is unix-only.
#![cfg(unix)]

mod common;

mod mid_turn_commands {
    //! What a command typed while the model streams does: a setting
    //! reaches the next queued line's request, an editor command opens at
    //! once, and a setting with side effects waits for the turn.
    use std::fs;
    use std::time::Duration;

    use super::common::fake_llm::{Chunk, FakeLlm, Reply};
    use super::common::tui_harness::*;
    use serial_test::serial;

    fn temperature_of(body: &str) -> Option<f64> {
        let json: serde_json::Value = serde_json::from_str(body).ok()?;
        json.get("temperature")?.as_f64()
    }

    /// A line queued during a turn and a `.set` typed after it: when the
    /// turn ends, the setting is in place before the queued line goes out,
    /// so the queued line's request carries it and the held turn's did not.
    #[test]
    #[serial]
    fn a_dot_set_typed_after_a_queued_line_reaches_that_lines_request() {
        let dir = fresh_config_dir("set-before-queued");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![
            Reply::Streamed(vec![
                Chunk::Text("part one sq1 ".to_string()),
                Chunk::WaitFor(release.clone()),
                Chunk::Text("reply one sq1".to_string()),
            ]),
            Reply::Text("reply two sq1".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"first prompt sq1\r").unwrap();
        wait_for_squeezed_after(&mut pty, "part one sq1", turn_at, "the held stream");
        pty.read_for(Duration::from_millis(300));

        let queued_at = pty.raw().len();
        pty.send(b"second prompt sq1\r").unwrap();
        wait_for_squeezed_after(&mut pty, "pending:", queued_at, "the queued line");
        let set_at = pty.raw().len();
        pty.send(b".set temperature 0.5\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "applies from the next turn",
            set_at,
            "the confirmation of `.set`",
        );

        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(&mut pty, "reply two sq1", set_at, "the queued turn");
        pty.read_for(Duration::from_millis(500));

        let bodies = fake.bodies();
        assert_eq!(bodies.len(), 2, "{bodies:?}");
        assert_ne!(
            temperature_of(&bodies[0]),
            Some(0.5),
            "the held turn's request already carried the setting: {}",
            bodies[0]
        );
        assert_eq!(
            temperature_of(&bodies[1]),
            Some(0.5),
            "the queued line went out without the setting typed during the turn: {}",
            bodies[1]
        );
        quit_and_check(&mut pty);
    }

    /// `.edit config` typed while the model streams opens the editor at
    /// once, before the turn ends, and never joins the queue.
    #[test]
    #[serial]
    fn a_dot_edit_config_typed_mid_stream_opens_the_editor_at_once() {
        let dir = fresh_config_dir("edit-mid-stream");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let launched = dir.join("editor-launched");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("part one ed2 ".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("reply one ed2".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let editor = recording_editor(&dir, &launched);
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"first prompt ed2\r").unwrap();
        wait_for_squeezed_after(&mut pty, "part one ed2", turn_at, "the held stream");
        pty.read_for(Duration::from_millis(300));

        let edit_at = pty.raw().len();
        pty.send(b".edit config\r").unwrap();
        wait_for_file(&mut pty, &launched, "the editor launched by `.edit config`");
        wait_for_squeezed_after(
            &mut pty,
            "Remember to restart",
            edit_at,
            "the reminder after `.edit config`",
        );
        assert_eq!(fake.served(), 1);
        let shown = since(&pty, edit_at);
        assert!(
            !squeezed(&shown).contains("replyoneed2"),
            "the turn ended before the editor opened and reported back:\n{shown}"
        );
        assert!(
            !shown.contains("pending:"),
            "`.edit config` was queued behind the turn:\n{shown}"
        );
        let opened = fs::read_to_string(&launched).unwrap();
        assert!(
            opened.contains("config.yaml"),
            "the editor was not handed the config file: {opened}"
        );

        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(&mut pty, "reply one ed2", edit_at, "the end of the turn");
        pty.read_for(Duration::from_millis(500));
        quit_and_check(&mut pty);
    }

    /// `.set` for a key that saves the RAG is queued like a prompt during
    /// a turn: it shows in the `pending:` strip and is not confirmed for
    /// the next turn.
    #[test]
    #[serial]
    fn a_side_effecting_dot_set_typed_mid_stream_is_queued() {
        let dir = fresh_config_dir("set-queued-key");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("part one rq6 ".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("reply one rq6".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"first prompt rq6\r").unwrap();
        wait_for_squeezed_after(&mut pty, "part one rq6", turn_at, "the held stream");
        pty.read_for(Duration::from_millis(300));

        let set_at = pty.raw().len();
        pty.send(b".set rag_top_k 3\r").unwrap();
        wait_for_squeezed_after(&mut pty, "pending:", set_at, "the queued `.set rag_top_k`");
        pty.read_for(Duration::from_millis(500));
        let shown = since(&pty, set_at);
        assert!(
            !squeezed(&shown).contains(&squeezed("applies from the next turn")),
            "a RAG-saving `.set` was taken for the next turn instead of queued:\n{shown}"
        );
        assert_eq!(fake.served(), 1, "`.set` reached the model as a prompt");

        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(&mut pty, "reply one rq6", set_at, "the end of the turn");
        pty.read_for(Duration::from_millis(800));
        quit_clearing_any_draft(&mut pty);
    }
}

mod welcome_and_footer {
    //! The coyote beside the welcome card at each width, under NO_COLOR,
    //! and the mouse mode on a narrow footer.
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// Two rows of the coyote's head, squeezed of spaces.
    const HEAD_ROWS: [&str; 2] = ["<____/", "`---.___/"];

    fn shows_head(frame: &str) -> bool {
        let flat = squeezed(frame);
        HEAD_ROWS.iter().all(|row| flat.contains(row))
    }

    /// The welcome card has the coyote's head beside it at 80 columns and
    /// wider, and leaves it out below 80 while keeping the card's version.
    #[test]
    #[serial]
    fn the_welcome_coyote_shows_wide_and_leaves_narrow() {
        let dir = fresh_config_dir("welcome-logo");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let version = env!("CARGO_PKG_VERSION");

        // The pty starts at 100x40, so 101 columns forces a full repaint.
        let wide = repainted_frame_after_resize(&mut pty, 40, 101);
        assert!(
            shows_head(&wide),
            "no coyote beside the card at 101x40:\n{wide}"
        );
        assert!(wide.contains(version), "no version on the card:\n{wide}");

        let eighty = repainted_frame_after_resize(&mut pty, 40, 80);
        assert!(
            shows_head(&eighty),
            "no coyote at exactly 80 columns:\n{eighty}"
        );

        let narrow = repainted_frame_after_resize(&mut pty, 40, 79);
        assert!(
            !shows_head(&narrow),
            "the coyote is drawn at 79 columns:\n{narrow}"
        );
        assert!(
            narrow.contains(version),
            "the card lost its version at 79 columns:\n{narrow}"
        );

        let back = repainted_frame_after_resize(&mut pty, 40, 100);
        assert!(
            shows_head(&back),
            "the coyote did not come back at 100 columns:\n{back}"
        );
        quit_and_check(&mut pty);
    }

    /// Under NO_COLOR the welcome screen, coyote included, carries no
    /// foreground or background color.
    #[test]
    #[serial]
    fn no_color_draws_the_welcome_coyote_without_color() {
        let dir = fresh_config_dir("welcome-no-color");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[("NO_COLOR", "1")]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        // The pty starts at 100x40, so 101 columns forces a full repaint.
        let at = repaint(&mut pty, 40, 101);
        let frame = since(&pty, at);
        assert!(shows_head(&frame), "no coyote at 101x40:\n{frame}");
        let colored: Vec<Vec<u16>> = sgr_params(&pty.raw()[at..])
            .into_iter()
            .filter(|p| {
                p.iter().any(|&n| {
                    (30..=37).contains(&n)
                        || (40..=47).contains(&n)
                        || (90..=97).contains(&n)
                        || (100..=107).contains(&n)
                        || n == 38
                        || n == 48
                })
            })
            .collect();
        assert!(
            colored.is_empty(),
            "NO_COLOR frame still sets colors: {colored:?}\n{frame}"
        );
        quit_and_check(&mut pty);
    }

    /// On a narrow terminal the footer cuts its left side first, so the
    /// mouse mode still shows whole, before and after Alt+M.
    #[test]
    #[serial]
    fn a_narrow_footer_keeps_the_mouse_mode_whole() {
        let dir = fresh_config_dir("narrow-footer-mouse");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        let narrow = repainted_frame_after_resize(&mut pty, 24, 44);
        assert!(
            narrow.contains("mouse scroll"),
            "the mouse mode is cut at 44 columns:\n{narrow}"
        );
        pty.send(b"\x1bm").unwrap();
        pty.read_for(std::time::Duration::from_millis(600));
        let switched = repainted_frame_after_resize(&mut pty, 24, 45);
        assert!(
            switched.contains("mouse select"),
            "after Alt+M the narrow footer does not show `mouse select` whole:\n{switched}"
        );
        pty.send(b"\x1bm").unwrap();
        pty.read_for(std::time::Duration::from_millis(600));
        quit_and_check(&mut pty);
    }
}

mod elapsed_format {
    //! The elapsed time a running tool row and the status row show.
    use std::fs;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::tui_harness::*;
    use serial_test::serial;

    const SLOW_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Wait until released, a minute at most and a bit.\n\
    # @option --release! The file that ends the wait.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   local release\n\
    \x20   release=\"$(jq -r '.release' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   for _ in $(seq 1 90); do [ -e \"$release\" ] && break; sleep 1; done\n\
    \x20   echo \"slept el4\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// Past a minute, the running tool row and the Working status both
    /// count in `<m>m <s>s`, never in bare seconds such as `61s`.
    #[test]
    #[serial]
    fn a_turn_past_a_minute_reads_minutes_and_seconds() {
        let dir = fresh_config_dir("elapsed-minute");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_slow".to_string(),
                arguments: format!("{{\"release\": \"{}\"}}", release.display()),
            },
            Reply::Text("slow done el4".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_slow.sh", SLOW_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"run the slow tool\r").unwrap();
        wait_for_squeezed_after(&mut pty, "fixture_slow(", turn_at, "the running row");
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(64) {
            pty.read_for(Duration::from_millis(500));
        }
        // A full repaint, since a cell-diffing frame only rewrites the
        // digits that changed.
        let late_at = pty.raw().len();
        let late = squeezed(&repainted_frame_after_resize(&mut pty, 24, 100));
        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(&mut pty, "slow done el4", late_at, "the reply");

        assert!(
            has_minutes_and_seconds(&late),
            "no `1m<s>s` elapsed was painted past a minute:\n{late}"
        );
        assert!(
            late.matches("1m").count() >= 2,
            "the running row and the Working status do not both read minutes:\n{late}"
        );
        for bare in [
            "61s", "62s", "63s", "64s", "65s", "66s", "67s", "68s", "69s",
        ] {
            let at = late.find(bare);
            let preceded_by_digit =
                at.is_some_and(|i| i > 0 && late.as_bytes()[i - 1].is_ascii_digit());
            assert!(
                at.is_none() || preceded_by_digit,
                "a bare `{bare}` was painted past a minute:\n{late}"
            );
        }
        quit_and_check(&mut pty);
    }

    /// `1m<digits>s` anywhere in the squeezed text.
    fn has_minutes_and_seconds(text: &str) -> bool {
        text.match_indices("1m").any(|(i, _)| {
            let rest = &text[i + 2..];
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            digits > 0 && rest[digits..].starts_with('s')
        })
    }

    /// While a tool runs, the status row is the Working style with a
    /// seconds count, not a `Running <tool>` line.
    #[test]
    #[serial]
    fn a_running_tool_shows_working_not_running_the_tool() {
        let dir = fresh_config_dir("working-status");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_slow".to_string(),
                arguments: format!("{{\"release\": \"{}\"}}", release.display()),
            },
            Reply::Text("slow done ws5".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_slow.sh", SLOW_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"run the slow tool\r").unwrap();
        wait_for_squeezed_after(&mut pty, "fixture_slow(", turn_at, "the running row");
        pty.read_for(Duration::from_secs(3));
        let during = since(&pty, turn_at);
        let frame = squeezed(&repainted_frame_after_resize(&mut pty, 24, 100));
        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(&mut pty, "slow done ws5", turn_at, "the reply");

        assert!(
            frame.contains("Working"),
            "no Working status while the tool runs:\n{frame}"
        );
        assert!(
            !squeezed(&during).contains("Runningfixture_slow")
                && !frame.contains("Runningfixture_slow"),
            "the old `Running <tool>` status is still painted:\n{frame}"
        );
        let has_seconds = ["2s", "3s", "4s", "5s", "6s"]
            .iter()
            .any(|s| frame.contains(s));
        assert!(
            has_seconds,
            "no `<n>s` elapsed while the tool runs:\n{frame}"
        );
        quit_and_check(&mut pty);
    }
}
