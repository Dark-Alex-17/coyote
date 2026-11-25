//! Real-process checks of commands typed during a turn, the welcome
//! screen and the footer, the elapsed time a long turn shows, and the mouse
//! switch. Every module drives the binary through the shared pty harness in
//! `common`, so the file is unix-only.
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
    //! The wordmark above the welcome card at each width, under NO_COLOR,
    //! and the mouse mode on a narrow footer.
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// Two rows of the "coyote" wordmark, squeezed of spaces.
    const MARK_ROWS: [&str; 2] = [r"/_/_\||/_\_/-_)", r"\__\___/\_,\___/\__\___|"];

    fn shows_mark(frame: &str) -> bool {
        let flat = squeezed(frame);
        MARK_ROWS.iter().all(|row| flat.contains(row))
    }

    /// The welcome card has the wordmark above it from a wide terminal
    /// down to 40x20, and leaves it out on one narrower than the wordmark
    /// while keeping the card's version.
    #[test]
    #[serial]
    fn the_welcome_wordmark_shows_down_to_forty_columns_and_leaves_narrower() {
        let dir = fresh_config_dir("welcome-logo");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let version = env!("CARGO_PKG_VERSION");

        // The pty starts at 100x40, so 101 columns forces a full repaint.
        let at = repaint_until(&mut pty, 40, 101, |f| shows_mark(f) && f.contains(version));
        let wide = since(&pty, at);
        assert!(shows_mark(&wide), "no wordmark at 101x40:\n{wide}");
        assert!(wide.contains(version), "no version on the card:\n{wide}");

        let at = repaint_until(&mut pty, 20, 40, |f| shows_mark(f) && f.contains(version));
        let small = since(&pty, at);
        assert!(shows_mark(&small), "no wordmark at 40x20:\n{small}");
        assert!(small.contains(version), "no version at 40x20:\n{small}");

        let at = repaint_showing(&mut pty, 40, 24, &[version]);
        let narrow = since(&pty, at);
        assert!(
            !shows_mark(&narrow),
            "the wordmark is drawn at 24 columns:\n{narrow}"
        );
        assert!(
            squeezed(&narrow).contains(&squeezed(version)),
            "the card lost its version at 24 columns:\n{narrow}"
        );

        let at = repaint_until(&mut pty, 40, 100, shows_mark);
        let back = since(&pty, at);
        assert!(
            shows_mark(&back),
            "the wordmark did not come back at 100 columns:\n{back}"
        );
        quit_and_check(&mut pty);
    }

    /// The wordmark's startup sweep repaints the screen for a moment and
    /// then stops: an idle TUI writes nothing more to the terminal.
    #[test]
    #[serial]
    fn the_welcome_sweep_runs_at_startup_and_then_goes_quiet() {
        let dir = fresh_config_dir("welcome-sweep");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        // What arrives after the first frame is the sweep's repainting.
        let booted = pty.raw().len();
        pty.read_for(std::time::Duration::from_millis(400));
        let painted = pty.raw().len() - booted;
        pty.read_for(std::time::Duration::from_millis(2500));
        let quiet_from = pty.raw().len();
        pty.read_for(std::time::Duration::from_millis(1500));
        let after = pty.raw().len() - quiet_from;
        assert!(
            painted > 0,
            "the sweep repainted nothing after the first frame"
        );
        assert_eq!(
            after,
            0,
            "the idle TUI kept writing after the sweep: {:?}",
            String::from_utf8_lossy(&pty.raw()[quiet_from..])
        );
        quit_and_check(&mut pty);
    }

    /// Under NO_COLOR the welcome screen, wordmark included, carries no
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
        let at = repaint_until(&mut pty, 40, 101, shows_mark);
        let frame = since(&pty, at);
        assert!(shows_mark(&frame), "no wordmark at 101x40:\n{frame}");
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

        let at = repaint_until(&mut pty, 24, 44, |f| f.contains("mouse scroll"));
        let narrow = since(&pty, at);
        assert!(
            narrow.contains("mouse scroll"),
            "the mouse mode is cut at 44 columns:\n{narrow}"
        );
        pty.send(b"\x1bm").unwrap();
        pty.read_for(std::time::Duration::from_millis(600));
        let at = repaint_until(&mut pty, 24, 45, |f| f.contains("mouse select"));
        let switched = since(&pty, at);
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
        let at = repaint_showing(&mut pty, 24, 100, &["fixture_slow(", "Working"]);
        let late = squeezed(&since(&pty, at));
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
        let at = repaint_showing(&mut pty, 24, 100, &["fixture_slow(", "Working"]);
        let frame = squeezed(&since(&pty, at));
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

