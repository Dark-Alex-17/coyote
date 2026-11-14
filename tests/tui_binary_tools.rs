//! Real-process checks of the questions a tool asks through the prompt
//! protocol: its confirm, list, checkbox and input questions as modals
//! under the UI and inquire prompts in the plain REPL, and what the tool
//! gets back for a typed, exact-option or blank answer on the custom row.
//! Next to them: the confirm a rebuild over a failing document asks, the
//! select an MCP server conflict asks during `.install`, the `init RAG?`
//! question an agent with documents asks when it starts, and the `.rag`
//! wizard and macro steps that reach those questions.
//! After them: the Ctrl+T pager over a tool's whole output with its find
//! and thread picker, the F3 roster cancelling a background job, and both
//! with nothing to show. The transcript printed when the UI leaves is
//! checked in `tui_binary_wizards.rs`.
//! Each case runs the binary on a pseudo-terminal under a bash wrapper
//! that reports the exit code and `stty -a` afterwards, so the file is
//! unix-only. The cases run one at a time: each spawn forks a session
//! leader and takes a controlling tty, and a fork landing between another
//! test's `openpty` and its `exec` has failed the spawn with a garbled
//! errno.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::fake_llm::{FakeLlm, Reply};
use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
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
/// characters is drawn in its stored order without them: the modal reads
/// `Delete txt.safe k7m?`, none of the characters reaches the terminal
/// (where a real emulator would reorder or hide the text around them),
/// `y` still answers the tool as a yes, and nothing of them is left on
/// the normal screen after the UI leaves.
#[test]
#[serial]
fn a_tools_confirm_question_with_format_characters_is_drawn_without_them() {
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
        squeezed(&since).contains("Deletetxt.safek7m?q9d-q"),
        "the question was not drawn in its stored order with the format characters dropped:\n{since}"
    );
    for (name, bytes) in FORMAT_CHARACTERS {
        assert!(
            find(pty.raw(), bytes, submitted_at).is_none(),
            "{name} reached the terminal while the modal was up:\n{since}"
        );
    }

    pty.send(b"y").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done f8c", "the reply after the tool");
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
fn dot_install_with_a_conflicting_mcp_server_asks_in_the_select_modal_and_keep_local_keeps_it() {
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
fn an_install_with_stdout_redirected_and_a_conflicting_mcp_server_is_refused_naming_a_terminal() {
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
        &[("COYOTE_UI", "repl")],
        &wait_wrapper_with_stdout_to(&captured),
        "/dev/tty",
        &format!(" --install '{}'", repo.display()),
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
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let asked_at = pty.raw().len();
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
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
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

    pty.read_for(Duration::from_millis(300));
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
    let transcript = wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
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
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
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
        !flat.contains("Aborted") && !flat.contains("Installed") && !flat.contains("Reinstalled"),
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
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let asked_at = pty.raw().len();
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
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
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

    pty.read_for(Duration::from_millis(300));
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
    let transcript = wait_for_squeezed(&mut pty, "fixture done p5y", "the reply after the tool");
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
        !flat.contains("Refusingto") && !flat.contains("Abortedmerge") && !flat.contains("Error:"),
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
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let install_at = pty.raw().len();
    pty.send(format!(".install {}\r", repo.display()).as_bytes())
        .unwrap();
    pty.wait_for_after("Conflict on MCP server", install_at, PTY_DEADLINE)
        .expect("`.install` did not ask about the first MCP conflict");
    pty.read_for(Duration::from_millis(600));
    let asked = strip_ansi(&String::from_utf8_lossy(&pty.raw()[install_at..]));
    let first_is_server = asked.contains("Conflict on MCP server 'bundle-k7s-server'");
    pty.send(b"\x1b").unwrap();
    wait_for_squeezed(
        &mut pty,
        "the question was cancelled.",
        "the note after Esc on the first MCP conflict",
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
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let asked_at = pty.raw().len();
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
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done e0d", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        "[typed-e0d]",
        "{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.read_for(Duration::from_millis(300));
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
    let transcript = wait_for_squeezed(&mut pty, "fixture done e0d", "the reply after the tool");
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
        let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", ui)], WAIT_WRAPPER, "/dev/tty");
        if ui == "repl" {
            pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
        } else {
            pty.wait_for("no session", PTY_DEADLINE).unwrap();
        }
        pty.read_for(Duration::from_millis(300));
        let asked_at = pty.raw().len();
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
        pty.send(b"\x1b").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer");
        wait_for_squeezed(&mut pty, "fixture done c3b", "the reply after the tool");
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "cancelled",
            "[{ui}] Esc on the checkbox question did not reach the tool as a cancel"
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(if ui == "repl" {
            b".exit\r" as &[u8]
        } else {
            b"\x04"
        })
        .unwrap();
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
    let mut pty = spawn_binary_under(
        built_binary(),
        &dir,
        &[("COYOTE_UI", "repl")],
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
    let transcript = wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
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
    let (answer, status, transcript) = list_custom_answer_under_the_ui("tool-list-typed-one", b"1");
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
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");

    pty.wait_for("fake-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let asked_at = pty.raw().len();
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
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done q7l", "the reply after the tool");
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

    pty.read_for(Duration::from_millis(300));
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
    let transcript = wait_for_squeezed(&mut pty, "fixture done p5y", "the reply after the tool");
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
    let inline = repainted_frame(&mut pty, 30, 100);
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
    let frame = repainted_frame_showing(&mut pty, "no session", "q did not give the screen back");
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

/// The first fully repainted frame that shows `needle`, panicking with
/// `what` after a few repaints without it. The pager covers cells that may
/// already hold the same characters, so a diff paint read straight off the
/// pty can miss part of a string the screen does show.
fn repainted_frame_showing(pty: &mut Pty, needle: &str, what: &str) -> String {
    let mut frame = String::new();
    for rows in [30, 32, 30, 32, 30] {
        frame = repainted_frame(pty, rows, 100);
        if squeezed(&frame).contains(&squeezed(needle)) {
            return frame;
        }
    }
    panic!("{what}:\n{frame}");
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
    let found = repainted_frame_showing(&mut pty, "pg-37", "find did not bring pg-37 on screen");
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
        "Threads  Enter = open  Esc = back",
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
        !squeezed(&back).contains(&squeezed("Threads  Enter = open  Esc = back")),
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

/// A tool that records its pid at `pid_file` and then becomes a sleep far
/// longer than a test runs, so the recorded pid is the sleeper's.
fn sleeping_tool(pid_file: &Path) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         \n\
         # @describe Sleep for a long time.\n\
         \n\
         # @env LLM_OUTPUT=/dev/stdout The output path\n\
         \n\
         main() {{\n\
         \x20   echo $$ > \"{}\"\n\
         \x20   exec sleep 300\n\
         }}\n",
        pid_file.display()
    )
}

fn alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks the pid and touches no memory.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Kills the recorded sleepers when the test ends, so a failed assertion
/// does not leave them running for five minutes.
struct Sleepers(Vec<PathBuf>);

impl Drop for Sleepers {
    fn drop(&mut self) {
        for file in &self.0 {
            if let Some(pid) = fs::read_to_string(file)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                // SAFETY: kill takes a pid and a signal number and touches no memory.
                let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
}

/// The roster band of a repainted frame: from its legend down to the rule
/// under it.
fn roster_band(frame: &str) -> &str {
    let legend = frame
        .rfind("Esc = close")
        .unwrap_or_else(|| panic!("the roster's legend is not on screen:\n{frame}"));
    frame[legend..].split('\u{2500}').next().unwrap_or_default()
}

/// A job the model started in the background and the user left running
/// by interrupting the turn shows on the F3 roster as running; `x` on its
/// row cancels it, so its process goes away and the row stops saying
/// running.
#[test]
#[serial]
fn f3_lists_a_job_left_running_and_x_cancels_it() {
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

    pty.send(b"\x1bOR").unwrap();
    let roster = repainted_frame_showing(&mut pty, "fixture_sleep", "F3 did not list the job");
    let band = roster_band(&roster);
    assert!(
        band.contains("fixture_sleep") && band.contains("running"),
        "the roster does not list the job as running: {band:?}\n{roster}"
    );
    pty.send(b"x").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while alive(pid) && std::time::Instant::now() < deadline {
        pty.read_for(Duration::from_millis(200));
    }
    let gone = !alive(pid);
    assert!(
        gone,
        "x did not stop the job's process {pid}; roster was {band:?}"
    );
    // The repaint is the screen's rows in order with the escapes gone:
    // the transcript's job row, then the roster band under its legend.
    let after = repainted_frame_showing(&mut pty, "Cancelled job_", "x reported no cancel");
    let at = after
        .find("job fixture_sleep")
        .unwrap_or_else(|| panic!("the transcript lost the job's row:\n{after}"));
    let job_row: String = after[at..].chars().take(60).collect();
    assert!(
        job_row.contains("cancelled"),
        "the job row does not say cancelled after x: {job_row:?}\n{after}"
    );
    let band = roster_band(&after);
    assert!(
        !band.contains("fixture_sleep"),
        "the roster still lists the cancelled job: {band:?}"
    );

    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(400));
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

/// F3 with no task and no question shows the empty roster; `x` and `o`
/// on it change nothing, Esc closes it and the composer takes keys.
#[test]
#[serial]
fn f3_with_nothing_running_shows_the_empty_roster_and_x_and_o_are_harmless() {
    let dir = fresh_config_dir("roster-empty");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"\x1bOR").unwrap();
    repainted_frame_showing(
        &mut pty,
        "(no tasks and no questions)",
        "F3 did not show the empty roster",
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
