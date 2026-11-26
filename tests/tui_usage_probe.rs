//! Real-process checks of commands typed during a turn, the welcome
//! screen and the footer, the elapsed time a long turn shows, the mouse
//! reports, and undo and rename in a session. Every module drives the binary
//! through the shared pty harness in `common`, so the file is unix-only.
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
    //! The welcome wordmark at each width and under NO_COLOR.
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// Two rows of the small "coyote" wordmark, squeezed of spaces.
    const MARK_ROWS: [&str; 2] = [r"/_/_\||/_\_/-_)", r"\__\___/\_,\___/\__\___|"];

    /// Two rows of the wide "COYOTE" wordmark, squeezed of spaces.
    const BIG_MARK_ROWS: [&str; 2] = [
        "██╔════╝██╔═══██╗╚██╗██╔╝██╔═══██╗╚══██╔══╝██╔════╝",
        "╚██████╗╚██████╔╝██║╚██████╔╝██║███████╗",
    ];

    fn shows_mark(frame: &str) -> bool {
        let flat = squeezed(frame);
        MARK_ROWS.iter().all(|row| flat.contains(row))
    }

    fn shows_big_mark(frame: &str) -> bool {
        let flat = squeezed(frame);
        BIG_MARK_ROWS.iter().all(|row| flat.contains(row))
    }

    /// The welcome shows the wide wordmark from 54 columns, the small one
    /// below that down to 40x20, and neither on a terminal narrower than
    /// the small one, keeping the version under each and on its own.
    #[test]
    #[serial]
    fn the_welcome_wordmark_narrows_to_the_small_one_and_then_leaves() {
        let dir = fresh_config_dir("welcome-logo");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let version = concat!("v", env!("CARGO_PKG_VERSION"));
        // The wordmark builds in over the first frames; once the startup
        // animation is over each repaint shows it whole.
        pty.read_for(std::time::Duration::from_millis(3700));

        // The pty starts at 100x40, so 101 columns forces a full repaint.
        let at = repaint_until(&mut pty, 40, 101, |f| {
            shows_big_mark(f) && f.contains(version)
        });
        let wide = since(&pty, at);
        assert!(shows_big_mark(&wide), "no wide wordmark at 101x40:\n{wide}");
        assert!(!shows_mark(&wide), "the small wordmark at 101x40:\n{wide}");
        assert!(wide.contains(version), "no version at 101x40:\n{wide}");

        let at = repaint_until(&mut pty, 24, 53, |f| shows_mark(f) && !shows_big_mark(f));
        let fallback = since(&pty, at);
        assert!(
            shows_mark(&fallback) && !shows_big_mark(&fallback),
            "no small wordmark in place of the wide one at 53x24:\n{fallback}"
        );

        let at = repaint_until(&mut pty, 20, 40, |f| shows_mark(f) && f.contains(version));
        let small = since(&pty, at);
        assert!(shows_mark(&small), "no wordmark at 40x20:\n{small}");
        assert!(small.contains(version), "no version at 40x20:\n{small}");

        let at = repaint_showing(&mut pty, 40, 24, &[version]);
        let narrow = since(&pty, at);
        assert!(
            !shows_mark(&narrow) && !shows_big_mark(&narrow),
            "the wordmark is drawn at 24 columns:\n{narrow}"
        );
        assert!(
            squeezed(&narrow).contains(version),
            "the welcome lost its version at 24 columns:\n{narrow}"
        );

        let at = repaint_until(&mut pty, 40, 100, shows_big_mark);
        let back = since(&pty, at);
        assert!(
            shows_big_mark(&back),
            "the wordmark did not come back at 100 columns:\n{back}"
        );
        quit_and_check(&mut pty);
    }

    /// The wordmark's startup animation repaints the screen for a few
    /// seconds and then stops: an idle TUI writes nothing more to the
    /// terminal.
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
        pty.read_for(std::time::Duration::from_millis(3500));
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
        let at = repaint_until(&mut pty, 40, 101, shows_big_mark);
        let frame = since(&pty, at);
        assert!(shows_big_mark(&frame), "no wordmark at 101x40:\n{frame}");
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
}

mod main_screen_popups_and_chrome {
    //! What a first-time user sees on the main screen: the idle hints, the
    //! threads and todo pop-ups opened with their chords and closed back to
    //! the composer, the `your turn` label around a turn, the context label
    //! and the dividers of a session and a role, and the palette's order.
    use std::fs;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{Chunk, FakeLlm, Reply};
    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    const ALT_S: &[u8] = b"\x1bs";
    const ALT_X: &[u8] = b"\x1bx";