mod welcome_sweep {
    //! The welcome wordmark's startup sweep as a user sees it: a bright
    //! band that moves over the wordmark for a moment, never under
    //! NO_COLOR, settled at once by the first key or click, and a 40x20
    //! welcome that keeps the wordmark, the version and the model.
    use std::time::{Duration, Instant};

    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// Whether an SGR parameter list sets a white or bright-white
    /// foreground, in any of the spellings a backend may use.
    fn sets_white(params: &[u16]) -> bool {
        params
            .windows(3)
            .any(|w| w == [38, 5, 15] || w == [38, 5, 7])
            || params.iter().any(|&p| p == 37 || p == 97)
    }

    fn white_in(raw: &[u8]) -> bool {
        sgr_params(raw).iter().any(|p| sets_white(p))
    }

    /// The primary device-attributes query a terminal answers at once.
    const DA1_QUERY: &[u8] = b"\x1b[c";
    /// A VT220-class terminal's answer to it.
    const DA1_REPLY: &[u8] = b"\x1b[?62;22c";
    /// What precedes the query when it closes the keyboard-enhancement
    /// probe, which the harness answers itself.
    const KEYBOARD_FLAGS_QUERY: &[u8] = b"\x1b[?u";

    /// Reads in short slices for `window`, answering each device-attributes
    /// query outside the keyboard probe as a real terminal would, and
    /// returns per slice when it ended (since `start`) and the raw range it
    /// added. `answered` is how far the queries have been answered, so none
    /// is answered twice.
    fn slices(
        pty: &mut Pty,
        start: Instant,
        window: Duration,
        answered: &mut usize,
    ) -> Vec<(Duration, usize, usize)> {
        let mut out = Vec::new();
        let end = Instant::now() + window;
        while Instant::now() < end {
            let from = pty.raw().len();
            pty.read_for(Duration::from_millis(20));
            while let Some(at) = find(pty.raw(), DA1_QUERY, *answered) {
                *answered = at + DA1_QUERY.len();
                if !pty.raw()[..at].ends_with(KEYBOARD_FLAGS_QUERY) {
                    pty.send(DA1_REPLY).unwrap();
                }
            }
            out.push((start.elapsed(), from, pty.raw().len()));
        }
        out
    }

    /// Boots the TUI as a terminal that answers its probes would, and
    /// returns once the first frame is on screen: the instant it showed,
    /// the raw offset it starts at, and how far the queries are answered.
    fn boot(pty: &mut Pty) -> (Instant, usize, usize) {
        let mut answered = 0;
        let deadline = Instant::now() + PTY_DEADLINE;
        loop {
            slices(
                pty,
                Instant::now(),
                Duration::from_millis(20),
                &mut answered,
            );
            if let Some(at) = find(pty.raw(), ENTER_ALT, 0)
                && squeezed(&since(pty, at)).contains("nosession")
            {
                return (Instant::now(), at, answered);
            }
            assert!(
                Instant::now() < deadline,
                "the TUI never drew its first frame"
            );
        }
    }

