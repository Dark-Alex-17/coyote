//! Real-process checks of the command palette, rewind and the barriers
//! around a running turn: macros, hidden verbs, history, type-ahead,
//! queue swaps and drains, failed macros, success-gated barriers and
//! creation barriers. Every module drives the binary through the shared
//! pty harness in `common`, so the file is unix-only.
#![cfg(unix)]

mod common;

mod palette_and_rewind {
    //! Black-box pins for the bottom pane's command palette, the rewind picker,
    //! the terminal lender and the prompt modals: the built binary on a
    //! pseudo-terminal, a scripted fake model where a tool call is needed, and
    //! the real `prompt-utils.sh` helpers as the prompt channel's client.
    //! Unix-only and serial like its sibling pty modules.
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// A tool that asks through one prompt helper and records what the helper
    /// printed (or `cancelled` when it printed nothing, the fail-closed path).
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

    /// Marks `<marker>-started`, then waits (at most eight seconds) for
    /// `<marker>-release` before finishing.
    const PALETTE_SLEEP_TOOL: &str = "#!/usr/bin/env bash\n\
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
    \x20   echo \"fixture slept r12s\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    fn tool_call_then_done(tool: &str, out: &Path) -> FakeLlm {
        FakeLlm::start(vec![
            Reply::ToolCall {
                name: tool.to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done z2s".to_string()),
        ])
    }

    fn finish_after_tool(pty: &mut Pty, from: usize) {
        wait_for_squeezed_after(pty, "fixture done z2s", from, "the reply after the tool");
        quit_and_check(pty);
    }

    // ---------------------------------------------------------------------
    // The terminal lender survives a second lend in one session.
    // ---------------------------------------------------------------------

    /// `!cmd` shell passthrough runs through the terminal lender. Regression
    /// target: the second lend deadlocked on the reader lock. Two `!true`
    /// lines in one session must both come back to a composer that takes keys.
    #[test]
    #[serial]
    fn two_bang_lines_in_one_session_both_return_to_the_composer() {
        let dir = fresh_config_dir("bang-twice");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        let first = pty.raw().len();
        pty.send(b"!true\r").unwrap();
        let back = wait_for_lend_and_return(&mut pty, first, "the first `!true`");
        assert_composer_takes_keys(&mut pty, back, "the first `!true`");

        let second = pty.raw().len();
        pty.send(b"!true\r").unwrap();
        let back = wait_for_lend_and_return(&mut pty, second, "the second `!true`");
        assert_composer_takes_keys(&mut pty, back, "the second `!true`");

        let transcript = quit_and_check(&mut pty);
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            3,
            "expected two lends and the final exit to leave the alternate screen:\n{transcript}"
        );
    }

    /// `.edit`-style editor shell-outs run through the lender like Ctrl+O.
    /// `.edit config` with `EDITOR=true` returns to the composer, and a
    /// `!true` straight after (lend after lend) returns too.
    #[test]
    #[serial]
    fn dot_edit_then_a_bang_line_both_return_to_the_composer() {
        let dir = fresh_config_dir("edit-then-bang");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let editor = if Path::new("/usr/bin/true").exists() {
            "/usr/bin/true"
        } else {
            "/bin/true"
        };
        let mut pty = spawn_tui(&dir, &[("EDITOR", editor), ("VISUAL", editor)]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));

        let edit_at = pty.raw().len();
        pty.send(b".edit config\r").unwrap();
        let back = wait_for_lend_and_return(&mut pty, edit_at, "`.edit config`");
        assert_composer_takes_keys(&mut pty, back, "`.edit config`");

        let bang_at = pty.raw().len();
        pty.send(b"!true\r").unwrap();
        let back = wait_for_lend_and_return(&mut pty, bang_at, "`!true` after `.edit`");
        assert_composer_takes_keys(&mut pty, back, "`!true` after `.edit`");

        quit_and_check(&mut pty);
    }

    // ---------------------------------------------------------------------
    // Prompt modals through the real helper; Enter settles from the modal's
    // appearance, not from the last keystroke.
    // ---------------------------------------------------------------------

    /// Enter on an Input field settles 1 s from the modal's appearance, never
    /// re-armed by typing. Typed text plus Enter inside the first second is
    /// not accepted (the modal stays, the tool gets nothing); Enter once a
    /// second has passed since the modal appeared is accepted even though a
    /// keystroke landed 300 ms earlier.
    #[test]
    #[serial]
    fn enter_on_an_input_modal_settles_from_appearance_not_from_typing() {
        let dir = fresh_config_dir("input-settle");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("input-answer");
        let fake = tool_call_then_done("fixture_input_rec", &out);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_input_rec.sh",
            &recording_prompt_tool("input \"Your name q9d-i\""),
        );
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"call the input tool\r").unwrap();
        pty.wait_for_after("q9d-i", turn_at, PTY_DEADLINE)
            .expect("the tool's input question never reached the screen");
        let appeared = Instant::now();

        // Inside the first second: text then Enter. Enter must be dropped.
        pty.send(b"abc\r").unwrap();
        pty.read_for(Duration::from_millis(400));
        assert!(
            appeared.elapsed() < Duration::from_millis(950),
            "the test itself was too slow to exercise the early Enter ({:?})",
            appeared.elapsed()
        );
        assert!(
            !out.exists(),
            "Enter inside the first second of the Input modal was accepted: {:?}",
            fs::read_to_string(&out)
        );

        // Past the appearance gate but within 300 ms of a keystroke: a
        // further character, then Enter. The appearance gate is the only one
        // on a field, so this Enter is accepted with the whole text.
        let settle = Duration::from_millis(1500).saturating_sub(appeared.elapsed());
        pty.read_for(settle);
        pty.send(b"d").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "abcd",
            "the Input modal did not hand the tool the typed text on the settled Enter"
        );
        finish_after_tool(&mut pty, turn_at);
    }

    /// The helper contract says a cancelled prompt returns 1 and prints
    /// nothing. Esc on a tool's Input request leaves the tool with nothing
    /// (the fixture records `cancelled`) and the turn still finishes.
    #[test]
    #[serial]
    fn esc_on_a_tools_input_request_cancels_and_the_tool_fails_closed() {
        let dir = fresh_config_dir("input-esc");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("input-answer");
        let fake = tool_call_then_done("fixture_input_rec", &out);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_input_rec.sh",
            &recording_prompt_tool("input \"Your name q9d-e\""),
        );
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"call the input tool\r").unwrap();
        pty.wait_for_after("q9d-e", turn_at, PTY_DEADLINE)
            .expect("the tool's input question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "cancelled",
            "Esc on the Input modal did not fail the helper closed"
        );
        finish_after_tool(&mut pty, turn_at);
    }

    /// Same contract for a Select request: Esc cancels, helper prints nothing.
    #[test]
    #[serial]
    fn esc_on_a_tools_select_request_cancels_and_the_tool_fails_closed() {
        let dir = fresh_config_dir("select-esc");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("select-answer");
        let fake = tool_call_then_done("fixture_select_rec", &out);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_select_rec.sh",
            &recording_prompt_tool("list \"Pick one q9d-x\" alpha beta gamma"),
        );
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"call the select tool\r").unwrap();
        pty.wait_for_after("q9d-x", turn_at, PTY_DEADLINE)
            .expect("the tool's select question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "cancelled",
            "Esc on the Select modal did not fail the helper closed"
        );
        finish_after_tool(&mut pty, turn_at);
    }

    /// Enter on a Select modal sends the highlighted row (row 0 by default);
    /// Down then Enter sends row 1. Arrow keys plus Enter is the keyboard
    /// path the digit shortcut does not exercise.
    #[test]
    #[serial]
    fn down_then_enter_on_a_tools_select_request_sends_the_second_row() {
        let dir = fresh_config_dir("select-arrows");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("select-answer");
        let fake = tool_call_then_done("fixture_select_rec", &out);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_select_rec.sh",
            &recording_prompt_tool("list \"Pick one q9d-y\" alpha beta gamma"),
        );
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let turn_at = pty.raw().len();
        pty.send(b"call the select tool\r").unwrap();
        pty.wait_for_after("q9d-y", turn_at, PTY_DEADLINE)
            .expect("the tool's select question never reached the screen");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "1",
            "Down then Enter on the Select modal did not reach the tool as beta's index"
        );
        finish_after_tool(&mut pty, turn_at);
    }

    // ---------------------------------------------------------------------
    // Queued input while a turn runs.
    // ---------------------------------------------------------------------

    /// Queued text is injected at the model's next request, else becomes the
    /// next prompt; the strip shows `pending: N`. Two drafts while the tool
    /// sleeps show `pending: 2`, the turn is not moved on, and once released
    /// the session finishes cleanly with both lines gone from the field.
    #[test]
    #[serial]
    fn two_drafts_while_a_tool_sleeps_show_pending_two_and_drain_after_release() {
        let dir = fresh_config_dir("pending-two");
        let _guard = TempDirGuard(dir.clone());
        let marker = dir.join("tool");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_sleep".to_string(),
                arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
            },
            Reply::Text("fixture done r12-b".to_string()),
            Reply::Text("fixture done r12-c".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", PALETTE_SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        let typed_at = pty.raw().len();
        pty.send(b"draft-one-r12\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        pty.send(b"draft-two-r12\r").unwrap();
        pty.read_for(Duration::from_millis(1000));
        // The strip is repainted cell by cell (`pending: 1` -> `pending: 2`
        // paints one digit), so a full frame is forced before reading it.
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        let flat = squeezed(&frame);
        let mid = since(&pty, typed_at);
        assert!(
            flat.contains("pending:2"),
            "two drafts while a tool ran did not show `pending: 2`:\n{frame}"
        );
        assert!(
            squeezed(&mid).contains("draft-one-r12") && squeezed(&mid).contains("draft-two-r12"),
            "the queued previews are missing:\n{mid}"
        );
        assert_eq!(
            fake.served(),
            1,
            "queued drafts moved the turn on before the tool finished:\n{mid}"
        );

        fs::write(dir.join("tool-release"), b"").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "fixture done r12-",
            typed_at,
            "the reply after the tool",
        );
        // Let any follow-up prompt the second draft became run to its end.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pty.read_for(Duration::from_millis(300));
            let repaint_at = pty.raw().len();
            pty.send(b"\x0c").unwrap();
            read_repaint(&mut pty, repaint_at, Duration::from_millis(500));
            let repainted = since(&pty, repaint_at);
            if !repainted.contains("pending:") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the queue strip outlived the turn:\n{repainted}"
            );
        }
        // Both drafts sat on the queue when the tool round ended, so the
        // request after it carries both and nothing is left to run as a
        // turn of its own.
        let bodies = fake.bodies();
        assert_eq!(fake.served(), 2, "served count:\n{bodies:#?}");
        assert_eq!(
            bodies.len(),
            2,
            "the queued drafts should ride the tool-round request, not open turns:\n{bodies:#?}"
        );
        assert!(
            bodies[1].contains("draft-one-r12") && bodies[1].contains("draft-two-r12"),
            "the request after the tool round is missing a queued draft:\n{}",
            bodies[1]
        );
        quit_and_check(&mut pty);
    }

    // ---------------------------------------------------------------------
    // The palette's set equals the REPL completer's set, per prefix, in the
    // same (cold) state. The oracle is the REPL itself: its Tab menu on the
    // bare sigil lists every candidate as `<sigil>name   description`.
    // ---------------------------------------------------------------------

    /// One command the REPL completer offers: its name without the sigil and
    /// its description.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct Candidate {
        name: String,
        description: String,
    }

    /// The candidates the REPL completer offers cold (fresh config dir, no
    /// command run yet) for `sigil`: the command table plus the enabled
    /// macros, which both UIs offer. The menu is one entry per row and
    /// stops at the terminal's height, so the pty grows past the set first.
    fn repl_completer_candidates(dir: &Path, sigil: char) -> Vec<Candidate> {
        let mut pty = spawn_repl_under(dir, &[], WAIT_WRAPPER, "/dev/tty");
        // Grown before the line editor starts reading: a resize mid-read is
        // a signal the editor repaints on, and the keys behind it are lost.
        pty.resize(100, 100).unwrap();
        pty.wait_for(")> ", REPL_PROMPT_DEADLINE)
            .expect("the REPL prompt never appeared");
        pty.read_for(Duration::from_millis(500));
        let menu_at = pty.raw().len();
        pty.send(sigil.to_string().as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\t").unwrap();
        pty.read_for(Duration::from_millis(800));
        pty.send(b"\t").unwrap();
        pty.read_for(Duration::from_millis(2500));
        let menu = since(&pty, menu_at);
        let mut offered = parse_sigil_rows(&menu, sigil);
        assert!(
            offered.len() > 10,
            "the REPL completer menu was not read ({} rows):\n{menu}",
            offered.len()
        );

        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x04").unwrap();
        let _ = pty.wait_for("tty-probe-done", PTY_DEADLINE);

        offered.sort();
        offered.dedup();
        offered
    }

    /// Rows of the form `<sigil>name[ sub]<2+ spaces>description`, wherever
    /// they start (the first menu row shares the prompt's line). A macro
    /// without a description is a padded name alone and is kept as such.
    fn parse_sigil_rows(text: &str, sigil: char) -> Vec<Candidate> {
        let mut out = Vec::new();
        for line in text.lines() {
            let mut rest = line;
            while let Some(at) = rest.find(sigil) {
                let tail = &rest[at + sigil.len_utf8()..];
                let name_end = tail.find("  ").unwrap_or(tail.len());
                let name = tail[..name_end].trim_end();
                let valid = !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '-' || c == ' ')
                    && name.split(' ').count() <= 2
                    && name.starts_with(|c: char| c.is_ascii_lowercase());
                if valid {
                    let description = tail[name_end..]
                        .split("\r")
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    out.push(Candidate {
                        name: name.to_string(),
                        description,
                    });
                    rest = &tail[name_end..];
                    continue;
                }
                rest = tail;
            }
        }
        out
    }

    /// The screen `raw` leaves behind, replayed cell by cell. The painter
    /// moves the cursor (`CSI r;c H`) and writes only the cells that changed,
    /// so the stripped stream runs a row's name into its description; the
    /// replayed screen keeps the padding between them, in the shape the REPL
    /// completer paints its menu.
    fn painted_screen(raw: &[u8]) -> String {
        let chars: Vec<char> = String::from_utf8_lossy(raw).chars().collect();
        let mut rows: Vec<Vec<char>> = Vec::new();
        let (mut row, mut col) = (0usize, 0usize);
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '\x1b' if chars.get(i + 1) == Some(&'[') => {
                    let start = i + 2;
                    let mut end = start;
                    while matches!(chars.get(end), Some(c) if ('\x20'..='\x3f').contains(c)) {
                        end += 1;
                    }
                    let params: String = chars[start..end].iter().collect();
                    match chars.get(end) {
                        Some('H') => {
                            let mut at = params.split(';').map(|n| n.parse::<usize>().unwrap_or(1));
                            row = at.next().unwrap_or(1).saturating_sub(1);
                            col = at.next().unwrap_or(1).saturating_sub(1);
                        }
                        Some('J') if params == "2" => rows.clear(),
                        Some('K') => {
                            if let Some(line) = rows.get_mut(row) {
                                line.truncate(col);
                            }
                        }
                        _ => {}
                    }
                    i = end + 1;
                }
                '\x1b' if matches!(chars.get(i + 1), Some(']' | 'P' | '_' | '^' | 'X')) => {
                    i += 2;
                    while !matches!(chars.get(i), None | Some('\x07' | '\x1b')) {
                        i += 1;
                    }
                    i += if chars.get(i) == Some(&'\x1b') { 2 } else { 1 };
                }
                '\x1b' if matches!(chars.get(i + 1), Some(c) if ('\x20'..='\x2f').contains(c)) => {
                    i += 3
                }
                '\x1b' => i += 2,
                '\r' => {
                    col = 0;
                    i += 1;
                }
                '\n' => {
                    row += 1;
                    i += 1;
                }
                c => {
                    if !c.is_control() {
                        if rows.len() <= row {
                            rows.resize(row + 1, Vec::new());
                        }
                        let line = &mut rows[row];
                        if line.len() <= col {
                            line.resize(col + 1, ' ');
                        }
                        line[col] = c;
                        col += 1;
                    }
                    i += 1;
                }
            }
        }
        let lines: Vec<String> = rows.iter().map(|line| line.iter().collect()).collect();
        lines.join("\n")
    }

    /// Types `<sigil>name` into an idle composer, nudges the width for a
    /// repaint, then closes the band and clears the draft. Returns the screen
    /// as painted from the typing on (see `painted_screen`).
    fn palette_frame_for(pty: &mut Pty, sigil: char, name: &str, toggle: &mut bool) -> String {
        let typed_at = pty.raw().len();
        pty.send(format!("{sigil}{name}").as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(400));
        *toggle = !*toggle;
        repainted_frame_after_resize(pty, 60, if *toggle { 141 } else { 140 });
        let frame = painted_screen(&pty.raw()[typed_at..]);
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(200));
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(300));
        frame
    }

    /// What a palette row for `c` reads as once whitespace is squeezed: the
    /// sigil, the name, and the head of the description (the REPL menu that
    /// produced `c` wraps long descriptions at its width). Once the space
    /// after a command word is typed the band lists its sub-verbs by verb
    /// alone (`mcp-server  Show an MCP ...`), so that form counts too.
    fn row_markers(sigil: char, c: &Candidate) -> Vec<String> {
        let head: String = c.description.chars().take(20).collect();
        let mut markers = vec![squeezed(&format!("{sigil}{}{head}", c.name))];
        if let Some((_, verb)) = c.name.split_once(' ') {
            markers.push(squeezed(&format!("{verb}{head}")));
        }
        markers
    }

    fn offers(frame: &str, sigil: char, c: &Candidate) -> bool {
        let flat = squeezed(frame);
        row_markers(sigil, c).iter().any(|m| flat.contains(m))
    }

    /// The palette rows painted in `frame`, parsed the way the REPL menu is:
    /// every `<sigil>name  description` row but the composer's own line,
    /// which reads as `typed` with nothing after it.
    fn palette_rows(frame: &str, sigil: char, typed: &str) -> Vec<Candidate> {
        parse_sigil_rows(frame, sigil)
            .into_iter()
            .filter(|row| !(row.description.is_empty() && row.name == typed))
            .collect()
    }

    /// Cold start under `sigil`: every candidate the REPL completer offers
    /// is a palette row when its name is typed, every row the palette paints
    /// (bare or while a name is typed) is a completer candidate, and the
    /// bare-sigil band's total (rows shown plus the `+N more` tail) equals
    /// the completer's count.
    fn assert_palette_matches_completer(prefix_yaml: &str, sigil: char, scenario: &str) {
        let repl_dir = fresh_config_dir(&format!("{scenario}-repl"));
        let _repl_guard = TempDirGuard(repl_dir.clone());
        write_dry_run_config_with(&repl_dir, prefix_yaml);
        let listed = repl_completer_candidates(&repl_dir, sigil);

        let dir = fresh_config_dir(&format!("{scenario}-tui"));
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, prefix_yaml);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(500));

        let mut toggle = false;
        let bare = palette_frame_for(&mut pty, sigil, "", &mut toggle);
        let bare_flat = squeezed(&bare);
        let mut painted = palette_rows(&bare, sigil, "");
        let shown = painted.len();
        let more: usize = bare_flat
            .split("(+")
            .nth(1)
            .and_then(|s| s.split("more").next())
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0);

        let mut missing = Vec::new();
        for c in &listed {
            // Keys typed in one write are handled one per loop iteration and
            // the band can be painted mid-word; one retry separates a
            // torn frame from a row that is not offered.
            let offered = (0..2).any(|_| {
                let frame = palette_frame_for(&mut pty, sigil, &c.name, &mut toggle);
                painted.extend(palette_rows(&frame, sigil, &c.name));
                offers(&frame, sigil, c)
            });
            if !offered {
                missing.push(c.name.clone());
            }
        }
        pty.read_for(HOTKEY_GAP);
        quit_and_check(&mut pty);

        assert!(
            missing.is_empty(),
            "under `{sigil}` the palette does not offer {} of the {} commands the REPL \
         completer offers cold: {missing:?}",
            missing.len(),
            listed.len()
        );
        let completer_names: BTreeSet<&str> = listed.iter().map(|c| c.name.as_str()).collect();
        let strangers: BTreeSet<&str> = painted
            .iter()
            .map(|row| row.name.as_str())
            .filter(|name| !completer_names.contains(name))
            .collect();
        assert!(
            strangers.is_empty(),
            "under `{sigil}` the palette paints rows the REPL completer does not offer: \
         {strangers:?}"
        );
        assert_eq!(
            shown + more,
            listed.len(),
            "under `{sigil}` the bare palette band paints {shown} rows + {more} more, the REPL \
         completer offers {} commands: {completer_names:?}\n{bare}",
            listed.len()
        );
    }

    #[test]
    #[serial]
    fn palette_set_equals_the_repl_completer_set_under_slash() {
        assert_palette_matches_completer("command_prefix: slash\n", '/', "palette-slash");
    }

    #[test]
    #[serial]
    fn palette_set_equals_the_repl_completer_set_under_both() {
        assert_palette_matches_completer("command_prefix: both\n", '/', "palette-both");
    }

    #[test]
    #[serial]
    fn palette_set_equals_the_repl_completer_set_under_both_with_a_dot() {
        assert_palette_matches_completer("command_prefix: both\n", '.', "palette-both-dot");
    }

    #[test]
    #[serial]
    fn palette_set_equals_the_repl_completer_set_under_dot() {
        assert_palette_matches_completer("", '.', "palette-dot");
    }

    /// `.tool enable` / `.tool disable` are offered by the `.tool` filter on
    /// a cold start, before any command has run, and still after one has
    /// (`.info tools` itself works cold, so the commands are available).
    #[test]
    #[serial]
    fn tool_verbs_are_offered_by_the_palette_cold_and_warm() {
        let dir = fresh_config_dir("palette-warm");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(500));

        let mut toggle = false;
        let cold = palette_frame_for(&mut pty, '.', "tool", &mut toggle);
        let cold_offers = squeezed(&cold).contains("toolenableEnableasingletool");

        let info_at = pty.raw().len();
        pty.send(b".info tools\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "Tools enabled for the next request",
            info_at,
            "`.info tools` running cold",
        );
        pty.read_for(Duration::from_millis(500));

        let warm = palette_frame_for(&mut pty, '.', "tool", &mut toggle);
        let warm_offers = squeezed(&warm).contains("toolenableEnableasingletool");
        pty.read_for(HOTKEY_GAP);
        quit_and_check(&mut pty);

        assert!(
            warm_offers,
            "`.tool` offers nothing even after a command ran:\n{warm}"
        );
        assert!(
            cold_offers,
            "`.tool enable|disable` are hidden on a cold start while `.info tools` works:\n\
         cold frame:\n{cold}"
        );
    }

    /// Writes an `$EDITOR` that replaces the buffer it is handed with `text`
    /// and returns the editor's path.
    fn rewriting_editor(dir: &Path, text: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let editor = dir.join("rewriting-editor.sh");
        fs::write(
            &editor,
            format!("#!/bin/sh\nprintf '%s\\n' '{text}' > \"$1\"\nexit 0\n"),
        )
        .unwrap();
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
        editor.to_str().unwrap().to_string()
    }

    /// The palette's token range is measured on the line the band opened
    /// over. Ctrl+O hands that line to an editor that shortens it to `.i`;
    /// Enter straight after, on the highlight, must not index the old range
    /// into the shorter line: the UI stays up and the composer takes keys.
    #[test]
    #[serial]
    fn the_editor_shrinking_the_line_under_an_open_palette_does_not_crash_enter() {
        let dir = fresh_config_dir("palette-editor-shrink");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let editor = rewriting_editor(&dir, ".i");
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        pty.send(b".in").unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\t").unwrap();
        pty.read_for(HOTKEY_GAP);
        let edit_at = pty.raw().len();
        pty.send(b"\x0f").unwrap();
        wait_for_lend_and_return(&mut pty, edit_at, "Ctrl+O under the palette");
        pty.send(b"\r").unwrap();
        let after = pty.read_for(HOTKEY_GAP);
        assert!(
            !after.contains("exit="),
            "Enter on the palette after the editor shrank the line ended the session:\n{after}"
        );

        // The palette was rebuilt for `.i` with no row highlighted, so that
        // Enter submitted `.i` as a command turn (an unknown one) and left
        // the composer empty; Ctrl+C here would be a quit.
        let settled_at = pty.raw().len();
        pty.read_for(Duration::from_millis(600));
        assert_composer_takes_keys(&mut pty, settled_at, "Enter on the stale palette");
        quit_and_check(&mut pty);
    }

    // ---------------------------------------------------------------------
    // The Esc Esc rewind picker pops N exchanges.
    // ---------------------------------------------------------------------

    /// Two dry-run exchanges, Esc Esc, Down (second row = two exchanges
    /// back), Enter: both exchanges leave the transcript.
    #[test]
    #[serial]
    fn esc_esc_then_the_second_row_rewinds_two_exchanges() {
        let dir = fresh_config_dir("rewind-two");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        let first_at = pty.raw().len();
        pty.send(b"hello one r3w\r").unwrap();
        wait_for_squeezed_after(&mut pty, "> hello one r3w", first_at, "the first exchange");
        pty.read_for(Duration::from_millis(2500));
        let second_at = pty.raw().len();
        pty.send(b"hello two r3w\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "> hello two r3w",
            second_at,
            "the second exchange",
        );
        pty.read_for(Duration::from_millis(2500));

        let picker_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(150));
        pty.send(b"\x1b").unwrap();
        pty.wait_for_after("Rewind to", picker_at, PTY_DEADLINE)
            .expect("Esc Esc did not open the rewind picker");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));

        let repaint_at = pty.raw().len();
        pty.send(b"\x0c").unwrap();
        pty.read_for(Duration::from_millis(800));
        let frame = squeezed(&since(&pty, repaint_at));
        assert!(
            !frame.contains("Rewindto"),
            "the picker is still up after Enter:\n{}",
            since(&pty, repaint_at)
        );
        assert!(
            !frame.contains(">hellotwor3w") && !frame.contains(">hellooner3w"),
            "rewinding two exchanges left one in the transcript:\n{}",
            since(&pty, repaint_at)
        );

        quit_clearing_any_draft(&mut pty);
    }

    /// Choosing a rewind row pops the exchanges and prefills the composer:
    /// after choosing the newest row the composer holds that prompt again,
    /// the way `.undo` restores the prompt in the REPL.
    #[test]
    #[serial]
    fn choosing_a_rewind_row_prefills_the_composer_with_that_prompt() {
        let dir = fresh_config_dir("rewind-prefill");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        let first_at = pty.raw().len();
        pty.send(b"hello one p4f\r").unwrap();
        wait_for_squeezed_after(&mut pty, "> hello one p4f", first_at, "the exchange");
        pty.read_for(Duration::from_millis(2500));

        let picker_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(150));
        pty.send(b"\x1b").unwrap();
        pty.wait_for_after("Rewind to", picker_at, PTY_DEADLINE)
            .expect("Esc Esc did not open the rewind picker");
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));

        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        let flat = squeezed(&frame);
        assert!(
            !flat.contains(">helloonep4f"),
            "the chosen exchange was not popped:\n{frame}"
        );
        assert!(
            !flat.contains("Typeapromptora.command"),
            "after choosing a rewind row the composer shows its placeholder instead of the \
         restored prompt:\n{frame}"
        );
        assert!(
            flat.contains("helloonep4f"),
            "the chosen prompt was not prefilled into the composer:\n{frame}"
        );

        quit_clearing_any_draft(&mut pty);
    }
}