    fn boot_dry_run(scenario: &str, extra: &str) -> (Pty, TempDirGuard, std::path::PathBuf) {
        let dir = fresh_config_dir(scenario);
        let guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, extra);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(HOTKEY_GAP);
        (pty, guard, dir)
    }

    /// Repaints until `shows` holds or the deadline lapses; the last frame.
    fn frame_where(pty: &mut Pty, shows: impl Fn(&str) -> bool) -> String {
        let at = repaint_until(pty, 40, 100, |frame| shows(&squeezed(frame)));
        since(pty, at)
    }

    /// What the composer's top rule starts with: the squeezed text just
    /// before the run of rule characters that ends in `your turn`.
    fn label_before_your_turn(frame: &str) -> String {
        let flat = squeezed(frame);
        let Some(at) = flat.rfind("yourturn") else {
            return String::new();
        };
        flat[..at].trim_end_matches('\u{2500}').to_string()
    }

    const EMPTY_COMPOSER: &str = "Typeapromptora.command";

    /// The idle hints name the thread picker's chord; Alt+S opens the
    /// `Threads` pop-up with the root thread over the main screen, and Esc
    /// goes back to the composer, which then quits on Ctrl+D (a pop-up
    /// still open would take the Ctrl+D as close instead).
    #[test]
    #[serial]
    fn alt_s_on_the_main_screen_opens_the_threads_popup_and_esc_goes_back() {
        let (mut pty, _guard, _dir) = boot_dry_run("probe-alt-s", "");
        let idle = frame_where(&mut pty, |f| f.contains("Alt+S=threads"));
        assert!(
            squeezed(&idle).contains("?=keysAlt+T=tasksAlt+S=threadsCtrl+X=todo"),
            "the idle hints do not read `? = keys  Alt+T = tasks  Alt+S = threads  Ctrl+X = todo`:\n{idle}"
        );

        pty.send(ALT_S).unwrap();
        pty.read_for(Duration::from_millis(500));
        let open = frame_where(&mut pty, |f| f.contains("Threads") && f.contains("main"));
        let flat = squeezed(&open);
        assert!(
            flat.contains("\u{250c}Threads") && flat.contains("main"),
            "Alt+S on the main screen did not open the Threads pop-up with `main`:\n{open}"
        );
        assert!(
            flat.contains("dryrun:dry-model"),
            "the Threads pop-up opened from the main screen did not keep the footer in view:\n{open}"
        );

        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(500));
        let back = frame_where(&mut pty, |f| {
            f.contains(EMPTY_COMPOSER) && !f.contains("\u{250c}Threads")
        });
        assert!(
            !squeezed(&back).contains("\u{250c}Threads"),
            "Esc did not close the Threads pop-up:\n{back}"
        );
        assert!(
            squeezed(&back).contains(EMPTY_COMPOSER),
            "Esc from the Threads pop-up did not go back to the empty composer:\n{back}"
        );
        quit_and_check(&mut pty);
    }

    /// Alt+X opens the todo pop-up, titled `Todo`, over the main screen;
    /// Esc closes it and the composer has the keyboard again.
    #[test]
    #[serial]
    fn alt_x_opens_the_todo_popup_and_esc_goes_back() {
        let (mut pty, _guard, _dir) = boot_dry_run("probe-alt-x", "");
        pty.send(ALT_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let open = frame_where(&mut pty, |f| f.contains("\u{250c}Todo"));
        assert!(
            squeezed(&open).contains("\u{250c}Todo"),
            "Alt+X did not open the todo pop-up:\n{open}"
        );
        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(500));
        let back = frame_where(&mut pty, |f| {
            f.contains(EMPTY_COMPOSER) && !f.contains("\u{250c}Todo")
        });
        assert!(
            !squeezed(&back).contains("\u{250c}Todo"),
            "Esc did not close the todo pop-up:\n{back}"
        );
        quit_and_check(&mut pty);
    }

    /// Alt+X is also a close key for the todo pop-up it opened.
    #[test]
    #[serial]
    fn alt_x_again_closes_the_todo_popup() {
        let (mut pty, _guard, _dir) = boot_dry_run("probe-alt-x-toggle", "");
        pty.send(ALT_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let open = frame_where(&mut pty, |f| f.contains("\u{250c}Todo"));
        assert!(
            squeezed(&open).contains("\u{250c}Todo"),
            "Alt+X did not open the todo pop-up:\n{open}"
        );
        pty.send(ALT_X).unwrap();
        pty.read_for(Duration::from_millis(500));
        let back = frame_where(&mut pty, |f| {
            f.contains(EMPTY_COMPOSER) && !f.contains("\u{250c}Todo")
        });
        assert!(
            !squeezed(&back).contains("\u{250c}Todo") && squeezed(&back).contains(EMPTY_COMPOSER),
            "a second Alt+X did not close the todo pop-up:\n{back}"
        );
        quit_and_check(&mut pty);
    }

    /// In the Threads pop-up, Ctrl+P neither closes it nor reaches the
    /// composer; q, Tab and Ctrl+C each close it back to the composer, and
    /// Ctrl+C there does not quit the app.
    #[test]
    #[serial]
    fn the_threads_popup_ignores_ctrl_p_and_closes_on_q_tab_and_ctrl_c() {
        let (mut pty, _guard, _dir) = boot_dry_run("probe-alt-s-keys", "");
        for (name, close) in [("q", &b"q"[..]), ("Tab", &b"\t"[..]), ("Ctrl+C", CTRL_C)] {
            pty.send(ALT_S).unwrap();
            pty.read_for(Duration::from_millis(500));
            let open = frame_where(&mut pty, |f| f.contains("\u{250c}Threads"));
            assert!(
                squeezed(&open).contains("\u{250c}Threads"),
                "Alt+S did not open the Threads pop-up (before {name}):\n{open}"
            );
            if name == "q" {
                pty.send(b"\x10").unwrap();
                pty.read_for(Duration::from_millis(500));
                let still = frame_where(&mut pty, |f| f.contains("\u{250c}Threads"));
                assert!(
                    squeezed(&still).contains("\u{250c}Threads"),
                    "Ctrl+P closed the Threads pop-up:\n{still}"
                );
            }
            pty.send(close).unwrap();
            pty.read_for(Duration::from_millis(500));
            let back = frame_where(&mut pty, |f| {
                f.contains(EMPTY_COMPOSER) && !f.contains("\u{250c}Threads")
            });
            let flat = squeezed(&back);
            assert!(
                !flat.contains("\u{250c}Threads") && flat.contains(EMPTY_COMPOSER),
                "{name} did not close the Threads pop-up back to an empty composer:\n{back}"
            );
        }
        quit_and_check(&mut pty);
    }

    /// `your turn` ends the composer's top rule on boot, is gone while the
    /// model streams (also with a line typed mid-turn), and is back once
    /// the reply is in.
    #[test]
    #[serial]
    fn your_turn_shows_on_boot_goes_during_the_turn_and_comes_back_after() {
        let dir = fresh_config_dir("probe-your-turn");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("held part yt9 ".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("reply done yt9".to_string()),
        ])]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(HOTKEY_GAP);

        let boot = frame_where(&mut pty, |f| f.contains("yourturn"));
        assert!(
            squeezed(&boot).contains("yourturn"),
            "the composer does not say `your turn` on boot:\n{boot}"
        );

        let sent = pty.raw().len();
        pty.send(b"hello yt9\r").unwrap();
        wait_for_squeezed_after(&mut pty, "held part yt9", sent, "the held stream");
        let busy = frame_where(&mut pty, |f| f.contains("Esctointerrupt"));
        assert!(
            !squeezed(&busy).contains("yourturn"),
            "`your turn` still shows while the model streams:\n{busy}"
        );
        pty.send(b"typing mid turn").unwrap();
        pty.read_for(Duration::from_millis(400));
        let typing = frame_where(&mut pty, |f| f.contains("typingmidturn"));
        assert!(
            !squeezed(&typing).contains("yourturn"),
            "`your turn` shows for a draft typed mid-turn:\n{typing}"
        );
        // Ctrl+U cuts the draft; Ctrl+C would interrupt the turn.
        pty.send(b"\x15").unwrap();
        pty.read_for(Duration::from_millis(300));

        fs::write(&release, b"").unwrap();
        wait_for_squeezed_after(&mut pty, "reply done yt9", sent, "the released reply");
        let deadline = Instant::now() + PTY_DEADLINE;
        let mut after = String::new();
        while Instant::now() < deadline {
            after = frame_where(&mut pty, |f| f.contains("yourturn"));
            if squeezed(&after).contains("yourturn") {
                break;
            }
        }
        assert!(
            squeezed(&after).contains("yourturn"),
            "`your turn` did not come back once the turn ended:\n{after}"
        );
        quit_and_check(&mut pty);
    }

    /// `.session probe-ctx` puts the session name in the composer's context
    /// label and draws an `entered session` divider; `.exit session` draws a
    /// `left session` divider over a fresh view that no longer holds the
    /// line sent inside the session.
    #[test]
    #[serial]
    fn entering_and_leaving_a_session_draws_dividers_and_clears_the_view() {
        let (mut pty, _guard, _dir) = boot_dry_run("probe-ctx-session", "");
        let at = pty.raw().len();
        pty.send(b".session probe-ctx\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "entered session probe-ctx",
            at,
            "the enter divider",
        );
        let inside = frame_where(&mut pty, |f| {
            label_before_your_turn(f).ends_with("probe-ctx")
        });
        let label = label_before_your_turn(&inside);
        assert!(
            label.ends_with("probe-ctx"),
            "the composer's top rule does not start with the session name ({label:?}):\n{inside}"
        );

        dry_run_exchange(&mut pty, "inside-line-c3x");
        let left_at = pty.raw().len();
        pty.send(b".exit session\r").unwrap();
        let deadline = Instant::now() + PTY_DEADLINE;
        loop {
            let text = pty.read_for(Duration::from_millis(300));
            if text.contains("Save session?") {
                pty.read_for(HOTKEY_GAP);
                pty.send(b"n").unwrap();
            }
            if squeezed(&since(&pty, left_at)).contains("leftsessionprobe-ctx") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no `left session probe-ctx` divider:\n{}",
                since(&pty, left_at)
            );
        }
        let after = frame_where(&mut pty, |f| f.contains("leftsessionprobe-ctx"));
        let flat = squeezed(&after);
        assert!(flat.contains("leftsessionprobe-ctx"), "{after}");
        assert!(
            !flat.contains("inside-line-c3x"),
            "leaving the session did not clear the view:\n{after}"
        );
        quit_declining_save(&mut pty);
    }

    /// Saving a temp session under a name, with either prefix, keeps the
    /// conversation on screen: the divider says what it was saved as and
    /// the line sent before the save is still drawn, with no redraw.
    #[test]
    #[serial]
    fn saving_a_temp_session_under_a_name_keeps_the_conversation_on_screen() {
        for (prefix, sigil) in [("dot", "."), ("slash", "/")] {
            let (mut pty, _guard, _dir) = boot_dry_run(
                &format!("probe-save-{prefix}"),
                &format!("command_prefix: {prefix}\n"),
            );
            let at = pty.raw().len();
            pty.send(format!("{sigil}session\r").as_bytes()).unwrap();
            wait_for_squeezed_after(&mut pty, "entered session temp", at, "the enter divider");
            dry_run_exchange(&mut pty, "kept-line-s4v");
            let at = pty.raw().len();
            pty.send(format!("{sigil}save session saved-s4v\r").as_bytes())
                .unwrap();
            wait_for_squeezed_after(
                &mut pty,
                "session temp saved as saved-s4v",
                at,
                "the rename divider",
            );
            let after = frame_where(&mut pty, |f| f.contains("sessiontempsavedassaved-s4v"));
            let flat = squeezed(&after);
            assert!(
                flat.contains("kept-line-s4v"),
                "{prefix}: saving the session cleared the view:\n{after}"
            );
            assert!(
                !flat.contains("leftsession"),
                "{prefix}: the save reads as a leave:\n{after}"
            );
            quit_declining_save(&mut pty);
        }
    }

    /// `.role probe-role` puts the role in the context label as the REPL
    /// prompt shows it with no session (`probe-role`), and an `entered
    /// role` divider; `.exit role` draws `left role` and drops the label.
    #[test]
    #[serial]
    fn entering_and_leaving_a_role_draws_dividers_and_labels_the_composer() {
        let dir = fresh_config_dir("probe-ctx-role");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        fs::create_dir_all(dir.join("roles")).unwrap();
        fs::write(dir.join("roles").join("probe-role.md"), "You are terse.\n").unwrap();
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(HOTKEY_GAP);

        let at = pty.raw().len();
        pty.send(b".role probe-role\r").unwrap();
        wait_for_squeezed_after(&mut pty, "entered role probe-role", at, "the enter divider");
        let inside = frame_where(&mut pty, |f| {
            label_before_your_turn(f).ends_with("probe-role")
        });
        assert!(
            label_before_your_turn(&inside).ends_with("probe-role"),
            "the context label does not show `probe-role`:\n{inside}"
        );
        let left_at = pty.raw().len();
        pty.send(b".exit role\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "left role probe-role",
            left_at,
            "the leave divider",
        );
        let after = frame_where(&mut pty, |f| {
            f.contains("yourturn") && !label_before_your_turn(f).ends_with("probe-role")
        });
        assert!(
            !label_before_your_turn(&after).ends_with("probe-role"),
            "the context label kept the role after `.exit role`:\n{after}"
        );
        quit_and_check(&mut pty);
    }

    /// The palette for `.a` lists the built-ins A-Z, then the macros: a
    /// macro named `aaa-probe` sorts after every built-in, though it would
    /// come first in a plain A-Z of all rows.
    #[test]
    #[serial]
    fn the_palette_lists_built_ins_before_a_macro_that_sorts_first() {
        let dir = fresh_config_dir("probe-palette-order");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        write_macro(&dir, "aaa-probe", &[".info"]);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b".a").unwrap();
        pty.read_for(Duration::from_millis(600));
        let at = repaint_showing(&mut pty, 60, 141, &["aaa-probe", ".agent"]);
        let frame = since(&pty, at);
        let row_of = |needle: &str| -> Option<u16> {
            painted_at(pty.raw(), needle.as_bytes(), at)
                .into_iter()
                .map(|(row, _)| row)
                .max()
        };
        let agent = row_of(".agent ");
        let mac = row_of("aaa-probe");
        assert!(
            agent.is_some() && mac.is_some(),
            "the palette for `.a` lacks `.agent` or the macro (agent {agent:?}, macro {mac:?}):\n{frame}"
        );
        assert!(
            mac > agent,
            "the macro `aaa-probe` is listed above the built-in `.agent` (macro row {mac:?}, agent row {agent:?}):\n{frame}"
        );
        pty.send(ESC).unwrap();
        pty.read_for(Duration::from_millis(300));
        quit_clearing_any_draft(&mut pty);
    }
}