    /// On start, with a terminal that answers its probes, the wordmark is
    /// repainted several times with a white band for a moment (long enough
    /// to be seen, about a second and a half), and then the band is gone
    /// and the screen goes still.
    #[test]
    #[serial]
    fn the_sweep_paints_a_moving_band_for_a_moment_and_ends() {
        let dir = fresh_config_dir("sweep-moves");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        let (shown, first, mut answered) = boot(&mut pty);
        let seen = slices(&mut pty, shown, Duration::from_millis(3000), &mut answered);
        let frames: Vec<Duration> = std::iter::once((Duration::ZERO, first, seen[0].1))
            .chain(seen.iter().copied())
            .filter(|(_, from, to)| white_in(&pty.raw()[*from..*to]))
            .map(|(at, _, _)| at)
            .collect();
        assert!(
            frames.len() >= 3,
            "the band was painted in {} slices from the first frame on, not a sweep: {frames:?}",
            frames.len()
        );
        let last = *frames.last().unwrap();
        assert!(
            last >= Duration::from_millis(500),
            "the band was gone {last:?} after the first frame: too short to be seen"
        );
        assert!(
            last <= Duration::from_millis(2500),
            "the band was still painted {last:?} after the first frame"
        );
        let quiet_from = pty.raw().len();
        pty.read_for(Duration::from_millis(1000));
        assert_eq!(
            pty.raw().len(),
            quiet_from,
            "the screen kept changing after the sweep"
        );
        let settled = repaint(&mut pty, 40, 101);
        assert!(
            !white_in(&pty.raw()[settled..]),
            "the settled wordmark still carries the band"
        );
        quit_and_check(&mut pty);
    }