mod rewind_macros_and_queue_drain {
    //! Black-box pins for the bottom pane: an interrupt takes queued lines
    //! off the turn's queue, a line the model reads mid-turn gets its
    //! transcript cell, the rewind picker stops at a non-isolated
    //! macro's row, the palette rebuilt after an editor shortens the line
    //! (Tab and Enter), and the vi layer's cursor clamp at the end of a line.
    //! The built binary on a pseudo-terminal, a scripted fake model where a
    //! turn has to be held open. Unix-only and serial like its sibling pty
    //! modules.
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// Writes an `$EDITOR` that replaces the buffer it is handed with `text`
    /// (an empty `text` empties the buffer) and returns the editor's path.
    fn rewriting_editor(dir: &Path, text: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let editor = dir.join("rewriting-editor.sh");
        let body = if text.is_empty() {
            "#!/bin/sh\n: > \"$1\"\nexit 0\n".to_string()
        } else {
            format!("#!/bin/sh\nprintf '%s\\n' '{text}' > \"$1\"\nexit 0\n")
        };
        fs::write(&editor, body).unwrap();
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
        editor.to_str().unwrap().to_string()
    }

    // ---------------------------------------------------------------------
    // An interrupt takes the queued lines off the turn's queue too.
    // ---------------------------------------------------------------------

