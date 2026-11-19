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
//! suites.
#![cfg(unix)]

mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::fake_llm::{FakeLlm, Reply};
use common::tui_harness::*;
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

// ---------------------------------------------------------------------
// The mid-turn cell: dim in colour, a rewind non-entity.
// ---------------------------------------------------------------------

/// After a plain exchange, a tool turn gets a line while its tool sleeps;
/// once the model read it the transcript carries the line as a cell
/// painted dim with its `(sent mid-turn)` marker. Esc Esc then offers
/// the tool turn's opener and the older prompt, never the mid-turn line,
/// and no macro-stop notice; choosing the opener pops the tool turn with
/// its mid-turn cell and reply, keeps the older exchange, and prefills
/// the opener.
#[test]
#[serial]
fn a_mid_turn_cell_is_dim_and_neither_a_rewind_row_nor_a_barrier() {
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
    // The committed cell: marker + dim.
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
        if !ends_dim(&params) {
            misses.push(format!(
                "the mid-turn cell is not painted dim (SGR state before it: {:?}):\n{frame}",
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
        .unwrap_or_else(|e| panic!("Ctrl+D during the tool round did not end the session: {e}"));
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