    /// Under NO_COLOR the sweep does not run: from the first frame on the
    /// idle screen is still, and no frame carries the band.
    #[test]
    #[serial]
    fn no_color_starts_still() {
        let dir = fresh_config_dir("sweep-no-color");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[("NO_COLOR", "1")]);
        let (_, _, mut answered) = boot(&mut pty);
        slices(
            &mut pty,
            Instant::now(),
            Duration::from_millis(150),
            &mut answered,
        );
        let quiet_from = pty.raw().len();
        slices(
            &mut pty,
            Instant::now(),
            Duration::from_millis(1000),
            &mut answered,
        );
        assert_eq!(
            pty.raw().len(),
            quiet_from,
            "the NO_COLOR screen kept changing at startup: {:?}",
            String::from_utf8_lossy(&pty.raw()[quiet_from..])
        );
        assert!(!white_in(pty.raw()), "a NO_COLOR frame carried the band");
        quit_and_check(&mut pty);
    }

    /// Waits for the band to show, then asserts the screen goes still soon
    /// after an input sent at that moment, well before the sweep would have
    /// run its course.
    fn settles_after(pty: &mut Pty, input: &[u8], what: &str) {
        let (_, first, mut answered) = boot(pty);
        let by = Instant::now() + Duration::from_millis(600);
        while !white_in(&pty.raw()[first..]) && Instant::now() < by {
            slices(
                pty,
                Instant::now(),
                Duration::from_millis(20),
                &mut answered,
            );
        }
        assert!(
            white_in(&pty.raw()[first..]),
            "the sweep was not running after the first frame, so {what} settles nothing"
        );
        pty.send(input).unwrap();
        slices(
            pty,
            Instant::now(),
            Duration::from_millis(250),
            &mut answered,
        );
        let quiet_from = pty.raw().len();
        slices(
            pty,
            Instant::now(),
            Duration::from_millis(800),
            &mut answered,
        );
        assert_eq!(
            pty.raw().len(),
            quiet_from,
            "the sweep kept running after {what}: {:?}",
            String::from_utf8_lossy(&pty.raw()[quiet_from..])
        );
    }

    #[test]
    #[serial]
    fn the_first_key_settles_the_sweep() {
        let dir = fresh_config_dir("sweep-key");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        settles_after(&mut pty, b"x", "a key");
        quit_declining_save(&mut pty);
    }

    #[test]
    #[serial]
    fn a_click_settles_the_sweep() {
        let dir = fresh_config_dir("sweep-click");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        settles_after(&mut pty, b"\x1b[<0;10;3M\x1b[<0;10;3m", "a click");
        quit_and_check(&mut pty);
    }

    /// At 40x20 the welcome keeps the wordmark above the box, and the box
    /// keeps its border, the version, the model and the help pointer while
    /// leaving the interrupt tip out.
    #[test]
    #[serial]
    fn forty_by_twenty_keeps_the_wordmark_version_and_model() {
        let dir = fresh_config_dir("sweep-40x20");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(2500));
        let at = repaint_showing(
            &mut pty,
            20,
            40,
            &[
                r"\__\___/\_,\___/\__\___|",
                concat!("coyote ", env!("CARGO_PKG_VERSION")),
            ],
        );
        let frame = since(&pty, at);
        let flat = squeezed(&frame);
        assert!(
            flat.contains(r"\__\___/\_,\___/\__\___|"),
            "no wordmark at 40x20:\n{frame}"
        );
        assert!(
            flat.contains(&squeezed(concat!("coyote ", env!("CARGO_PKG_VERSION")))),
            "no version at 40x20:\n{frame}"
        );
        assert!(
            flat.contains("model:"),
            "no model in the box at 40x20:\n{frame}"
        );
        assert!(
            flat.contains(&squeezed("Type \".help\" for additional help.")),
            "no help pointer at 40x20:\n{frame}"
        );
        assert!(
            !flat.contains(&squeezed("Tip: Press Ctrl-C")),
            "the interrupt tip shows at 40x20:\n{frame}"
        );
        let mark_rows = painted_at(pty.raw(), b"\\__\\___/", at);
        let box_rows = painted_at(pty.raw(), "┌".as_bytes(), at);
        assert!(
            matches!((mark_rows.first(), box_rows.first()), (Some(m), Some(b)) if m.0 < b.0),
            "the wordmark is not above the box at 40x20: mark {mark_rows:?} box {box_rows:?}\n{frame}"
        );
        quit_and_check(&mut pty);
    }

    #[test]
    #[serial]
    fn a_paste_settles_the_sweep() {
        let dir = fresh_config_dir("sweep-paste");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        settles_after(&mut pty, b"\x1b[200~pasted p4s\x1b[201~", "a paste");
        quit_declining_save(&mut pty);
    }

    /// A resize while the band moves does not settle it: the band is
    /// still painted well after the resize's own repaint.
    #[test]
    #[serial]
    fn a_resize_does_not_settle_the_sweep() {
        let dir = fresh_config_dir("sweep-resize");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        let (_, first, mut answered) = boot(&mut pty);
        let by = Instant::now() + Duration::from_millis(600);
        while !white_in(&pty.raw()[first..]) && Instant::now() < by {
            slices(
                &mut pty,
                Instant::now(),
                Duration::from_millis(20),
                &mut answered,
            );
        }
        assert!(
            white_in(&pty.raw()[first..]),
            "the sweep was not running after the first frame"
        );
        pty.resize(40, 101).unwrap();
        slices(
            &mut pty,
            Instant::now(),
            Duration::from_millis(250),
            &mut answered,
        );
        let later = pty.raw().len();
        slices(
            &mut pty,
            Instant::now(),
            Duration::from_millis(600),
            &mut answered,
        );
        assert!(
            white_in(&pty.raw()[later..]),
            "the band stopped with the resize: {:?}",
            String::from_utf8_lossy(&pty.raw()[later..])
        );
        quit_and_check(&mut pty);
    }

    /// The sweep reveals the wordmark: on the first frame the letters the
    /// band has yet to reach are dim, and once it has run the wordmark is
    /// bold and nothing of it is dim.
    #[test]
    #[serial]
    fn the_sweep_reveals_dim_letters_and_settles_bold() {
        let dir = fresh_config_dir("sweep-reveal");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        let (_, first, mut answered) = boot(&mut pty);
        let first_end = pty.raw().len();
        // Dim is set once at the row's start, so the state is read from
        // the frame's start up to the wordmark's right end.
        let right_end =
            find(&pty.raw()[..first_end], b"-_)", first).expect("no wordmark on the first frame");
        let dim = sgr_params(&pty.raw()[first..right_end])
            .iter()
            .fold(false, |dim, params| dim_after(params).unwrap_or(dim));
        assert!(
            dim,
            "the wordmark's right end is not dim on the first frame: {:?}",
            String::from_utf8_lossy(&pty.raw()[first..first_end])
        );
        slices(
            &mut pty,
            Instant::now(),
            Duration::from_millis(2500),
            &mut answered,
        );
        let settled = repaint_until(&mut pty, 40, 101, |f| f.contains("-_)"));
        let raw = pty.raw();
        let at = find(raw, b"-_)", settled).expect("no wordmark after the sweep");
        let params = sgr_params(&raw[settled..at]);
        let dim = params
            .iter()
            .fold(false, |dim, p| dim_after(p).unwrap_or(dim));
        // Bold is `1` on its own; `22` or `0` resets it.
        let bold = params.iter().fold(false, |bold, p| {
            let mut state = None;
            let mut i = 0;
            while i < p.len() {
                match p[i] {
                    38 | 48 | 58 => {
                        i += match p.get(i + 1) {
                            Some(5) => 3,
                            Some(2) => 5,
                            _ => 1,
                        };
                        continue;
                    }
                    1 => state = Some(true),
                    0 | 22 => state = Some(false),
                    _ => {}
                }
                i += 1;
            }
            state.unwrap_or(bold)
        });
        let shown = String::from_utf8_lossy(&raw[settled..at]).into_owned();
        assert!(!dim, "the settled wordmark is still dim: {shown:?}");
        assert!(bold, "the settled wordmark is not bold: {shown:?}");
        quit_and_check(&mut pty);
    }

    /// A short terminal gives up the box's hints before the wordmark: at
    /// every height the box keeps its border, the version and the model,
    /// and once there is room for the wordmark over the bare box (eight
    /// transcript rows, 80x16) the wordmark shows.
    #[test]
    #[serial]
    fn a_short_terminal_drops_the_hints_before_the_wordmark() {
        let dir = fresh_config_dir("sweep-short");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(2500));
        let version = squeezed(concat!("coyote ", env!("CARGO_PKG_VERSION")));
        let mut seen = Vec::new();
        for (i, rows) in (12u16..=20).enumerate() {
            // Alternate the width so no resize is coalesced away.
            let cols = if i % 2 == 0 { 80 } else { 81 };
            let at = repaint_until(&mut pty, rows, cols, |f| {
                squeezed(f).contains(&version) && f.contains("model:") && f.contains('┌')
            });
            let frame = since(&pty, at);
            let flat = squeezed(&frame);
            let mark = flat.contains(r"\__\___/\_,\___/\__\___|");
            let hint = flat.contains(&squeezed("Empty prompt"))
                || flat.contains(&squeezed("Type \".help\""));
            assert!(
                flat.contains(&version) && flat.contains("model:") && frame.contains('┌'),
                "at {cols}x{rows} the box lost its border, version or model:\n{frame}"
            );
            if rows >= 16 {
                assert!(
                    mark,
                    "at {cols}x{rows} the wordmark is gone (hints shown: {hint}):\n{frame}"
                );
            }
            seen.push((rows, mark, hint));
        }
        eprintln!("rows/wordmark/hint: {seen:?}");
        quit_and_check(&mut pty);
    }
}