    /// A line queued while a tool sleeps, then Ctrl+C, the interrupt that
    /// drops what is held: the turn is interrupted, the queue strip goes,
    /// and the line is neither sent as the next prompt nor left on the
    /// turn's queue. The next turn's own queued line still rides its tool
    /// round. Regression target: the interrupt dropped the preview but left
    /// the `UserMessage` on the turn queue, and the stale entry made the
    /// reconcile walk drop the line queued in the next turn.
    #[test]
    #[serial]
    fn ctrl_c_during_a_tool_round_drops_the_queued_line_from_the_turn_queue_too() {
        let dir = fresh_config_dir("ctrl-c-drains-queue");
        let _guard = TempDirGuard(dir.clone());
        let tool_a = dir.join("tool-a");
        let tool_b = dir.join("tool-b");
        let fake = FakeLlm::start(vec![
            sleep_call(&tool_a),
            sleep_call(&tool_b),
            Reply::Text("done-two".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        // Turn 1: the tool sleeps; one line is queued; Ctrl+C interrupts.
        pty.send(b"first-turn\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-a-started"),
            "the first sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"ghost-line\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        assert!(
            squeezed(&frame).contains("pending:1"),
            "the line queued while the tool slept did not show `pending: 1`:\n{frame}"
        );

        let ctrl_c_at = pty.raw().len();
        pty.send(b"\x03").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pty.read_for(Duration::from_millis(200));
            let text = since(&pty, ctrl_c_at).to_lowercase();
            // A tool round ends with the tool aborted; a held stream with the
            // interrupted marker.
            if text.contains("interrupted") || text.contains("aborted") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no cell said the turn was interrupted or the tool aborted after Ctrl+C:\n{}",
                since(&pty, ctrl_c_at)
            );
        }
        // Long enough for a wrongly surviving queued line to open a turn.
        pty.read_for(Duration::from_millis(2500));
        let frame = repainted_frame_after_resize(&mut pty, 40, 100);
        assert!(
            !frame.contains("pending:"),
            "the queue strip survived the interrupt:\n{frame}"
        );
        assert_eq!(
            fake.served(),
            1,
            "the interrupted turn's queued line opened a request of its own:\n{:#?}",
            fake.bodies()
        );

        // Turn 2: the tool sleeps again; the new queued line must ride the
        // tool round and the stale one must stay gone.
        let second_at = pty.raw().len();
        pty.send(b"second-turn\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-b-started"),
            "the second sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"live-line\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        assert!(
            squeezed(&frame).contains("pending:1"),
            "the line queued in the second turn did not show `pending: 1`:\n{frame}"
        );
        fs::write(dir.join("tool-b-release"), b"").unwrap();
        wait_for_squeezed_after(&mut pty, "done-two", second_at, "the second turn's reply");
        wait_until_no_pending(&mut pty);

        let bodies = fake.bodies();
        assert!(
            bodies.iter().all(|b| !b.contains("ghost-line")),
            "the line queued before the interrupt reached the model:\n{}",
            bodies.join("\n---\n")
        );
        assert_eq!(
            bodies.len(),
            3,
            "unexpected request count (the interrupted turn's line opened a turn, or the \
         second turn's line became a prompt of its own):\n{}",
            bodies.join("\n---\n")
        );
        assert!(
            bodies[2].contains("live-line"),
            "the line queued in the second turn did not ride its tool round:\n{}",
            bodies[2]
        );
        quit_and_check(&mut pty);
    }

    /// Esc while a tool runs, with the tool allowed to finish on its own in
    /// the same instant: the interrupt still ends the turn. No second
    /// request goes out, the scripted reply behind it never streams, and the
    /// composer takes keys. Esc with a turn running is the interrupt; the
    /// tool's own exit status must not resume the turn.
    #[test]
    #[serial]
    fn esc_during_a_tool_round_ends_the_turn_even_when_the_tool_finishes_on_its_own() {
        let dir = fresh_config_dir("esc-tool-finishes");
        let _guard = TempDirGuard(dir.clone());
        let tool_a = dir.join("tool-a");
        let fake = FakeLlm::start(vec![
            sleep_call(&tool_a),
            Reply::Text("never-streamed".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        pty.send(b"first-turn\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-a-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        let esc_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        fs::write(dir.join("tool-a-release"), b"").unwrap();
        // Long enough for the tool to exit, the kill to land, and a wrongly
        // resumed turn to open its next request.
        pty.read_for(Duration::from_secs(6));
        let after = since(&pty, esc_at);
        assert_eq!(
            fake.served(),
            1,
            "Esc did not end the turn: the tool's own exit resumed it into a second request \
         (tool status in the transcript: {}):\n{after}",
            if after.contains("aborted") {
                "aborted"
            } else if after.contains("ok") {
                "ok"
            } else {
                "unknown"
            }
        );
        assert!(
            !squeezed(&after).contains("never-streamed"),
            "the reply scripted behind the interrupted tool round streamed:\n{after}"
        );
        let settled_at = pty.raw().len();
        assert_composer_takes_keys(&mut pty, settled_at, "Esc during the tool round");
        quit_and_check(&mut pty);
    }

    /// A line queued during a tool round rides the next request and, at
    /// that moment, is committed to the transcript as a dim cell with its
    /// marker, so what the model read is on record once the strip's preview
    /// is gone. Under NO_COLOR the marker is the only distinction left.
    #[test]
    #[serial]
    fn a_line_sent_mid_turn_is_committed_to_the_transcript_when_the_model_reads_it() {
        let dir = fresh_config_dir("mid-turn-cell");
        let _guard = TempDirGuard(dir.clone());
        let tool_a = dir.join("tool-a");
        let fake = FakeLlm::start(vec![
            sleep_call(&tool_a),
            Reply::Text("done-one".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[("NO_COLOR", "1")]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        let turn_at = pty.raw().len();
        pty.send(b"first-turn\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-a-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"follow-up k4e-m\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        assert!(
            squeezed(&frame).contains("pending:1"),
            "the line queued while the tool slept did not show `pending: 1`:\n{frame}"
        );
        assert!(
            !squeezed(&frame).contains("(sentmid-turn)"),
            "the cell was committed before the model read the line:\n{frame}"
        );

        fs::write(dir.join("tool-a-release"), b"").unwrap();
        wait_for_squeezed_after(&mut pty, "done-one", turn_at, "the turn's reply");
        wait_until_no_pending(&mut pty);

        let bodies = fake.bodies();
        assert_eq!(bodies.len(), 2, "{}", bodies.join("\n---\n"));
        assert!(
            bodies[1].contains("follow-up k4e-m"),
            "the queued line did not ride the tool round:\n{}",
            bodies[1]
        );
        let frame = repainted_frame_after_resize(&mut pty, 40, 100);
        assert!(
            squeezed(&frame).contains(">follow-upk4e-m(sentmid-turn)"),
            "the transcript has no mid-turn cell for the line the model read:\n{frame}"
        );
        assert!(
            !frame.contains("pending:"),
            "the queue strip outlived the line it previewed:\n{frame}"
        );
        quit_and_check(&mut pty);
    }

    // ---------------------------------------------------------------------
    // The rewind picker stops at a non-isolated macro's row.
    // ---------------------------------------------------------------------

    /// The two ways a user runs a macro from the composer.
    const MACRO_INVOCATIONS: [&str; 2] = [".macro two-step", ".two-step"];

    /// Without a session the transcript is the whole of the rewind: after a
    /// two-prompt macro and a plain exchange, the picker offers the plain
    /// exchange and nothing at or below the macro, says it stops at the
    /// macro, and choosing the row pops only that exchange and prefills it;
    /// the macro's exchanges stay and the UI is live afterwards. Both macro
    /// invocations are one macro turn.
    #[test]
    #[serial]
    fn the_rewind_picker_stops_at_a_macro_row_with_a_notice_and_pops_the_newer_exchange() {
        let mut misses: Vec<String> = Vec::new();
        for invocation in MACRO_INVOCATIONS {
            let dir = fresh_config_dir("rewind-macro-dry");
            let _guard = TempDirGuard(dir.clone());
            write_dry_run_config_with(&dir, "");
            write_two_prompt_macro(&dir, "two-step");
            let mut pty = spawn_tui(&dir, &[]);
            pty.wait_for("no session", PTY_DEADLINE).unwrap();
            pty.resize(60, 100).unwrap();
            pty.read_for(Duration::from_millis(400));

            let macro_at = pty.raw().len();
            pty.send(format!("{invocation}\r").as_bytes()).unwrap();
            wait_for_squeezed_after(
                &mut pty,
                "macro-prompt-two",
                macro_at,
                "the macro's second step",
            );
            pty.read_for(Duration::from_millis(2500));
            let after_at = pty.raw().len();
            pty.send(b"after-macro\r").unwrap();
            wait_for_squeezed_after(&mut pty, "> after-macro", after_at, "the plain exchange");
            pty.read_for(Duration::from_millis(2500));

            let picker_at = open_rewind_picker(&mut pty);
            let picker = repainted_frame_after_resize(&mut pty, 60, 101);
            let band_flat = squeezed(&picker_band(&picker));
            if !band_flat.contains("after-macro") {
                misses.push(format!(
                    "`{invocation}`: the exchange above the macro is not offered:\n{picker}"
                ));
            }
            if band_flat.contains("macro-prompt-one") || band_flat.contains("macro-prompt-two") {
                misses.push(format!(
                    "`{invocation}`: the picker offers a prompt the macro ran as a row of its \
                 own:\n{picker}"
                ));
            }
            if band_flat.contains("two-step") {
                misses.push(format!(
                    "`{invocation}`: the macro turn is listed as a rewind row instead of \
                 stopping the list:\n{picker}"
                ));
            }
            if !squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
                misses.push(format!(
                    "`{invocation}`: opening the picker over a macro turn gave no notice that the \
                 list stops at the macro:\n{}",
                    since(&pty, picker_at)
                ));
            }

            pty.send(b"\r").unwrap();
            pty.read_for(Duration::from_millis(1500));
            let frame = repainted_frame_after_resize(&mut pty, 60, 100);
            let flat = squeezed(&frame);
            if flat.contains("Rewindto") {
                misses.push(format!(
                    "`{invocation}`: the picker is still up after Enter:\n{frame}"
                ));
            }
            if flat.contains(">after-macro") {
                misses.push(format!(
                    "`{invocation}`: the chosen exchange was not popped:\n{frame}"
                ));
            }
            if !flat.contains("after-macro") {
                misses.push(format!(
                "`{invocation}`: the chosen prompt was not prefilled into the composer:\n{frame}"
            ));
            }
            if !flat.contains("macro-prompt-two") {
                misses.push(format!(
                    "`{invocation}`: popping the plain exchange took the macro's exchanges with \
                 it:\n{frame}"
                ));
            }

            // Clear whatever was prefilled and prove the UI is live.
            clear_any_draft(&mut pty);
            let alive_at = pty.raw().len();
            pty.send(b"still-alive\r").unwrap();
            wait_for_squeezed_after(
                &mut pty,
                "> still-alive",
                alive_at,
                "the exchange after the rewind",
            );
            pty.read_for(Duration::from_millis(1000));
            quit_clearing_any_draft(&mut pty);
        }
        assert!(
            misses.is_empty(),
            "{} rewind-picker expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    /// With a session the transcript and the session must stay in step
    /// across a rewind over a non-isolated macro's row: after `old`, the
    /// two-prompt macro and `new`, the picker offers `new` alone (the macro's
    /// one row stands for two exchanges, so nothing at or below it can be
    /// counted), Down cannot reach past it, and Enter pops `new` only. The
    /// prefill is cleared and a fresh prompt is sent; every earlier reply the
    /// transcript still shows is in the request, and the popped one is not.
    /// Run for both ways of invoking the macro.
    #[test]
    #[serial]
    fn rewinding_past_a_macro_row_keeps_the_session_in_step_with_the_transcript() {
        let mut misses: Vec<String> = Vec::new();
        for invocation in MACRO_INVOCATIONS {
            let dir = fresh_config_dir("rewind-macro-session");
            let _guard = TempDirGuard(dir.clone());
            let fake = FakeLlm::start(vec![
                Reply::Text("reply-old".to_string()),
                Reply::Text("reply-mone".to_string()),
                Reply::Text("reply-mtwo".to_string()),
                Reply::Text("reply-new".to_string()),
                Reply::Text("reply-after".to_string()),
            ]);
            write_fake_llm_config(&dir, &fake);
            write_two_prompt_macro(&dir, "two-step");
            let mut pty = spawn_tui(&dir, &[]);
            pty.wait_for("no session", PTY_DEADLINE).unwrap();
            pty.resize(60, 100).unwrap();
            pty.read_for(Duration::from_millis(400));
            pty.send(b".session\r").unwrap();
            pty.wait_for("temp", PTY_DEADLINE).unwrap();
            pty.read_for(Duration::from_millis(500));

            let at = pty.raw().len();
            pty.send(b"old-prompt\r").unwrap();
            wait_for_squeezed_after(&mut pty, "reply-old", at, "the first reply");
            pty.read_for(Duration::from_millis(1500));
            let at = pty.raw().len();
            pty.send(format!("{invocation}\r").as_bytes()).unwrap();
            wait_for_squeezed_after(&mut pty, "reply-mtwo", at, "the macro's second reply");
            pty.read_for(Duration::from_millis(2500));
            let at = pty.raw().len();
            pty.send(b"new-prompt\r").unwrap();
            wait_for_squeezed_after(&mut pty, "reply-new", at, "the newest reply");
            pty.read_for(Duration::from_millis(2500));
            assert_eq!(
                fake.served(),
                4,
                "{invocation}: request count before the rewind"
            );

            let picker_at = open_rewind_picker(&mut pty);
            let picker = repainted_frame_after_resize(&mut pty, 60, 101);
            let band_flat = squeezed(&picker_band(&picker));
            if !band_flat.contains("new-prompt") {
                misses.push(format!(
                    "`{invocation}`: the exchange above the macro is not offered:\n{picker}"
                ));
            }
            if band_flat.contains("old-prompt") || band_flat.contains("two-step") {
                misses.push(format!(
                    "`{invocation}`: the picker offers a row at or below the macro, whose one \
                 row stands for two exchanges:\n{picker}"
                ));
            }
            if !squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
                misses.push(format!(
                    "`{invocation}`: opening the picker over a macro turn gave no notice that the \
                 list stops at the macro:\n{}",
                    since(&pty, picker_at)
                ));
            }
            pty.send(b"\x1b[B").unwrap();
            pty.read_for(Duration::from_millis(400));
            pty.send(b"\r").unwrap();
            pty.read_for(Duration::from_millis(1500));
            let frame = repainted_frame_after_resize(&mut pty, 60, 100);
            let flat = squeezed(&frame);
            if flat.contains("Rewindto") {
                misses.push(format!(
                    "`{invocation}`: the picker is still up after Enter:\n{frame}"
                ));
            }
            if flat.contains("reply-new") {
                misses.push(format!(
                    "`{invocation}`: the newest exchange survived the rewind:\n{frame}"
                ));
            }
            let kept = ["reply-old", "reply-mone", "reply-mtwo"];
            let shown: Vec<&str> = kept
                .into_iter()
                .filter(|marker| flat.contains(marker))
                .collect();
            if shown != kept {
                misses.push(format!(
                    "`{invocation}`: Down then Enter reached past the macro row; the transcript \
                 kept only {shown:?}:\n{frame}"
                ));
            }

            clear_any_draft(&mut pty);
            let after_at = pty.raw().len();
            pty.send(b"after-rewind\r").unwrap();
            wait_for_squeezed_after(
                &mut pty,
                "reply-after",
                after_at,
                "the reply after the rewind",
            );
            pty.read_for(Duration::from_millis(800));
            let bodies = fake.bodies();
            let last = bodies.last().unwrap().clone();
            assert!(
                last.contains("after-rewind"),
                "{invocation}: the prompt after the rewind is not the last request:\n{last}"
            );
            if last.contains("reply-new") {
                misses.push(format!(
                    "`{invocation}`: the popped newest exchange is still in the session:\n{last}"
                ));
            }
            for marker in kept {
                let on_screen = shown.contains(&marker);
                let in_session = last.contains(marker);
                if on_screen != in_session {
                    misses.push(format!(
                        "`{invocation}`: after the rewind `{marker}` is {} the transcript but {} \
                     the session's next request\n--- picker ---\n{picker}\n\
                     --- frame after the rewind ---\n{frame}\n--- next request ---\n{last}",
                        if on_screen { "in" } else { "gone from" },
                        if in_session { "in" } else { "gone from" },
                    ));
                }
            }
            quit_declining_save(&mut pty);
        }
        assert!(
            misses.is_empty(),
            "{} session/transcript expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // A palette left over from a longer line cannot panic on Tab or Enter.
    // ---------------------------------------------------------------------

    /// `.sess` Tab (row highlighted), Ctrl+O to an editor that shortens the
    /// line to `.i`, then Tab and Enter: the UI stays up both times, twice in
    /// one session, and a fresh `.` still opens the palette afterwards.
    #[test]
    #[serial]
    fn tab_and_enter_on_a_palette_outlived_by_an_editor_shortened_line_do_not_crash_twice() {
        let dir = fresh_config_dir("stale-palette-tab");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let editor = rewriting_editor(&dir, ".i");
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        for (pass, typed) in [(1, ".sess"), (2, ".in")] {
            pty.send(typed.as_bytes()).unwrap();
            pty.read_for(Duration::from_millis(400));
            pty.send(b"\t").unwrap();
            pty.read_for(HOTKEY_GAP);
            let edit_at = pty.raw().len();
            pty.send(b"\x0f").unwrap();
            wait_for_lend_and_return(
                &mut pty,
                edit_at,
                &format!("Ctrl+O under the palette (pass {pass})"),
            );
            pty.send(b"\t").unwrap();
            let after_tab = pty.read_for(Duration::from_millis(800));
            assert!(
                !after_tab.contains("exit="),
                "pass {pass}: Tab on the palette after the editor shrank the line ended the \
             session:\n{after_tab}"
            );
            pty.send(b"\r").unwrap();
            let after_enter = pty.read_for(HOTKEY_GAP);
            assert!(
                !after_enter.contains("exit="),
                "pass {pass}: Enter after Tab on the stale palette ended the session:\n{after_enter}"
            );
            // Whatever Tab/Enter did with `.i`, the composer must take keys.
            // A draft may be left (Tab completed to a command); clear it
            // first with End + backspaces rather than Ctrl+C, which would quit
            // an empty idle composer.
            pty.send(b"\x1b[F").unwrap();
            pty.read_for(Duration::from_millis(200));
            for _ in 0..24 {
                pty.send(b"\x7f").unwrap();
            }
            pty.read_for(Duration::from_millis(400));
            let settled_at = pty.raw().len();
            assert_composer_takes_keys(&mut pty, settled_at, &format!("pass {pass}"));
        }

        // The palette state was rebuilt, not disabled: `.` at column 0 opens it
        // on the commands A-Z, `.agent` first.
        pty.send(b".").unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        let flat = squeezed(&frame);
        assert!(
            flat.contains("Useanagent") && flat.contains("more)"),
            "after two editor passes `.` no longer opens the palette:\n{frame}"
        );
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(300));
        quit_clearing_any_draft(&mut pty);
    }

    /// The editor empties the line under an open palette: Tab, then Enter on
    /// the empty composer, neither crashes; the placeholder is back.
    #[test]
    #[serial]
    fn tab_and_enter_after_an_editor_empties_the_line_under_a_palette_do_not_crash() {
        let dir = fresh_config_dir("stale-palette-empty");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let editor = rewriting_editor(&dir, "");
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        pty.send(b".in").unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\t").unwrap();
        pty.read_for(HOTKEY_GAP);
        let edit_at = pty.raw().len();
        pty.send(b"\x0f").unwrap();
        wait_for_lend_and_return(&mut pty, edit_at, "Ctrl+O under the palette");
        pty.send(b"\t").unwrap();
        let after_tab = pty.read_for(Duration::from_millis(800));
        assert!(
            !after_tab.contains("exit="),
            "Tab on the palette after the editor emptied the line ended the session:\n{after_tab}"
        );
        pty.send(b"\r").unwrap();
        let after_enter = pty.read_for(HOTKEY_GAP);
        assert!(
            !after_enter.contains("exit="),
            "Enter on the emptied composer under the stale palette ended the session:\n{after_enter}"
        );
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        let flat = squeezed(&frame);
        assert!(
            flat.contains("Typeapromptora.command") || !flat.contains(".in"),
            "the emptied line came back as a draft:\n{frame}"
        );
        let settled_at = pty.raw().len();
        assert_composer_takes_keys(&mut pty, settled_at, "the emptied palette line");
        quit_clearing_any_draft(&mut pty);
    }

    // ---------------------------------------------------------------------
    // vi normal mode clamps the cursor to the last character.
    // ---------------------------------------------------------------------

    /// `$` lands ON the last character and `l` cannot walk past it, so `x`
    /// deletes the last character each time, `i` inserts before it and `a`
    /// after it; `A` still appends; Enter submits exactly the edited text.
    /// Every step is observed before the verdict.
    #[test]
    #[serial]
    fn vi_normal_mode_keeps_the_cursor_on_the_last_character_at_the_end_of_a_line() {
        let dir = fresh_config_dir("vi-eol-clamp");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "keybindings: vi\n");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let mut misses: Vec<String> = Vec::new();
        let mut toggle = false;
        let mut check = |pty: &mut Pty, toggle: &mut bool, want: &str, step: &str| {
            let frame = next_frame(pty, toggle);
            let got = composer_row(&frame);
            if got != want {
                misses.push(format!("{step}: expected `{want}`, got `{got}`:\n{frame}"));
            }
        };

        pty.send(b"hello world").unwrap();
        pty.read_for(Duration::from_millis(400));
        key(&mut pty, b"\x1b");
        key(&mut pty, b"$");
        key(&mut pty, b"x");
        check(&mut pty, &mut toggle, "hello worl", "$ x");
        key(&mut pty, b"x");
        check(
            &mut pty,
            &mut toggle,
            "hello wor",
            "x again at the end (clamp re-applied)",
        );
        key(&mut pty, b"$");
        key(&mut pty, b"l");
        key(&mut pty, b"l");
        key(&mut pty, b"x");
        check(
            &mut pty,
            &mut toggle,
            "hello wo",
            "$ l l x (l must not pass the last char)",
        );
        key(&mut pty, b"$");
        key(&mut pty, b"i");
        key(&mut pty, b"!");
        key(&mut pty, b"\x1b");
        check(
            &mut pty,
            &mut toggle,
            "hello w!o",
            "$ i ! (insert before the last char)",
        );
        key(&mut pty, b"$");
        key(&mut pty, b"a");
        key(&mut pty, b"Z");
        key(&mut pty, b"\x1b");
        check(
            &mut pty,
            &mut toggle,
            "hello w!oZ",
            "$ a Z (append after the last char)",
        );
        key(&mut pty, b"A");
        key(&mut pty, b"Q");
        check(&mut pty, &mut toggle, "hello w!oZQ", "A Q");

        let submit_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        let deadline = Instant::now() + PTY_DEADLINE;
        loop {
            pty.read_for(Duration::from_millis(200));
            if squeezed(&since(&pty, submit_at)).contains(">hellow!oZQ") {
                break;
            }
            if Instant::now() >= deadline {
                misses.push(format!(
                    "Enter did not submit `hello w!oZQ` as the prompt:\n{}",
                    since(&pty, submit_at)
                ));
                break;
            }
        }
        pty.read_for(Duration::from_millis(1000));
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} vi clamp step(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }
}

mod hidden_verbs_history_and_barriers {
    //! Black-box pins for the bottom pane's formerly hidden verbs, history
    //! walk and rewind barriers: the verbs the palette once left out while
    //! their prompts refused under the UI are listed cold and after the
    //! state warms up (a command ran, an agent was switched) under both
    //! prefixes, and typing one in full still runs it; the history walk
    //! (Up/Down, Ctrl+R) never reopens the palette over
    //! the recalled line; `.macro enable` is not a rewind stop and a picker
    //! with nothing above the macro cannot crash; a backtick document
    //! command's grandchild dies on Esc while its shell still leads the
    //! group; Ctrl+D during a turn drops the queued lines. The built binary
    //! on a pseudo-terminal, a scripted fake model where a turn has to be
    //! held open. Unix-only and serial like its sibling pty modules.
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// The verbs the palette left out while their prompts still refused
    /// under the UI, each paired with a typed head that narrows the palette
    /// to it (and to nothing else spelling it).
    const FORMERLY_HIDDEN_HEADS: [(&str, &str); 5] = [
        ("ra", "rag"),
        ("va", "vault"),
        ("ins", "install"),
        ("unin", "uninstall"),
        ("upd", "update"),
    ];

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
             \x20 You are the plain agent. Reply briefly.\n"
            ),
        )
        .unwrap();
    }

    /// Alternates the width (141/140) so every call is a real resize, and
    /// returns the frame.
    fn next_wide_frame(pty: &mut Pty, toggle: &mut bool) -> String {
        *toggle = !*toggle;
        repainted_frame_after_resize(pty, 60, if *toggle { 141 } else { 140 })
    }

    /// Types `<sigil><head>` into an idle composer, repaints, then closes the
    /// band and clears the draft. Returns every printable byte written from
    /// the typing on.
    fn palette_frame_for(pty: &mut Pty, sigil: char, head: &str, toggle: &mut bool) -> String {
        let typed_at = pty.raw().len();
        pty.send(format!("{sigil}{head}").as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(400));
        next_wide_frame(pty, toggle);
        let frame = since(pty, typed_at);
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(200));
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(300));
        frame
    }

    /// Every formerly hidden verb that does NOT show up as `<sigil><verb>`
    /// in the frame painted for its head (the composer holds only the head,
    /// so a full spelling is a palette row), plus the positive control:
    /// `ses` must offer `<sigil>session` or the band was not read at all.
    /// While an agent is active `.rag` is not a command at all (the REPL
    /// table gates it on no agent), so in that state its row must be absent
    /// and its absence is not a miss.
    fn formerly_hidden_verbs_missing(
        pty: &mut Pty,
        sigil: char,
        toggle: &mut bool,
        state: &str,
        agent_active: bool,
    ) -> Vec<String> {
        let control = palette_frame_for(pty, sigil, "ses", toggle);
        assert!(
            control.contains(&format!("{sigil}session")),
            "{state}: under `{sigil}` typing `{sigil}ses` offers no `{sigil}session` row; the band \
         was not read:\n{control}"
        );
        let mut missing = Vec::new();
        for (head, verb) in FORMERLY_HIDDEN_HEADS {
            let frame = palette_frame_for(pty, sigil, head, toggle);
            let spelled = format!("{sigil}{verb}");
            let expected = !(agent_active && verb == "rag");
            if frame.contains(&spelled) != expected {
                missing.push(format!(
                    "{state}: `{sigil}{head}` {} {spelled}:\n{frame}",
                    if expected { "does not offer" } else { "offers" }
                ));
            }
        }
        missing
    }

    // ---------------------------------------------------------------------
    // Formerly hidden verbs are listed in every palette state.
    // ---------------------------------------------------------------------

    /// Under `sigil` the five formerly hidden verbs are offered by the
    /// palette on a cold start, after a command ran, after a dry-run
    /// exchange, and after an `.agent` switch rebuilt the palette's state;
    /// the `session` control row is offered in every state, and `.rag`,
    /// which the REPL table withholds while an agent is active, is absent
    /// from the agent state alone.
    fn assert_formerly_hidden_verbs_are_listed(prefix_yaml: &str, sigil: char, scenario: &str) {
        let dir = fresh_config_dir(scenario);
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, prefix_yaml);
        write_plain_agent(&dir, "plain-agent");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(500));
        let mut toggle = false;
        let mut missing = Vec::new();

        missing.extend(formerly_hidden_verbs_missing(
            &mut pty,
            sigil,
            &mut toggle,
            "cold",
            false,
        ));

        let info_at = pty.raw().len();
        pty.send(format!("{sigil}info tools\r").as_bytes()).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "Tools enabled for the next request",
            info_at,
            "`info tools` running cold",
        );
        pty.read_for(Duration::from_millis(500));
        missing.extend(formerly_hidden_verbs_missing(
            &mut pty,
            sigil,
            &mut toggle,
            "after a command ran",
            false,
        ));

        let turn_at = pty.raw().len();
        pty.send(b"warm-up-q8w\r").unwrap();
        wait_for_squeezed_after(&mut pty, "> warm-up-q8w", turn_at, "the dry-run exchange");
        pty.read_for(Duration::from_millis(2000));
        missing.extend(formerly_hidden_verbs_missing(
            &mut pty,
            sigil,
            &mut toggle,
            "after an exchange",
            false,
        ));

        let agent_at = pty.raw().len();
        pty.send(format!("{sigil}agent plain-agent\r").as_bytes())
            .unwrap();
        wait_for_squeezed_after(&mut pty, "plain-agent", agent_at, "the agent switch");
        pty.read_for(Duration::from_millis(1500));
        missing.extend(formerly_hidden_verbs_missing(
            &mut pty,
            sigil,
            &mut toggle,
            "after an agent switch",
            true,
        ));

        pty.read_for(HOTKEY_GAP);
        quit_clearing_any_draft(&mut pty);
        assert!(
            missing.is_empty(),
            "under `{sigil}` the palette leaves out formerly hidden verbs in {} state(s):\n\n{}",
            missing.len(),
            missing.join("\n\n")
        );
    }

    #[test]
    #[serial]
    fn formerly_hidden_verbs_are_listed_in_the_palette_cold_and_warm_under_dot() {
        assert_formerly_hidden_verbs_are_listed("", '.', "listed-warm-dot");
    }

    #[test]
    #[serial]
    fn formerly_hidden_verbs_are_listed_in_the_palette_cold_and_warm_under_slash() {
        assert_formerly_hidden_verbs_are_listed(
            "command_prefix: slash\n",
            '/',
            "listed-warm-slash",
        );
    }

    /// A formerly hidden verb typed in full still runs: `.rag`, `.vault` and
    /// `.uninstall` under the TUI end in the command's own error or usage
    /// (`.rag` with no embedding model configured fails before it has
    /// anything to ask), never a hang or a crash, and the composer takes
    /// keys afterwards. (`.install`/`.update` reach the network.)
    #[test]
    #[serial]
    fn a_formerly_hidden_verb_typed_in_full_still_runs_and_the_ui_survives() {
        let dir = fresh_config_dir("listed-typed");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(500));

        for (verb, outcome) in [
            ("rag", "No available embedding model"),
            ("vault", "Usage: .vault"),
            ("uninstall", "Usage: .uninstall"),
        ] {
            let at = pty.raw().len();
            pty.send(format!(".{verb}\r").as_bytes()).unwrap();
            let started = Instant::now();
            // The outcome is a command cell; wait for the command header to
            // be committed, then for the composer to take keys again.
            pty.read_for(Duration::from_millis(1500));
            assert_composer_takes_keys(&mut pty, at, &format!("`.{verb}` typed in full"));
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "`.{verb}` held the turn for {:?}",
                started.elapsed()
            );
            // The command cell's header and its error or usage: the typed
            // line alone would also be echoed by the composer.
            let after = squeezed(&since(&pty, at));
            assert!(
                after.contains(&format!(">.{verb}")) && after.contains(&squeezed(outcome)),
                "`.{verb}` did not run to `{outcome}` in the transcript:\n{}",
                since(&pty, at)
            );
        }
        quit_and_check(&mut pty);
    }

    // ---------------------------------------------------------------------
    // The history walk never reopens the palette.
    // ---------------------------------------------------------------------

    /// After `.info tools` and `.info` ran, Up on the empty composer recalls
    /// `.info` with NO palette band over it (no `Show system info` row), the
    /// next Up walks on to `.info tools` instead of moving a band highlight,
    /// Down comes back to `.info`, and Enter runs it. Ctrl+R to `.info` is a
    /// recall without a band too. The palette returns with the next real
    /// edit.
    #[test]
    #[serial]
    fn walking_the_history_with_up_down_and_ctrl_r_never_reopens_the_palette() {
        let dir = fresh_config_dir("history-walk");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 140).unwrap();
        pty.read_for(Duration::from_millis(500));

        let at = pty.raw().len();
        pty.send(b".info tools\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "Tools enabled for the next request",
            at,
            "`.info tools`",
        );
        pty.read_for(Duration::from_millis(800));
        let at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", at, PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(1500));

        let mut toggle = false;
        let mut misses: Vec<String> = Vec::new();

        // Up: `.info` recalled, no band.
        key(&mut pty, b"\x1b[A");
        let frame = next_wide_frame(&mut pty, &mut toggle);
        let row = composer_row_with_footer(&frame, "dryrun:dry-model");
        if row != ".info" {
            misses.push(format!(
                "Up did not recall `.info` (composer: `{row}`):\n{frame}"
            ));
        }
        if frame.contains("Show system info") {
            misses.push(format!(
                "Up reopened the palette over the recalled `.info`:\n{frame}"
            ));
        }

        // Up again: the walk continues to `.info tools`.
        key(&mut pty, b"\x1b[A");
        let frame = next_wide_frame(&mut pty, &mut toggle);
        let row = composer_row_with_footer(&frame, "dryrun:dry-model");
        if row != ".info tools" {
            misses.push(format!(
                "a second Up did not walk on to `.info tools` (composer: `{row}`):\n{frame}"
            ));
        }
        if frame.contains("Show system info") {
            misses.push(format!("the second Up shows a palette band:\n{frame}"));
        }

        // Down: back to `.info`.
        key(&mut pty, b"\x1b[B");
        let frame = next_wide_frame(&mut pty, &mut toggle);
        let row = composer_row_with_footer(&frame, "dryrun:dry-model");
        if row != ".info" {
            misses.push(format!(
                "Down did not come back to `.info` (composer: `{row}`):\n{frame}"
            ));
        }

        // Enter runs the recalled command (a palette would swallow it).
        let run_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        if pty
            .wait_for_after("log_path", run_at, PTY_DEADLINE)
            .is_err()
        {
            misses.push(format!(
                "Enter on the recalled `.info` did not run it:\n{}",
                since(&pty, run_at)
            ));
        }
        pty.read_for(Duration::from_millis(1500));

        // Ctrl+R, `info`, Enter: recalled without a band.
        pty.send(b"\x12").unwrap();
        pty.read_for(Duration::from_millis(600));
        pty.send(b"info").unwrap();
        pty.read_for(Duration::from_millis(600));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        let frame = next_wide_frame(&mut pty, &mut toggle);
        let row = composer_row_with_footer(&frame, "dryrun:dry-model");
        if !row.starts_with(".info") {
            misses.push(format!(
                "Ctrl+R `info` Enter did not recall an `.info` line (composer: `{row}`):\n{frame}"
            ));
        }
        if frame.contains("Show system info") {
            misses.push(format!("the Ctrl+R recall reopened the palette:\n{frame}"));
        }
        // Enter on the recalled line must run it, as it does after Up.
        let run_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        if pty
            .wait_for_after("log_path", run_at, Duration::from_secs(6))
            .is_err()
        {
            let frame = next_wide_frame(&mut pty, &mut toggle);
            misses.push(format!(
                "Enter on the Ctrl+R-recalled `.info` did not run it (composer now: `{}`):\n{}",
                composer_row_with_footer(&frame, "dryrun:dry-model"),
                since(&pty, run_at)
            ));
        }
        pty.read_for(Duration::from_millis(800));

        // A real edit brings the palette back: any draft cleared, `.in`
        // opens.
        clear_any_draft(&mut pty);
        pty.send(b".in").unwrap();
        pty.read_for(Duration::from_millis(500));
        let frame = next_wide_frame(&mut pty, &mut toggle);
        if !frame.contains("Show system info") {
            misses.push(format!(
                "after the history walk a fresh `.in` no longer opens the palette:\n{frame}"
            ));
        }
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(300));
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} history-walk expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // `.macro enable` is not a rewind stop; nothing above the macro is safe.
    // ---------------------------------------------------------------------

    /// `before`, `.macro enable two-step`, `after`: the toggle only enables
    /// one macro and runs no exchange, so the picker offers BOTH prompts
    /// without the macro-stop notice, and choosing `before` pops both
    /// exchanges and prefills `before`.
    #[test]
    #[serial]
    fn macro_enable_is_not_a_rewind_stop_and_two_rows_pop_two_exchanges() {
        let dir = fresh_config_dir("rewind-macro-enable");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        write_two_prompt_macro(&dir, "two-step");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        let at = pty.raw().len();
        pty.send(b"before-q8w\r").unwrap();
        wait_for_squeezed_after(&mut pty, "> before-q8w", at, "the first exchange");
        pty.read_for(Duration::from_millis(2500));
        let at = pty.raw().len();
        pty.send(b".macro enable two-step\r").unwrap();
        pty.read_for(Duration::from_millis(2000));
        let toggled = since(&pty, at);
        assert!(
            !squeezed(&toggled).contains("macro-prompt-one"),
            "`.macro enable two-step` ran the macro instead of enabling it:\n{toggled}"
        );
        let at = pty.raw().len();
        pty.send(b"after-q8w\r").unwrap();
        wait_for_squeezed_after(&mut pty, "> after-q8w", at, "the second exchange");
        pty.read_for(Duration::from_millis(2500));

        let picker_at = open_rewind_picker(&mut pty);
        let picker = repainted_frame_after_resize(&mut pty, 60, 101);
        let band_flat = squeezed(&picker_band(&picker));
        let mut misses: Vec<String> = Vec::new();
        if !band_flat.contains("after-q8w") || !band_flat.contains("before-q8w") {
            misses.push(format!(
                "the picker does not offer both prompts across `.macro enable`:\n{picker}"
            ));
        }
        if squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
            misses.push(format!(
                "`.macro enable` was taken for a macro turn (the stop notice is shown):\n{}",
                since(&pty, picker_at)
            ));
        }
        // Down to `before-q8w`, Enter: both exchanges go, `before-q8w` is the
        // draft.
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        if flat.contains("Rewindto") {
            misses.push(format!("the picker is still up after Enter:\n{frame}"));
        }
        if flat.contains(">after-q8w") || flat.contains(">before-q8w") {
            misses.push(format!(
                "choosing the older row did not pop both exchanges:\n{frame}"
            ));
        }
        let row = composer_row_with_footer(&frame, "dryrun:dry-model");
        if row != "before-q8w" {
            misses.push(format!(
                "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
            ));
        }
        clear_any_draft(&mut pty);
        let alive_at = pty.raw().len();
        pty.send(b"still-alive-q8w\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "> still-alive-q8w",
            alive_at,
            "the exchange after",
        );
        pty.read_for(Duration::from_millis(800));
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} rewind expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    /// The newest turn IS the macro: Esc Esc has nothing to offer. Whatever
    /// the pane shows (the notice, an empty picker, nothing), Enter and Esc
    /// cannot crash it, no macro-internal prompt is offered or prefilled, the
    /// macro's exchanges stay, and the composer takes keys afterwards.
    #[test]
    #[serial]
    fn esc_esc_with_nothing_above_the_macro_turn_does_not_crash_or_pop() {
        let dir = fresh_config_dir("rewind-only-macro");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        write_two_prompt_macro(&dir, "two-step");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        let at = pty.raw().len();
        pty.send(b".two-step\r").unwrap();
        wait_for_squeezed_after(&mut pty, "macro-prompt-two", at, "the macro's second step");
        pty.read_for(Duration::from_millis(2500));

        let picker_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(150));
        pty.send(b"\x1b").unwrap();
        pty.read_for(HOTKEY_GAP);
        let opened = since(&pty, picker_at);
        let frame = repainted_frame_after_resize(&mut pty, 60, 101);
        let band_flat = squeezed(&picker_band(&frame));
        assert!(
            !band_flat.contains("macro-prompt-one") && !band_flat.contains("macro-prompt-two"),
            "the picker offers a prompt the macro ran:\n{frame}"
        );
        assert!(
            !band_flat.contains("two-step"),
            "the macro turn is a rewind row:\n{frame}"
        );
        // Enter, then Esc: neither may crash or pop the macro's exchanges.
        let keys_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        let after_enter = since(&pty, keys_at);
        assert!(
            !after_enter.contains("tty-probe-done"),
            "Enter on the picker over a macro-only transcript ended the process (exit {}); \
         Esc Esc painted:\n{opened}\n--- after Enter ---\n{after_enter}",
            exit_code(&after_enter)
        );
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(800));
        let after_esc = since(&pty, keys_at);
        assert!(
            !after_esc.contains("tty-probe-done"),
            "Esc after Enter on the picker over a macro-only transcript ended the process (exit {}); \
         Esc Esc painted:\n{opened}\n--- after Enter, Esc ---\n{after_esc}",
            exit_code(&after_esc)
        );
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        assert!(
            flat.contains("macro-prompt-two"),
            "Enter on an empty picker popped the macro's exchanges (Esc Esc painted:\n{opened}\n):\n{frame}"
        );
        let row = composer_row_with_footer(&frame, "dryrun:dry-model");
        assert!(
            !row.contains("macro-prompt"),
            "a macro-internal prompt was prefilled (composer: `{row}`):\n{frame}"
        );
        clear_any_draft(&mut pty);
        let settled_at = pty.raw().len();
        assert_composer_takes_keys(&mut pty, settled_at, "Esc Esc over a macro-only transcript");
        quit_clearing_any_draft(&mut pty);
    }

    // ---------------------------------------------------------------------
    // A backtick document command's grandchild dies on Esc while its shell
    // still leads the group.
    // ---------------------------------------------------------------------

    /// `` .file `sh <script>` `` where the script forks a sleeper and waits
    /// on it (a grandchild of the document command's shell), then one where
    /// it backgrounds the sleeper and exits, leaving it holding the capture
    /// pipe. Each script records the sleeper's pid, so the checks and the
    /// cleanup touch that process and no other. Esc ends each turn within
    /// ten seconds and the composer takes keys. Both sleepers die with the
    /// shell's group within two seconds of Esc: the exited shell is kept a
    /// zombie until then, so its pid still names the group it led.
    #[test]
    #[serial]
    fn a_backtick_document_commands_grandchild_dies_on_esc_even_after_its_shell_exited() {
        let dir = fresh_config_dir("backtick-grandchild");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let waits_pid = dir.join("waits.pid");
        let orphans_pid = dir.join("orphans.pid");
        let waits = write_script(
            &dir,
            "gc-waits.sh",
            &format!(
                "#!/bin/sh\nsleep 6174 & echo $! > '{}'\nwait\necho waited\n",
                waits_pid.display()
            ),
        );
        let orphans = write_script(
            &dir,
            "gc-orphans.sh",
            &format!(
                "#!/bin/sh\nsleep 6175 & echo $! > '{}'\necho backgrounded\n",
                orphans_pid.display()
            ),
        );
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        let mut misses: Vec<String> = Vec::new();
        let mut sleepers = SleeperPids(Vec::new());
        for (script, pid_file, what) in [
            (waits, &waits_pid, "a waited-on grandchild"),
            (orphans, &orphans_pid, "a backgrounded, orphaned grandchild"),
        ] {
            let turn_at = pty.raw().len();
            pty.send(format!(".file `sh {script}` -- summarize\r").as_bytes())
                .unwrap();
            // The sleeper must be up before Esc, or the test proves nothing.
            let pid = wait_for_pid_in(&mut pty, pid_file, &format!("{what}: the sleeper's pid"));
            sleepers.0.push(pid);
            assert!(
                alive(pid),
                "{what}: the sleeper (pid {pid}) is gone before Esc:\n{}",
                since(&pty, turn_at)
            );
            pty.read_for(Duration::from_millis(800));
            pty.send(b"\x1b").unwrap();
            let esc_at = Instant::now();
            if wait_for_process_gone(pid, Duration::from_secs(2)) {
                sleepers.forget(pid);
            } else {
                misses.push(format!(
                    "{what}: the sleeper (pid {pid}) outlived Esc by two seconds:\n{}",
                    since(&pty, turn_at)
                ));
            }
            assert_composer_takes_keys(&mut pty, turn_at, &format!("Esc on `.file` with {what}"));
            if esc_at.elapsed() > Duration::from_secs(10) {
                misses.push(format!(
                    "{what}: Esc took {:?} to end the backtick turn",
                    esc_at.elapsed()
                ));
            }
        }
        quit_and_check(&mut pty);
        assert!(
            misses.is_empty(),
            "{} grandchild expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // Ctrl+D during a turn drops the queued lines.
    // ---------------------------------------------------------------------

    /// A line queued while a tool sleeps, then Ctrl+D: the session ends
    /// cleanly (exit 0, terminal restored) within the deadline, and the
    /// queued line never opens a request of its own on the way out.
    #[test]
    #[serial]
    fn ctrl_d_during_a_tool_round_quits_without_sending_the_queued_line() {
        let dir = fresh_config_dir("ctrl-d-queued");
        let _guard = TempDirGuard(dir.clone());
        let tool_a = dir.join("tool-a");
        let fake = FakeLlm::start(vec![
            sleep_call(&tool_a),
            Reply::Text("never-sent-q8w".to_string()),
            Reply::Text("never-sent-either-q8w".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        pty.send(b"first-turn-q8w\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-a-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"ghost-d-q8w\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        assert!(
            squeezed(&frame).contains("pending:1"),
            "the line queued while the tool slept did not show `pending: 1`:\n{frame}"
        );

        let quit_at = Instant::now();
        pty.send(b"\x04").unwrap();
        let transcript = pty
            .wait_for("tty-probe-done", PTY_DEADLINE)
            .unwrap_or_else(|e| {
                panic!("Ctrl+D during the tool round did not end the session: {e}")
            });
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert!(
            quit_at.elapsed() < Duration::from_secs(15),
            "Ctrl+D took {:?} to end the session",
            quit_at.elapsed()
        );
        // Time for a wrongly flushed queued line to open a request.
        std::thread::sleep(Duration::from_millis(1500));
        let bodies = fake.bodies();
        assert!(
            bodies.iter().all(|b| !b.contains("ghost-d-q8w")),
            "the line queued before Ctrl+D reached the model:\n{}",
            bodies.join("\n---\n")
        );
        assert_eq!(
            fake.served(),
            1,
            "Ctrl+D let the queued line (or the tool's result) open another request:\n{}",
            bodies.join("\n---\n")
        );
        assert!(
            !transcript.contains("never-sent"),
            "a reply scripted behind the quit streamed:\n{transcript}"
        );
    }
}

mod mid_turn_barriers_typeahead_and_queue_swap {
    //! Black-box pins for the bottom pane's queue and rewind edges: a line
    //! the model read mid-turn is a dim transcript cell that is neither a rewind
    //! row nor a barrier; a rewind barrier is decided when the command cell
    //! is committed, so `.macro disable` after a macro ran does not move it
    //! and a disabled macro typed later never made a macro turn; typeahead
    //! Space as a checkbox modal appears ticks nothing; Alt+Up over a draft
    //! swaps it with the newest queued line instead of wiping it; Ctrl+D
    //! purges the turn queue even when the tool finishes right behind it.
    //! The built binary on a pseudo-terminal, a scripted fake model where a
    //! turn has to be held open. Unix-only and serial like its sibling pty
    //! modules.
    use std::fs;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// A tool that asks through one of the prompt helpers (`call` is the
    /// helper invocation) and records the answer it got at `out`.
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
         \x20   echo \"fixture prompt answered k7p\" >> \"$LLM_OUTPUT\"\n\
         }}\n"
        )
    }

    /// The SGR parameter lists written from the start of a repaint up to
    /// `at`: the painter tracks its attribute state across the whole frame
    /// (a row painted in the attributes the previous row left set gets no
    /// SGR of its own), and every frame starts from and ends in a reset.
    fn sgr_since(raw: &[u8], from: usize, at: usize) -> Vec<Vec<u16>> {
        sgr_params(&raw[from..at])
    }

    /// Whether the lists set the dim attribute (SGR 2) without a later reset
    /// or un-dim (SGR 0 / 22) cancelling it. The parameters are read in
    /// order: an extended colour (38 / 48 / 58) carries its own arguments,
    /// `5;N` or `2;r;g;b`, and the `2` inside those is not an attribute.
    fn ends_dim(params: &[Vec<u16>]) -> bool {
        let mut dim = false;
        for list in params {
            let mut rest = list.as_slice();
            while let Some((&p, tail)) = rest.split_first() {
                rest = tail;
                match p {
                    2 => dim = true,
                    0 | 22 => dim = false,
                    38 | 48 | 58 => {
                        let arguments = match rest.first() {
                            Some(5) => 2,
                            Some(2) => 4,
                            _ => 0,
                        };
                        rest = &rest[arguments.min(rest.len())..];
                    }
                    _ => {}
                }
            }
        }
        dim
    }

    #[test]
    fn ends_dim_reads_an_extended_colours_arguments_as_arguments() {
        assert!(!ends_dim(&[vec![38, 5, 2]]));
        assert!(ends_dim(&[vec![2], vec![38, 5, 2]]));
        assert!(!ends_dim(&[vec![2], vec![22]]));
        assert!(!ends_dim(&[vec![48, 2, 2, 2, 2]]));
        assert!(ends_dim(&[vec![38, 2, 10, 20, 30, 2]]));
    }

    /// Whether the lists leave an explicit foreground in effect: a
    /// `38;5;N` or `38;2;r;g;b` with no later `39` or `0` resetting it to
    /// the terminal's default.
    fn ends_coloured(params: &[Vec<u16>]) -> bool {
        let mut coloured = false;
        for list in params {
            let mut rest = list.as_slice();
            while let Some((&p, tail)) = rest.split_first() {
                rest = tail;
                match p {
                    0 | 39 => coloured = false,
                    38 | 48 | 58 => {
                        let arguments = match rest.first() {
                            Some(5) => 2,
                            Some(2) => 4,
                            _ => 0,
                        };
                        if p == 38 && arguments > 0 {
                            coloured = true;
                        }
                        rest = &rest[arguments.min(rest.len())..];
                    }
                    _ => {}
                }
            }
        }
        coloured
    }

    #[test]
    fn ends_coloured_reads_a_reset_and_a_background_as_no_foreground() {
        assert!(ends_coloured(&[vec![38, 5, 248, 48, 5, 235]]));
        assert!(ends_coloured(&[vec![38, 2, 10, 20, 30]]));
        assert!(!ends_coloured(&[vec![38, 5, 248], vec![39]]));
        assert!(!ends_coloured(&[vec![38, 5, 248], vec![0]]));
        assert!(!ends_coloured(&[vec![48, 5, 235]]));
    }

    // ---------------------------------------------------------------------
    // The mid-turn cell: faded in colour, a rewind non-entity.
    // ---------------------------------------------------------------------

    /// After a plain exchange, a tool turn gets a line while its tool sleeps;
    /// once the model read it the transcript carries the line as a cell
    /// painted faded (a foreground colour, not DIM) with its
    /// `(sent mid-turn)` marker. Esc Esc then offers
    /// the tool turn's opener and the older prompt, never the mid-turn line,
    /// and no macro-stop notice; choosing the opener pops the tool turn with
    /// its mid-turn cell and reply, keeps the older exchange, and prefills
    /// the opener.
    #[test]
    #[serial]
    fn a_mid_turn_cell_is_faded_and_neither_a_rewind_row_nor_a_barrier() {
        let dir = fresh_config_dir("mid-turn-rewind");
        let _guard = TempDirGuard(dir.clone());
        let tool_a = dir.join("tool-a");
        let fake = FakeLlm::start(vec![
            Reply::Text("older-reply-k7m".to_string()),
            sleep_call(&tool_a),
            Reply::Text("tool-reply-k7m".to_string()),
            Reply::Text("unexpected-reply-k7m".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        let at = pty.raw().len();
        pty.send(b"older-k7m\r").unwrap();
        wait_for_squeezed_after(&mut pty, "older-reply-k7m", at, "the first reply");
        pty.read_for(Duration::from_millis(800));

        let turn_at = pty.raw().len();
        pty.send(b"opener-k7m\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-a-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"ghost-k7m\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        assert!(
            squeezed(&frame).contains("pending:1"),
            "the line queued while the tool slept did not show `pending: 1`:\n{frame}"
        );

        fs::write(dir.join("tool-a-release"), b"").unwrap();
        wait_for_squeezed_after(&mut pty, "tool-reply-k7m", turn_at, "the tool turn's reply");
        wait_until_no_pending(&mut pty);
        let bodies = fake.bodies();
        assert_eq!(bodies.len(), 3, "{}", bodies.join("\n---\n"));
        assert!(
            bodies[2].contains("ghost-k7m"),
            "the queued line did not ride the tool round:\n{}",
            bodies[2]
        );

        let mut misses: Vec<String> = Vec::new();
        // The committed cell: marker + faded.
        let repaint_at = repaint(&mut pty, 40, 100);
        let frame = since(&pty, repaint_at);
        if !squeezed(&frame).contains(&squeezed(&format!("> ghost-k7m {MID_TURN_MARKER}"))) {
            misses.push(format!(
                "the transcript has no `> ghost-k7m {MID_TURN_MARKER}` cell:\n{frame}"
            ));
        } else {
            let raw = pty.raw();
            let ghost_at = find(raw, b"ghost-k7m", repaint_at)
                .expect("the raw output has the mid-turn cell the stripped frame shows");
            let params = sgr_since(raw, repaint_at, ghost_at);
            if !ends_coloured(&params) || ends_dim(&params) {
                misses.push(format!(
                    "the mid-turn cell is not painted faded, a foreground colour without DIM (SGR state before it: {:?}):\n{frame}",
                    params.last()
                ));
            }
            let opener_at = find(raw, b"opener-k7m", repaint_at)
                .expect("the raw output has the opener cell the stripped frame shows");
            if ends_dim(&sgr_since(raw, repaint_at, opener_at)) {
                misses.push(format!(
                    "the opener cell is dim too, so dim is not the mid-turn distinction:\n{frame}"
                ));
            }
        }

        // Esc Esc: the picker offers the opener and the older prompt only.
        let picker_at = open_rewind_picker(&mut pty);
        let picker = repainted_frame_after_resize(&mut pty, 40, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if band_flat.contains("ghost-k7m") {
            misses.push(format!("the mid-turn line is a rewind row:\n{picker}"));
        }
        if !band_flat.contains("opener-k7m") {
            misses.push(format!(
                "the tool turn's opener is not a rewind row:\n{picker}"
            ));
        }
        if !band_flat.contains("older-k7m") {
            misses.push(format!(
            "the exchange below the mid-turn cell is not offered (the cell acts as a barrier):\n{picker}"
        ));
        }
        if squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
            misses.push(format!(
                "the mid-turn cell raised the macro-stop notice:\n{}",
                since(&pty, picker_at)
            ));
        }
        // Enter on the first row (the opener): the tool turn goes whole.
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame_after_resize(&mut pty, 40, 100);
        let flat = squeezed(&frame);
        if flat.contains("Rewindto") {
            misses.push(format!("the picker is still up after Enter:\n{frame}"));
        }
        if flat.contains(">opener-k7m") || flat.contains("tool-reply-k7m") {
            misses.push(format!(
                "choosing the opener did not pop the tool turn:\n{frame}"
            ));
        }
        if flat.contains("ghost-k7m(sentmid-turn)") {
            misses.push(format!(
                "the mid-turn cell outlived the exchange it was sent in:\n{frame}"
            ));
        }
        if !flat.contains(">older-k7m") || !flat.contains("older-reply-k7m") {
            misses.push(format!("the older exchange was popped too:\n{frame}"));
        }
        let row = composer_row_with_footer(&frame, "fake:fake-model");
        if row != "opener-k7m" {
            misses.push(format!(
                "the opener was not prefilled (composer: `{row}`):\n{frame}"
            ));
        }
        clear_any_draft(&mut pty);
        let settled_at = pty.raw().len();
        assert_composer_takes_keys(&mut pty, settled_at, "rewinding over a mid-turn cell");
        quit_clearing_any_draft(&mut pty);
        assert_eq!(
            fake.served(),
            3,
            "the rewind opened a request of its own:\n{}",
            fake.bodies().join("\n---\n")
        );
        assert!(
            misses.is_empty(),
            "{} mid-turn cell expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // Rewind barriers are decided when the command cell is committed.
    // ---------------------------------------------------------------------

    /// `before`, `.two-step`, `after-one`, `.macro disable two-step`,
    /// `after-two`: the macro turn is the barrier whatever the macro's state
    /// is now, so the picker offers the two prompts above it with the notice,
    /// not `before`; choosing `after-one` pops two exchanges, keeps `before`
    /// and the macro's cells, and prefills `after-one`.
    #[test]
    #[serial]
    fn disabling_a_macro_after_it_ran_does_not_move_the_rewind_barrier() {
        let dir = fresh_config_dir("barrier-disable-after");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        write_two_prompt_macro(&dir, "two-step");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        dry_run_exchange(&mut pty, "before-k7b");
        let at = pty.raw().len();
        pty.send(b".two-step\r").unwrap();
        wait_for_squeezed_after(&mut pty, "macro-prompt-two", at, "the macro's second step");
        pty.read_for(Duration::from_millis(2500));
        dry_run_exchange(&mut pty, "after-one-k7b");
        let at = pty.raw().len();
        pty.send(b".macro disable two-step\r").unwrap();
        pty.read_for(Duration::from_millis(2000));
        let toggled = since(&pty, at);
        assert!(
            !squeezed(&toggled).contains("macro-prompt-one"),
            "`.macro disable two-step` ran the macro instead of disabling it:\n{toggled}"
        );
        dry_run_exchange(&mut pty, "after-two-k7b");

        let mut misses: Vec<String> = Vec::new();
        let picker_at = open_rewind_picker(&mut pty);
        let picker = repainted_frame_after_resize(&mut pty, 60, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if !band_flat.contains("after-two-k7b") || !band_flat.contains("after-one-k7b") {
            misses.push(format!(
                "the picker does not offer both prompts above the macro turn:\n{picker}"
            ));
        }
        if band_flat.contains("before-k7b") {
            misses.push(format!(
            "`.macro disable` after the macro ran moved the barrier: `before` is offered:\n{picker}"
        ));
        }
        if band_flat.contains("macro-prompt") {
            misses.push(format!(
                "a macro-internal prompt is a rewind row:\n{picker}"
            ));
        }
        if !squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
            misses.push(format!(
                "the macro-stop notice is missing:\n{}",
                since(&pty, picker_at)
            ));
        }
        // Down to `after-one`, Enter: two exchanges go.
        pty.send(DOWN).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        if flat.contains("Rewindto") {
            misses.push(format!("the picker is still up after Enter:\n{frame}"));
        }
        if flat.contains(">after-one-k7b") || flat.contains(">after-two-k7b") {
            misses.push(format!(
                "choosing `after-one` did not pop both exchanges above it:\n{frame}"
            ));
        }
        if !flat.contains(">before-k7b") || !flat.contains("macro-prompt-two") {
            misses.push(format!("the rewind went past the macro turn:\n{frame}"));
        }
        let row = composer_row_with_footer(&frame, "dryrun:dry-model");
        if row != "after-one-k7b" {
            misses.push(format!(
                "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
            ));
        }
        clear_any_draft(&mut pty);
        let settled_at = pty.raw().len();
        assert_composer_takes_keys(
            &mut pty,
            settled_at,
            "rewinding to a disabled macro's barrier",
        );
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} barrier expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    /// `before`, `.macro disable two-step`, `.two-step` (refused: the macro is
    /// disabled, no exchange ran), `after`, `.macro enable two-step`: no
    /// macro turn was committed, so the picker offers both prompts with no
    /// notice, and re-enabling later changes nothing retroactively. Choosing
    /// `before` pops both exchanges and prefills `before`.
    #[test]
    #[serial]
    fn a_disabled_macro_typed_later_commits_no_barrier_and_enabling_it_is_not_retroactive() {
        let dir = fresh_config_dir("barrier-disable-before");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        write_two_prompt_macro(&dir, "two-step");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        dry_run_exchange(&mut pty, "before-k7d");
        pty.send(b".macro disable two-step\r").unwrap();
        pty.read_for(Duration::from_millis(2000));
        let at = pty.raw().len();
        pty.send(b".two-step\r").unwrap();
        wait_for_squeezed_after(&mut pty, "is disabled", at, "the disabled-macro refusal");
        pty.read_for(Duration::from_millis(1500));
        let refused = since(&pty, at);
        assert!(
            !squeezed(&refused).contains("macro-prompt-one"),
            "the disabled macro ran:\n{refused}"
        );
        dry_run_exchange(&mut pty, "after-k7d");
        pty.send(b".macro enable two-step\r").unwrap();
        pty.read_for(Duration::from_millis(2000));

        let mut misses: Vec<String> = Vec::new();
        let picker_at = open_rewind_picker(&mut pty);
        let picker = repainted_frame_after_resize(&mut pty, 60, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if !band_flat.contains("after-k7d") || !band_flat.contains("before-k7d") {
            misses.push(format!(
                "a refused `.two-step` (macro disabled, no exchange) was committed as a barrier: \
             the picker does not offer both prompts:\n{picker}"
            ));
        }
        if squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
            misses.push(format!(
                "the macro-stop notice is shown though no macro turn ran:\n{}",
                since(&pty, picker_at)
            ));
        }
        pty.send(DOWN).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        if flat.contains("Rewindto") {
            misses.push(format!("the picker is still up after Enter:\n{frame}"));
        }
        if misses.is_empty() && (flat.contains(">after-k7d") || flat.contains(">before-k7d")) {
            misses.push(format!(
                "choosing `before` did not pop both exchanges:\n{frame}"
            ));
        }
        let row = composer_row_with_footer(&frame, "dryrun:dry-model");
        if misses.is_empty() && row != "before-k7d" {
            misses.push(format!(
                "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
            ));
        }
        clear_any_draft(&mut pty);
        let settled_at = pty.raw().len();
        assert_composer_takes_keys(&mut pty, settled_at, "rewinding over a refused macro");
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} barrier expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // Typeahead Space cannot tick a checkbox.
    // ---------------------------------------------------------------------

    /// The user is typing as a tool's checkbox modal appears and the burst
    /// ends on a Space right after it is on screen: no row is ticked. Once
    /// the gates have passed, Space ticks the highlighted row and Enter
    /// sends exactly that row to the tool.
    #[test]
    #[serial]
    fn typeahead_space_as_a_checkbox_modal_appears_ticks_nothing() {
        let dir = fresh_config_dir("checkbox-typeahead");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("checkbox-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_checkbox_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture done k7p".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(
            &dir,
            "fixture_checkbox_rec.sh",
            &recording_prompt_tool("checkbox \"Tick some k7p-c\" alpha beta gamma"),
        );
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        let asked_at = pty.raw().len();
        pty.send(b"call the checkbox tool\r").unwrap();
        // Keep typing (one harmless letter every 100 ms) until the question
        // is on screen, then Space at once: a typeahead Space.
        let deadline = Instant::now() + PTY_DEADLINE;
        loop {
            pty.send(b"z").unwrap();
            pty.read_for(Duration::from_millis(100));
            if squeezed(&since(&pty, asked_at)).contains("k7p-c") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the tool's checkbox question never reached the screen:\n{}",
                since(&pty, asked_at)
            );
        }
        pty.send(b" ").unwrap();
        pty.read_for(HOTKEY_GAP);
        let mut misses: Vec<String> = Vec::new();
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        let flat = squeezed(&frame);
        if !flat.contains("[]alpha") || !flat.contains("[]beta") || !flat.contains("[]gamma") {
            misses.push(format!(
            "the checkbox modal is not up with three unticked rows after the typeahead burst:\n{frame}"
        ));
        }
        if flat.contains("[x]") {
            misses.push(format!("a typeahead Space ticked a row:\n{frame}"));
        }
        if out.exists() {
            misses.push(format!(
                "the typeahead burst answered the modal (recorded `{}`):\n{frame}",
                fs::read_to_string(&out).unwrap_or_default()
            ));
        }
        // Past both gates, Space ticks the highlighted row.
        pty.send(b" ").unwrap();
        pty.read_for(Duration::from_millis(400));
        let frame = repainted_frame_after_resize(&mut pty, 40, 100);
        if !squeezed(&frame).contains("[x]alpha") {
            misses.push(format!(
                "an idle Space did not tick the first row:\n{frame}"
            ));
        }
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\r").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        let answer = fs::read_to_string(&out).unwrap().trim().to_string();
        if answer != "0" {
            misses.push(format!(
                "the tool got `{answer}`, not the single idle-ticked row `0`"
            ));
        }
        wait_for_squeezed_after(
            &mut pty,
            "fixture done k7p",
            asked_at,
            "the reply after the tool",
        );
        pty.read_for(Duration::from_millis(500));
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} checkbox typeahead expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // Alt+Up over a draft swaps it with the newest queued line.
    // ---------------------------------------------------------------------

    /// While a tool sleeps: `queued-a` is queued, `draft-b` is typed. Alt+Up
    /// brings `queued-a` back into the composer and holds `draft-b` on the
    /// queue (`pending: 1`, nothing lost); a second Alt+Up swaps them back.
    /// Releasing the tool sends what the queue holds (`queued-a`) and leaves
    /// the draft (`draft-b`) in the composer, which Enter sends as the next
    /// prompt.
    #[test]
    #[serial]
    fn alt_up_over_a_draft_swaps_it_with_the_newest_queued_line() {
        let dir = fresh_config_dir("alt-up-swap");
        let _guard = TempDirGuard(dir.clone());
        let tool_a = dir.join("tool-a");
        let fake = FakeLlm::start(vec![
            sleep_call(&tool_a),
            Reply::Text("done-first-k7s".to_string()),
            Reply::Text("done-second-k7s".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        let turn_at = pty.raw().len();
        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-a-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"queued-a-k7s\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        pty.send(b"draft-b-k7s").unwrap();
        pty.read_for(Duration::from_millis(400));
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        assert!(
            squeezed(&frame).contains("pending:1"),
            "the line queued while the tool slept did not show `pending: 1`:\n{frame}"
        );
        assert_eq!(
            composer_row_with_footer(&frame, "fake:fake-model"),
            "draft-b-k7s",
            "the draft is not in the composer before Alt+Up:\n{frame}"
        );

        let mut misses: Vec<String> = Vec::new();
        pty.send(ALT_UP).unwrap();
        pty.read_for(Duration::from_millis(600));
        let frame = repainted_frame_after_resize(&mut pty, 40, 100);
        let flat = squeezed(&frame);
        let row = composer_row_with_footer(&frame, "fake:fake-model");
        if row != "queued-a-k7s" {
            misses.push(format!(
            "Alt+Up did not bring the queued line back for editing (composer: `{row}`):\n{frame}"
        ));
        }
        if !flat.contains("pending:1") {
            misses.push(format!(
                "after Alt+Up over a draft the queue does not hold one line:\n{frame}"
            ));
        }
        if !squeezed(&above_rule(&frame)).contains("draft-b-k7s") {
            misses.push(format!(
                "Alt+Up wiped the draft: `draft-b-k7s` is not previewed on the queue:\n{frame}"
            ));
        }
        // Swap back.
        pty.send(ALT_UP).unwrap();
        pty.read_for(Duration::from_millis(600));
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        let flat = squeezed(&frame);
        let row = composer_row_with_footer(&frame, "fake:fake-model");
        if row != "draft-b-k7s" {
            misses.push(format!(
                "a second Alt+Up did not swap the lines back (composer: `{row}`):\n{frame}"
            ));
        }
        if !flat.contains("pending:1") || !squeezed(&above_rule(&frame)).contains("queued-a-k7s") {
            misses.push(format!(
                "after the second Alt+Up the queue does not hold `queued-a-k7s` alone:\n{frame}"
            ));
        }
        if fake.served() != 1 {
            misses.push(format!(
                "Alt+Up moved the turn on ({} requests served)",
                fake.served()
            ));
        }

        fs::write(dir.join("tool-a-release"), b"").unwrap();
        wait_for_squeezed_after(&mut pty, "done-first-k7s", turn_at, "the tool turn's reply");
        wait_until_no_pending(&mut pty);
        let bodies = fake.bodies();
        if bodies.len() < 2 {
            misses.push(format!(
                "the tool round made no second request:\n{}",
                bodies.join("\n---\n")
            ));
        } else {
            if !bodies[1].contains("queued-a-k7s") {
                misses.push(format!(
                    "the line on the queue when the tool round ended did not ride the request:\n{}",
                    bodies[1]
                ));
            }
            if bodies[1].contains("draft-b-k7s") {
                misses.push(format!(
                    "the draft in the composer was sent though Enter was never pressed:\n{}",
                    bodies[1]
                ));
            }
        }
        if bodies.len() > 2 {
            misses.push(format!(
                "a request opened on its own after the tool turn:\n{}",
                bodies[2]
            ));
        }
        let frame = repainted_frame_after_resize(&mut pty, 40, 100);
        let row = composer_row_with_footer(&frame, "fake:fake-model");
        if row != "draft-b-k7s" {
            misses.push(format!(
                "the draft did not survive the turn (composer: `{row}`):\n{frame}"
            ));
        }
        // Enter sends the draft as the next prompt.
        let next_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        if misses.is_empty() {
            wait_for_squeezed_after(&mut pty, "done-second-k7s", next_at, "the draft's own turn");
            pty.read_for(Duration::from_millis(500));
            let bodies = fake.bodies();
            if bodies.len() != 3 || !bodies[2].contains("draft-b-k7s") {
                misses.push(format!(
                    "Enter on the surviving draft did not open its own request:\n{}",
                    bodies.join("\n---\n")
                ));
            }
        } else {
            pty.read_for(Duration::from_millis(2000));
        }
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} Alt+Up expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // Quit purges the turn queue even when the tool finishes right behind it.
    // ---------------------------------------------------------------------

    /// A line queued while a tool sleeps, Ctrl+D, and the tool released at
    /// once (so an unpurged queue would ride the next request): the session
    /// ends cleanly within the deadline and the queued line reaches no
    /// request body; no scripted reply behind the quit streams.
    #[test]
    #[serial]
    fn ctrl_d_purges_the_turn_queue_even_when_the_tool_is_released_right_after() {
        let dir = fresh_config_dir("ctrl-d-release");
        let _guard = TempDirGuard(dir.clone());
        let tool_a = dir.join("tool-a");
        let fake = FakeLlm::start(vec![
            sleep_call(&tool_a),
            Reply::Text("never-sent-k7q".to_string()),
            Reply::Text("never-sent-either-k7q".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        pty.send(b"first-turn-k7q\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-a-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"ghost-q-k7q\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        assert!(
            squeezed(&frame).contains("pending:1"),
            "the line queued while the tool slept did not show `pending: 1`:\n{frame}"
        );

        let quit_at = Instant::now();
        pty.send(b"\x04").unwrap();
        fs::write(dir.join("tool-a-release"), b"").unwrap();
        let transcript = pty
            .wait_for("tty-probe-done", PTY_DEADLINE)
            .unwrap_or_else(|e| {
                panic!("Ctrl+D during the tool round did not end the session: {e}")
            });
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert!(
            quit_at.elapsed() < Duration::from_secs(15),
            "Ctrl+D took {:?} to end the session",
            quit_at.elapsed()
        );
        // Time for a wrongly flushed queued line to open a request.
        std::thread::sleep(Duration::from_millis(1500));
        let bodies = fake.bodies();
        assert!(
            bodies.iter().all(|b| !b.contains("ghost-q-k7q")),
            "the line queued before Ctrl+D reached the model once the tool finished:\n{}",
            bodies.join("\n---\n")
        );
        assert!(
            !transcript.contains("never-sent"),
            "a reply scripted behind the quit streamed:\n{transcript}"
        );
        assert_tty_restored(&transcript);
    }
}

mod sent_line_failed_macro_and_sigterm {
    //! Black-box pins for the bottom pane's sent-line, failed-macro and
    //! SIGTERM edges: a queued line the model received leaves the `pending:`
    //! strip while the SAME turn still runs and Alt+Up never brings it back;
    //! a non-isolated macro that recorded an exchange before a later step
    //! failed is still a rewind barrier (one that failed before any exchange
    //! is not); SIGTERM with queued lines drops them even when the tool is
    //! released in the same instant. The built binary on a pseudo-terminal,
    //! a scripted fake model where a tool round or a session is needed.
    //! Unix-only and serial like its sibling pty modules.
    use std::fs;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::tui_harness::*;
    use serial_test::serial;

    // ---------------------------------------------------------------------
    // A line the model received leaves the strip while the turn still runs,
    // and Alt+Up never brings it back.
    // ---------------------------------------------------------------------

    /// Tool A sleeps: `sent-one`, `sent-two` are queued (`pending: 2`).
    /// Releasing A hands both to the model at the tool-round boundary; while
    /// tool B sleeps in the SAME turn the strip is gone and both lines are
    /// dim `(sent mid-turn)` cells. Alt+Up on the empty composer brings
    /// nothing back. A later `late-three` is queued, Alt+Up pulls THAT line
    /// (not a sent one) into the composer, Enter re-queues it, and releasing
    /// B sends it; three requests in all.
    #[test]
    #[serial]
    fn a_sent_line_leaves_the_strip_mid_turn_and_alt_up_never_pops_it() {
        let dir = fresh_config_dir("sent-strip");
        let _guard = TempDirGuard(dir.clone());
        let tool_a = dir.join("tool-a");
        let tool_b = dir.join("tool-b");
        let fake = FakeLlm::start(vec![
            sleep_call(&tool_a),
            sleep_call(&tool_b),
            Reply::Text("done-w3s".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(Duration::from_millis(400));
        let mut toggle = false;

        let turn_at = pty.raw().len();
        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-a-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"sent-one-w3\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        pty.send(b"sent-two-w3\r").unwrap();
        pty.read_for(Duration::from_millis(600));
        let frame = next_frame(&mut pty, &mut toggle);
        assert!(
            squeezed(&frame).contains("pending:2"),
            "two lines queued while tool A slept did not show `pending: 2`:\n{frame}"
        );

        // Release A: the queue is drained into request 2, which asks for B.
        fs::write(dir.join("tool-a-release"), b"").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-b-started"),
            "the second sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(800));

        let mut misses: Vec<String> = Vec::new();
        // The strip is gone and cells committed while B still sleeps.
        let frame = next_frame(&mut pty, &mut toggle);
        let flat = squeezed(&frame);
        let marker = squeezed(MID_TURN_MARKER);
        for line in ["sent-one-w3", "sent-two-w3"] {
            if !flat.contains(&format!(">{line}{marker}")) {
                misses.push(format!(
                    "`{line}` was handed to the model but has no `(sent mid-turn)` cell while the \
                 turn still runs:\n{frame}"
                ));
            }
        }
        if frame.contains("pending:") {
            misses.push(format!(
                "the queue strip outlived the lines it previewed (the model has them, tool B is \
             still running):\n{frame}"
            ));
        }
        if fake.served() != 2 {
            misses.push(format!(
                "expected two requests after tool A was released, got {}",
                fake.served()
            ));
        }

        // Alt+Up on the empty composer does not resurrect a sent line.
        pty.send(ALT_UP).unwrap();
        pty.read_for(Duration::from_millis(600));
        let frame = next_frame(&mut pty, &mut toggle);
        let flat = squeezed(&frame);
        let row = composer_row_with_footer(&frame, "fake:fake-model");
        if row.contains("sent-one-w3") || row.contains("sent-two-w3") {
            misses.push(format!(
                "Alt+Up popped a line the model already received back into the composer \
             (`{row}`):\n{frame}"
            ));
        } else if !shows_placeholder(&flat) {
            misses.push(format!(
                "Alt+Up over an empty queue left the composer non-empty (`{row}`):\n{frame}"
            ));
        }
        if frame.contains("pending:") {
            misses.push(format!(
                "Alt+Up over an empty queue made a `pending:` strip appear:\n{frame}"
            ));
        }

        // A later line queues alone; Alt+Up pulls it, not a sent line.
        pty.send(b"late-three-w3\r").unwrap();
        pty.read_for(Duration::from_millis(600));
        let frame = next_frame(&mut pty, &mut toggle);
        let flat = squeezed(&frame);
        if !flat.contains("pending:1") {
            misses.push(format!(
                "a line queued while tool B slept did not show `pending: 1`:\n{frame}"
            ));
        }
        let strip = squeezed(&above_rule(&frame));
        if !strip.contains("late-three-w3") {
            misses.push(format!(
                "the strip does not preview the one line it holds:\n{frame}"
            ));
        }
        pty.send(ALT_UP).unwrap();
        pty.read_for(Duration::from_millis(600));
        let frame = next_frame(&mut pty, &mut toggle);
        let row = composer_row_with_footer(&frame, "fake:fake-model");
        if row != "late-three-w3" {
            misses.push(format!(
            "Alt+Up did not bring the one queued line back for editing (composer: `{row}`):\n{frame}"
        ));
        }
        if frame.contains("pending:") {
            misses.push(format!(
                "after Alt+Up took the only queued line the strip still shows `pending:`:\n{frame}"
            ));
        }
        if fake.served() != 2 {
            misses.push(format!(
                "Alt+Up moved the turn on ({} requests served)",
                fake.served()
            ));
        }
        // Enter re-queues it.
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(600));
        let frame = next_frame(&mut pty, &mut toggle);
        if !squeezed(&frame).contains("pending:1") {
            misses.push(format!(
                "Enter on the line Alt+Up brought back did not re-queue it:\n{frame}"
            ));
        }

        // Release B; the re-queued line rides request 3.
        fs::write(dir.join("tool-b-release"), b"").unwrap();
        wait_for_squeezed_after(&mut pty, "done-w3s", turn_at, "the turn's final reply");
        wait_until_no_pending(&mut pty);
        let bodies = fake.bodies();
        if bodies.len() != 3 {
            misses.push(format!(
                "expected three requests, got {}:\n{}",
                bodies.len(),
                bodies.join("\n---\n")
            ));
        } else {
            if !bodies[1].contains("sent-one-w3") || !bodies[1].contains("sent-two-w3") {
                misses.push(format!(
                    "the two lines queued during tool A did not ride request 2:\n{}",
                    bodies[1]
                ));
            }
            if !bodies[2].contains("late-three-w3") {
                misses.push(format!(
                    "the line re-queued during tool B did not ride request 3:\n{}",
                    bodies[2]
                ));
            }
            // The sent lines stay in the history exactly once each: a
            // resurrected line would be sent again as a new user message.
            for line in ["sent-one-w3", "sent-two-w3"] {
                let times = bodies[2].matches(line).count();
                if times != 1 {
                    misses.push(format!(
                        "`{line}` appears {times} times in request 3 (sent again?):\n{}",
                        bodies[2]
                    ));
                }
            }
        }
        let frame = next_frame(&mut pty, &mut toggle);
        let flat = squeezed(&frame);
        if !flat.contains(&format!(">late-three-w3{marker}")) {
            misses.push(format!(
                "the re-queued line has no `(sent mid-turn)` cell after the turn:\n{frame}"
            ));
        }
        if !shows_placeholder(&flat) {
            misses.push(format!(
                "the composer is not empty after the turn (composer: `{}`):\n{frame}",
                composer_row_with_footer(&frame, "fake:fake-model")
            ));
        }
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} sent-line expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // A macro that recorded an exchange is a barrier even when a later step
    // failed; one that failed before any exchange is not.
    // ---------------------------------------------------------------------

    /// `.session s7`, `before`, macro `half` = [`inside`, `.session nosuch`]
    /// (the first step records an exchange, the second fails inside a
    /// session), `after`. The picker offers `after` alone with the macro
    /// notice; Enter pops `after` only; `inside` and `before` stay on the
    /// transcript and in the next request.
    #[test]
    #[serial]
    fn a_macro_that_recorded_an_exchange_is_a_barrier_even_when_a_later_step_failed() {
        let dir = fresh_config_dir("failed-macro-barrier");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::Text("reply-before-w3m".to_string()),
            Reply::Text("reply-inside-w3m".to_string()),
            Reply::Text("reply-after-w3m".to_string()),
            Reply::Text("reply-tail-w3m".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_macro(&dir, "half-w3", &["inside-w3m", ".session nosuch-w3"]);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        start_session(&mut pty, "s7-w3m");
        fake_exchange(&mut pty, "before-w3m", "reply-before-w3m");
        let macro_at = pty.raw().len();
        pty.send(b".half-w3\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "reply-inside-w3m",
            macro_at,
            "the macro's first step's reply",
        );
        let failed = wait_for_squeezed_after(
            &mut pty,
            "Already in a session",
            macro_at,
            "the macro's failing second step",
        );
        assert!(
            !squeezed(&failed).contains("nosuch-w3.agents"),
            "`.session nosuch-w3` inside the macro switched sessions instead of failing:\n{failed}"
        );
        pty.read_for(Duration::from_millis(1500));
        fake_exchange(&mut pty, "after-w3m", "reply-after-w3m");
        assert_eq!(fake.served(), 3, "{}", fake.bodies().join("\n---\n"));

        let mut misses: Vec<String> = Vec::new();
        let picker_at = open_rewind_picker(&mut pty);
        let picker = picker_frame(&mut pty, picker_at, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if !band_flat.contains("after-w3m") {
            misses.push(format!("the newest prompt is not a rewind row:\n{picker}"));
        }
        if band_flat.contains("before-w3m") {
            misses.push(format!(
                "a macro that recorded an exchange before failing is NOT a barrier: `before` is \
             offered:\n{picker}"
            ));
        }
        if band_flat.contains("inside-w3m") {
            misses.push(format!(
                "the failed macro's own exchange is offered as a rewind row:\n{picker}"
            ));
        }
        if !squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
            misses.push(format!(
                "the picker does not say it stops at the last macro turn:\n{}",
                since(&pty, picker_at)
            ));
        }
        // Down cannot reach past the barrier; Enter pops `after` only.
        pty.send(DOWN).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        if flat.contains("Rewindto") {
            misses.push(format!("the picker is still up after Enter:\n{frame}"));
        }
        if flat.contains(">after-w3m") {
            misses.push(format!("the chosen exchange was not popped:\n{frame}"));
        }
        if !flat.contains(">inside-w3m") || !flat.contains(">before-w3m") {
            misses.push(format!(
                "the rewind went past the failed macro's barrier:\n{frame}"
            ));
        }
        let row = composer_row_with_footer(&frame, "fake:fake-model");
        if row != "after-w3m" {
            misses.push(format!(
                "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
            ));
        }
        clear_any_draft(&mut pty);
        fake_exchange(&mut pty, "tail-w3m", "reply-tail-w3m");
        let bodies = fake.bodies();
        if bodies.len() != 4 {
            misses.push(format!(
                "expected four requests, got {}:\n{}",
                bodies.len(),
                bodies.join("\n---\n")
            ));
        } else {
            if bodies[3].contains("after-w3m") {
                misses.push(format!(
                    "the popped exchange is still in the session's next request:\n{}",
                    bodies[3]
                ));
            }
            if !bodies[3].contains("inside-w3m") || !bodies[3].contains("before-w3m") {
                misses.push(format!(
                    "the session lost an exchange the transcript still shows:\n{}",
                    bodies[3]
                ));
            }
        }
        quit_declining_save(&mut pty);
        assert!(
            misses.is_empty(),
            "{} failed-macro barrier expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    /// Control: `.session s7`, `before`, macro `nothing` = [`.session
    /// nosuch`] (fails before any exchange), `after`. The picker offers
    /// `after` AND `before`: a macro that neither succeeded nor recorded an
    /// exchange cuts nothing.
    #[test]
    #[serial]
    fn a_macro_that_failed_before_any_exchange_is_not_a_barrier() {
        let dir = fresh_config_dir("empty-failed-macro");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::Text("reply-before-w3n".to_string()),
            Reply::Text("reply-after-w3n".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_macro(&dir, "nothing-w3", &[".session nosuch-w3"]);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        start_session(&mut pty, "s7-w3n");
        fake_exchange(&mut pty, "before-w3n", "reply-before-w3n");
        let macro_at = pty.raw().len();
        pty.send(b".nothing-w3\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "Already in a session",
            macro_at,
            "the macro's failing step",
        );
        pty.read_for(Duration::from_millis(1500));
        fake_exchange(&mut pty, "after-w3n", "reply-after-w3n");
        assert_eq!(fake.served(), 2, "{}", fake.bodies().join("\n---\n"));

        let mut misses: Vec<String> = Vec::new();
        let picker_at = open_rewind_picker(&mut pty);
        let picker = picker_frame(&mut pty, picker_at, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if !band_flat.contains("after-w3n") {
            misses.push(format!("the newest prompt is not a rewind row:\n{picker}"));
        }
        if !band_flat.contains("before-w3n") {
            misses.push(format!(
                "a macro that failed before recording anything cut the rewind rows: `before` is \
             not offered:\n{picker}"
            ));
        }
        if squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
            misses.push(format!(
                "the picker says it stops at a macro turn though the macro recorded nothing:\n{}",
                since(&pty, picker_at)
            ));
        }
        // Back out without popping.
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(600));
        quit_declining_save(&mut pty);
        assert!(
            misses.is_empty(),
            "{} control expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // SIGTERM drops the queued lines even when the tool is released at once.
    // ---------------------------------------------------------------------

    /// Two lines queued while a tool sleeps, SIGTERM and the release file in
    /// the same instant (so an unpurged queue would ride the next request):
    /// the process leaves cleanly within the deadline, neither line reaches a
    /// request body, and the scripted reply behind the exit never streams.
    #[test]
    #[serial]
    fn sigterm_purges_the_turn_queue_even_when_the_tool_is_released_right_after() {
        let dir = fresh_config_dir("sigterm-release");
        let _guard = TempDirGuard(dir.clone());
        let tool_a = dir.join("tool-a");
        let fake = FakeLlm::start(vec![
            sleep_call(&tool_a),
            Reply::Text("never-sent-w3t".to_string()),
            Reply::Text("never-sent-either-w3t".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);
        let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let pid = child_pid(&transcript);

        pty.send(b"first-turn-w3t\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-a-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"ghost-a-w3t\r").unwrap();
        pty.read_for(Duration::from_millis(500));
        pty.send(b"ghost-b-w3t\r").unwrap();
        pty.read_for(Duration::from_millis(800));
        let frame = repainted_frame_after_resize(&mut pty, 40, 101);
        assert!(
            squeezed(&frame).contains("pending:2"),
            "two lines queued while the tool slept did not show `pending: 2`:\n{frame}"
        );

        let signalled_at = Instant::now();
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        fs::write(dir.join("tool-a-release"), b"").unwrap();
        let transcript = pty
            .wait_for("tty-probe-done", PTY_DEADLINE)
            .unwrap_or_else(|e| {
                panic!("SIGTERM during the tool round did not end the session: {e}")
            });
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert!(
            signalled_at.elapsed() < Duration::from_secs(15),
            "SIGTERM took {:?} to end the session",
            signalled_at.elapsed()
        );
        assert_tty_restored(&transcript);
        assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
        // Time for a wrongly flushed queued line to open a request.
        std::thread::sleep(Duration::from_millis(1500));
        let bodies = fake.bodies();
        assert!(
            bodies
                .iter()
                .all(|b| !b.contains("ghost-a-w3t") && !b.contains("ghost-b-w3t")),
            "a line queued before SIGTERM reached the model once the tool finished:\n{}",
            bodies.join("\n---\n")
        );
        assert!(
            !transcript.contains("never-sent"),
            "a reply scripted behind the signal exit streamed:\n{transcript}"
        );
    }
}

mod success_gated_barriers_band_close_and_headless_create {
    //! Black-box pins for the bottom pane's barrier edges: a rewind barrier is
    //! only committed when the command succeeded (`.session nosuch` inside a
    //! session fails and cuts nothing; `.edit session` with no session fails
    //! and cuts nothing; `.edit session` that ran the editor is a barrier);
    //! Ctrl+C and Ctrl+D with a history-search or rewind band open close the
    //! band and leave the process alive, quitting only on the next press;
    //! `--headless --macro` refuses `.role <new>` and `.macro <new>` without
    //! creating a role or macro file or launching an editor. The built binary
    //! on a pseudo-terminal, a scripted fake model where a session has to
    //! record exchanges. Unix-only and serial like its sibling pty modules.
    use std::fs;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// The stop notice names what cut the list ("the last macro turn", "the
    /// last session command"); the prefix is the contract.
    const STOP_NOTICE_PREFIX: &str = "Rewind stops at";

    /// `squeezed` minus the box-drawing rule, whose length follows the width
    /// a frame was repainted at.
    fn flat_frame(text: &str) -> String {
        text.chars()
            .filter(|c| !c.is_whitespace() && *c != '\u{2500}' && *c != '\u{2502}')
            .collect()
    }

    /// A `$EDITOR` that records the path it was handed and exits 0.
    fn path_recording_editor(dir: &Path, launched: &Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let editor = dir.join("recording-editor.sh");
        fs::write(
            &editor,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$1\" > '{}'\nexit 0\n",
                launched.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
        editor.to_str().unwrap().to_string()
    }

    // ---------------------------------------------------------------------
    // Success-gated barriers: a failed `.session` cuts nothing, a successful
    // one does.
    // ---------------------------------------------------------------------

    /// `zero`, `.session s1` (a barrier), `before`, `.session nosuch` (fails:
    /// already in a session), `after`. The picker offers `after` and `before`
    /// with the notice; `zero` sits below the successful switch. Choosing
    /// `before` pops both exchanges, keeps `zero`, prefills `before`, and the
    /// next request carries neither popped exchange (session and transcript
    /// in step).
    #[test]
    #[serial]
    fn a_failed_dot_session_inside_a_session_is_not_a_rewind_barrier() {
        let dir = fresh_config_dir("failed-session");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::Text("reply-zero-m8s".to_string()),
            Reply::Text("reply-before-m8s".to_string()),
            Reply::Text("reply-after-m8s".to_string()),
            Reply::Text("reply-tail-m8s".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        fake_exchange(&mut pty, "zero-m8s", "reply-zero-m8s");
        start_session(&mut pty, "s1-m8s");
        fake_exchange(&mut pty, "before-m8s", "reply-before-m8s");
        let at = pty.raw().len();
        pty.send(b".session nosuch-m8s\r").unwrap();
        let refused = wait_for_squeezed_after(
            &mut pty,
            "Already in a session",
            at,
            "the `.session nosuch` refusal",
        );
        assert!(
            !squeezed(&refused).contains("nosuch-m8s.agents"),
            "`.session nosuch-m8s` switched sessions instead of failing:\n{refused}"
        );
        pty.read_for(Duration::from_millis(1200));
        fake_exchange(&mut pty, "after-m8s", "reply-after-m8s");
        assert_eq!(fake.served(), 3, "{}", fake.bodies().join("\n---\n"));

        let mut misses: Vec<String> = Vec::new();
        let picker_at = open_rewind_picker(&mut pty);
        let picker = picker_frame(&mut pty, picker_at, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if !band_flat.contains("after-m8s") {
            misses.push(format!("the newest prompt is not a rewind row:\n{picker}"));
        }
        if !band_flat.contains("before-m8s") {
            misses.push(format!(
                "a FAILED `.session nosuch` cut the rewind rows: `before` is not offered:\n{picker}"
            ));
        }
        if band_flat.contains("zero-m8s") {
            misses.push(format!(
                "the successful `.session s1` is not a barrier: `zero` is offered:\n{picker}"
            ));
        }
        if !squeezed(&since(&pty, picker_at)).contains(&squeezed(STOP_NOTICE_PREFIX)) {
            misses.push(format!(
                "the stop notice is missing though `.session s1` cut `zero`:\n{}",
                since(&pty, picker_at)
            ));
        }
        // Down to `before`, Enter: two exchanges go, `zero` stays.
        pty.send(DOWN).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        if flat.contains("Rewindto") {
            misses.push(format!("the picker is still up after Enter:\n{frame}"));
        }
        if flat.contains("Rewindstopped") {
            misses.push(format!("the rewind stopped short:\n{frame}"));
        }
        if flat.contains(">after-m8s") || flat.contains(">before-m8s") {
            misses.push(format!(
                "choosing `before` did not pop both exchanges above the failed `.session`:\n{frame}"
            ));
        }
        if !flat.contains(">zero-m8s") {
            misses.push(format!(
                "the rewind went past the successful `.session`:\n{frame}"
            ));
        }
        let row = composer_row_with_footer(&frame, "fake:fake-model");
        if row != "before-m8s" {
            misses.push(format!(
                "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
            ));
        }
        clear_any_draft(&mut pty);
        // The session is in step: the next request carries neither popped
        // exchange.
        fake_exchange(&mut pty, "tail-m8s", "reply-tail-m8s");
        let bodies = fake.bodies();
        if bodies.len() != 4 {
            misses.push(format!(
                "expected four requests, got {}:\n{}",
                bodies.len(),
                bodies.join("\n---\n")
            ));
        } else if bodies[3].contains("before-m8s") || bodies[3].contains("after-m8s") {
            misses.push(format!(
                "a popped exchange is still in the session's next request:\n{}",
                bodies[3]
            ));
        }
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} barrier expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // `.edit session`: a barrier when it ran, not when it failed.
    // ---------------------------------------------------------------------

    /// `.session s2`, `before`, `.edit session` (the editor runs on the lent
    /// terminal and returns), `after`. The picker offers `after` only, with
    /// the notice; choosing it pops one exchange, keeps `before`, prefills
    /// `after`, and the next request still carries `before` but not `after`.
    #[test]
    #[serial]
    fn editing_the_session_is_a_rewind_barrier() {
        let dir = fresh_config_dir("edit-session-barrier");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::Text("reply-before-m8e".to_string()),
            Reply::Text("reply-after-m8e".to_string()),
            Reply::Text("reply-tail-m8e".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        let launched = dir.join("editor-launched");
        let editor = path_recording_editor(&dir, &launched);
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        start_session(&mut pty, "s2-m8e");
        fake_exchange(&mut pty, "before-m8e", "reply-before-m8e");
        let before = pty.raw().len();
        pty.send(b".edit session\r").unwrap();
        let left = pty
            .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
            .expect("`.edit session` did not release the terminal for the editor");
        let re_entered = pty
            .wait_for_raw(ENTER_ALT, left, PTY_DEADLINE)
            .expect("the alternate screen was not re-entered after the editor");
        pty.wait_for_raw(PASTE_ON, re_entered, PTY_DEADLINE)
            .expect("bracketed paste was not re-enabled on re-entry");
        pty.read_for(Duration::from_millis(500));
        let edited = fs::read_to_string(&launched)
            .unwrap_or_else(|e| panic!("`.edit session` never launched the editor ({e})"));
        assert!(
            edited.contains("s2-m8e"),
            "the editor was not handed the session file: {edited:?}"
        );
        pty.wait_for_after("Type a prompt", re_entered, PTY_DEADLINE)
            .expect("the composer was not repainted after the session editor returned");
        fake_exchange(&mut pty, "after-m8e", "reply-after-m8e");
        assert_eq!(fake.served(), 2, "{}", fake.bodies().join("\n---\n"));

        let mut misses: Vec<String> = Vec::new();
        let picker_at = open_rewind_picker(&mut pty);
        let picker = picker_frame(&mut pty, picker_at, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if !band_flat.contains("after-m8e") {
            misses.push(format!(
                "the prompt after `.edit session` is not a rewind row:\n{picker}"
            ));
        }
        if band_flat.contains("before-m8e") {
            misses.push(format!(
                "`.edit session` is not a rewind barrier: `before` is offered:\n{picker}"
            ));
        }
        if !squeezed(&since(&pty, picker_at)).contains(&squeezed(STOP_NOTICE_PREFIX)) {
            misses.push(format!(
                "the stop notice is missing though `.edit session` cut `before`:\n{}",
                since(&pty, picker_at)
            ));
        }
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        if flat.contains("Rewindto") {
            misses.push(format!("the picker is still up after Enter:\n{frame}"));
        }
        if flat.contains("Rewindstopped") {
            misses.push(format!("the rewind stopped short:\n{frame}"));
        }
        if flat.contains(">after-m8e") || flat.contains("reply-after-m8e") {
            misses.push(format!("choosing `after` did not pop it:\n{frame}"));
        }
        if !flat.contains(">before-m8e") {
            misses.push(format!("the rewind went past `.edit session`:\n{frame}"));
        }
        let row = composer_row_with_footer(&frame, "fake:fake-model");
        if row != "after-m8e" {
            misses.push(format!(
                "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
            ));
        }
        clear_any_draft(&mut pty);
        fake_exchange(&mut pty, "tail-m8e", "reply-tail-m8e");
        let bodies = fake.bodies();
        if bodies.len() != 3 {
            misses.push(format!(
                "expected three requests, got {}:\n{}",
                bodies.len(),
                bodies.join("\n---\n")
            ));
        } else {
            if bodies[2].contains("after-m8e") {
                misses.push(format!(
                    "the popped exchange is still in the session's next request:\n{}",
                    bodies[2]
                ));
            }
            if !bodies[2].contains("before-m8e") {
                misses.push(format!(
                    "the exchange below the `.edit session` barrier left the session:\n{}",
                    bodies[2]
                ));
            }
        }
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} barrier expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    /// `before`, `.edit session` with NO session (fails, the editor never
    /// runs), `after`: nothing was edited, so the picker offers both prompts
    /// with no notice and choosing `before` pops both.
    #[test]
    #[serial]
    fn a_failed_edit_session_outside_a_session_is_not_a_rewind_barrier() {
        let dir = fresh_config_dir("edit-session-failed");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let launched = dir.join("editor-launched");
        let editor = path_recording_editor(&dir, &launched);
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        dry_run_exchange(&mut pty, "before-m8f");
        let at = pty.raw().len();
        pty.send(b".edit session\r").unwrap();
        pty.read_for(Duration::from_millis(2500));
        let refused = since(&pty, at);
        assert!(
            !launched.exists(),
            "`.edit session` with no session launched the editor:\n{refused}"
        );
        assert!(
            squeezed(&refused).to_lowercase().contains("session"),
            "`.edit session` with no session said nothing about it:\n{refused}"
        );
        dry_run_exchange(&mut pty, "after-m8f");

        let mut misses: Vec<String> = Vec::new();
        let picker_at = open_rewind_picker(&mut pty);
        let picker = picker_frame(&mut pty, picker_at, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if !band_flat.contains("after-m8f") || !band_flat.contains("before-m8f") {
            misses.push(format!(
                "a FAILED `.edit session` (no session) was committed as a barrier: \
             the picker does not offer both prompts:\n{picker}"
            ));
        }
        if squeezed(&since(&pty, picker_at)).contains(&squeezed(STOP_NOTICE_PREFIX)) {
            misses.push(format!(
                "the stop notice is shown though nothing was edited:\n{}",
                since(&pty, picker_at)
            ));
        }
        pty.send(DOWN).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        if flat.contains(">before-m8f") || flat.contains(">after-m8f") {
            misses.push(format!(
                "choosing `before` did not pop both exchanges:\n{frame}"
            ));
        }
        let row = composer_row_with_footer(&frame, "dryrun:dry-model");
        if row != "before-m8f" {
            misses.push(format!(
                "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
            ));
        }
        clear_any_draft(&mut pty);
        let settled_at = pty.raw().len();
        assert_composer_takes_keys(
            &mut pty,
            settled_at,
            "rewinding over a failed `.edit session`",
        );
        quit_clearing_any_draft(&mut pty);
        assert!(
            misses.is_empty(),
            "{} barrier expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // Ctrl+C / Ctrl+D with a band open close the band first.
    // ---------------------------------------------------------------------

    #[derive(Clone, Copy, Debug)]
    enum Band {
        Search,
        Rewind,
    }

    /// Opens `band` over an idle empty composer after one exchange; returns
    /// the raw offset it was asked at.
    fn open_band(pty: &mut Pty, band: Band) -> usize {
        match band {
            Band::Rewind => open_rewind_picker(pty),
            Band::Search => {
                let at = pty.raw().len();
                pty.send(CTRL_R).unwrap();
                pty.read_for(Duration::from_millis(800));
                at
            }
        }
    }

    /// One exchange, the band, `key`: the band closes and the UI lives on;
    /// `key` again on the empty composer quits with the terminal restored.
    fn band_closes_before_the_key_quits(band: Band, key: &[u8], key_name: &str) -> Vec<String> {
        let dir = fresh_config_dir("band-close");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(Duration::from_millis(400));
        dry_run_exchange(&mut pty, "row-m8b");

        let what = format!("{band:?} band + {key_name}");
        let mut misses: Vec<String> = Vec::new();
        let baseline = flat_frame(&repainted_frame_after_resize(&mut pty, 40, 101));
        open_band(&mut pty, band);
        let opened = repainted_frame_after_resize(&mut pty, 40, 100);
        if flat_frame(&opened) == baseline {
            misses.push(format!(
                "[{what}] the band did not open (frame unchanged):\n{opened}"
            ));
        }
        if matches!(band, Band::Rewind) && !opened.contains("Rewind to") {
            misses.push(format!(
                "[{what}] the rewind picker is not on screen:\n{opened}"
            ));
        }

        // The key: the band goes, the process stays.
        let key_at = pty.raw().len();
        pty.send(key).unwrap();
        let after = pty.read_for(HOTKEY_GAP);
        if after.contains("tty-probe-done") {
            misses.push(format!(
            "[{what}] {key_name} with the band open quit the UI instead of closing the band:\n{}",
            since(&pty, key_at)
        ));
            return misses;
        }
        let closed = repainted_frame_after_resize(&mut pty, 40, 101);
        if closed.contains("Rewind to") {
            misses.push(format!(
                "[{what}] the rewind picker survived {key_name}:\n{closed}"
            ));
        }
        if flat_frame(&closed) != baseline {
            misses.push(format!(
            "[{what}] after {key_name} the screen is not back to the pre-band frame:\n--- before ---\n{}\n--- after ---\n{closed}",
            baseline
        ));
        }
        if !squeezed(&closed).contains(">row-m8b") {
            misses.push(format!(
                "[{what}] {key_name} on the band popped the exchange:\n{closed}"
            ));
        }
        if !shows_placeholder(&squeezed(&closed)) {
            misses.push(format!(
                "[{what}] the composer is not empty after the band closed:\n{closed}"
            ));
        }
        let live_at = pty.raw().len();
        assert_composer_takes_keys(&mut pty, live_at, &what);

        // The same key on an idle empty composer is the quit chord.
        pty.send(key).unwrap();
        match pty.wait_for("tty-probe-done", PTY_DEADLINE) {
            Ok(transcript) => {
                if exit_code(&transcript) != 0 {
                    misses.push(format!("[{what}] the quit exited non-zero:\n{transcript}"));
                }
                assert_tty_restored(&transcript);
            }
            Err(e) => {
                misses.push(format!(
                    "[{what}] a second {key_name} on the empty composer did not quit: {e}"
                ));
                quit_clearing_any_draft(&mut pty);
            }
        }
        misses
    }

    /// Ctrl+C and Ctrl+D over the history-search band and the rewind picker:
    /// four spawns, each closes the band then quits on the repeat.
    #[test]
    #[serial]
    fn ctrl_c_and_ctrl_d_close_an_open_band_before_they_quit() {
        let mut misses: Vec<String> = Vec::new();
        for (band, key, key_name) in [
            (Band::Search, CTRL_C, "Ctrl+C"),
            (Band::Search, CTRL_D, "Ctrl+D"),
            (Band::Rewind, CTRL_C, "Ctrl+C"),
            (Band::Rewind, CTRL_D, "Ctrl+D"),
        ] {
            misses.extend(band_closes_before_the_key_quits(band, key, key_name));
        }
        assert!(
            misses.is_empty(),
            "{} band-close expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // Headless `.role <new>` / `.macro <new>` create nothing.
    // ---------------------------------------------------------------------

    /// Runs `--headless --macro <name> go` with stdin closed and a recording
    /// editor; returns (exit status text, stdout+stderr). The positional
    /// `go` matters: with no prompt argument and no stdin text `--headless`
    /// bails with "requires a prompt argument" before the macro runs. A
    /// macro without variables tolerates the surplus argument.
    fn run_headless_macro(dir: &Path, name: &str, editor: &str) -> (String, String) {
        let mut cmd = ttyless_command(built_binary(), dir);
        cmd.args(["--headless", "--macro", name, "go"])
            .env("EDITOR", editor)
            .env("VISUAL", editor)
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

    /// Macro `mkrole` = `.role m8-role`: the step is echoed, no role file,
    /// no editor, the creation gate's refusal. Macro `mkmacro` = `.macro
    /// m8-macro`: the same for a macro.
    #[test]
    #[serial]
    fn a_headless_macro_cannot_create_a_role_or_a_macro() {
        let dir = fresh_config_dir("headless-create");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let macros = dir.join("macros");
        fs::create_dir_all(&macros).unwrap();
        fs::write(
            macros.join("mkrole.yaml"),
            "description: create a role headless\n\
         steps:\n\
         \x20 - .role m8-role\n",
        )
        .unwrap();
        fs::write(
            macros.join("mkmacro.yaml"),
            "description: create a macro headless\n\
         steps:\n\
         \x20 - .macro m8-macro\n",
        )
        .unwrap();
        fs::create_dir_all(dir.join("roles")).unwrap();
        let launched = dir.join("editor-launched");
        let editor = path_recording_editor(&dir, &launched);

        let mut misses: Vec<String> = Vec::new();
        for (macro_name, needle, kind) in [
            ("mkrole", "m8-role", "role"),
            ("mkmacro", "m8-macro", "macro"),
        ] {
            let (status, everything) = run_headless_macro(&dir, macro_name, &editor);
            if !everything.contains(&format!(">> .{kind} {needle}")) {
                misses.push(format!(
                    "`--headless --macro {macro_name}` never echoed its step (did the macro run at \
                 all?) (exit {status}):\n{everything}"
                ));
            }
            let created = files_named(&dir, needle);
            if !created.is_empty() {
                misses.push(format!(
                    "headless `.{kind} {needle}` created {created:?} (exit {status}):\n{everything}"
                ));
            }
            if launched.exists() {
                misses.push(format!(
                "headless `.{kind} {needle}` launched the editor ({}; exit {status}):\n{everything}",
                fs::read_to_string(&launched).unwrap_or_default().trim()
            ));
                let _ = fs::remove_file(&launched);
            }
            let refusal = format!(
                "Creating {kind} '{needle}' needs a human to answer the prompt and none is attached \
             to this run; run it without --headless or --acp-server"
            );
            if !everything.contains(&refusal) {
                misses.push(format!(
                    "headless `.{kind} {needle}` did not give the refusal `{refusal}` (exit \
                 {status}):\n{everything}"
                ));
            }
        }
        assert!(
            misses.is_empty(),
            "{} headless expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }
}

mod zombie_hold_editor_parity_and_creation_barriers {
    //! Black-box pins for the bottom pane's process and creation edges: a
    //! backtick `.file` command's no-`wait` orphan dies on Esc AND on Ctrl+C
    //! while its exited shell is still unreaped (the zombie hold), and a
    //! sleeper left behind by a shell that finished on its own is observed;
    //! Ctrl+O and `.edit` report an editor that wrote the file and then
    //! exited 3 with one identical line; a `.macro <new>` created in the
    //! editor (no request made) is not a rewind barrier yet its file exists;
    //! `.role <new>` writes the role file with the editor's content and `n`
    //! creates nothing. The built binary on a pseudo-terminal with the
    //! dry-run model. Unix-only and serial like its sibling pty modules.
    use std::fs;
    use std::time::{Duration, Instant};

    use super::common::pty::Pty;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// Quit from a composer that may hold a draft: Ctrl+C clears a draft (or
    /// quits an idle empty composer); Ctrl+D follows if still up.
    fn quit_clearing_any_draft_and_check(pty: &mut Pty) -> String {
        pty.send(CTRL_C).unwrap();
        let after = pty.read_for(HOTKEY_GAP);
        if !after.contains("tty-probe-done") {
            pty.send(CTRL_D).unwrap();
        }
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert!(
            !transcript.contains("panicked"),
            "the UI panicked somewhere:\n{transcript}"
        );
        transcript
    }

    /// Every report above the composer rule that names the editor, cut by
    /// its shape: from the word `editor` (any case) through the first run of
    /// digits that FOLLOWS it, whitespace folded. A repainted frame has no
    /// row breaks, so the word before `editor` may be glued to the previous
    /// cell's text and is left out of the cut.
    fn editor_report_lines(frame: &str) -> Vec<String> {
        let flat = above_rule(frame)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let lower = flat.to_lowercase();
        let mut reports = Vec::new();
        let mut start = 0;
        while let Some(rel) = lower[start..].find("editor") {
            let at = start + rel;
            let tail = &flat[at..];
            let end = tail
                .find(|c: char| c.is_ascii_digit())
                .map(|digits| {
                    digits
                        + tail[digits..]
                            .find(|c: char| !c.is_ascii_digit())
                            .unwrap_or(tail.len() - digits)
                })
                .unwrap_or(tail.len());
            let report = tail[..end].to_string();
            start = (at + end).max(at + "editor".len());
            if report.to_lowercase().contains("exit") || report.to_lowercase().contains("status") {
                reports.push(report);
            }
            if start >= flat.len() {
                break;
            }
        }
        reports
    }

    // ---------------------------------------------------------------------
    // The no-`wait` orphan dies on Esc and on Ctrl+C under the zombie hold.
    // ---------------------------------------------------------------------

    /// Two `.file` turns whose backtick shell backgrounds a sleeper WITHOUT
    /// `wait` and exits (the sleeper keeps the shell's stdout, so the shell
    /// is exited-but-unreaped while it lives). The first turn is ended by
    /// Esc, the second by Ctrl+C. Each sleeper is gone within three seconds
    /// of the key, the composer takes keys again, and the session quits
    /// cleanly. A third turn pairs a sleeper whose shell released the pipe
    /// at once (that command finished on its own, so nothing is held for
    /// it) with a live blocker; the blocker dies, the other sleeper's fate
    /// is reported, not asserted.
    #[test]
    #[serial]
    fn a_no_wait_orphan_dies_on_esc_and_on_ctrl_c_while_its_shell_is_unreaped() {
        let dir = fresh_config_dir("zombie-hold");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let esc_pid = dir.join("esc.pid");
        let ctrlc_pid = dir.join("ctrlc.pid");
        let finished_pid = dir.join("finished.pid");
        let blocker_pid = dir.join("blocker.pid");
        let orphan_esc = write_script(
            &dir,
            "orphan-esc.sh",
            &format!(
                "#!/bin/sh\nsleep 6181 & echo $! > '{}'\necho backgrounded\n",
                esc_pid.display()
            ),
        );
        let orphan_ctrlc = write_script(
            &dir,
            "orphan-ctrlc.sh",
            &format!(
                "#!/bin/sh\nsleep 6182 & echo $! > '{}'\necho backgrounded\n",
                ctrlc_pid.display()
            ),
        );
        let finished = write_script(
            &dir,
            "finished.sh",
            &format!(
                "#!/bin/sh\nsleep 6183 </dev/null >/dev/null 2>&1 & echo $! > '{}'\necho released\n",
                finished_pid.display()
            ),
        );
        let blocker = write_script(
            &dir,
            "blocker.sh",
            &format!(
                "#!/bin/sh\nsleep 6184 & echo $! > '{}'\nwait\n",
                blocker_pid.display()
            ),
        );
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        let mut misses: Vec<String> = Vec::new();
        let mut sleepers = SleeperPids(Vec::new());
        for (script, pid_file, interrupt, what) in [
            (&orphan_esc, &esc_pid, ESC, "Esc on a no-wait orphan"),
            (
                &orphan_ctrlc,
                &ctrlc_pid,
                CTRL_C,
                "Ctrl+C on a no-wait orphan",
            ),
        ] {
            let turn_at = pty.raw().len();
            pty.send(format!(".file `sh {script}` -- summarize\r").as_bytes())
                .unwrap();
            let pid = wait_for_pid_in(&mut pty, pid_file, &format!("{what}: the sleeper's pid"));
            sleepers.0.push(pid);
            assert!(
                alive(pid),
                "{what}: the sleeper (pid {pid}) is gone before the key:\n{}",
                since(&pty, turn_at)
            );
            // Let the shell exit so the key lands on an exited-but-unreaped
            // leader, which is the case under test. The turn cannot have
            // ended on its own: the orphan still holds the shell's stdout, so
            // the capture never reaches EOF while it lives.
            pty.read_for(Duration::from_millis(1200));
            pty.send(interrupt).unwrap();
            let key_at = Instant::now();
            if wait_for_process_gone(pid, Duration::from_secs(3)) {
                sleepers.forget(pid);
            } else {
                misses.push(format!(
                    "{what}: the sleeper (pid {pid}) outlived the key by three seconds:\n{}",
                    since(&pty, turn_at)
                ));
            }
            assert_composer_takes_keys(&mut pty, turn_at, what);
            if key_at.elapsed() > Duration::from_secs(10) {
                misses.push(format!(
                    "{what}: took {:?} to end the backtick turn",
                    key_at.elapsed()
                ));
            }
            let after = since(&pty, turn_at);
            if after.contains("panicked") {
                misses.push(format!("{what}: the UI panicked:\n{after}"));
            }
        }

        let turn_at = pty.raw().len();
        pty.send(format!(".file `sh {finished}` `sh {blocker}` -- summarize\r").as_bytes())
            .unwrap();
        let finished_sleeper =
            wait_for_pid_in(&mut pty, &finished_pid, "the finished shell's sleeper pid");
        sleepers.0.push(finished_sleeper);
        let blocker_sleeper = wait_for_pid_in(&mut pty, &blocker_pid, "the blocker sleeper's pid");
        sleepers.0.push(blocker_sleeper);
        pty.read_for(Duration::from_millis(1200));
        pty.send(ESC).unwrap();
        let blocker_gone = wait_for_process_gone(blocker_sleeper, Duration::from_secs(3));
        let finished_gone = wait_for_process_gone(finished_sleeper, Duration::from_secs(1));
        if blocker_gone {
            sleepers.forget(blocker_sleeper);
        } else {
            misses.push(format!(
                "the live blocker's grandchild (pid {blocker_sleeper}) outlived Esc:\n{}",
                since(&pty, turn_at)
            ));
        }
        if finished_gone {
            sleepers.forget(finished_sleeper);
        } else {
            // Alive a moment ago and this test's own sleeper, which no held
            // group will ever reach: end it here rather than leave it behind.
            // SAFETY: kill takes a pid and a signal number and touches no memory.
            let _ = unsafe { libc::kill(finished_sleeper, libc::SIGKILL) };
            sleepers.forget(finished_sleeper);
        }
        eprintln!(
            "observation: the sleeper left by the shell that finished on its own (pid {finished_sleeper}) {} Esc",
            if finished_gone {
                "DIED on"
            } else {
                "SURVIVED (nothing is held for a finished command)"
            }
        );
        assert_composer_takes_keys(&mut pty, turn_at, "Esc on the finished + blocker pair");

        quit_clearing_any_draft_and_check(&mut pty);
        assert!(
            misses.is_empty(),
            "{} zombie-hold expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // Ctrl+O and `.edit config` report an editor that wrote the file and
    // then exited 3 with one identical line.
    // ---------------------------------------------------------------------

    /// Opens an `$EDITOR` script via `open`; returns the new editor report
    /// lines above the composer and the frame they were read from.
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
        dry_run_exchange(&mut pty, &format!("warm-up-{scenario}"));
        let mut toggle = false;
        let before = next_frame(&mut pty, &mut toggle);
        let reports_before = editor_report_lines(&before);

        let edit_at = pty.raw().len();
        if open == CTRL_O {
            pty.send(b"draft-text").unwrap();
            pty.read_for(Duration::from_millis(300));
        }
        pty.send(open).unwrap();
        wait_for_lend_and_return(&mut pty, edit_at, what);
        pty.read_for(Duration::from_millis(1200));
        let frame = next_frame(&mut pty, &mut toggle);
        let reports: Vec<String> = editor_report_lines(&frame)
            .into_iter()
            .filter(|line| !reports_before.contains(line))
            .collect();
        let settled_at = pty.raw().len();
        clear_any_draft(&mut pty);
        assert_composer_takes_keys(&mut pty, settled_at, what);
        quit_clearing_any_draft_and_check(&mut pty);
        (reports, frame)
    }

    /// An editor that appends ` edited-by-script` to the file it was handed
    /// and exits 3: Ctrl+O (with a draft) and `.edit config` each leave
    /// exactly one report naming the exit status 3, and the two reports are
    /// the same text.
    #[test]
    #[serial]
    fn ctrl_o_and_dot_edit_report_a_modifying_editor_that_exits_3_identically() {
        const EDITOR: &str = "#!/bin/sh\nprintf ' edited-by-script\\n' >> \"$1\"\nexit 3\n";
        let (from_ctrl_o, ctrl_o_frame) =
            editor_report_after(CTRL_O, "Ctrl+O", EDITOR, "ctrl-o-mod-3");
        let (from_dot_edit, dot_edit_frame) = editor_report_after(
            b".edit config\r",
            "`.edit config`",
            EDITOR,
            "dot-edit-mod-3",
        );
        let mut misses: Vec<String> = Vec::new();
        if from_ctrl_o.len() != 1 {
            misses.push(format!(
            "Ctrl+O into an editor exiting 3 left {} report(s) {from_ctrl_o:?}:\n{ctrl_o_frame}",
            from_ctrl_o.len()
        ));
        }
        if from_dot_edit.len() != 1 {
            misses.push(format!(
            "`.edit config` into an editor exiting 3 left {} report(s) {from_dot_edit:?}:\n{dot_edit_frame}",
            from_dot_edit.len()
        ));
        }
        if !from_ctrl_o.iter().all(|r| r.contains('3'))
            || !from_dot_edit.iter().all(|r| r.contains('3'))
        {
            misses.push(format!(
            "a report does not carry the editor's status 3: Ctrl+O {from_ctrl_o:?}, `.edit` {from_dot_edit:?}"
        ));
        }
        if from_ctrl_o != from_dot_edit {
            misses.push(format!(
            "Ctrl+O and `.edit config` report the editor differently:\n  Ctrl+O: {from_ctrl_o:?}\n  .edit:  {from_dot_edit:?}\n{ctrl_o_frame}\n----\n{dot_edit_frame}"
        ));
        }
        assert!(
            misses.is_empty(),
            "{} editor-parity expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // `.macro <new>` created in the editor is not a rewind barrier.
    // ---------------------------------------------------------------------

    /// Answers a `Create ...` confirm with `key` after the idle gap and, on
    /// `y`, waits for the editor's lend to return. Returns the offset the
    /// command was sent at.
    fn create_via_modal(pty: &mut Pty, command: &str, key: &[u8], what: &str) -> usize {
        let at = pty.raw().len();
        pty.send(format!("{command}\r").as_bytes()).unwrap();
        pty.wait_for_after("Create", at, PTY_DEADLINE)
            .unwrap_or_else(|e| panic!("`{command}` did not ask to create in a modal: {e}"));
        pty.read_for(HOTKEY_GAP);
        let answered = pty.raw().len();
        pty.send(key).unwrap();
        if key == b"y" {
            wait_for_lend_and_return(pty, answered, what);
            pty.read_for(Duration::from_millis(1200));
        } else {
            pty.read_for(Duration::from_millis(1200));
        }
        at
    }

    /// `before-create`, `.macro fresh-macro` (confirm `y`, editor writes a
    /// one-step body, exit 0), `after-create`. The macro file exists. Esc
    /// Esc: the picker offers BOTH prompts with no "Rewind stops at the last
    /// macro turn" notice; choosing `before-create` pops both and prefills
    /// it.
    #[test]
    #[serial]
    fn creating_a_macro_in_the_editor_is_not_a_rewind_barrier_and_writes_its_file() {
        let dir = fresh_config_dir("macro-create-barrier");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let editor = write_script(
            &dir,
            "macro-editor.sh",
            "#!/bin/sh\nprintf 'description: a macro made in the editor\\nisolated: false\\nsteps:\\n  - fresh-macro-step\\n' > \"$1\"\nexit 0\n",
        );
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        dry_run_exchange(&mut pty, "before-create");
        let create_at = create_via_modal(&mut pty, ".macro fresh-macro", b"y", "the macro editor");
        let mut misses: Vec<String> = Vec::new();
        let created = files_named(&dir, "fresh-macro");
        if !created.iter().any(|p| p.is_file()) {
            misses.push(format!(
                "`.macro fresh-macro` + `y` + editor wrote no macro file (found {created:?}):\n{}",
                since(&pty, create_at)
            ));
        } else {
            let body = fs::read_to_string(&created[0]).unwrap_or_default();
            if !body.contains("fresh-macro-step") {
                misses.push(format!(
                    "the macro file does not hold the editor's body: {body:?}"
                ));
            }
        }
        let after_create = since(&pty, create_at);
        if squeezed(&after_create).contains("fresh-macro-step") {
            misses.push(format!(
            "creating the macro ran its step (the creation requested the model):\n{after_create}"
        ));
        }
        clear_any_draft(&mut pty);
        dry_run_exchange(&mut pty, "after-create");

        let picker_at = open_rewind_picker(&mut pty);
        let picker = repainted_frame_after_resize(&mut pty, 60, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if !band_flat.contains("after-create") || !band_flat.contains("before-create") {
            misses.push(format!(
                "the picker does not offer both prompts across `.macro <new>` creation:\n{picker}"
            ));
        }
        if squeezed(&since(&pty, picker_at)).contains(&squeezed(STOP_NOTICE)) {
            misses.push(format!(
                "`.macro <new>` creation was taken for a barrier (a stop notice is shown):\n{}",
                since(&pty, picker_at)
            ));
        }
        pty.send(DOWN).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        if flat.contains("Rewindto") {
            misses.push(format!("the picker is still up after Enter:\n{frame}"));
        }
        if flat.contains(">after-create") || flat.contains(">before-create") {
            misses.push(format!(
                "choosing the older row did not pop both exchanges:\n{frame}"
            ));
        }
        let row = composer_row_with_footer(&frame, "dryrun:dry-model");
        if row != "before-create" {
            misses.push(format!(
                "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
            ));
        }
        clear_any_draft(&mut pty);
        quit_clearing_any_draft_and_check(&mut pty);
        assert!(
            misses.is_empty(),
            "{} macro-creation expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }

    // ---------------------------------------------------------------------
    // `.role <new>` writes the role file with the editor's content; `n`
    // creates nothing.
    // ---------------------------------------------------------------------

    /// `.role declined-role` + `n`: no file. `.role fresh-role` + `y`: the
    /// editor (appends a marker line, exits 0) is lent the terminal and
    /// afterwards a file named for the role exists under the config dir and
    /// carries the marker; no editor report is shown; the UI takes keys and
    /// quits cleanly.
    #[test]
    #[serial]
    fn dot_role_new_writes_the_role_file_with_the_editors_content_and_n_creates_nothing() {
        let dir = fresh_config_dir("role-create");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let editor = write_script(
            &dir,
            "role-editor.sh",
            "#!/bin/sh\nprintf 'role-marker-line\\n' >> \"$1\"\nexit 0\n",
        );
        let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(40, 100).unwrap();
        pty.read_for(Duration::from_millis(400));
        let mut misses: Vec<String> = Vec::new();

        let declined_at =
            create_via_modal(&mut pty, ".role declined-role", b"n", "the declined role");
        let declined = files_named(&dir, "declined-role");
        if !declined.is_empty() {
            misses.push(format!(
                "`n` at the create-role confirm still created {declined:?}:\n{}",
                since(&pty, declined_at)
            ));
        }
        if since(&pty, declined_at).contains("\u{1b}[?1049l") {
            misses.push("`n` lent the terminal to an editor".to_string());
        }
        clear_any_draft(&mut pty);

        let create_at = create_via_modal(&mut pty, ".role fresh-role", b"y", "the role editor");
        let created = files_named(&dir, "fresh-role");
        match created.iter().find(|p| p.is_file()) {
            None => misses.push(format!(
                "`.role fresh-role` + `y` + editor wrote no role file (found {created:?}):\n{}",
                since(&pty, create_at)
            )),
            Some(file) => {
                let body = fs::read_to_string(file).unwrap_or_default();
                if !body.contains("role-marker-line") {
                    misses.push(format!(
                        "the role file {} does not hold the editor's line: {body:?}",
                        file.display()
                    ));
                }
            }
        }
        let mut toggle = false;
        let frame = next_frame(&mut pty, &mut toggle);
        let reports = editor_report_lines(&frame);
        if !reports.is_empty() {
            misses.push(format!(
                "a clean (exit 0) role editor was reported: {reports:?}\n{frame}"
            ));
        }
        let settled_at = pty.raw().len();
        clear_any_draft(&mut pty);
        assert_composer_takes_keys(&mut pty, settled_at, "the role editor");
        quit_clearing_any_draft_and_check(&mut pty);
        assert!(
            misses.is_empty(),
            "{} role-creation expectation(s) missed:\n\n{}",
            misses.len(),
            misses.join("\n\n")
        );
    }
}