mod footer_usage {
    //! The right prompt's usage segment at the footer's right end.
    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// Once the first reply of a session is in, the footer row shows the
    /// session's tokens and the context percentage, counting that reply.
    #[test]
    #[serial]
    fn the_footer_shows_the_session_tokens_and_percent_after_the_first_reply() {
        let dir = fresh_config_dir("footer-usage");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text("first reply u4a".to_string())]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b".session\r").unwrap();
        pty.wait_for("temp", PTY_DEADLINE).unwrap();
        let sent = pty.raw().len();
        pty.send(b"hello\r").unwrap();
        wait_for_squeezed(&mut pty, "first reply u4a", "the first reply");
        pty.wait_for_after("%)", sent, PTY_DEADLINE).unwrap();
        let rows: Vec<u16> = painted_at(pty.raw(), b"%)", sent)
            .into_iter()
            .map(|(row, _)| row)
            .collect();
        let height = pty.rows().unwrap();
        let raw = String::from_utf8_lossy(&pty.raw()[sent..]).into_owned();
        let footer_paints: Vec<&str> = raw
            .match_indices(&format!("\x1b[{height};"))
            .map(|(at, _)| &raw[at..(at + 160).min(raw.len())])
            .collect();
        assert!(
            rows.contains(&height),
            "no tokens(percent%) on the footer row {height}: {rows:?}\n{footer_paints:#?}"
        );
        quit_and_check(&mut pty);
    }

    /// A reply whose provider reports usage: prompt tokens with cached
    /// reads among them, and output tokens, priced per million.
    fn priced_turn(dir: &std::path::Path, text: &str) -> FakeLlm {
        let fake = FakeLlm::start(vec![Reply::WithUsage {
            reply: Box::new(Reply::Text(text.to_string())),
            usage: serde_json::json!({
                "prompt_tokens": 11000,
                "completion_tokens": 1000,
                "total_tokens": 12000,
                "prompt_tokens_details": {"cached_tokens": 10000},
            }),
        }]);
        write_fake_llm_config(dir, &fake);
        let path = dir.join("config.yaml");
        let config = std::fs::read_to_string(&path).unwrap().replace(
            "max_input_tokens: 100000\n",
            "max_input_tokens: 100000\n        input_price: 3\n        output_price: 15\n        cache_read_price: 0.3\n",
        );
        std::fs::write(&path, config).unwrap();
        fake
    }

    /// The footer row of a forced repaint, whitespace squeezed out: from the
    /// left half, which starts with `left`, to the end of the frame, as the
    /// footer is the last row the repaint writes.
    fn footer_row(pty: &mut Pty, left: &str) -> String {
        let left = squeezed(left);
        let at = repaint_showing(pty, 24, 100, &[&left]);
        let flat = squeezed(&since(pty, at));
        let at = flat
            .rfind(&left)
            .unwrap_or_else(|| panic!("no footer starting {left:?}: {flat}"));
        flat[at..].to_string()
    }

    /// On a real turn in a session with a provider that reports usage, the
    /// footer keeps every segment it had: the model, the session and the
    /// task counts on the left, then on the right the cached reads, the
    /// session's spend and its tokens with the context percentage, in the
    /// REPL right prompt's order and format.
    #[test]
    #[serial]
    fn a_priced_turn_puts_cache_spend_and_context_on_the_footer() {
        let dir = fresh_config_dir("footer-priced");
        let _guard = TempDirGuard(dir.clone());
        let _fake = priced_turn(&dir, "priced reply p7c");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b".session\r").unwrap();
        pty.wait_for("temp", PTY_DEADLINE).unwrap();
        let sent = pty.raw().len();
        pty.send(b"hello\r").unwrap();
        wait_for_squeezed(&mut pty, "priced reply p7c", "the priced reply");
        pty.wait_for_after("$0.0210", sent, PTY_DEADLINE).unwrap();
        let left = squeezed("fake:fake-model . temp . agents 0 . jobs 0");
        let footer = footer_row(&mut pty, &left);
        let right = &footer[left.len()..];
        let cache = right.find("\u{26a1}10000");
        let spend = right.find("$0.0210");
        let context = right.find("%)");
        assert!(
            cache.is_some() && cache < spend && spend < context,
            "the right half is not cache, spend, tokens(percent%): {footer}"
        );
        assert!(right.ends_with("%)"), "{footer}");
        quit_and_check(&mut pty);
    }

    /// Without a session the right prompt has no spend or tokens to show,
    /// as in the REPL, but the cached reads of the last reply still show.
    #[test]
    #[serial]
    fn without_a_session_the_footer_still_shows_the_cached_reads() {
        let dir = fresh_config_dir("footer-priced-nosession");
        let _guard = TempDirGuard(dir.clone());
        let _fake = priced_turn(&dir, "priced reply q8d");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let sent = pty.raw().len();
        pty.send(b"hello\r").unwrap();
        wait_for_squeezed(&mut pty, "priced reply q8d", "the priced reply");
        pty.wait_for_after("\u{26a1}10000", sent, PTY_DEADLINE)
            .unwrap();
        let footer = footer_row(&mut pty, "fake:fake-model . no session . agents 0 . jobs 0");
        assert!(footer.ends_with("jobs0\u{26a1}10000"), "{footer}");
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
    //! The welcome wordmark's startup animation as a user sees it: the
    //! letters build in, then a bright band moves over them for a few
    //! seconds, never under NO_COLOR, settled at once by the first key or
    //! click, and a 40x20 welcome that keeps the small wordmark and the
    //! version.
    use std::time::{Duration, Instant};

    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// Whether an SGR parameter list sets a white or bright-white
    /// foreground, in any of the spellings a backend may use. The
    /// components of an extended colour are skipped, so an RGB channel of
    /// 97 is not read as bright white.
    fn sets_white(params: &[u16]) -> bool {
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                38 | 48 | 58 => match params.get(i + 1) {
                    Some(5) => {
                        if params[i] == 38 && matches!(params.get(i + 2), Some(15 | 7)) {
                            return true;
                        }
                        i += 3;
                    }
                    Some(2) => i += 5,
                    _ => i += 1,
                },
                37 | 97 => return true,
                _ => i += 1,
            }
        }
        false
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
    /// repainted several times with a white band once it has built in
    /// (long enough to be seen, about three seconds in all), and then the
    /// band is gone and the screen goes still.
    #[test]
    #[serial]
    fn the_sweep_paints_a_moving_band_for_a_moment_and_ends() {
        let dir = fresh_config_dir("sweep-moves");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        let (shown, first, mut answered) = boot(&mut pty);
        let seen = slices(&mut pty, shown, Duration::from_millis(4000), &mut answered);
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
            last <= Duration::from_millis(3500),
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
        // The rules under the transcript are white too, so only the rows
        // down to the version line under the wordmark are read.
        let version = concat!("v", env!("CARGO_PKG_VERSION"));
        let below = find(pty.raw(), version.as_bytes(), settled).expect("no version line");
        assert!(
            !white_in(&pty.raw()[settled..below]),
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
        let by = Instant::now() + Duration::from_millis(1000);
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

    /// At 40x20 the welcome is the small wordmark with the version under
    /// it, and nothing else: no model, no hints.
    #[test]
    #[serial]
    fn forty_by_twenty_keeps_the_small_wordmark_and_the_version() {
        let dir = fresh_config_dir("sweep-40x20");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(3700));
        let at = repaint(&mut pty, 20, 40);
        let frame = since(&pty, at);
        let flat = squeezed(&frame);
        let version = concat!("v", env!("CARGO_PKG_VERSION"));
        assert!(
            flat.contains(r"\__\___/\_,\___/\__\___|"),
            "no wordmark at 40x20:\n{frame}"
        );
        assert!(flat.contains(version), "no version at 40x20:\n{frame}");
        for gone in [
            "model:",
            "Type \".help\"",
            "Tip: Press Ctrl-C",
            "Empty prompt",
        ] {
            assert!(
                !flat.contains(&squeezed(gone)),
                "{gone:?} shows at 40x20:\n{frame}"
            );
        }
        // The repaint writes the screen top to bottom.
        let mark_at = flat.find(r"\__\___/\_,\___/\__\___|");
        let version_at = flat.find(version);
        assert!(
            matches!((mark_at, version_at), (Some(m), Some(v)) if m < v),
            "the wordmark is not above the version at 40x20:\n{frame}"
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
        let by = Instant::now() + Duration::from_millis(1000);
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

    /// The wordmark builds in from the left: the first frame lacks its
    /// right part, and once the animation has run the whole wordmark is
    /// there, bold and not dim.
    #[test]
    #[serial]
    fn the_wordmark_builds_in_and_settles_bold() {
        let full_row = "██╔════╝██╔═══██╗╚██╗██╔╝██╔═══██╗╚══██╔══╝██╔════╝";
        let dir = fresh_config_dir("sweep-reveal");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        let (_, first, mut answered) = boot(&mut pty);
        let built = squeezed(&since(&pty, first));
        assert!(
            !built.contains(full_row),
            "the whole wordmark is on the first frame:\n{built}"
        );
        slices(
            &mut pty,
            Instant::now(),
            Duration::from_millis(3700),
            &mut answered,
        );
        let settled = repaint(&mut pty, 40, 101);
        let frame = squeezed(&since(&pty, settled));
        assert!(
            frame.contains(full_row),
            "the settled frame lacks the whole wordmark:\n{frame}"
        );
        let raw = pty.raw();
        let at = find(raw, "█".as_bytes(), settled).expect("no wordmark after the sweep");
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

    /// A short terminal gives up the wordmark before the version: at every
    /// height the version shows, the wide wordmark from 80x16 up, and no
    /// wordmark at all at 80x12.
    #[test]
    #[serial]
    fn a_short_terminal_drops_the_wordmark_before_the_version() {
        let dir = fresh_config_dir("sweep-short");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(3700));
        let version = concat!("v", env!("CARGO_PKG_VERSION"));
        let big_row = "╚██████╗╚██████╔╝██║╚██████╔╝██║███████╗";
        let small_row = r"\__\___/\_,\___/\__\___|";
        let mut seen = Vec::new();
        for (i, rows) in (12u16..=20).enumerate() {
            // Alternate the width so no resize is coalesced away.
            let cols = if i % 2 == 0 { 80 } else { 81 };
            // The footer is the frame's last row, so a frame that shows it
            // has painted the welcome above it too.
            let at = repaint_showing(&mut pty, rows, cols, &[version, "no session"]);
            let frame = since(&pty, at);
            let flat = squeezed(&frame);
            let big = flat.contains(big_row);
            let small = flat.contains(small_row);
            assert!(
                flat.contains(version),
                "at {cols}x{rows} the version is gone:\n{frame}"
            );
            if rows >= 16 {
                assert!(big, "at {cols}x{rows} the wide wordmark is gone:\n{frame}");
            }
            if rows == 12 {
                assert!(
                    !big && !small,
                    "at {cols}x{rows} a wordmark is drawn:\n{frame}"
                );
            }
            seen.push((rows, big, small));
        }
        eprintln!("rows/wide/small: {seen:?}");
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
    //! Mouse reports, drags included, come back on when the screen comes
    //! back from a `!` command.
    use std::time::Duration;

    use super::common::tui_harness::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn the_mouse_reports_come_back_after_a_bang_command() {
        let dir = fresh_config_dir("mouse-after-bang");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(HOTKEY_GAP);

        let bang_at = pty.raw().len();
        pty.send(b"!echo bang-mouse-m5b\r").unwrap();
        let left = pty
            .wait_for_raw(LEAVE_ALT, bang_at, PTY_DEADLINE)
            .expect("`!echo` did not release the terminal");
        let re_entered = wait_for_re_entry(&mut pty, left);
        pty.read_for(Duration::from_millis(800));
        let back = String::from_utf8_lossy(&pty.raw()[re_entered..]).into_owned();
        for mode in ["\x1b[?1000h", "\x1b[?1002h", "\x1b[?1006h"] {
            assert!(
                back.contains(mode),
                "the return from `!` did not ask for {mode:?}: {back:?}"
            );
        }
        quit_and_check(&mut pty);
    }
}

mod session_undo_and_rename {
    //! What the model is sent after `.undo` takes back a line sent mid-turn,
    //! and what a named session's rename and later leave draw.
    use std::fs;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// The messages of a request body, as (role, text, tool_call ids,
    /// tool_call_id) rows.
    fn messages(body: &str) -> Vec<(String, String, Vec<String>, Option<String>)> {
        let json: serde_json::Value = serde_json::from_str(body).expect("a JSON request body");
        json["messages"]
            .as_array()
            .expect("a messages array")
            .iter()
            .map(|m| {
                let text = match &m["content"] {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let calls = m["tool_calls"]
                    .as_array()
                    .map(|calls| {
                        calls
                            .iter()
                            .filter_map(|c| c["id"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                (
                    m["role"].as_str().unwrap_or_default().to_string(),
                    text,
                    calls,
                    m["tool_call_id"].as_str().map(str::to_string),
                )
            })
            .collect()
    }

    /// After a tool turn that took a line mid-turn, one `.undo` puts the line
    /// back in the composer; sending it again goes to the model once, as a
    /// user prompt after the kept opener, with nothing of the reply that
    /// followed the line and every tool call still answered by its result.
    #[test]
    #[serial]
    fn a_resent_line_after_undo_reaches_the_model_once_without_the_undone_reply() {
        let dir = fresh_config_dir("probe-undo-resend");
        let _guard = TempDirGuard(dir.clone());
        let tool_a = dir.join("tool-a");
        let fake = FakeLlm::start(vec![
            sleep_call(&tool_a),
            Reply::Text("undone-reply-p7q".to_string()),
            Reply::Text("fresh-reply-p7q".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(Duration::from_millis(400));
        let at = pty.raw().len();
        pty.send(b".session\r").unwrap();
        wait_for_squeezed_after(&mut pty, "entered session temp", at, "the session");

        let turn_at = pty.raw().len();
        pty.send(b"opener-p7q\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-a-started"),
            "the sleep tool's start",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"midline-p7q\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        fs::write(dir.join("tool-a-release"), b"").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "undone-reply-p7q",
            turn_at,
            "the tool turn's reply",
        );
        wait_until_no_pending(&mut pty);
        assert_eq!(
            fake.served(),
            2,
            "the mid-turn line went out as its own turn"
        );

        clear_any_draft(&mut pty);
        pty.send(b".undo\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let mut toggle = false;
        let frame = next_frame(&mut pty, &mut toggle);
        let draft = composer_row_with_footer(&frame, "fake:fake-model");
        assert_eq!(
            draft, "midline-p7q",
            "the undo did not put the line back:\n{frame}"
        );

        let sent_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "fresh-reply-p7q",
            sent_at,
            "the resent line's reply",
        );
        pty.read_for(Duration::from_millis(800));
        quit_declining_save(&mut pty);

        let bodies = fake.bodies();
        assert_eq!(bodies.len(), 3, "{bodies:?}");
        let last = &bodies[2];
        let rows = messages(last);
        let users: Vec<&str> = rows
            .iter()
            .filter(|r| r.0 == "user")
            .map(|r| r.1.as_str())
            .collect();
        assert!(
            users.iter().any(|t| t.contains("opener-p7q")),
            "the kept opener is not in the next request: {rows:?}"
        );
        assert_eq!(
            last.matches("midline-p7q").count(),
            1,
            "the resent line is not in the request exactly once: {rows:?}"
        );
        assert!(
            users.last().is_some_and(|t| t.contains("midline-p7q")),
            "the resent line is not the last user prompt: {rows:?}"
        );
        assert!(
            !last.contains("undone-reply-p7q"),
            "the reply that followed the undone line still reached the model: {rows:?}"
        );
        let called: Vec<&String> = rows.iter().flat_map(|r| r.2.iter()).collect();
        let answered: Vec<&String> = rows.iter().filter_map(|r| r.3.as_ref()).collect();
        for id in &called {
            assert!(
                answered.contains(id),
                "tool call {id} has no result in the request: {rows:?}"
            );
        }
    }

    /// Waits for `marker` after `from`, answering a save question `n`.
    fn wait_declining(pty: &mut Pty, marker: &str, from: usize) -> String {
        let deadline = Instant::now() + PTY_DEADLINE;
        loop {
            let text = pty.read_for(Duration::from_millis(300));
            if text.contains("Save session?") {
                pty.read_for(HOTKEY_GAP);
                pty.send(b"n").unwrap();
            }
            let shown = since(pty, from);
            if squeezed(&shown).contains(&squeezed(marker)) {
                return shown;
            }
            assert!(
                Instant::now() < deadline,
                "{marker:?} never appeared:\n{shown}"
            );
        }
    }

    fn frame_where(pty: &mut Pty, shows: impl Fn(&str) -> bool) -> String {
        let at = repaint_until(pty, 40, 100, |frame| shows(&squeezed(frame)));
        since(pty, at)
    }

    /// A named session saved under a new name says `session <old> saved as
    /// <new>`, keeps the conversation on screen and writes the new file; a
    /// re-save under its own name keeps the view too; a later `.exit
    /// session` leaves under the new name and clears the view.
    #[test]
    #[serial]
    fn renaming_a_named_session_keeps_the_view_and_a_later_leave_clears_it() {
        let dir = fresh_config_dir("probe-rename-named");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(HOTKEY_GAP);

        let at = pty.raw().len();
        pty.send(b".session alpha-r9\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "entered session alpha-r9",
            at,
            "the enter divider",
        );
        dry_run_exchange(&mut pty, "kept-line-r9");

        let at = pty.raw().len();
        pty.send(b".save session\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let resaved = frame_where(&mut pty, |f| f.contains("kept-line-r9"));
        let flat = squeezed(&resaved);
        assert!(
            flat.contains("kept-line-r9") && !squeezed(&since(&pty, at)).contains("leftsession"),
            "re-saving under the same name cleared the view or read as a leave:\n{resaved}"
        );

        let at = pty.raw().len();
        pty.send(b".save session beta-r9\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "session alpha-r9 saved as beta-r9",
            at,
            "the rename divider",
        );
        let renamed = frame_where(&mut pty, |f| f.contains("sessionalpha-r9savedasbeta-r9"));
        let flat = squeezed(&renamed);
        assert!(
            flat.contains("kept-line-r9"),
            "the rename cleared the view:\n{renamed}"
        );
        assert!(
            !squeezed(&since(&pty, at)).contains("leftsession"),
            "the rename reads as a leave:\n{renamed}"
        );
        let saved: Vec<String> = walk(&dir)
            .into_iter()
            .filter(|p| p.contains("beta-r9"))
            .collect();
        assert!(
            !saved.is_empty(),
            "no file for the session saved as beta-r9"
        );

        let left_at = pty.raw().len();
        pty.send(b".exit session\r").unwrap();
        wait_declining(&mut pty, "left session beta-r9", left_at);
        let after = frame_where(&mut pty, |f| f.contains("leftsessionbeta-r9"));
        assert!(
            !squeezed(&after).contains("kept-line-r9"),
            "leaving the renamed session did not clear the view:\n{after}"
        );
        quit_declining_save(&mut pty);
    }

    fn walk(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    out.extend(walk(&path));
                } else {
                    out.push(path.display().to_string());
                }
            }
        }
        out
    }
}