mod wait_race {
    //! A line sent mid-turn ends a wait only while the awaited work is
    //! still running: a job already finished when the line is waiting is
    //! reported ready, and the line reaches the model with that result.
    use std::fs;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::tui_harness::*;
    use serial_test::serial;

    fn quick_tool(done: &std::path::Path) -> String {
        format!(
            "#!/usr/bin/env bash\n\
             \n\
             # @describe Finish at once.\n\
             \n\
             # @env LLM_OUTPUT=/dev/stdout The output path\n\
             \n\
             main() {{\n\
             \x20   echo \"own result r8k\" >> \"$LLM_OUTPUT\"\n\
             \x20   touch \"{}\"\n\
             }}\n",
            done.display()
        )
    }

    /// The job finishes, a line is sent while the model is still deciding
    /// to wait, and then the model calls `job__wait`: the wait reports the
    /// job ready rather than `user_message`, and the same request carries
    /// the line, sent once.
    #[test]
    #[serial]
    fn a_wait_on_a_finished_job_reports_it_ready_with_a_line_waiting() {
        let dir = fresh_config_dir("wait-race-ready");
        let _guard = TempDirGuard(dir.clone());
        let done = dir.join("job.done");
        let gate = dir.join("gate");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "job__start".to_string(),
                arguments: r#"{"tool":"fixture_quick","arguments":{}}"#.to_string(),
            },
            Reply::After {
                gate: gate.clone(),
                reply: Box::new(Reply::ToolCall {
                    name: "job__wait".to_string(),
                    arguments: "{}".to_string(),
                }),
            },
            Reply::Text("waited r8k".to_string()),
        ]);
        write_fake_llm_config_with(&dir, &fake, "enabled_tools:\n  - fixture_quick\n");
        write_tool(&dir, "fixture_quick.sh", &quick_tool(&done));
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        pty.send(b"start it then wait\r").unwrap();
        wait_for_file(&mut pty, &done, "the job's done file");
        let deadline = Instant::now() + PTY_DEADLINE;
        while fake.served() < 2 {
            assert!(Instant::now() < deadline, "the model was never asked again");
            pty.read_for(Duration::from_millis(100));
        }
        // Let the job's process exit and its task end.
        pty.read_for(Duration::from_millis(800));
        pty.send(b"sent with it r8k\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        fs::write(&gate, b"").unwrap();
        wait_for_squeezed_after(&mut pty, "waited r8k", 0, "the reply after the wait");

        let bodies = fake.bodies();
        let after_wait = bodies.get(2).expect("no request followed the wait");
        assert!(
            after_wait.contains(r#"\"status\":\"ready\""#),
            "the finished job's wait did not report it ready: {after_wait}"
        );
        assert!(
            !after_wait.contains(r#"\"status\":\"user_message\""#),
            "the wait gave up a finished job for the line: {after_wait}"
        );
        assert_eq!(
            after_wait.matches("sent with it r8k").count(),
            1,
            "the line did not reach the model once with the result: {after_wait}"
        );
        quit_clearing_any_draft(&mut pty);
    }
}

mod mouse {
    //! A mouse handed to the terminal with `Alt+M` stays handed over when
    //! the screen comes back from a `!` command.
    use std::time::Duration;

    use super::common::tui_harness::*;
    use serial_test::serial;

    const ALT_M: &[u8] = b"\x1bm";

    #[test]
    #[serial]
    fn the_mouse_stays_with_the_terminal_after_a_bang_command() {
        let dir = fresh_config_dir("mouse-after-bang");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(HOTKEY_GAP);
        let off_at = pty.raw().len();
        pty.send(ALT_M).unwrap();
        pty.wait_for_raw(b"\x1b[?1000l", off_at, PTY_DEADLINE)
            .unwrap_or_else(|e| panic!("Alt+M did not switch the mouse reports off: {e}"));

        let bang_at = pty.raw().len();
        pty.send(b"!echo bang-mouse-m5b\r").unwrap();
        let left = pty
            .wait_for_raw(LEAVE_ALT, bang_at, PTY_DEADLINE)
            .expect("`!echo` did not release the terminal");
        let re_entered = wait_for_re_entry(&mut pty, left);
        pty.read_for(Duration::from_millis(800));
        let back = String::from_utf8_lossy(&pty.raw()[re_entered..]).into_owned();
        assert!(
            !back.contains("\x1b[?1000h"),
            "the return from `!` took the mouse back: {back:?}"
        );
        let at = repaint_until(&mut pty, 24, 101, |f| f.contains("mouse select"));
        let frame = since(&pty, at);
        assert!(
            frame.contains("mouse select"),
            "the footer no longer says `mouse select` after `!`:\n{frame}"
        );
        quit_and_check(&mut pty);
    }
}
